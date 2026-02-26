/// Asynchronous tool-call approval queue.
///
/// When an "ask" policy fires on a non-interactive threaded channel, the
/// tool loop suspends and an approval prompt is sent to the user via the
/// channel's registered `ChannelSender`. The pending approval is tracked
/// in the `pending_tool_approvals` table and an in-memory oneshot channel
/// so that the user's inline-reply can resolve the waiting task.
use anyhow::Result;
use sqlx::{Row, SqlitePool};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::state::AppState;

/// Default timeout for pending approvals (5 minutes).
pub const APPROVAL_TIMEOUT_SECS: i64 = 300;

/// Create a pending approval in the database.
///
/// Returns the approval ID. The caller is responsible for also inserting
/// a `oneshot::Sender<bool>` into `state.pending_approvals` under this ID.
pub async fn create_pending(
    db: &SqlitePool,
    user_id: &str,
    session_id: &str,
    thread_id: &str,
    pipe_id: &str,
    agent_id: &str,
    capability_id: &str,
    tool_name: &str,
    args_json: &str,
    tool_call_id: &str,
) -> Result<String> {
    let id = Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO pending_tool_approvals
            (id, user_id, session_id, thread_id, pipe_id, agent_id,
             capability_id, tool_name, args_json, tool_call_id, status, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending',
                 datetime('now', '+' || ? || ' seconds'))",
    )
    .bind(&id)
    .bind(user_id)
    .bind(session_id)
    .bind(thread_id)
    .bind(pipe_id)
    .bind(agent_id)
    .bind(capability_id)
    .bind(tool_name)
    .bind(args_json)
    .bind(tool_call_id)
    .bind(APPROVAL_TIMEOUT_SECS)
    .execute(db)
    .await?;

    debug!(
        approval_id = %id,
        thread_id = %thread_id,
        tool_name = %tool_name,
        "Created pending tool approval"
    );

    Ok(id)
}

/// Try to resolve a pending approval for a thread.
///
/// Called by channel adapters when a message arrives on a thread that
/// has a pending approval. If a pending approval exists, it is marked
/// as approved and the in-memory oneshot is signalled.
///
/// Returns `true` if the message was consumed as an approval response
/// (caller should NOT proceed to `run_turn`).
pub async fn try_resolve_pending(
    state: &AppState,
    thread_id: &str,
) -> bool {
    // Check DB for a pending approval on this thread
    let pending = sqlx::query(
        "SELECT id FROM pending_tool_approvals
         WHERE thread_id = ? AND status = 'pending' AND expires_at > datetime('now')
         ORDER BY created_at ASC LIMIT 1",
    )
    .bind(thread_id)
    .fetch_optional(&state.db)
    .await;

    let approval_id: String = match pending {
        Ok(Some(row)) => match row.try_get::<String, _>("id") {
            Ok(id) => id,
            Err(_) => return false,
        },
        _ => return false,
    };

    // Mark as approved in DB
    let _ = sqlx::query(
        "UPDATE pending_tool_approvals SET status = 'approved' WHERE id = ?",
    )
    .bind(&approval_id)
    .execute(&state.db)
    .await;

    // Signal the in-memory oneshot
    let sender = {
        let mut pending = state.pending_approvals.lock().await;
        pending.remove(&approval_id)
    };

    if let Some(tx) = sender {
        let _ = tx.send(true);
        info!(approval_id = %approval_id, thread_id = %thread_id, "Tool approval resolved via thread reply");
        true
    } else {
        // Oneshot already gone (timed out or resolved another way)
        warn!(
            approval_id = %approval_id,
            "Pending approval found in DB but no in-memory oneshot (likely expired)"
        );
        false
    }
}

/// Expire all pending approvals whose deadline has passed.
///
/// For each expired approval, the in-memory oneshot is sent `false`
/// (denied) so the suspended tool loop resumes with a denial.
pub async fn expire_pending(state: &AppState) -> Result<()> {
    let expired = sqlx::query(
        "SELECT id FROM pending_tool_approvals
         WHERE status = 'pending' AND expires_at <= datetime('now')"
    )
    .fetch_all(&state.db)
    .await?;

    if expired.is_empty() {
        return Ok(());
    }

    let count = expired.len();

    for row in &expired {
        let id: String = match row.try_get("id") {
            Ok(id) => id,
            Err(_) => continue,
        };

        // Mark expired in DB
        let _ = sqlx::query(
            "UPDATE pending_tool_approvals SET status = 'expired' WHERE id = ?",
        )
        .bind(&id)
        .execute(&state.db)
        .await;

        // Signal the oneshot as denied
        let sender = {
            let mut pending = state.pending_approvals.lock().await;
            pending.remove(&id)
        };

        if let Some(tx) = sender {
            let _ = tx.send(false);
        }
    }

    info!(count = count, "Expired pending tool approvals");
    Ok(())
}
