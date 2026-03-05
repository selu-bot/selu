use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use tracing::info;
use uuid::Uuid;

use selu_core::types::{Session, SessionStatus};

/// Opens or resumes a session for a given user + agent.
///
/// Behavior:
/// - If `thread_id` is set, first try to resume that thread's previous session
///   (active or idle). If resuming from idle, status is switched back to active.
/// - If the thread's prior session cannot be resumed, create a fresh session
///   and rebind the thread to it.
/// - If `thread_id` is not set, reuse the most recent active session for
///   `(user_id, agent_id)` when available.
///
/// Returns the session (new or existing active one).
pub async fn open_session(
    db: &SqlitePool,
    pipe_id: &str,
    user_id: &str,
    agent_id: &str,
    thread_id: Option<&str>,
) -> Result<Session> {
    if let Some(tid) = thread_id {
        if let Some(resumed) =
            try_resume_thread_session(db, tid, pipe_id, user_id, agent_id).await?
        {
            return Ok(resumed);
        }

        let created = create_session(db, pipe_id, user_id, agent_id).await?;
        sqlx::query(
            "UPDATE threads SET session_id = ? WHERE id = ? AND user_id = ? AND pipe_id = ?",
        )
        .bind(created.id.to_string())
        .bind(tid)
        .bind(user_id)
        .bind(pipe_id)
        .execute(db)
        .await?;
        return Ok(created);
    }

    // Non-threaded flow: reuse existing active session for this user + agent.
    let existing = sqlx::query(
        "SELECT id, pipe_id, user_id, agent_id, status, workspace_id, created_at, last_active_at
         FROM sessions
         WHERE user_id = ? AND agent_id = ? AND status = 'active'
         ORDER BY last_active_at DESC
         LIMIT 1",
    )
    .bind(user_id)
    .bind(agent_id)
    .fetch_optional(db)
    .await?;

    if let Some(row) = existing {
        let sid: String = row.get("id");
        sqlx::query("UPDATE sessions SET last_active_at = datetime('now') WHERE id = ?")
            .bind(&sid)
            .execute(db)
            .await?;

        // Re-read after touch so returned timestamp reflects current state.
        if let Some(refreshed) = fetch_session_by_id(db, &sid).await? {
            return Ok(refreshed);
        }
    }

    create_session(db, pipe_id, user_id, agent_id).await
}

async fn try_resume_thread_session(
    db: &SqlitePool,
    thread_id: &str,
    pipe_id: &str,
    user_id: &str,
    agent_id: &str,
) -> Result<Option<Session>> {
    let row = sqlx::query(
        "SELECT s.id, s.pipe_id, s.user_id, s.agent_id, s.status, s.workspace_id, s.created_at, s.last_active_at
         FROM threads t
         LEFT JOIN sessions s ON s.id = t.session_id
         WHERE t.id = ? AND t.user_id = ? AND t.pipe_id = ?
         LIMIT 1",
    )
    .bind(thread_id)
    .bind(user_id)
    .bind(pipe_id)
    .fetch_optional(db)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let sid_opt: Option<String> = row.try_get("id").ok();
    let Some(sid) = sid_opt else {
        return Ok(None);
    };

    let session_user: String = row.get("user_id");
    let session_agent: String = row.get("agent_id");
    let status: String = row.get("status");

    // If the thread references a session from another user/agent, don't reuse it.
    if session_user != user_id || session_agent != agent_id {
        return Ok(None);
    }

    if status != "active" && status != "idle" {
        return Ok(None);
    }

    sqlx::query(
        "UPDATE sessions SET status = 'active', last_active_at = datetime('now') WHERE id = ?",
    )
    .bind(&sid)
    .execute(db)
    .await?;

    fetch_session_by_id(db, &sid).await
}

async fn create_session(
    db: &SqlitePool,
    pipe_id: &str,
    user_id: &str,
    agent_id: &str,
) -> Result<Session> {
    let session_id = Uuid::new_v4().to_string();
    info!(session_id = %session_id, agent_id = %agent_id, pipe_id = %pipe_id, "Opening new session");

    sqlx::query(
        "INSERT INTO sessions (id, pipe_id, user_id, agent_id, status)
         VALUES (?, ?, ?, ?, 'active')",
    )
    .bind(&session_id)
    .bind(pipe_id)
    .bind(user_id)
    .bind(agent_id)
    .execute(db)
    .await?;

    fetch_session_by_id(db, &session_id)
        .await?
        .context("newly inserted session missing")
}

async fn fetch_session_by_id(db: &SqlitePool, session_id: &str) -> Result<Option<Session>> {
    let row = sqlx::query(
        "SELECT id, pipe_id, user_id, agent_id, status, workspace_id, created_at, last_active_at
         FROM sessions
         WHERE id = ?
         LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(db)
    .await?;

    Ok(row.map(|r| map_session_row(&r)).transpose()?)
}

fn map_session_row(row: &sqlx::sqlite::SqliteRow) -> Result<Session> {
    let status = match row.get::<String, _>("status").as_str() {
        "active" => SessionStatus::Active,
        "idle" => SessionStatus::Idle,
        "closed" => SessionStatus::Closed,
        _ => SessionStatus::Active,
    };

    let id: String = row.get("id");
    let pipe_id: String = row.get("pipe_id");
    let user_id: String = row.get("user_id");
    let agent_id: String = row.get("agent_id");
    let created_at: String = row.get("created_at");
    let last_active_at: String = row.get("last_active_at");
    let workspace_id: Option<String> = row.try_get("workspace_id").ok();

    Ok(Session {
        id: Uuid::parse_str(&id)?,
        pipe_id: Uuid::parse_str(&pipe_id)?,
        user_id: Uuid::parse_str(&user_id)?,
        agent_id,
        status,
        workspace_id: workspace_id
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok()),
        created_at: created_at.parse().unwrap_or_default(),
        last_active_at: last_active_at.parse().unwrap_or_default(),
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

/// Closes idle sessions whose last activity exceeds each agent's idle timeout.
/// Called by the background maintenance task.
///
/// Returns the IDs of sessions that were marked idle, so that the caller can
/// tear down their capability containers.
pub async fn close_idle_sessions(
    db: &SqlitePool,
    agent_idle_minutes: &HashMap<String, u32>,
    default_idle_minutes: u32,
) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT id, agent_id,
                CAST((julianday('now') - julianday(last_active_at)) * 1440 AS INTEGER) AS idle_minutes
         FROM sessions
         WHERE status = 'active'",
    )
    .fetch_all(db)
    .await?;

    let mut ids = Vec::new();
    for row in rows {
        let id: String = row.get("id");
        let agent_id: String = row.get("agent_id");
        let idle_minutes: i64 = row.get("idle_minutes");

        let timeout = agent_idle_minutes
            .get(&agent_id)
            .copied()
            .unwrap_or(default_idle_minutes) as i64;
        if idle_minutes >= timeout {
            ids.push(id);
        }
    }

    if ids.is_empty() {
        return Ok(ids);
    }

    for session_id in &ids {
        sqlx::query("UPDATE sessions SET status = 'idle' WHERE id = ? AND status = 'active'")
            .bind(session_id)
            .execute(db)
            .await?;
    }

    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_db() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                pipe_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                status TEXT NOT NULL,
                workspace_id TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                last_active_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .execute(&db)
        .await
        .unwrap();

        sqlx::query(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                pipe_id TEXT NOT NULL,
                session_id TEXT,
                user_id TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active'
            )",
        )
        .execute(&db)
        .await
        .unwrap();

        db
    }

    #[tokio::test]
    async fn close_idle_sessions_uses_per_agent_timeout() {
        let db = setup_db().await;

        let s1 = Uuid::new_v4().to_string();
        let s2 = Uuid::new_v4().to_string();
        let pipe = Uuid::new_v4().to_string();
        let user = Uuid::new_v4().to_string();

        sqlx::query(
            "INSERT INTO sessions (id, pipe_id, user_id, agent_id, status, last_active_at)
             VALUES (?, ?, ?, 'fast-agent', 'active', datetime('now', '-40 minutes'))",
        )
        .bind(&s1)
        .bind(&pipe)
        .bind(&user)
        .execute(&db)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO sessions (id, pipe_id, user_id, agent_id, status, last_active_at)
             VALUES (?, ?, ?, 'slow-agent', 'active', datetime('now', '-40 minutes'))",
        )
        .bind(&s2)
        .bind(&pipe)
        .bind(&user)
        .execute(&db)
        .await
        .unwrap();

        let mut idle_map = HashMap::new();
        idle_map.insert("fast-agent".to_string(), 30);
        idle_map.insert("slow-agent".to_string(), 90);

        let idled = close_idle_sessions(&db, &idle_map, 30).await.unwrap();
        assert_eq!(idled.len(), 1);
        assert_eq!(idled[0], s1);

        let status1: String = sqlx::query("SELECT status FROM sessions WHERE id = ?")
            .bind(&s1)
            .fetch_one(&db)
            .await
            .unwrap()
            .get("status");
        let status2: String = sqlx::query("SELECT status FROM sessions WHERE id = ?")
            .bind(&s2)
            .fetch_one(&db)
            .await
            .unwrap()
            .get("status");

        assert_eq!(status1, "idle");
        assert_eq!(status2, "active");
    }

    #[tokio::test]
    async fn open_session_resumes_idle_thread_session() {
        let db = setup_db().await;

        let pipe = Uuid::new_v4().to_string();
        let user = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();
        let thread_id = Uuid::new_v4().to_string();

        sqlx::query(
            "INSERT INTO sessions (id, pipe_id, user_id, agent_id, status, last_active_at)
             VALUES (?, ?, ?, 'coding-github', 'idle', datetime('now', '-60 minutes'))",
        )
        .bind(&session_id)
        .bind(&pipe)
        .bind(&user)
        .execute(&db)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO threads (id, pipe_id, session_id, user_id, status)
             VALUES (?, ?, ?, ?, 'active')",
        )
        .bind(&thread_id)
        .bind(&pipe)
        .bind(&session_id)
        .bind(&user)
        .execute(&db)
        .await
        .unwrap();

        let session = open_session(&db, &pipe, &user, "coding-github", Some(thread_id.as_str()))
            .await
            .unwrap();

        assert_eq!(session.id.to_string(), session_id);
        assert_eq!(session.status, SessionStatus::Active);
    }

    #[tokio::test]
    async fn open_session_rebinds_thread_when_prior_session_is_closed() {
        let db = setup_db().await;

        let pipe = Uuid::new_v4().to_string();
        let user = Uuid::new_v4().to_string();
        let old_session = Uuid::new_v4().to_string();
        let thread_id = Uuid::new_v4().to_string();

        sqlx::query(
            "INSERT INTO sessions (id, pipe_id, user_id, agent_id, status)
             VALUES (?, ?, ?, 'coding-github', 'closed')",
        )
        .bind(&old_session)
        .bind(&pipe)
        .bind(&user)
        .execute(&db)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO threads (id, pipe_id, session_id, user_id, status)
             VALUES (?, ?, ?, ?, 'active')",
        )
        .bind(&thread_id)
        .bind(&pipe)
        .bind(&old_session)
        .bind(&user)
        .execute(&db)
        .await
        .unwrap();

        let session = open_session(&db, &pipe, &user, "coding-github", Some(thread_id.as_str()))
            .await
            .unwrap();

        assert_ne!(session.id.to_string(), old_session);

        let rebound_session_id: String = sqlx::query("SELECT session_id FROM threads WHERE id = ?")
            .bind(&thread_id)
            .fetch_one(&db)
            .await
            .unwrap()
            .get("session_id");
        assert_eq!(rebound_session_id, session.id.to_string());
    }
}
