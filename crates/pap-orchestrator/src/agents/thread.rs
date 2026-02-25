use anyhow::Result;
use sqlx::SqlitePool;
use tracing::info;
use uuid::Uuid;

use pap_core::types::{Thread, ThreadStatus};

use crate::agents::session as session_mgr;

/// Creates a new thread for an inbound message.
///
/// Each inbound message gets its own thread, so if a user sends multiple
/// messages while the agent is still processing the first, each gets a
/// separate thread with its own message history.
///
/// Threads share the session (and therefore semantic memory) but have
/// independent conversation histories.
pub async fn create_thread(
    db: &SqlitePool,
    pipe_id: &str,
    user_id: &str,
    agent_id: &str,
    origin_message_ref: Option<&str>,
) -> Result<Thread> {
    // First, ensure we have a session for this user + agent
    let session = session_mgr::open_session(db, pipe_id, user_id, agent_id).await?;
    let session_id = session.id.to_string();

    let thread_id = Uuid::new_v4().to_string();

    info!(
        thread_id = %thread_id,
        session_id = %session_id,
        pipe_id = %pipe_id,
        origin_ref = ?origin_message_ref,
        "Creating new thread"
    );

    sqlx::query!(
        "INSERT INTO threads (id, pipe_id, session_id, user_id, status, origin_message_ref)
         VALUES (?, ?, ?, ?, 'active', ?)",
        thread_id,
        pipe_id,
        session_id,
        user_id,
        origin_message_ref,
    )
    .execute(db)
    .await?;

    let now = chrono::Utc::now();
    Ok(Thread {
        id: Uuid::parse_str(&thread_id)?,
        pipe_id: Uuid::parse_str(pipe_id)?,
        session_id: session.id,
        user_id: Uuid::parse_str(user_id)?,
        status: ThreadStatus::Active,
        origin_message_ref: origin_message_ref.map(|s| s.to_string()),
        created_at: now,
        completed_at: None,
    })
}

/// Mark a thread as completed after the agent finishes its turn.
pub async fn complete_thread(db: &SqlitePool, thread_id: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE threads SET status = 'completed', completed_at = datetime('now') WHERE id = ?",
        thread_id
    )
    .execute(db)
    .await?;
    info!(thread_id = %thread_id, "Thread completed");
    Ok(())
}

/// Mark a thread as failed if the agent turn errors out.
pub async fn fail_thread(db: &SqlitePool, thread_id: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE threads SET status = 'failed', completed_at = datetime('now') WHERE id = ?",
        thread_id
    )
    .execute(db)
    .await?;
    info!(thread_id = %thread_id, "Thread failed");
    Ok(())
}

/// Count active (in-flight) threads for a given user + session.
/// Used to determine if we need thread markers in outbound messages.
#[allow(dead_code)]
pub async fn count_active_threads(
    db: &SqlitePool,
    session_id: &str,
) -> Result<i64> {
    let row = sqlx::query!(
        "SELECT COUNT(*) as cnt FROM threads WHERE session_id = ? AND status = 'active'",
        session_id,
    )
    .fetch_one(db)
    .await?;

    Ok(row.cnt.into())
}
