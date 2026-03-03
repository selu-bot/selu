/// LLM Provider management API.
///
/// GET  /api/providers         - list all providers
/// PUT  /api/providers/{id}/key    - set (encrypted) API key
/// PUT  /api/providers/{id}/region - set region/base_url
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct ProviderResponse {
    pub id: String,
    pub display_name: String,
    pub has_api_key: bool,
    pub base_url: Option<String>,
    pub active: bool,
}

#[derive(Debug, Deserialize)]
pub struct SetKeyRequest {
    pub api_key: String,
}

#[derive(Debug, Deserialize)]
pub struct SetRegionRequest {
    pub base_url: String,
}

pub async fn list_providers(State(state): State<AppState>) -> impl IntoResponse {
    let rows = sqlx::query!(
        "SELECT id, display_name, api_key_encrypted, base_url, active FROM llm_providers ORDER BY id"
    )
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let providers: Vec<ProviderResponse> = rows
                .into_iter()
                .map(|r| ProviderResponse {
                    id: r.id.unwrap_or_default(),
                    display_name: r.display_name,
                    has_api_key: r
                        .api_key_encrypted
                        .as_ref()
                        .map_or(false, |k| !k.is_empty()),
                    base_url: r.base_url,
                    active: r.active != 0,
                })
                .collect();
            Json(providers).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to list providers: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// PUT /api/providers/{id}/key - encrypt and store an API key
pub async fn set_api_key(
    Path(provider_id): Path<String>,
    State(state): State<AppState>,
    Json(req): Json<SetKeyRequest>,
) -> impl IntoResponse {
    if req.api_key.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "api_key is required"})),
        )
            .into_response();
    }

    // Encrypt the API key
    let encrypted = match state.credentials.encrypt_raw(req.api_key.trim().as_bytes()) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("Failed to encrypt API key: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let result = sqlx::query!(
        "UPDATE llm_providers SET api_key_encrypted = ? WHERE id = ?",
        encrypted,
        provider_id
    )
    .execute(&state.db)
    .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => {
            // Invalidate cached provider so the new key is picked up
            state.provider_cache.invalidate().await;
            Json(serde_json::json!({"ok": true})).into_response()
        }
        Ok(_) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("Failed to update provider key: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// PUT /api/providers/{id}/region - set the base_url / region
pub async fn set_region(
    Path(provider_id): Path<String>,
    State(state): State<AppState>,
    Json(req): Json<SetRegionRequest>,
) -> impl IntoResponse {
    let result = sqlx::query!(
        "UPDATE llm_providers SET base_url = ? WHERE id = ?",
        req.base_url,
        provider_id
    )
    .execute(&state.db)
    .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => {
            // Invalidate cached provider since region/URL changed
            state.provider_cache.invalidate().await;
            Json(serde_json::json!({"ok": true})).into_response()
        }
        Ok(_) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("Failed to update provider region: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
