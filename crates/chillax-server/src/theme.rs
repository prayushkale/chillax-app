//! Theme serving. The default theme is embedded in the binary; others are plain
//! directories of static files.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::Extension;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../web/themes/default/"]
struct DefaultTheme;

/// Every file in the embedded default theme, for tests.
#[cfg(test)]
pub(crate) fn embedded_paths() -> Vec<String> {
    DefaultTheme::iter().map(|p| p.to_string()).collect()
}

#[derive(Debug, Clone)]
pub enum Theme {
    Embedded,
    Directory(PathBuf),
}

impl Theme {
    pub fn embedded() -> Self {
        Self::Embedded
    }

    pub fn directory(path: impl Into<PathBuf>) -> Self {
        Self::Directory(path.into())
    }

    async fn load(&self, path: &str) -> Option<Vec<u8>> {
        match self {
            Self::Embedded => DefaultTheme::get(path).map(|f| f.data.into_owned()),
            Self::Directory(root) => {
                let rel = Path::new(path);
                // No `..` or absolute paths: stay inside the theme directory.
                if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
                    return None;
                }
                tokio::fs::read(root.join(rel)).await.ok()
            }
        }
    }
}

pub async fn serve(Extension(theme): Extension<Arc<Theme>>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    // Unknown non-asset paths fall back to the SPA entry point.
    let (path, bytes) = match theme.load(path).await {
        Some(bytes) => (path, bytes),
        None if !path.contains('.') => match theme.load("index.html").await {
            Some(bytes) => ("index.html", bytes),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, content_type(path))], bytes).into_response()
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
