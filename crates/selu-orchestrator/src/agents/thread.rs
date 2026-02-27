use anyhow::Result;
use sqlx::SqlitePool;
use tracing::info;
use uuid::Uuid;

use selu_core::types::{Thread, ThreadStatus};

use crate::agents::session as session_mgr;

/// Creates a new thread for a conversation.
///
/// Threads are long-lived conversations that accumulate message history.
/// They share the session (and therefore semantic memory / capability
/// containers) but have independent conversation histories.
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
        title: None,
        origin_message_ref: origin_message_ref.map(|s| s.to_string()),
        last_reply_guid: None,
        created_at: now,
        completed_at: None,
    })
}

/// Find an active thread by matching a message GUID.
///
/// Checks `origin_message_ref` (the inbound message that started the
/// thread), `last_reply_guid` (legacy single-GUID field), AND the
/// `thread_reply_guids` table which stores ALL outbound message GUIDs.
/// This enables iMessage reply-to chains: when a user replies to *any*
/// of Selu's messages in the thread, we can route it back correctly.
pub async fn find_thread_by_message_ref(
    db: &SqlitePool,
    pipe_id: &str,
    message_ref: &str,
) -> Result<Option<Thread>> {
    // First try origin_message_ref and last_reply_guid (fast path)
    let row = sqlx::query!(
        r#"SELECT id, pipe_id, session_id, user_id, status, title,
                  origin_message_ref, last_reply_guid, created_at, completed_at
           FROM threads
           WHERE pipe_id = ? AND status = 'active'
             AND (origin_message_ref = ? OR last_reply_guid = ?)
           ORDER BY created_at DESC
           LIMIT 1"#,
        pipe_id,
        message_ref,
        message_ref,
    )
    .fetch_optional(db)
    .await?;

    if let Some(r) = row {
        return Ok(Some(Thread {
            id: Uuid::parse_str(r.id.as_deref().unwrap_or_default())?,
            pipe_id: Uuid::parse_str(&r.pipe_id)?,
            session_id: Uuid::parse_str(&r.session_id)?,
            user_id: Uuid::parse_str(&r.user_id)?,
            status: ThreadStatus::Active,
            title: r.title,
            origin_message_ref: r.origin_message_ref,
            last_reply_guid: r.last_reply_guid,
            created_at: r.created_at.parse().unwrap_or_default(),
            completed_at: None,
        }));
    }

    // Fall back to the thread_reply_guids lookup table (all outbound GUIDs)
    let row = sqlx::query!(
        r#"SELECT t.id, t.pipe_id, t.session_id, t.user_id, t.status, t.title,
                  t.origin_message_ref, t.last_reply_guid, t.created_at, t.completed_at
           FROM threads t
           JOIN thread_reply_guids g ON g.thread_id = t.id
           WHERE g.pipe_id = ? AND g.reply_guid = ? AND t.status = 'active'
           ORDER BY t.created_at DESC
           LIMIT 1"#,
        pipe_id,
        message_ref,
    )
    .fetch_optional(db)
    .await?;

    match row {
        Some(r) => Ok(Some(Thread {
            id: Uuid::parse_str(r.id.as_deref().unwrap_or_default())?,
            pipe_id: Uuid::parse_str(&r.pipe_id)?,
            session_id: Uuid::parse_str(&r.session_id)?,
            user_id: Uuid::parse_str(&r.user_id)?,
            status: ThreadStatus::Active,
            title: r.title,
            origin_message_ref: r.origin_message_ref,
            last_reply_guid: r.last_reply_guid,
            created_at: r.created_at.parse().unwrap_or_default(),
            completed_at: None,
        })),
        None => Ok(None),
    }
}

/// Find or create a thread for an inbound message.
///
/// If `reply_to_ref` is provided and matches an existing active thread,
/// that thread is reused (enabling reply-to chains). Otherwise a new
/// thread is created.
pub async fn find_or_create_thread(
    db: &SqlitePool,
    pipe_id: &str,
    user_id: &str,
    agent_id: &str,
    origin_message_ref: Option<&str>,
    reply_to_ref: Option<&str>,
) -> Result<Thread> {
    // Try to match an existing thread via the reply-to reference
    if let Some(ref_guid) = reply_to_ref {
        if let Some(existing) = find_thread_by_message_ref(db, pipe_id, ref_guid).await? {
            info!(
                thread_id = %existing.id,
                reply_to = %ref_guid,
                "Reusing existing thread via reply-to match"
            );
            return Ok(existing);
        }
    }

    // No match — create a new thread
    create_thread(db, pipe_id, user_id, agent_id, origin_message_ref).await
}

/// Store Selu's outbound reply GUID on the thread so future incoming
/// replies can be matched back to this thread.
///
/// Updates the `last_reply_guid` convenience column AND inserts into the
/// `thread_reply_guids` lookup table so that ALL outbound GUIDs are
/// searchable (not just the most recent one).
pub async fn update_reply_guid(db: &SqlitePool, thread_id: &str, pipe_id: &str, reply_guid: &str) -> Result<()> {
    // Update the convenience column (latest reply)
    sqlx::query!(
        "UPDATE threads SET last_reply_guid = ? WHERE id = ?",
        reply_guid,
        thread_id,
    )
    .execute(db)
    .await?;

    // Insert into the lookup table so ALL outbound GUIDs are searchable
    let id = Uuid::new_v4().to_string();
    sqlx::query!(
        "INSERT OR IGNORE INTO thread_reply_guids (id, thread_id, pipe_id, reply_guid)
         VALUES (?, ?, ?, ?)",
        id,
        thread_id,
        pipe_id,
        reply_guid,
    )
    .execute(db)
    .await?;

    Ok(())
}

/// Set the thread title (e.g. LLM-generated from first message).
pub async fn set_title(db: &SqlitePool, thread_id: &str, title: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE threads SET title = ? WHERE id = ?",
        title,
        thread_id,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Mark a thread as completed.
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

/// Close idle threads whose last message is older than `idle_hours`.
///
/// Completed threads remain visible in the UI (read-only history) but
/// won't match for BlueBubbles reply-to routing.
///
/// Returns the number of threads closed.
pub async fn close_idle_threads(db: &SqlitePool, idle_hours: u32) -> Result<u64> {
    let threshold = format!("-{} hours", idle_hours);

    let result = sqlx::query!(
        r#"UPDATE threads SET status = 'completed', completed_at = datetime('now')
           WHERE status = 'active'
             AND id NOT IN (
                 SELECT DISTINCT thread_id FROM messages
                 WHERE thread_id IS NOT NULL
                   AND created_at > datetime('now', ?)
             )
             AND created_at < datetime('now', ?)"#,
        threshold,
        threshold,
    )
    .execute(db)
    .await?;

    let count = result.rows_affected();
    if count > 0 {
        info!(count = count, idle_hours = idle_hours, "Closed idle threads");
    }
    Ok(count)
}

/// Count active (in-flight) threads for a given session.
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
