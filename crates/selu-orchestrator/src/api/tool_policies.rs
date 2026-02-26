/// REST API for tool policies and pending approvals.
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing::error;

use crate::permissions::tool_policy::{self, ToolPolicy};
use crate::state::AppState;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PoliciesQuery {
    pub agent_id: String,
    pub user_id: String,
}

#[derive(Debug, Serialize)]
pub struct PolicyResponse {
    pub capability_id: String,
    pub tool_name: String,
    pub policy: String,
}

#[derive(Debug, Deserialize)]
pub struct BulkPolicyRequest {
    pub user_id: String,
    pub agent_id: String,
    pub policies: Vec<PolicyEntry>,
}

#[derive(Debug, Deserialize)]
pub struct PolicyEntry {
    pub capability_id: String,
    pub tool_name: String,
    pub policy: String,
}

#[derive(Debug, Deserialize)]
pub struct ApprovalQuery {
    pub user_id: String,
    pub status: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApprovalResponse {
    pub id: String,
    pub thread_id: String,
    pub capability_id: String,
    pub tool_name: String,
    pub args_json: String,
    pub status: String,
    pub created_at: String,
    pub expires_at: String,
}

#[derive(Debug, Deserialize)]
pub struct ApprovalAction {
    pub approved: Option<bool>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /api/tool-policies?agent_id=X&user_id=Y
pub async fn list_policies(
    Query(q): Query<PoliciesQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    match tool_policy::get_policies_for_agent(&state.db, &q.user_id, &q.agent_id).await {
        Ok(policies) => {
            let resp: Vec<PolicyResponse> = policies
                .into_iter()
                .map(|p| PolicyResponse {
                    capability_id: p.capability_id,
                    tool_name: p.tool_name,
                    policy: p.policy.as_str().to_string(),
                })
                .collect();
            Json(resp).into_response()
        }
        Err(e) => {
            error!("Failed to list tool policies: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// PUT /api/tool-policies
pub async fn bulk_set_policies(
    State(state): State<AppState>,
    Json(req): Json<BulkPolicyRequest>,
) -> impl IntoResponse {
    let policies: Vec<(String, String, ToolPolicy)> = req.policies
        .iter()
        .filter_map(|p| {
            ToolPolicy::from_str(&p.policy)
                .ok()
                .map(|pol| (p.capability_id.clone(), p.tool_name.clone(), pol))
        })
        .collect();

    match tool_policy::set_policies(&state.db, &req.user_id, &req.agent_id, &policies).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
            error!("Failed to set tool policies: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// GET /api/approvals?user_id=X&status=pending
pub async fn list_approvals(
    Query(q): Query<ApprovalQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let status = q.status.as_deref().unwrap_or("pending");

    let rows = sqlx::query(
        "SELECT id, thread_id, capability_id, tool_name, args_json, status, created_at, expires_at
         FROM pending_tool_approvals
         WHERE user_id = ? AND status = ?
         ORDER BY created_at DESC",
    )
    .bind(&q.user_id)
    .bind(status)
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let resp: Vec<ApprovalResponse> = rows
                .into_iter()
                .map(|r| ApprovalResponse {
                    id: r.try_get::<String, _>("id").unwrap_or_default(),
                    thread_id: r.get("thread_id"),
                    capability_id: r.get("capability_id"),
                    tool_name: r.get("tool_name"),
                    args_json: r.get("args_json"),
                    status: r.get("status"),
                    created_at: r.try_get::<String, _>("created_at").unwrap_or_default(),
                    expires_at: r.get("expires_at"),
                })
                .collect();
            Json(resp).into_response()
        }
        Err(e) => {
            error!("Failed to list approvals: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// POST /api/approvals/{id}?approved=true|false
pub async fn resolve_approval(
    Path(approval_id): Path<String>,
    Query(action): Query<ApprovalAction>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let approved = action.approved.unwrap_or(false);
    let new_status = if approved { "approved" } else { "denied" };

    // Update DB
    let result = sqlx::query(
        "UPDATE pending_tool_approvals SET status = ? WHERE id = ? AND status = 'pending'",
    )
    .bind(new_status)
    .bind(&approval_id)
    .execute(&state.db)
    .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => {
            // Signal the in-memory oneshot
            let sender = {
                let mut pending = state.pending_approvals.lock().await;
                pending.remove(&approval_id)
            };

            if let Some(tx) = sender {
                let _ = tx.send(approved);
            }

            StatusCode::OK.into_response()
        }
        Ok(_) => {
            // No pending approval found with this ID
            StatusCode::NOT_FOUND.into_response()
        }
        Err(e) => {
            error!("Failed to resolve approval: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
