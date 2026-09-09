//! Durable, client-neutral conversation state used by `/api/v1`.
//!
//! The event log is deliberately separate from `agent_events`:
//! UI updates must never invoke an agent subscription.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::broadcast;

use crate::services::timestamps;

pub const EVENT_CHANNEL_CAPACITY: usize = 512;

#[derive(Clone)]
pub struct ConversationEventBus {
    sender: broadcast::Sender<ConversationEvent>,
}

impl ConversationEventBus {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ConversationEvent> {
        self.sender.subscribe()
    }

    pub async fn publish(
        &self,
        db: &SqlitePool,
        user_id: &str,
        thread_id: &str,
        run_id: Option<&str>,
        event_type: &str,
        entity_id: Option<&str>,
        payload: Value,
    ) -> Result<ConversationEvent> {
        let payload_json =
            serde_json::to_string(&payload).context("serialize conversation event")?;
        let result = sqlx::query(
            "INSERT INTO conversation_events (user_id, thread_id, run_id, event_type, entity_id, payload_json) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(user_id)
        .bind(thread_id)
        .bind(run_id)
        .bind(event_type)
        .bind(entity_id)
        .bind(&payload_json)
        .execute(db)
        .await
        .context("persist conversation event")?;

        let event = ConversationEvent {
            id: result.last_insert_rowid(),
            user_id: user_id.to_owned(),
            conversation_id: thread_id.to_owned(),
            run_id: run_id.map(str::to_owned),
            event_type: event_type.to_owned(),
            entity_id: entity_id.map(str::to_owned),
            payload,
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        };
        // Persistence, not this in-process broadcast, is authoritative. A
        // reconnect replays by event id if a receiver was unavailable or slow.
        let _ = self.sender.send(event.clone());
        Ok(event)
    }

    /// Broadcast an event without journaling it. Used for facts a
    /// reconnecting client re-derives from the snapshot anyway, such as a
    /// deleted conversation whose thread row can no longer be referenced.
    pub fn publish_transient(
        &self,
        user_id: &str,
        thread_id: &str,
        event_type: &str,
        payload: Value,
    ) -> ConversationEvent {
        let event = ConversationEvent {
            id: 0,
            user_id: user_id.to_owned(),
            conversation_id: thread_id.to_owned(),
            run_id: None,
            event_type: event_type.to_owned(),
            entity_id: Some(thread_id.to_owned()),
            payload,
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        };
        let _ = self.sender.send(event.clone());
        event
    }
}

/// Keyset cursor for the conversation list: the activity timestamp and thread
/// id of the last row of the previous page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListCursor {
    pub last_activity_at: String,
    pub id: String,
}

impl ListCursor {
    pub fn encode(&self) -> String {
        format!("{}|{}", self.last_activity_at, self.id)
    }

    pub fn decode(raw: &str) -> Result<Self> {
        let (last_activity_at, id) = raw
            .split_once('|')
            .filter(|(activity, id)| !activity.is_empty() && !id.is_empty())
            .context("malformed conversation list cursor")?;
        Ok(Self {
            last_activity_at: timestamps::canonical_utc(last_activity_at)?,
            id: id.to_owned(),
        })
    }
}

pub struct ConversationPage {
    pub conversations: Vec<ConversationSummary>,
    pub next_cursor: Option<ListCursor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationEvent {
    pub id: i64,
    #[serde(skip_serializing)]
    pub user_id: String,
    pub conversation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    pub payload: Value,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationSummary {
    pub id: String,
    pub channel_id: String,
    pub channel_name: String,
    pub title: Option<String>,
    pub status: String,
    pub kind: String,
    pub schedule_id: Option<String>,
    pub created_at: String,
    pub last_activity_at: String,
    pub active_run_id: Option<String>,
    pub saved_at: Option<String>,
    pub can_save: bool,
    pub preview: Option<String>,
    pub message_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationMessage {
    pub id: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
    pub tool_calls: Option<Value>,
    pub tool_call_id: Option<String>,
    pub attachments: Option<Value>,
    pub compacted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationRun {
    pub id: String,
    pub client_message_id: String,
    pub status: String,
    pub error_code: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

fn canonical_optional(value: Option<String>) -> Result<Option<String>> {
    value.as_deref().map(timestamps::canonical_utc).transpose()
}

impl ConversationSummary {
    fn canonicalized(mut self) -> Result<Self> {
        self.created_at = timestamps::canonical_utc(&self.created_at)?;
        self.last_activity_at = timestamps::canonical_utc(&self.last_activity_at)?;
        self.saved_at = canonical_optional(self.saved_at)?;
        Ok(self)
    }
}

impl ConversationMessage {
    fn canonicalized(mut self) -> Result<Self> {
        self.created_at = timestamps::canonical_utc(&self.created_at)?;
        Ok(self)
    }
}

impl ConversationRun {
    pub fn canonicalized(mut self) -> Result<Self> {
        self.created_at = timestamps::canonical_utc(&self.created_at)?;
        self.started_at = canonical_optional(self.started_at)?;
        self.completed_at = canonical_optional(self.completed_at)?;
        Ok(self)
    }
}

impl ConversationEvent {
    fn canonicalized(mut self) -> Result<Self> {
        self.created_at = timestamps::canonical_utc(&self.created_at)?;
        Ok(self)
    }
}

pub async fn list_conversations(
    db: &SqlitePool,
    user_id: &str,
    limit: i64,
    before: Option<&ListCursor>,
    saved: Option<bool>,
) -> Result<ConversationPage> {
    let limit = limit.clamp(1, 100);
    let saved_filter = saved.map(i64::from);
    // Fetch one extra row to learn whether an older page exists without a
    // second COUNT query. Empty conversations are provisional: they become
    // visible only after the first message is accepted. Schedule threads stay
    // visible because their lifecycle is owned by the automation executor.
    let rows = sqlx::query_as::<_, (String, String, String, Option<String>, String, String, Option<String>, String, String, Option<String>, Option<String>, Option<String>, i64)>(
        r#"WITH ranked AS (
               SELECT t.id, t.pipe_id, p.name, t.title, t.status, t.thread_kind, t.schedule_id, t.created_at,
                      COALESCE((SELECT m.created_at FROM messages m WHERE m.thread_id = t.id
                                ORDER BY julianday(m.created_at) DESC, m.id DESC LIMIT 1), t.created_at) AS last_activity_at,
                      (SELECT r.id FROM conversation_runs r
                       WHERE r.thread_id = t.id AND r.status IN ('queued','running','waiting_for_approval','cancelling')
                       ORDER BY julianday(r.created_at) DESC, r.id DESC LIMIT 1) AS active_run_id,
                      t.saved_at,
                      (SELECT NULLIF(TRIM(m.content), '') FROM messages m
                       WHERE m.thread_id = t.id AND m.role IN ('user','assistant') AND TRIM(m.content) <> ''
                       ORDER BY julianday(m.created_at) DESC, m.id DESC LIMIT 1) AS preview,
                      (SELECT COUNT(*) FROM messages m
                       WHERE m.thread_id = t.id AND m.role IN ('user','assistant') AND TRIM(m.content) <> '') AS message_count
               FROM threads t JOIN pipes p ON p.id = t.pipe_id
               WHERE t.user_id = ?
                 AND (t.started_at IS NOT NULL OR t.thread_kind = 'schedule')
                 AND (? IS NULL
                      OR (? = 1 AND t.saved_at IS NOT NULL)
                      OR (? = 0 AND t.saved_at IS NULL))
           )
           SELECT id, pipe_id, name, title, status, thread_kind, schedule_id, created_at,
                  last_activity_at, active_run_id, saved_at, preview, message_count
           FROM ranked
           WHERE (? IS NULL OR julianday(last_activity_at) < julianday(?)
                  OR (julianday(last_activity_at) = julianday(?) AND id < ?))
           ORDER BY julianday(last_activity_at) DESC, id DESC
           LIMIT ?"#,
    )
    .bind(user_id)
    .bind(saved_filter)
    .bind(saved_filter)
    .bind(saved_filter)
    .bind(before.map(|cursor| cursor.last_activity_at.as_str()))
    .bind(before.map(|cursor| cursor.last_activity_at.as_str()))
    .bind(before.map(|cursor| cursor.last_activity_at.as_str()))
    .bind(before.map(|cursor| cursor.id.as_str()))
    .bind(limit + 1)
    .fetch_all(db)
    .await
    .context("list conversations")?;

    let mut conversations: Vec<ConversationSummary> = rows
        .into_iter()
        .map(|r| {
            ConversationSummary {
                id: r.0,
                channel_id: r.1,
                channel_name: r.2,
                title: r.3,
                status: r.4,
                can_save: r.5 != "schedule",
                kind: r.5,
                schedule_id: r.6,
                created_at: r.7,
                last_activity_at: r.8,
                active_run_id: r.9,
                saved_at: r.10,
                preview: r.11,
                message_count: r.12,
            }
            .canonicalized()
        })
        .collect::<Result<Vec<_>>>()?;
    let next_cursor = if conversations.len() as i64 > limit {
        conversations.truncate(limit as usize);
        conversations.last().map(|last| ListCursor {
            last_activity_at: last.last_activity_at.clone(),
            id: last.id.clone(),
        })
    } else {
        None
    };
    Ok(ConversationPage {
        conversations,
        next_cursor,
    })
}

/// Remove a conversation and every row that references it. Returns the
/// persisted artifact files so the caller can drop them from the in-memory
/// store and disk after the transaction committed.
pub async fn delete_conversation(
    db: &SqlitePool,
    user_id: &str,
    conversation_id: &str,
) -> Result<Vec<DeletedArtifact>> {
    let mut tx = db.begin().await.context("begin conversation delete")?;
    let artifacts = sqlx::query_as::<_, (String, String)>(
        "SELECT id, file_path FROM thread_artifacts WHERE thread_id = ? AND user_id = ?",
    )
    .bind(conversation_id)
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await
    .context("list conversation artifacts")?
    .into_iter()
    .map(|(artifact_id, file_path)| DeletedArtifact {
        artifact_id,
        file_path,
    })
    .collect::<Vec<_>>();

    for statement in [
        "DELETE FROM pending_tool_approvals WHERE thread_id = ?",
        "DELETE FROM thread_reply_guids WHERE thread_id = ?",
        "DELETE FROM thread_agent_sessions WHERE thread_id = ?",
        "DELETE FROM thread_artifacts WHERE thread_id = ?",
        "DELETE FROM conversation_events WHERE thread_id = ?",
        "DELETE FROM conversation_runs WHERE thread_id = ?",
        "DELETE FROM messages WHERE thread_id = ?",
    ] {
        sqlx::query(statement)
            .bind(conversation_id)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("delete conversation dependents: {statement}"))?;
    }
    let deleted = sqlx::query("DELETE FROM threads WHERE id = ? AND user_id = ?")
        .bind(conversation_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .context("delete conversation thread")?;
    if deleted.rows_affected() == 0 {
        anyhow::bail!("conversation {conversation_id} not found for user");
    }
    tx.commit().await.context("commit conversation delete")?;
    Ok(artifacts)
}

#[derive(Debug, Clone)]
pub struct DeletedArtifact {
    pub artifact_id: String,
    pub file_path: String,
}

pub async fn conversation_owner(
    db: &SqlitePool,
    conversation_id: &str,
) -> Result<Option<(String, String)>> {
    sqlx::query_as::<_, (String, String)>("SELECT user_id, pipe_id FROM threads WHERE id = ?")
        .bind(conversation_id)
        .fetch_optional(db)
        .await
        .context("load conversation owner")
}

pub async fn get_conversation(
    db: &SqlitePool,
    user_id: &str,
    conversation_id: &str,
) -> Result<Option<ConversationSummary>> {
    let row = sqlx::query_as::<_, (String, String, String, Option<String>, String, String, Option<String>, String, String, Option<String>, Option<String>, Option<String>, i64)>(
        r#"SELECT t.id, t.pipe_id, p.name, t.title, t.status, t.thread_kind, t.schedule_id, t.created_at,
                  COALESCE((SELECT m.created_at FROM messages m WHERE m.thread_id = t.id
                            ORDER BY julianday(m.created_at) DESC, m.id DESC LIMIT 1), t.created_at) AS last_activity_at,
                  (SELECT r.id FROM conversation_runs r
                   WHERE r.thread_id = t.id AND r.status IN ('queued','running','waiting_for_approval','cancelling')
                   ORDER BY julianday(r.created_at) DESC, r.id DESC LIMIT 1) AS active_run_id,
                  t.saved_at,
                  (SELECT NULLIF(TRIM(m.content), '') FROM messages m
                   WHERE m.thread_id = t.id AND m.role IN ('user','assistant') AND TRIM(m.content) <> ''
                   ORDER BY julianday(m.created_at) DESC, m.id DESC LIMIT 1) AS preview,
                  (SELECT COUNT(*) FROM messages m
                   WHERE m.thread_id = t.id AND m.role IN ('user','assistant') AND TRIM(m.content) <> '') AS message_count
           FROM threads t JOIN pipes p ON p.id = t.pipe_id
           WHERE t.user_id = ? AND t.id = ?"#,
    )
    .bind(user_id)
    .bind(conversation_id)
    .fetch_optional(db)
    .await
    .context("load conversation")?;
    row.map(|r| ConversationSummary {
        id: r.0,
        channel_id: r.1,
        channel_name: r.2,
        title: r.3,
        status: r.4,
        can_save: r.5 != "schedule",
        kind: r.5,
        schedule_id: r.6,
        created_at: r.7,
        last_activity_at: r.8,
        active_run_id: r.9,
        saved_at: r.10,
        preview: r.11,
        message_count: r.12,
    })
    .map(ConversationSummary::canonicalized)
    .transpose()
}

pub async fn list_messages(
    db: &SqlitePool,
    conversation_id: &str,
    limit: i64,
) -> Result<Vec<ConversationMessage>> {
    let rows = sqlx::query_as::<_, (String, String, String, String, Option<String>, Option<String>, Option<String>, i64)>(
        "SELECT id, role, content, created_at, tool_calls_json, tool_call_id, attachments_json, compacted FROM messages WHERE thread_id = ? ORDER BY julianday(created_at) DESC, id DESC LIMIT ?",
    )
    .bind(conversation_id)
    .bind(limit.clamp(1, 200))
    .fetch_all(db)
    .await
    .context("list conversation messages")?;

    rows.into_iter()
        .rev()
        .map(|r| {
            ConversationMessage {
                id: r.0,
                role: r.1,
                content: r.2,
                created_at: r.3,
                tool_calls: r.4.and_then(|v| serde_json::from_str(&v).ok()),
                tool_call_id: r.5,
                attachments: r.6.and_then(|v| serde_json::from_str(&v).ok()),
                compacted: r.7 != 0,
            }
            .canonicalized()
        })
        .collect()
}

pub async fn list_runs(db: &SqlitePool, conversation_id: &str) -> Result<Vec<ConversationRun>> {
    let rows = sqlx::query_as::<_, (String, String, String, Option<String>, String, Option<String>, Option<String>)>(
        "SELECT id, client_message_id, status, error_code, created_at, started_at, completed_at FROM conversation_runs WHERE thread_id = ? ORDER BY julianday(created_at) DESC, id DESC LIMIT 20",
    )
    .bind(conversation_id)
    .fetch_all(db)
    .await
    .context("list conversation runs")?;
    rows.into_iter()
        .map(|r| {
            ConversationRun {
                id: r.0,
                client_message_id: r.1,
                status: r.2,
                error_code: r.3,
                created_at: r.4,
                started_at: r.5,
                completed_at: r.6,
            }
            .canonicalized()
        })
        .collect()
}

pub async fn list_events(
    db: &SqlitePool,
    user_id: &str,
    after: i64,
    conversation_id: Option<&str>,
) -> Result<Vec<ConversationEvent>> {
    let rows = if let Some(conversation_id) = conversation_id {
        sqlx::query_as::<_, (i64, String, String, Option<String>, String, Option<String>, String, String)>(
            "SELECT id, user_id, thread_id, run_id, event_type, entity_id, payload_json, created_at FROM conversation_events WHERE user_id = ? AND thread_id = ? AND id > ? ORDER BY id ASC LIMIT 500",
        ).bind(user_id).bind(conversation_id).bind(after).fetch_all(db).await?
    } else {
        sqlx::query_as::<_, (i64, String, String, Option<String>, String, Option<String>, String, String)>(
            "SELECT id, user_id, thread_id, run_id, event_type, entity_id, payload_json, created_at FROM conversation_events WHERE user_id = ? AND id > ? ORDER BY id ASC LIMIT 500",
        ).bind(user_id).bind(after).fetch_all(db).await?
    };
    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        let Ok(payload) = serde_json::from_str::<Value>(&row.6) else {
            continue;
        };
        events.push(
            ConversationEvent {
                id: row.0,
                user_id: row.1,
                conversation_id: row.2,
                run_id: row.3,
                event_type: row.4,
                entity_id: row.5,
                payload,
                created_at: row.7,
            }
            .canonicalized()?,
        );
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn setup_db() -> SqlitePool {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        db
    }

    async fn seed_user_and_pipe(db: &SqlitePool) -> (String, String) {
        let user_id = uuid::Uuid::new_v4().to_string();
        let pipe_id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO users (id, username, display_name, password_hash) VALUES (?, ?, 'Test', 'x')")
            .bind(&user_id).bind(&user_id).execute(db).await.unwrap();
        sqlx::query("INSERT INTO pipes (id, user_id, name, transport, inbound_token, outbound_url) VALUES (?, ?, 'Web', 'web', 't', '')")
            .bind(&pipe_id).bind(&user_id).execute(db).await.unwrap();
        sqlx::query("INSERT INTO sessions (id, pipe_id, user_id, agent_id) VALUES ('session', ?, ?, 'default')")
            .bind(&pipe_id).bind(&user_id).execute(db).await.unwrap();
        (user_id, pipe_id)
    }

    async fn seed_thread(
        db: &SqlitePool,
        user_id: &str,
        pipe_id: &str,
        created_at: &str,
        kind: &str,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO threads (id, pipe_id, session_id, user_id, status, thread_kind, created_at, started_at) VALUES (?, ?, 'session', ?, 'active', ?, ?, ?)")
            .bind(&id).bind(pipe_id).bind(user_id).bind(kind).bind(created_at).bind(created_at).execute(db).await.unwrap();
        id
    }

    #[test]
    fn cursor_round_trips_and_rejects_garbage() {
        let cursor = ListCursor {
            last_activity_at: "2026-09-04T07:16:46.906Z".into(),
            id: "abc".into(),
        };
        assert_eq!(ListCursor::decode(&cursor.encode()).unwrap(), cursor);
        let legacy = ListCursor::decode("2026-09-04 07:16:46|legacy").unwrap();
        assert_eq!(legacy.last_activity_at, "2026-09-04T07:16:46.000Z");
        assert!(ListCursor::decode("").is_err());
        assert!(ListCursor::decode("no-separator").is_err());
        assert!(ListCursor::decode("|missing-activity").is_err());
    }

    #[tokio::test]
    async fn mixed_timestamp_formats_are_canonical_and_paginate_by_instant() {
        let db = setup_db().await;
        let (user_id, pipe_id) = seed_user_and_pipe(&db).await;
        let oldest = seed_thread(
            &db,
            &user_id,
            &pipe_id,
            "2026-09-08 21:30:00",
            "conversation",
        )
        .await;
        let middle = seed_thread(
            &db,
            &user_id,
            &pipe_id,
            "2026-09-08T23:45:00+02:00",
            "conversation",
        )
        .await;
        let newest = seed_thread(
            &db,
            &user_id,
            &pipe_id,
            "2026-09-08T21:50:00.000Z",
            "conversation",
        )
        .await;

        let first = list_conversations(&db, &user_id, 2, None, None)
            .await
            .unwrap();
        assert_eq!(
            first
                .conversations
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec![newest.as_str(), middle.as_str()]
        );
        assert!(
            first
                .conversations
                .iter()
                .all(|item| item.created_at.ends_with('Z'))
        );

        let second = list_conversations(&db, &user_id, 2, first.next_cursor.as_ref(), None)
            .await
            .unwrap();
        assert_eq!(second.conversations.len(), 1);
        assert_eq!(second.conversations[0].id, oldest);
        assert!(second.next_cursor.is_none());
    }

    #[tokio::test]
    async fn list_pages_through_every_conversation_exactly_once() {
        let db = setup_db().await;
        let (user_id, pipe_id) = seed_user_and_pipe(&db).await;
        let mut expected = Vec::new();
        for day in 1..=5 {
            expected.push(
                seed_thread(
                    &db,
                    &user_id,
                    &pipe_id,
                    &format!("2026-09-0{day} 10:00:00"),
                    "conversation",
                )
                .await,
            );
        }
        // Two threads share an activity timestamp so the id tie-breaker is exercised.
        expected
            .push(seed_thread(&db, &user_id, &pipe_id, "2026-09-05 10:00:00", "schedule").await);

        let mut seen = Vec::new();
        let mut cursor = None;
        let mut pages = 0;
        loop {
            let page = list_conversations(&db, &user_id, 2, cursor.as_ref(), None)
                .await
                .unwrap();
            assert!(page.conversations.len() <= 2);
            seen.extend(page.conversations.iter().map(|c| c.id.clone()));
            pages += 1;
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(pages, 3);
        assert_eq!(seen.len(), expected.len());
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            expected.len(),
            "no conversation may repeat across pages"
        );
        // Newest activity first.
        let first_two: Vec<_> = seen.iter().take(2).collect();
        assert!(first_two.contains(&&expected[4]) && first_two.contains(&&expected[5]));
        assert_eq!(seen.last().unwrap(), &expected[0]);
    }

    #[tokio::test]
    async fn provisional_conversation_is_hidden_until_started() {
        let db = setup_db().await;
        let (user_id, pipe_id) = seed_user_and_pipe(&db).await;
        let thread_id = seed_thread(
            &db,
            &user_id,
            &pipe_id,
            "2026-09-08 10:00:00",
            "conversation",
        )
        .await;
        sqlx::query("UPDATE threads SET started_at = NULL WHERE id = ?")
            .bind(&thread_id)
            .execute(&db)
            .await
            .unwrap();

        let hidden = list_conversations(&db, &user_id, 20, None, None)
            .await
            .unwrap();
        assert!(!hidden.conversations.iter().any(|item| item.id == thread_id));

        sqlx::query("UPDATE threads SET started_at = created_at WHERE id = ?")
            .bind(&thread_id)
            .execute(&db)
            .await
            .unwrap();
        let visible = list_conversations(&db, &user_id, 20, None, None)
            .await
            .unwrap();
        assert!(
            visible
                .conversations
                .iter()
                .any(|item| item.id == thread_id)
        );
    }

    #[tokio::test]
    async fn saved_filter_and_schedule_metadata_are_stable() {
        let db = setup_db().await;
        let (user_id, pipe_id) = seed_user_and_pipe(&db).await;
        let normal = seed_thread(
            &db,
            &user_id,
            &pipe_id,
            "2026-09-08 10:00:00",
            "conversation",
        )
        .await;
        let scheduled =
            seed_thread(&db, &user_id, &pipe_id, "2026-09-08 11:00:00", "schedule").await;
        sqlx::query("UPDATE threads SET saved_at = '2026-09-08 12:00:00' WHERE id = ?")
            .bind(&normal)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE threads SET schedule_id = 'daily-briefing' WHERE id = ?")
            .bind(&scheduled)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages (id, pipe_id, session_id, thread_id, role, content, created_at) VALUES ('preview', ?, 'session', ?, 'assistant', 'Latest useful reply', '2026-09-08 12:00:00')")
            .bind(&pipe_id)
            .bind(&normal)
            .execute(&db)
            .await
            .unwrap();

        let saved = list_conversations(&db, &user_id, 20, None, Some(true))
            .await
            .unwrap();
        assert_eq!(saved.conversations.len(), 1);
        assert_eq!(saved.conversations[0].id, normal);
        assert_eq!(
            saved.conversations[0].preview.as_deref(),
            Some("Latest useful reply")
        );
        assert!(saved.conversations[0].can_save);

        let all = list_conversations(&db, &user_id, 20, None, None)
            .await
            .unwrap();
        let schedule = all
            .conversations
            .iter()
            .find(|item| item.id == scheduled)
            .unwrap();
        assert_eq!(schedule.schedule_id.as_deref(), Some("daily-briefing"));
        assert!(!schedule.can_save);
    }

    #[tokio::test]
    async fn delete_removes_thread_and_dependents() {
        let db = setup_db().await;
        let (user_id, pipe_id) = seed_user_and_pipe(&db).await;
        let thread_id = seed_thread(
            &db,
            &user_id,
            &pipe_id,
            "2026-09-01 10:00:00",
            "conversation",
        )
        .await;
        sqlx::query("INSERT INTO messages (id, pipe_id, session_id, thread_id, role, content) VALUES ('m1', ?, 'session', ?, 'user', 'hi')")
            .bind(&pipe_id).bind(&thread_id).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO conversation_runs (id, thread_id, user_id, client_message_id, status) VALUES ('r1', ?, ?, 'c1', 'completed')")
            .bind(&thread_id).bind(&user_id).execute(&db).await.unwrap();
        let bus = ConversationEventBus::new();
        bus.publish(
            &db,
            &user_id,
            &thread_id,
            Some("r1"),
            "run.created",
            None,
            serde_json::json!({}),
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO thread_artifacts (id, thread_id, user_id, filename, mime_type, size_bytes, file_path) VALUES ('a1', ?, ?, 'f.txt', 'text/plain', 1, '/nonexistent/f.txt')")
            .bind(&thread_id).bind(&user_id).execute(&db).await.unwrap();

        // Another user's id must not be able to delete it.
        assert!(
            delete_conversation(&db, "someone-else", &thread_id)
                .await
                .is_err()
        );
        assert!(
            get_conversation(&db, &user_id, &thread_id)
                .await
                .unwrap()
                .is_some()
        );

        let artifacts = delete_conversation(&db, &user_id, &thread_id)
            .await
            .unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].artifact_id, "a1");
        assert!(
            get_conversation(&db, &user_id, &thread_id)
                .await
                .unwrap()
                .is_none()
        );
        for (table, statement) in [
            (
                "messages",
                "SELECT COUNT(*) FROM messages WHERE thread_id = ?",
            ),
            (
                "conversation_runs",
                "SELECT COUNT(*) FROM conversation_runs WHERE thread_id = ?",
            ),
            (
                "conversation_events",
                "SELECT COUNT(*) FROM conversation_events WHERE thread_id = ?",
            ),
            (
                "thread_artifacts",
                "SELECT COUNT(*) FROM thread_artifacts WHERE thread_id = ?",
            ),
        ] {
            let count: i64 = sqlx::query_scalar(statement)
                .bind(&thread_id)
                .fetch_one(&db)
                .await
                .unwrap();
            assert_eq!(count, 0, "{table} still references the deleted thread");
        }
    }
}
