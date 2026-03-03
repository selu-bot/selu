use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state::AppState;

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
    pub user_id: Uuid,
    pub name: String,
    pub transport: Option<String>,
    pub outbound_url: String,
    pub outbound_auth: Option<String>,
    pub default_agent_id: Option<String>,
}

pub async fn list_pipes(State(state): State<AppState>) -> impl IntoResponse {
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
    State(state): State<AppState>,
    Json(req): Json<CreatePipeRequest>,
) -> impl IntoResponse {
    let id = Uuid::new_v4().to_string();
    // Generate a random inbound token
    let inbound_token = Uuid::new_v4().to_string().replace('-', "");
    let transport = req.transport.unwrap_or_else(|| "webhook".to_string());
    let user_id_str = req.user_id.to_string();

    let result = sqlx::query!(
        "INSERT INTO pipes (id, user_id, name, transport, inbound_token, outbound_url, outbound_auth, default_agent_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        id,
        user_id_str,
        req.name,
        transport,
        inbound_token,
        req.outbound_url,
        req.outbound_auth,
        req.default_agent_id,
    )
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => {
            // Return the full pipe including the inbound token (only time it's shown)
            Json(serde_json::json!({
                "id": id,
                "inbound_token": inbound_token,
                "inbound_url": format!("/api/pipes/{}/inbound", id),
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!("Failed to create pipe: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn delete_pipe(
    Path(pipe_id): Path<Uuid>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let pipe_id_str = pipe_id.to_string();
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
