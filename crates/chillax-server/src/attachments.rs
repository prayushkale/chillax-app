//! Image attachments over HTTP: raw-body upload and content-addressed download.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chillax_core::attachment::{
    Attachment, AttachmentError, MAX_ATTACHMENT_BYTES, clean_name, is_valid_id,
};
use chillax_core::{AttachmentStore, Command};
use serde_json::json;

use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/attachments",
            // A little over the cap so the store, not axum, produces the message.
            post(upload).layer(DefaultBodyLimit::max(MAX_ATTACHMENT_BYTES as usize + 4096)),
        )
        .route("/attachments/{id}", get(download))
}

fn failure(err: AttachmentError) -> Response {
    let status = match err {
        AttachmentError::BadType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        AttachmentError::BadSize => StatusCode::PAYLOAD_TOO_LARGE,
        AttachmentError::NotFound => StatusCode::NOT_FOUND,
        AttachmentError::OutsideRoots(_) => StatusCode::FORBIDDEN,
        AttachmentError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, axum::Json(json!({"error": err.to_string()}))).into_response()
}

/// `POST /attachments`: the body is the image; `x-filename` names it. The type comes
/// from the bytes, so `content-type` is ignored.
async fn upload(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(store) = app.attachments.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let name = headers
        .get("x-filename")
        .and_then(|v| v.to_str().ok())
        .map(percent_decode)
        .unwrap_or_default();
    match tokio::task::spawn_blocking(move || store.put(&name, &body)).await {
        Ok(Ok(attachment)) => axum::Json(attachment).into_response(),
        Ok(Err(err)) => failure(err),
        Err(err) => failure(AttachmentError::Io(err.to_string())),
    }
}

async fn download(State(app): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(store) = app.attachments.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !is_valid_id(&id) {
        return failure(AttachmentError::NotFound);
    }
    let found = tokio::task::spawn_blocking(move || {
        let meta = store.stat(&id)?;
        let path = store.path(&id)?;
        let bytes = std::fs::read(path).map_err(|e| AttachmentError::Io(e.to_string()))?;
        Ok::<_, AttachmentError>((meta.mime, bytes))
    })
    .await;
    match found {
        Ok(Ok((mime, bytes))) => (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_str(&mime).expect("mime is ascii"),
                ),
                (
                    header::X_CONTENT_TYPE_OPTIONS,
                    HeaderValue::from_static("nosniff"),
                ),
                (
                    header::CONTENT_SECURITY_POLICY,
                    HeaderValue::from_static("default-src 'none'; sandbox"),
                ),
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("private, max-age=31536000, immutable"),
                ),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(err)) => failure(err),
        Err(err) => failure(AttachmentError::Io(err.to_string())),
    }
}

/// Browsers send non-ASCII names percent-encoded in a header.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Replace what a client claims about each attachment with what the store holds, and
/// refuse ids it does not have. Only the (cleaned) name is the client's.
pub fn resolve(store: &dyn AttachmentStore, cmd: &mut Command) -> Result<(), AttachmentError> {
    if let Command::CommentTicket { attachments, .. } = cmd {
        for a in attachments.iter_mut() {
            let stored = store.stat(&a.id)?;
            *a = Attachment {
                name: clean_name(&a.name),
                ..stored
            };
        }
    }
    Ok(())
}
