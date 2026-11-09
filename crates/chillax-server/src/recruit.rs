//! `/api/v1` routes for hiring from the persona catalog. Additive to v1.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chillax_core::{Actor, Command, RecruitError, Recruiter, Role};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/catalog", get(status))
        .route("/catalog/refresh", post(refresh))
        .route("/personas", get(personas))
        .route("/drafts", post(create_draft))
        .route("/drafts/{id}", get(get_draft).delete(discard))
        .route("/drafts/{id}/revise", post(revise))
        .route("/drafts/{id}/tailor", post(tailor))
        .route("/drafts/{id}/undo", post(undo))
        .route("/drafts/{id}/base", post(set_base))
        .route("/drafts/{id}/adopt", post(adopt))
}

/// The public JSON shape of a role (the prompt is left out; it is in the role file).
pub fn role_json(r: &Role) -> serde_json::Value {
    json!({
        "id": r.id,
        "title": r.title,
        "description": r.description,
        "default_backend": r.default_backend,
        "model": r.model,
        "mcp_tools": r.mcp_tools,
        "permissions": r.permissions,
        "base_role": r.base_role,
        "source": r.source,
        "tags": r.tags,
        "owns_states": r.owns_states,
    })
}

fn recruiter(app: &AppState) -> Result<Arc<dyn Recruiter>, RecruitError> {
    app.recruiter.clone().ok_or(RecruitError::Disabled)
}

fn error(err: RecruitError) -> Response {
    let status = match &err {
        RecruitError::Disabled
        | RecruitError::UnknownPersona(_)
        | RecruitError::UnknownRole(_)
        | RecruitError::UnknownDraft(_) => StatusCode::NOT_FOUND,
        RecruitError::NotReady => StatusCode::SERVICE_UNAVAILABLE,
        RecruitError::NothingToUndo => StatusCode::CONFLICT,
        RecruitError::Editor(_) | RecruitError::Refresh(_) => StatusCode::BAD_GATEWAY,
        RecruitError::Command(_) => StatusCode::UNPROCESSABLE_ENTITY,
        RecruitError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(json!({"error": err.to_string()}))).into_response()
}

fn reply<T: serde::Serialize>(result: Result<T, RecruitError>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(err) => error(err),
    }
}

async fn status(State(app): State<AppState>) -> Response {
    match recruiter(&app) {
        Ok(r) => Json(r.status().await).into_response(),
        Err(err) => error(err),
    }
}

async fn refresh(State(app): State<AppState>) -> Response {
    match recruiter(&app) {
        Ok(r) => reply(r.refresh().await),
        Err(err) => error(err),
    }
}

#[derive(Deserialize)]
struct PersonaQuery {
    #[serde(default)]
    q: String,
    /// Every persona (for clients that search locally); ignores `q` and `limit`.
    #[serde(default)]
    all: bool,
    #[serde(default = "ten")]
    limit: usize,
}

fn ten() -> usize {
    10
}

/// Featured personas when `q` is empty, otherwise search results; `all=true` lists all.
async fn personas(State(app): State<AppState>, Query(query): Query<PersonaQuery>) -> Response {
    let r = match recruiter(&app) {
        Ok(r) => r,
        Err(err) => return error(err),
    };
    let limit = query.limit.clamp(1, 500);
    if query.all {
        reply(r.personas().await)
    } else if query.q.trim().is_empty() {
        reply(r.featured(limit).await)
    } else {
        reply(r.search(&query.q, limit).await)
    }
}

#[derive(Deserialize)]
struct NewDraft {
    /// A catalog persona; give this or `from_role`.
    #[serde(default)]
    persona: Option<String>,
    /// An existing role to copy (works with the catalog disabled); needs `role_id` and `title`.
    #[serde(default)]
    from_role: Option<String>,
    #[serde(default)]
    role_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    /// Defaults to the persona's `suggested_base`.
    #[serde(default)]
    base_role: Option<String>,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

async fn create_draft(State(app): State<AppState>, Json(body): Json<NewDraft>) -> Response {
    let r = match recruiter(&app) {
        Ok(r) => r,
        Err(err) => return error(err),
    };
    if let Some(from) = &body.from_role {
        if body.base_role.is_some() {
            return error(RecruitError::Command(chillax_core::CommandError::Invalid(
                "`base_role` cannot be combined with `from_role`".into(),
            )));
        }
        return reply(
            r.draft_from_role(
                from,
                body.role_id.as_deref().unwrap_or_default(),
                body.title.as_deref().unwrap_or_default(),
                body.backend,
                body.model,
            )
            .await,
        );
    }
    let Some(persona) = &body.persona else {
        return error(RecruitError::Command(chillax_core::CommandError::Invalid(
            "give `persona` or `from_role`".into(),
        )));
    };
    reply(
        r.draft(persona, body.base_role.as_deref(), body.backend, body.model)
            .await,
    )
}

async fn get_draft(State(app): State<AppState>, Path(id): Path<String>) -> Response {
    match recruiter(&app) {
        Ok(r) => reply(r.get_draft(&id).await),
        Err(err) => error(err),
    }
}

async fn discard(State(app): State<AppState>, Path(id): Path<String>) -> Response {
    match recruiter(&app) {
        Ok(r) => {
            r.discard(&id).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => error(err),
    }
}

#[derive(Deserialize)]
struct BaseBody {
    base_role: String,
}

/// Move the draft onto another trusted base role; revisions are kept.
async fn set_base(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<BaseBody>,
) -> Response {
    match recruiter(&app) {
        Ok(r) => reply(r.set_base(&id, &body.base_role).await),
        Err(err) => error(err),
    }
}

#[derive(Deserialize)]
struct ReviseBody {
    instruction: String,
}

/// Runs the editor agent; this can take minutes.
async fn revise(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ReviseBody>,
) -> Response {
    match recruiter(&app) {
        Ok(r) => reply(r.revise(&id, &body.instruction).await),
        Err(err) => error(err),
    }
}

/// Tailors the persona to the project's plan files; runs the editor agent like `revise`.
async fn tailor(State(app): State<AppState>, Path(id): Path<String>) -> Response {
    match recruiter(&app) {
        Ok(r) => reply(r.tailor(&id).await),
        Err(err) => error(err),
    }
}

async fn undo(State(app): State<AppState>, Path(id): Path<String>) -> Response {
    match recruiter(&app) {
        Ok(r) => reply(r.undo(&id).await),
        Err(err) => error(err),
    }
}

#[derive(Deserialize)]
struct AdoptBody {
    #[serde(default)]
    role_id: Option<String>,
    #[serde(default)]
    overwrite: bool,
    /// Hire someone into the new role right away.
    #[serde(default)]
    hire: Option<HireBody>,
}

#[derive(Deserialize)]
struct HireBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
}

async fn adopt(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AdoptBody>,
) -> Response {
    let r = match recruiter(&app) {
        Ok(r) => r,
        Err(err) => return error(err),
    };
    let role = match r.adopt(&id, body.role_id, body.overwrite).await {
        Ok(role) => role,
        Err(err) => return error(err),
    };
    let mut events = Vec::new();
    if let Some(hire) = body.hire {
        let cmd = Command::HireEmployee {
            role: role.id.clone(),
            name: hire.name,
            backend: hire.backend,
            model: hire.model,
            effort: hire.effort,
            keep_defaults: false,
        };
        match app.company.send(Actor::Stakeholder, cmd).await {
            Ok(hired) => events = hired,
            Err(err) => {
                // The role exists now; say so, so the client can retry just the hire.
                let message = format!("adopted role `{}`, but hiring failed: {err}", role.id);
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({"error": message, "role": role_json(&role)})),
                )
                    .into_response();
            }
        }
    }
    Json(json!({"role": role_json(&role), "events": events})).into_response()
}
