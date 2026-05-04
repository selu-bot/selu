use sqlx::SqlitePool;
use std::collections::HashMap;

// ── Shared data types ───────────────────────────────────────────────────────

pub struct PipeRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub transport: String,
    pub outbound_url: String,
    pub default_agent_id: Option<String>,
    pub active: bool,
    pub created_at: String,
}

pub struct ThreadRow {
    pub id: String,
    pub pipe_id: String,
    pub title: Option<String>,
    pub status: String,
    pub thread_kind: String,
    pub schedule_id: Option<String>,
    pub created_at: String,
    pub last_activity_at: Option<String>,
}

pub struct MessageRow {
    pub id: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
    pub tool_calls: Vec<ToolCallRow>,
    pub attachments_json: Option<String>,
    pub compacted: bool,
}

pub struct ToolCallRow {
    pub name: String,
    pub result: Option<String>,
}

pub struct SearchResultRow {
    pub thread_id: String,
    pub pipe_id: String,
    pub title: Option<String>,
    pub snippet: String,
    pub last_activity_at: Option<String>,
}

// ── Queries ─────────────────────────────────────────────────────────────────

/// List active pipes for a user. Optionally filter by transport type
/// (e.g. `Some("web")` for the web UI, `None` for all active pipes).
pub async fn list_user_pipes(
    db: &SqlitePool,
    user_id: &str,
    transport_filter: Option<&str>,
) -> Vec<PipeRow> {
    let rows = sqlx::query!(
        "SELECT id, user_id, name, transport, outbound_url, default_agent_id, active, created_at
         FROM pipes WHERE user_id = ? AND active = 1 AND (? IS NULL OR transport = ?) ORDER BY created_at",
        user_id,
        transport_filter,
        transport_filter,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    rows.into_iter()
        .map(|r| PipeRow {
            id: r.id.unwrap_or_default(),
            user_id: r.user_id,
            name: r.name,
            transport: r.transport,
            outbound_url: r.outbound_url,
            default_agent_id: r.default_agent_id,
            active: r.active != 0,
            created_at: r.created_at,
        })
        .collect()
}

/// List threads for a pipe, ordered by most recent activity. The title falls
/// back to the first 40 characters of the first message when not explicitly set.
pub async fn list_pipe_threads(
    db: &SqlitePool,
    pipe_id: &str,
    user_id: &str,
    limit: i32,
) -> Vec<ThreadRow> {
    let rows = sqlx::query!(
        r#"SELECT t.id, t.pipe_id, t.title, t.status, t.thread_kind, t.schedule_id, t.created_at,
                  (SELECT content FROM messages WHERE thread_id = t.id ORDER BY created_at ASC LIMIT 1) as first_msg,
                  (SELECT MAX(created_at) FROM messages WHERE thread_id = t.id) as "last_activity_at?: String"
           FROM threads t
           WHERE t.pipe_id = ? AND t.user_id = ?
           ORDER BY COALESCE(
               (SELECT MAX(created_at) FROM messages WHERE thread_id = t.id),
               t.created_at
           ) DESC
           LIMIT ?"#,
        pipe_id,
        user_id,
        limit,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    rows.into_iter()
        .map(|r| {
            let title = r.title.or_else(|| {
                if r.first_msg.is_empty() {
                    None
                } else {
                    Some(r.first_msg.chars().take(40).collect::<String>())
                }
            });
            ThreadRow {
                id: r.id.unwrap_or_default(),
                pipe_id: r.pipe_id,
                title,
                status: r.status,
                thread_kind: r.thread_kind,
                schedule_id: r.schedule_id,
                created_at: r.created_at,
                last_activity_at: r.last_activity_at,
            }
        })
        .collect()
}

/// List messages for a thread with tool-call result matching.
///
/// Messages are returned in chronological order. Tool-result messages are
/// merged into their parent assistant message's `tool_calls` vec rather than
/// appearing as standalone rows.
///
/// Returns `(messages, has_more)` where `has_more` indicates whether older
/// messages exist beyond the page boundary.
pub async fn list_thread_messages(
    db: &SqlitePool,
    thread_id: &str,
    before: Option<&str>,
    page_size: i32,
) -> (Vec<MessageRow>, bool) {
    let mut rows = sqlx::query!(
        "SELECT id, role, content, created_at, tool_calls_json, tool_call_id, attachments_json, compacted
         FROM messages
         WHERE thread_id = ? AND (? IS NULL OR created_at < ?)
         ORDER BY created_at DESC LIMIT ?",
        thread_id,
        before,
        before,
        page_size,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    rows.reverse();
    let has_more = rows.len() as i32 >= page_size;

    let mut messages: Vec<Option<MessageRow>> = Vec::with_capacity(rows.len());
    let mut tool_call_map: HashMap<String, (usize, usize)> = HashMap::new();

    for r in &rows {
        // Tool-result rows: merge into parent assistant message
        if r.role == "tool" {
            if let Some(tc_id) = &r.tool_call_id {
                if let Some(&(msg_idx, tc_idx)) = tool_call_map.get(tc_id) {
                    if let Some(Some(msg)) = messages.get_mut(msg_idx) {
                        if tc_idx < msg.tool_calls.len() {
                            msg.tool_calls[tc_idx].result = Some(r.content.clone());
                        }
                    }
                    continue;
                }
            }
        }

        // Parse tool_calls_json
        let mut tool_calls = Vec::new();
        if let Some(json_str) = &r.tool_calls_json {
            if let Ok(calls) = serde_json::from_str::<Vec<serde_json::Value>>(json_str) {
                let msg_idx = messages.len();
                for (tc_idx, call) in calls.iter().enumerate() {
                    let name = call["name"].as_str().unwrap_or("tool").to_string();
                    let id = call["id"].as_str().unwrap_or_default().to_string();
                    tool_calls.push(ToolCallRow { name, result: None });
                    if !id.is_empty() {
                        tool_call_map.insert(id, (msg_idx, tc_idx));
                    }
                }
            }
        }

        messages.push(Some(MessageRow {
            id: r.id.clone().unwrap_or_default(),
            role: r.role.clone(),
            content: r.content.clone(),
            created_at: r.created_at.clone(),
            tool_calls,
            attachments_json: r.attachments_json.clone(),
            compacted: r.compacted != 0,
        }));
    }

    let result = messages.into_iter().flatten().collect();
    (result, has_more)
}

/// Search threads by title or message content using LIKE matching.
/// Returns threads whose title or any message content matches the query,
/// along with a snippet of the matching (or most recent) message.
pub async fn search_threads(
    db: &SqlitePool,
    pipe_id: &str,
    user_id: &str,
    query: &str,
    limit: i32,
) -> Vec<SearchResultRow> {
    let like_pattern = format!("%{query}%");

    let rows = sqlx::query!(
        r#"SELECT t.id as "id!", t.pipe_id, t.title,
                  COALESCE(
                      (SELECT SUBSTR(content, 1, 200) FROM messages
                       WHERE thread_id = t.id AND role IN ('user', 'assistant') AND content LIKE ?1
                       ORDER BY created_at DESC LIMIT 1),
                      (SELECT SUBSTR(content, 1, 200) FROM messages
                       WHERE thread_id = t.id AND role IN ('user', 'assistant')
                       ORDER BY created_at ASC LIMIT 1)
                  ) as "snippet!: String",
                  (SELECT MAX(created_at) FROM messages WHERE thread_id = t.id) as "last_activity_at?: String"
           FROM threads t
           WHERE t.pipe_id = ?2 AND t.user_id = ?3
             AND (
               t.title LIKE ?1
               OR EXISTS (SELECT 1 FROM messages m WHERE m.thread_id = t.id AND m.content LIKE ?1)
             )
           ORDER BY COALESCE(
               (SELECT MAX(created_at) FROM messages WHERE thread_id = t.id),
               t.created_at
           ) DESC
           LIMIT ?4"#,
        like_pattern,
        pipe_id,
        user_id,
        limit,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    rows.into_iter()
        .map(|r| SearchResultRow {
            thread_id: r.id,
            pipe_id: r.pipe_id,
            title: r.title,
            snippet: r.snippet,
            last_activity_at: r.last_activity_at,
        })
        .collect()
}
