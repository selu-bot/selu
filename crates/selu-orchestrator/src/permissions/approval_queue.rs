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
                 '9999-12-31 23:59:59')",
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
pub async fn try_resolve_pending(state: &AppState, thread_id: &str) -> bool {
    // Check DB for a pending approval on this thread
    let pending = sqlx::query(
        "SELECT id FROM pending_tool_approvals
         WHERE thread_id = ? AND status = 'pending'
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
    let _ = sqlx::query("UPDATE pending_tool_approvals SET status = 'approved' WHERE id = ?")
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
        // Oneshot already gone (likely process restart or resolved another way)
        warn!(
            approval_id = %approval_id,
            "Pending approval found in DB but no in-memory oneshot"
        );
        false
    }
}

/// Try to resolve a pending approval by user+pipe when thread correlation
/// metadata is unavailable (e.g. plain "yes" reply without inline threading).
///
/// Fallback rule: resolve the oldest pending approval for this user+pipe.
/// This keeps non-threaded channels (like plain webhook/WhatsApp replies)
/// usable even when multiple prompts are queued.
pub async fn try_resolve_pending_for_user_pipe(
    state: &AppState,
    user_id: &str,
    pipe_id: &str,
) -> bool {
    let rows = match sqlx::query(
        "SELECT id FROM pending_tool_approvals
         WHERE user_id = ? AND pipe_id = ? AND status = 'pending'
         ORDER BY created_at ASC LIMIT 2",
    )
    .bind(user_id)
    .bind(pipe_id)
    .fetch_all(&state.db)
    .await
    {
        Ok(r) => r,
        Err(_) => return false,
    };

    if rows.is_empty() {
        return false;
    }

    if rows.len() > 1 {
        warn!(
            user_id = %user_id,
            pipe_id = %pipe_id,
            count = rows.len(),
            "Multiple pending approvals for user+pipe; resolving oldest pending approval"
        );
    }

    let approval_id = match rows[0].try_get::<String, _>("id") {
        Ok(id) => id,
        Err(_) => return false,
    };

    let _ = sqlx::query("UPDATE pending_tool_approvals SET status = 'approved' WHERE id = ?")
        .bind(&approval_id)
        .execute(&state.db)
        .await;

    let sender = {
        let mut pending = state.pending_approvals.lock().await;
        pending.remove(&approval_id)
    };

    if let Some(tx) = sender {
        let _ = tx.send(true);
        info!(
            approval_id = %approval_id,
            user_id = %user_id,
            pipe_id = %pipe_id,
            "Tool approval resolved via user+pipe fallback"
        );
        true
    } else {
        warn!(
            approval_id = %approval_id,
            "Pending approval found for user+pipe but no in-memory oneshot"
        );
        false
    }
}
