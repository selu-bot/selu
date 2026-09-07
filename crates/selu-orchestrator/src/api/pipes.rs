use axum::{
    Json,
    extract::{Path, State},
    http::{StatusCode, header},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    api::auth::{ApiPrincipal, forbidden},
    state::AppState,
};

#[derive(Debug, Serialize)]
pub struct PipeResponse {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub transport: String,
    pub outbound_url: String,
    pub default_agent_id: Option<String>,
    pub active: bool,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CreatePipeRequest {
    pub name: String,
    pub transport: Option<String>,
    pub outbound_url: String,
    pub outbound_auth: Option<String>,
    pub default_agent_id: Option<String>,
}

pub async fn list_pipes(
    principal: ApiPrincipal,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let rows = sqlx::query!(
        "SELECT id, user_id, name, transport, outbound_url, default_agent_id, active, created_at
         FROM pipes ORDER BY created_at DESC"
    )
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let pipes: Vec<PipeResponse> = rows
                .into_iter()
                .filter(|row| principal.is_admin || row.user_id == principal.user_id)
                .map(|r| PipeResponse {
                    id: r.id.unwrap_or_default(),
                    user_id: r.user_id,
                    name: r.name,
                    transport: r.transport,
                    outbound_url: r.outbound_url,
                    default_agent_id: r.default_agent_id,
                    active: r.active != 0,
                    created_at: r.created_at,
                })
                .collect();
            Json(pipes).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to list pipes: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn create_pipe(
    principal: ApiPrincipal,
    State(state): State<AppState>,
    Json(req): Json<CreatePipeRequest>,
) -> impl IntoResponse {
    let id = Uuid::new_v4().to_string();
    let inbound_token = Uuid::new_v4().to_string().replace('-', "");
    let inbound_token_encrypted = match state.credentials.encrypt_raw(inbound_token.as_bytes()) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(error = %error, "Failed to encrypt inbound pipe token");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let outbound_auth_encrypted = match req
        .outbound_auth
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| state.credentials.encrypt_raw(value.as_bytes()))
        .transpose()
    {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(error = %error, "Failed to encrypt outbound pipe authorization");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let transport = req.transport.unwrap_or_else(|| "webhook".to_string());
    let user_id_str = principal.user_id.clone();

    let result = sqlx::query(
        "INSERT INTO pipes \
         (id, user_id, name, transport, inbound_token, inbound_token_encrypted, \
          outbound_url, outbound_auth, outbound_auth_encrypted, default_agent_id) \
         VALUES (?, ?, ?, ?, '', ?, ?, NULL, ?, ?)",
    )
    .bind(&id)
    .bind(&user_id_str)
    .bind(&req.name)
    .bind(&transport)
    .bind(&inbound_token_encrypted)
    .bind(&req.outbound_url)
    .bind(&outbound_auth_encrypted)
    .bind(&req.default_agent_id)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => (
            StatusCode::CREATED,
            [
                (header::CACHE_CONTROL, "no-store"),
                (header::PRAGMA, "no-cache"),
            ],
            Json(serde_json::json!({
                "id": id,
                "inbound_token": inbound_token,
                "inbound_url": format!("/api/pipes/{}/inbound", id),
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Failed to create pipe: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn delete_pipe(
    principal: ApiPrincipal,
    Path(pipe_id): Path<Uuid>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let pipe_id_str = pipe_id.to_string();
    let owner = match sqlx::query_scalar::<_, String>("SELECT user_id FROM pipes WHERE id = ?")
        .bind(&pipe_id_str)
        .fetch_optional(&state.db)
        .await
    {
        Ok(Some(owner)) => owner,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            tracing::error!("Failed to resolve pipe owner: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if principal.user_scope(&owner).is_err() {
        return forbidden();
    }

    let result = sqlx::query!("UPDATE pipes SET active = 0 WHERE id = ?", pipe_id_str)
        .execute(&state.db)
        .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("Failed to deactivate pipe: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
