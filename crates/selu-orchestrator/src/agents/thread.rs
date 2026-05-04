use anyhow::Result;
use sqlx::SqlitePool;
use tracing::{debug, info};
use uuid::Uuid;

use selu_core::types::{Thread, ThreadStatus};

use crate::agents::session as session_mgr;

/// Creates a new thread for a conversation.
///
/// Threads are long-lived conversations that accumulate message history.
/// They can either share a session with other threads of the same agent/user
/// or force a dedicated session per thread, depending on agent settings.
pub async fn create_thread(
    db: &SqlitePool,
    pipe_id: &str,
    user_id: &str,
    agent_id: &str,
    force_new_session: bool,
    origin_message_ref: Option<&str>,
) -> Result<Thread> {
    create_thread_with_origin(
        db,
        pipe_id,
        user_id,
        agent_id,
        force_new_session,
        origin_message_ref,
        "conversation",
        None,
    )
    .await
}

/// Create or reuse the single thread that belongs to a schedule on a pipe.
pub async fn find_or_create_schedule_thread(
    db: &SqlitePool,
    pipe_id: &str,
    user_id: &str,
    agent_id: &str,
    schedule_id: &str,
    force_new_session: bool,
) -> Result<Thread> {
    if let Some(thread) = find_schedule_thread(db, pipe_id, user_id, schedule_id).await? {
        let thread_id = thread.id.to_string();
        sqlx::query!(
            "UPDATE threads SET status = 'active', completed_at = NULL WHERE id = ?",
            thread_id,
        )
        .execute(db)
        .await?;
        session_mgr::bind_thread_agent_session(
            db,
            &thread_id,
            agent_id,
            &thread.session_id.to_string(),
        )
        .await?;
        return Ok(thread);
    }

    create_thread_with_origin(
        db,
        pipe_id,
        user_id,
        agent_id,
        force_new_session,
        None,
        "schedule",
        Some(schedule_id),
    )
    .await
}

async fn create_thread_with_origin(
    db: &SqlitePool,
    pipe_id: &str,
    user_id: &str,
    agent_id: &str,
    force_new_session: bool,
    origin_message_ref: Option<&str>,
    thread_kind: &str,
    schedule_id: Option<&str>,
) -> Result<Thread> {
    let thread_id = Uuid::new_v4().to_string();
    // Ensure we have a session for this user + agent.
    // For thread-isolated agents, always start with a fresh session.
    let session = if force_new_session {
        session_mgr::create_fresh_session(db, pipe_id, user_id, agent_id).await?
    } else {
        session_mgr::open_session(db, pipe_id, user_id, agent_id, None).await?
    };
    let session_id = session.id.to_string();

    debug!(
        thread_id = %thread_id,
        session_id = %session_id,
        pipe_id = %pipe_id,
        force_new_session,
        origin_ref = ?origin_message_ref,
        "Creating new thread"
    );

    sqlx::query!(
        "INSERT INTO threads (id, pipe_id, session_id, user_id, status, origin_message_ref, thread_kind, schedule_id)
         VALUES (?, ?, ?, ?, 'active', ?, ?, ?)",
        thread_id,
        pipe_id,
        session_id,
        user_id,
        origin_message_ref,
        thread_kind,
        schedule_id,
    )
    .execute(db)
    .await?;
    session_mgr::bind_thread_agent_session(db, &thread_id, agent_id, &session_id).await?;

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

async fn find_schedule_thread(
    db: &SqlitePool,
    pipe_id: &str,
    user_id: &str,
    schedule_id: &str,
) -> Result<Option<Thread>> {
    let row = sqlx::query!(
        r#"SELECT id, pipe_id, session_id, user_id, status, title,
                  origin_message_ref, last_reply_guid, created_at, completed_at
           FROM threads
           WHERE pipe_id = ? AND user_id = ? AND schedule_id = ?
           LIMIT 1"#,
        pipe_id,
        user_id,
        schedule_id,
    )
    .fetch_optional(db)
    .await?;

    row.map(|r| {
        Ok(Thread {
            id: Uuid::parse_str(r.id.as_deref().unwrap_or_default())?,
            pipe_id: Uuid::parse_str(&r.pipe_id)?,
            session_id: Uuid::parse_str(&r.session_id)?,
            user_id: Uuid::parse_str(&r.user_id)?,
            status: match r.status.as_str() {
                "completed" => ThreadStatus::Completed,
                "failed" => ThreadStatus::Failed,
                _ => ThreadStatus::Active,
            },
            title: r.title,
            origin_message_ref: r.origin_message_ref,
            last_reply_guid: r.last_reply_guid,
            created_at: r.created_at.parse().unwrap_or_default(),
            completed_at: r.completed_at.and_then(|s: String| s.parse().ok()),
        })
    })
    .transpose()
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
    debug!(
        pipe_id = %pipe_id,
        message_ref = %message_ref,
        "find_thread_by_message_ref: searching"
    );

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
        debug!(
            thread_id = ?r.id,
            origin_message_ref = ?r.origin_message_ref,
            last_reply_guid = ?r.last_reply_guid,
            "find_thread_by_message_ref: MATCHED via origin/last_reply_guid"
        );
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

    debug!(
        message_ref = %message_ref,
        "find_thread_by_message_ref: no match in threads table, checking thread_reply_guids"
    );

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
        Some(r) => {
            debug!(
                thread_id = ?r.id,
                "find_thread_by_message_ref: MATCHED via thread_reply_guids table"
            );
            Ok(Some(Thread {
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
            }))
        }
        None => {
            debug!(
                pipe_id = %pipe_id,
                message_ref = %message_ref,
                "find_thread_by_message_ref: no match — will create new thread"
            );
            Ok(None)
        }
    }
}

/// Find or create a thread for an inbound message.
///
/// Resolution order:
///   1. If `reply_to_ref` is provided, try to match an existing active thread
///      by message GUID (enables iMessage/WhatsApp/Telegram reply-chain routing).
///   2. Otherwise, create a new thread.  Every message without an explicit
///      reply-to reference is treated as a new conversation.
pub async fn find_or_create_thread(
    db: &SqlitePool,
    pipe_id: &str,
    user_id: &str,
    agent_id: &str,
    force_new_session: bool,
    origin_message_ref: Option<&str>,
    reply_to_ref: Option<&str>,
) -> Result<Thread> {
    debug!(
        pipe_id = %pipe_id,
        origin_message_ref = ?origin_message_ref,
        reply_to_ref = ?reply_to_ref,
        "find_or_create_thread: called"
    );

    // 1. Try to match an existing thread via the reply-to reference
    if let Some(ref_guid) = reply_to_ref {
        if let Some(existing) = find_thread_by_message_ref(db, pipe_id, ref_guid).await? {
            debug!(
                thread_id = %existing.id,
                reply_to = %ref_guid,
                "Reusing existing thread via reply-to match"
            );
            return Ok(existing);
        }
        debug!(
            pipe_id = %pipe_id,
            reply_to_ref = %ref_guid,
            "reply_to_ref provided but no matching thread found"
        );
    }

    // 2. No match — create a new thread.
    //
    // Every message without an explicit reply-to reference starts a fresh
    // thread.  All supported channels (iMessage, WhatsApp, Telegram) provide
    // reply-to metadata when the user replies inline, so the absence of
    // reply_to_ref reliably signals a new conversation.
    debug!("find_or_create_thread: no existing thread found — creating new");
    create_thread(
        db,
        pipe_id,
        user_id,
        agent_id,
        force_new_session,
        origin_message_ref,
    )
    .await
}

/// Store Selu's outbound reply GUID on the thread so future incoming
/// replies can be matched back to this thread.
///
/// Updates the `last_reply_guid` convenience column AND inserts into the
/// `thread_reply_guids` lookup table so that ALL outbound GUIDs are
/// searchable (not just the most recent one).
pub async fn update_reply_guid(
    db: &SqlitePool,
    thread_id: &str,
    pipe_id: &str,
    reply_guid: &str,
) -> Result<()> {
    debug!(
        thread_id = %thread_id,
        pipe_id = %pipe_id,
        reply_guid = %reply_guid,
        "update_reply_guid: storing outbound GUID"
    );

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
        info!(
            count = count,
            idle_hours = idle_hours,
            "Closed idle threads"
        );
    }
    Ok(count)
}

/// Count active (in-flight) threads for a given session.
#[allow(dead_code)]
pub async fn count_active_threads(db: &SqlitePool, session_id: &str) -> Result<i64> {
    let row = sqlx::query!(
        "SELECT COUNT(*) as cnt FROM threads WHERE session_id = ? AND status = 'active'",
        session_id,
    )
    .fetch_one(db)
    .await?;

    Ok(row.cnt.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    async fn setup_db() -> SqlitePool {
        let db = SqlitePool::connect(":memory:").await.unwrap();
        sqlx::query(
            r#"CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                pipe_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                workspace_id TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                last_active_at TEXT NOT NULL DEFAULT (datetime('now'))
            )"#,
        )
        .execute(&db)
        .await
        .unwrap();

        sqlx::query(
            r#"CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                pipe_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                title TEXT,
                origin_message_ref TEXT,
                last_reply_guid TEXT,
                thread_kind TEXT NOT NULL DEFAULT 'conversation',
                schedule_id TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                completed_at TEXT
            )"#,
        )
        .execute(&db)
        .await
        .unwrap();

        sqlx::query(
            r#"CREATE TABLE messages (
                id TEXT PRIMARY KEY,
                pipe_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                thread_id TEXT,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                tool_call_id TEXT,
                tool_calls_json TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            )"#,
        )
        .execute(&db)
        .await
        .unwrap();

        sqlx::query(
            r#"CREATE TABLE thread_reply_guids (
                id TEXT PRIMARY KEY,
                thread_id TEXT NOT NULL,
                pipe_id TEXT NOT NULL,
                reply_guid TEXT NOT NULL
            )"#,
        )
        .execute(&db)
        .await
        .unwrap();

        sqlx::query(
            r#"CREATE TABLE thread_agent_sessions (
                id TEXT PRIMARY KEY,
                thread_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(thread_id, agent_id)
            )"#,
        )
        .execute(&db)
        .await
        .unwrap();

        db
    }

    #[tokio::test]
    async fn create_thread_binds_initial_agent_session() {
        let db = setup_db().await;
        let pipe_id = Uuid::new_v4().to_string();
        let user_id = Uuid::new_v4().to_string();
        let agent_id = "coding-github";

        let thread = create_thread(&db, &pipe_id, &user_id, agent_id, true, Some("msg-1"))
            .await
            .unwrap();

        let session_id = thread.session_id.to_string();
        let thread_id = thread.id.to_string();
        let mapped_session_id: String = sqlx::query(
            "SELECT session_id FROM thread_agent_sessions
             WHERE thread_id = ? AND agent_id = ?",
        )
        .bind(&thread_id)
        .bind(agent_id)
        .fetch_one(&db)
        .await
        .unwrap()
        .get("session_id");
        assert_eq!(mapped_session_id, session_id);
    }

    #[tokio::test]
    async fn schedule_thread_reuses_existing_thread_for_same_schedule_and_pipe() {
        let db = setup_db().await;
        let pipe_id = Uuid::new_v4().to_string();
        let user_id = Uuid::new_v4().to_string();
        let schedule_id = Uuid::new_v4().to_string();

        let first =
            find_or_create_schedule_thread(&db, &pipe_id, &user_id, "default", &schedule_id, false)
                .await
                .unwrap();
        complete_thread(&db, &first.id.to_string()).await.unwrap();

        let second =
            find_or_create_schedule_thread(&db, &pipe_id, &user_id, "default", &schedule_id, false)
                .await
                .unwrap();

        assert_eq!(second.id, first.id);
        let status: String = sqlx::query_scalar("SELECT status FROM threads WHERE id = ?")
            .bind(first.id.to_string())
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(status, "active");
    }

    #[tokio::test]
    async fn new_message_without_reply_ref_always_creates_new_thread() {
        let db = setup_db().await;
        let pipe_id = Uuid::new_v4().to_string();
        let user_id = Uuid::new_v4().to_string();
        let agent_id = "weather";

        // Create an existing thread with a message
        let first = create_thread(&db, &pipe_id, &user_id, agent_id, false, Some("msg-1"))
            .await
            .unwrap();
        let first_tid = first.id.to_string();
        let first_sid = first.session_id.to_string();

        sqlx::query(
            "INSERT INTO messages (id, pipe_id, session_id, thread_id, role, content)
             VALUES (?, ?, ?, ?, 'assistant', ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&pipe_id)
        .bind(&first_sid)
        .bind(&first_tid)
        .bind("Sunny, 19C.")
        .execute(&db)
        .await
        .unwrap();

        // A new message without reply_to_ref always creates a new thread
        let resolved = find_or_create_thread(
            &db,
            &pipe_id,
            &user_id,
            agent_id,
            false,
            Some("msg-2"),
            None,
        )
        .await
        .unwrap();

        assert_ne!(resolved.id, first.id);
    }

    #[tokio::test]
    async fn reply_to_ref_reuses_existing_thread() {
        let db = setup_db().await;
        let pipe_id = Uuid::new_v4().to_string();
        let user_id = Uuid::new_v4().to_string();
        let agent_id = "weather";

        let first = create_thread(&db, &pipe_id, &user_id, agent_id, false, Some("msg-1"))
            .await
            .unwrap();
        let thread_id = first.id.to_string();

        // Store a reply GUID for thread matching
        update_reply_guid(&db, &thread_id, &pipe_id, "reply-guid-1")
            .await
            .unwrap();

        // A reply referencing the stored GUID reuses the thread
        let resolved = find_or_create_thread(
            &db,
            &pipe_id,
            &user_id,
            agent_id,
            false,
            Some("msg-2"),
            Some("reply-guid-1"),
        )
        .await
        .unwrap();

        assert_eq!(resolved.id, first.id);
    }
}
