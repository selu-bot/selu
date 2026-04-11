/// Credential management API.
///
/// These endpoints allow setting and deleting credentials used by capability
/// containers. All values are stored AES-256-GCM encrypted at rest.
///
/// System credentials (shared across all users):
///   PUT  /api/credentials/system/{capability_id}/{name}   {"value": "..."}
///   DELETE /api/credentials/system/{capability_id}/{name}
///   GET  /api/credentials/system/{capability_id}          → [names]
///
/// User credentials (per-user, per-capability):
///   PUT  /api/credentials/user/{user_id}/{capability_id}/{name}   {"value": "..."}
///   DELETE /api/credentials/user/{user_id}/{capability_id}/{name}
///   GET  /api/credentials/user/{user_id}/{capability_id}          → [names]
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct SetCredentialRequest {
    pub value: String,
}

// ── System credentials ────────────────────────────────────────────────────────

pub async fn set_system(
    Path((capability_id, name)): Path<(String, String)>,
    State(state): State<AppState>,
    Json(req): Json<SetCredentialRequest>,
) -> impl IntoResponse {
    match state
        .credentials
        .set_system(&capability_id, &name, &req.value)
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!("Failed to set system credential: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn delete_system(
    Path((capability_id, name)): Path<(String, String)>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    match state.credentials.delete_system(&capability_id, &name).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!("Failed to delete system credential: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn list_system(
    Path(capability_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    match state.credentials.list_system(&capability_id).await {
        Ok(names) => Json(names).into_response(),
        Err(e) => {
            tracing::error!("Failed to list system credentials: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── User credentials ──────────────────────────────────────────────────────────

pub async fn set_user(
    Path((user_id, capability_id, name)): Path<(String, String, String)>,
    State(state): State<AppState>,
    Json(req): Json<SetCredentialRequest>,
) -> impl IntoResponse {
    match state
        .credentials
        .set_user(&user_id, &capability_id, &name, &req.value)
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!("Failed to set user credential: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn delete_user(
    Path((user_id, capability_id, name)): Path<(String, String, String)>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    match state
        .credentials
        .delete_user(&user_id, &capability_id, &name)
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!("Failed to delete user credential: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn list_user(
    Path((user_id, capability_id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    match state.credentials.list_user(&user_id, &capability_id).await {
        Ok(names) => Json(names).into_response(),
        Err(e) => {
            tracing::error!("Failed to list user credentials: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
