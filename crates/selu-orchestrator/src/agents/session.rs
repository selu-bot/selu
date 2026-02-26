use anyhow::Result;
use sqlx::SqlitePool;
use uuid::Uuid;
use tracing::info;

use selu_core::types::{Session, SessionStatus};

/// Opens or resumes an active session for a given user + agent.
///
/// Sessions are deduplicated on `(user_id, agent_id)` so that the same user
/// talking to the same agent always reuses a single session (and therefore a
/// single set of capability containers) regardless of which pipe the message
/// arrives through.  The `pipe_id` is still recorded on new sessions for
/// auditing, but it is **not** part of the dedup key.
///
/// Returns the session (new or existing active one).
pub async fn open_session(
    db: &SqlitePool,
    pipe_id: &str,
    user_id: &str,
    agent_id: &str,
) -> Result<Session> {
    // Look for an existing active session for this user + agent
    let existing = sqlx::query!(
        "SELECT id, pipe_id, user_id, agent_id, status, workspace_id, created_at, last_active_at
         FROM sessions
         WHERE user_id = ? AND agent_id = ? AND status = 'active'
         ORDER BY last_active_at DESC
         LIMIT 1",
        user_id,
        agent_id,
    )
    .fetch_optional(db)
    .await?;

    if let Some(row) = existing {
        // Touch last_active_at
        sqlx::query!(
            "UPDATE sessions SET last_active_at = datetime('now') WHERE id = ?",
            row.id
        )
        .execute(db)
        .await?;

        return Ok(Session {
            id: Uuid::parse_str(row.id.as_deref().unwrap_or_default())?,
            pipe_id: Uuid::parse_str(&row.pipe_id)?,
            user_id: Uuid::parse_str(&row.user_id)?,
            agent_id: row.agent_id,
            status: SessionStatus::Active,
            workspace_id: row.workspace_id.as_deref().and_then(|s| Uuid::parse_str(s).ok()),
            created_at: row.created_at.parse().unwrap_or_default(),
            last_active_at: row.last_active_at.parse().unwrap_or_default(),
        });
    }

    // Create a new session
    let session_id = Uuid::new_v4().to_string();
    info!(session_id = %session_id, agent_id = %agent_id, pipe_id = %pipe_id, "Opening new session");

    sqlx::query!(
        "INSERT INTO sessions (id, pipe_id, user_id, agent_id, status)
         VALUES (?, ?, ?, ?, 'active')",
        session_id,
        pipe_id,
        user_id,
        agent_id,
    )
    .execute(db)
    .await?;

    let now = chrono::Utc::now();
    Ok(Session {
        id: Uuid::parse_str(&session_id)?,
        pipe_id: Uuid::parse_str(pipe_id)?,
        user_id: Uuid::parse_str(user_id)?,
        agent_id: agent_id.to_string(),
        status: SessionStatus::Active,
        workspace_id: None,
        created_at: now,
        last_active_at: now,
    })
}

/// Closes a session explicitly. Called when cleaning up after workspace TTL
/// expiry or when a user explicitly ends a session.
#[allow(dead_code)]
pub async fn close_session(db: &SqlitePool, session_id: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE sessions SET status = 'closed' WHERE id = ?",
        session_id
    )
    .execute(db)
    .await?;
    info!(session_id = %session_id, "Session closed");
    Ok(())
}

/// Closes idle sessions whose last_active_at is older than their agent's idle_timeout_minutes.
/// Called by the background maintenance task.
///
/// Returns the IDs of sessions that were marked idle, so that the caller can
/// tear down their capability containers.
pub async fn close_idle_sessions(db: &SqlitePool, idle_minutes: u32) -> Result<Vec<String>> {
    let threshold = format!("-{}", idle_minutes);

    // First, collect the IDs of sessions that will be idled.
    let rows = sqlx::query!(
        "SELECT id FROM sessions
         WHERE status = 'active'
           AND last_active_at < datetime('now', ? || ' minutes')",
        threshold
    )
    .fetch_all(db)
    .await?;

    let ids: Vec<String> = rows
        .into_iter()
        .filter_map(|r| r.id)
        .collect();

    if ids.is_empty() {
        return Ok(ids);
    }

    // Then mark them idle.
    sqlx::query!(
        "UPDATE sessions SET status = 'idle'
         WHERE status = 'active'
           AND last_active_at < datetime('now', ? || ' minutes')",
        threshold
    )
    .execute(db)
    .await?;

    Ok(ids)
}
