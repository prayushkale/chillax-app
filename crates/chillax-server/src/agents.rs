//! `/api/v1/agents`: the coding agents and models the stakeholder can hire on.
//! Additive to v1.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chillax_core::AgentDirectoryError;
use serde_json::json;

use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/agents", get(list))
        .route("/agents/{name}/models", get(models))
        .route("/agents/{name}/efforts", get(efforts))
        .route("/agents/{name}/enable", post(enable))
}

fn missing() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "no agent directory on this server"})),
    )
        .into_response()
}

fn error(err: AgentDirectoryError) -> Response {
    let status = match &err {
        AgentDirectoryError::Unknown(_) => StatusCode::NOT_FOUND,
        AgentDirectoryError::Agent(_) => StatusCode::BAD_GATEWAY,
        AgentDirectoryError::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(json!({"error": err.to_string()}))).into_response()
}

/// Configured backends, then installed presets that can be added.
async fn list(State(app): State<AppState>) -> Response {
    match &app.agents {
        Some(d) => Json(d.backends().await).into_response(),
        None => missing(),
    }
}

/// The backend's models; the first call starts the agent (no prompt is sent).
async fn models(State(app): State<AppState>, Path(name): Path<String>) -> Response {
    let Some(d) = &app.agents else {
        return missing();
    };
    match d.models(&name).await {
        Ok(models) => Json(models).into_response(),
        Err(err) => error(err),
    }
}

/// The backend's effort levels, `[]` when it has none.
async fn efforts(State(app): State<AppState>, Path(name): Path<String>) -> Response {
    let Some(d) = &app.agents else {
        return missing();
    };
    match d.efforts(&name).await {
        Ok(efforts) => Json(efforts).into_response(),
        Err(err) => error(err),
    }
}

/// Add a known preset to `chillax.toml`.
async fn enable(State(app): State<AppState>, Path(name): Path<String>) -> Response {
    let Some(d) = &app.agents else {
        return missing();
    };
    match d.enable(&name).await {
        Ok(info) => Json(info).into_response(),
        Err(err) => error(err),
    }
}
