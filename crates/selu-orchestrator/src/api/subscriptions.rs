/// Event subscription management API.
///
/// GET    /api/subscriptions?user_id=<id>
/// POST   /api/subscriptions
/// DELETE /api/subscriptions/{id}
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct SubscriptionResponse {
    pub id: String,
    pub user_id: String,
    pub event_type: String,
    pub reaction_type: String,
    pub reaction_config: serde_json::Value,
    pub filter_expression: Option<String>,
    pub filter_description: Option<String>,
    pub active: bool,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateSubscriptionRequest {
    pub user_id: Uuid,
    pub event_type: String,
    pub reaction_type: String,
    pub reaction_config: serde_json::Value,
    pub filter_expression: Option<String>,
    pub filter_description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub user_id: Option<String>,
}

pub async fn list_subscriptions(
    Query(q): Query<ListQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    // Fetch all, then optionally filter by user_id in Rust
    // (avoids sqlx compile-time type mismatch between conditional query branches)
    let rows = sqlx::query!(
        r#"SELECT id, user_id, event_type, reaction_type,
                  reaction_config_json, filter_expression, filter_description,
                  active, created_at
           FROM event_subscriptions ORDER BY created_at DESC"#
    )
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let subs: Vec<SubscriptionResponse> = rows
                .into_iter()
                .filter(|r| {
                    // Apply optional user_id filter
                    q.user_id.as_deref().map_or(true, |uid| r.user_id == uid)
                })
                .filter_map(|r| {
                    let config: serde_json::Value =
                        serde_json::from_str(&r.reaction_config_json).ok()?;
                    Some(SubscriptionResponse {
                        id: r.id?.clone(),
                        user_id: r.user_id,
                        event_type: r.event_type,
                        reaction_type: r.reaction_type,
                        reaction_config: config,
                        filter_expression: r.filter_expression,
                        filter_description: r.filter_description,
                        active: r.active != 0,
                        created_at: r.created_at,
                    })
                })
                .collect();
            Json(subs).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to list subscriptions: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn create_subscription(
    State(state): State<AppState>,
    Json(req): Json<CreateSubscriptionRequest>,
) -> impl IntoResponse {
    if req.reaction_type != "pipe_notification" && req.reaction_type != "agent_invocation" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "reaction_type must be 'pipe_notification' or 'agent_invocation'"
            })),
        )
            .into_response();
    }

    let id = Uuid::new_v4().to_string();
    let user_id_str = req.user_id.to_string();
    let config_json = match serde_json::to_string(&req.reaction_config) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Invalid reaction_config: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    let result = sqlx::query!(
        r#"INSERT INTO event_subscriptions
           (id, user_id, event_type, reaction_type, reaction_config_json,
            filter_expression, filter_description)
           VALUES (?, ?, ?, ?, ?, ?, ?)"#,
        id,
        user_id_str,
        req.event_type,
        req.reaction_type,
        config_json,
        req.filter_expression,
        req.filter_description,
    )
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Json(serde_json::json!({ "id": id })).into_response(),
        Err(e) => {
            tracing::error!("Failed to create subscription: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn delete_subscription(
    Path(sub_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let result = sqlx::query!(
        "UPDATE event_subscriptions SET active = 0 WHERE id = ?",
        sub_id
    )
    .execute(&state.db)
    .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("Failed to delete subscription: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
