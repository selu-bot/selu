use askama::Template;
use axum::{
    Form,
    extract::{Multipart, Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response, Sse},
};
use base64::Engine;
use chrono::{DateTime, Datelike, NaiveDateTime, Utc};
use futures::StreamExt;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, warn};
use uuid::Uuid;

use crate::agents::engine::{ChannelKind, InboundAttachmentInput, TurnParams, run_turn};
use crate::agents::router as agent_router;
use crate::agents::thread as thread_mgr;
use crate::commands;
use crate::llm::tool_loop::LoopEvent;
use crate::state::AppState;
use crate::web::BasePath;
use crate::web::auth::AuthUser;

const MAX_WEB_CHAT_IMAGE_BYTES: usize = 5 * 1024 * 1024;

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PipeView {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct ThreadView {
    pub id: String,
    pub title: String,
    pub status: String,
    pub last_activity_at: String,
}

#[derive(Debug, Clone)]
pub struct ToolCallView {
    pub name: String,
    pub result: String,
}

#[derive(Debug, Clone)]
pub struct MessageView {
    pub role: String,
    pub content: String,
    pub image_urls: Vec<String>,
    pub created_at: String,
    pub tool_calls: Vec<ToolCallView>,
}

// ── Templates ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "chat.html")]
struct ChatTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    pipes: Vec<PipeView>,
    selected_pipe_id: String,
    selected_pipe: Option<PipeView>,
    threads: Vec<ThreadView>,
    selected_thread_id: String,
    messages: Vec<MessageView>,
    has_more_messages: bool,
    oldest_message_ts: String,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn fetch_pipes(db: &sqlx::SqlitePool, user_id: &str) -> Vec<PipeView> {
    sqlx::query!(
        "SELECT id, name, transport FROM pipes WHERE active = 1 AND transport = 'web' AND user_id = ? ORDER BY created_at",
        user_id
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| PipeView {
        id: r.id.unwrap_or_default(),
        name: r.name,
    })
    .collect()
}

async fn fetch_threads(
    db: &sqlx::SqlitePool,
    pipe_id: &str,
    user_id: &str,
    language: &str,
) -> Vec<ThreadView> {
    sqlx::query!(
        r#"SELECT t.id, t.pipe_id, t.title, t.status, t.created_at,
                  (SELECT content FROM messages WHERE thread_id = t.id ORDER BY created_at ASC LIMIT 1) as first_msg,
                  (SELECT MAX(created_at) FROM messages WHERE thread_id = t.id) as "last_message_at?: String"
           FROM threads t
           WHERE t.pipe_id = ? AND t.user_id = ?
           ORDER BY COALESCE((SELECT MAX(created_at) FROM messages WHERE thread_id = t.id), t.created_at) DESC, t.created_at DESC"#,
        pipe_id,
        user_id,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        let title = r.title.unwrap_or_else(|| {
            // Fall back to truncated first message
            let msg = &r.first_msg;
            if msg.is_empty() {
                "New conversation".to_string()
            } else {
                msg.chars().take(40).collect::<String>()
            }
        });
        let last_activity_raw = r.last_message_at.unwrap_or_else(|| r.created_at.clone());
        ThreadView {
            id: r.id.unwrap_or_default(),
            title,
            status: r.status,
            last_activity_at: format_timestamp_for_user(&last_activity_raw, language),
        }
    })
    .collect()
}

/// Fetch messages for display. Returns (messages, has_more, oldest_raw_ts).
async fn fetch_thread_messages(
    state: &AppState,
    thread_id: &str,
    language: &str,
    user_id: &str,
    before: Option<&str>,
) -> (Vec<MessageView>, bool, Option<String>) {
    let page_size: i32 = if before.is_some() { 50 } else { 200 };

    let mut rows = sqlx::query!(
        "SELECT role, content, created_at, tool_calls_json, tool_call_id
         FROM messages
         WHERE thread_id = ? AND (? IS NULL OR created_at < ?)
         ORDER BY created_at DESC LIMIT ?",
        thread_id,
        before,
        before,
        page_size,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    // Reverse from DESC to chronological order
    rows.reverse();
    let has_more = rows.len() as i32 >= page_size;
    let oldest_ts = rows.first().map(|r| r.created_at.clone());

    // First pass: build message views and collect tool-call ID → name mappings.
    // tool_call_id_to_index maps a tool_call_id to (index in out, index in tool_calls vec).
    let mut out: Vec<MessageView> = Vec::with_capacity(rows.len());
    let mut tool_call_id_to_index: HashMap<String, (usize, usize)> = HashMap::new();

    for r in &rows {
        if r.role == "tool" {
            // Tool result — try to merge into the parent tool-call message.
            if let Some(tc_id) = &r.tool_call_id {
                if let Some(&(msg_idx, tc_idx)) = tool_call_id_to_index.get(tc_id) {
                    out[msg_idx].tool_calls[tc_idx].result = r.content.clone();
                    continue; // Don't emit as a separate message
                }
            }
            // Orphaned tool result (no matching parent) — show as standalone
            out.push(MessageView {
                role: r.role.clone(),
                content: r.content.clone(),
                image_urls: Vec::new(),
                tool_calls: Vec::new(),
                created_at: format_timestamp_for_user(&r.created_at, language),
            });
            continue;
        }

        let (content, image_urls) = if r.role == "user" {
            user_message_for_display(
                state,
                user_id,
                &r.content,
                crate::i18n::t(language, "chat.image_only_sent"),
            )
            .await
        } else {
            (r.content.clone(), Vec::new())
        };

        // Parse tool_calls_json to extract individual call names and IDs.
        let mut tool_calls = Vec::new();
        if let Some(json_str) = &r.tool_calls_json {
            if let Ok(calls) = serde_json::from_str::<Vec<serde_json::Value>>(json_str) {
                let msg_idx = out.len();
                for (tc_idx, call) in calls.iter().enumerate() {
                    let name = call["name"].as_str().unwrap_or("tool").to_string();
                    let id = call["id"].as_str().unwrap_or_default().to_string();
                    tool_calls.push(ToolCallView {
                        name,
                        result: String::new(),
                    });
                    if !id.is_empty() {
                        tool_call_id_to_index.insert(id, (msg_idx, tc_idx));
                    }
                }
            }
        }

        out.push(MessageView {
            role: r.role.clone(),
            content,
            image_urls,
            tool_calls,
            created_at: format_timestamp_for_user(&r.created_at, language),
        });
    }
    (out, has_more, oldest_ts)
}

fn format_timestamp_for_user(raw: &str, language: &str) -> String {
    let parsed = NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S"))
        .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S%.f"))
        .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f"))
        .ok();
    let Some(naive) = parsed else {
        return raw.to_string();
    };

    let dt: DateTime<Utc> = DateTime::from_naive_utc_and_offset(naive, Utc);
    let now = Utc::now();
    let same_day = dt.date_naive() == now.date_naive();
    let same_year = dt.year() == now.year();

    if language == "de" {
        if same_day {
            dt.format("%H:%M").to_string()
        } else if same_year {
            dt.format("%d.%m. %H:%M").to_string()
        } else {
            dt.format("%d.%m.%Y %H:%M").to_string()
        }
    } else if same_day {
        dt.format("%-I:%M %p").to_string()
    } else if same_year {
        dt.format("%b %-d, %-I:%M %p").to_string()
    } else {
        dt.format("%b %-d, %Y %-I:%M %p").to_string()
    }
}

fn render_template(tmpl: ChatTemplate) -> Response {
    match tmpl.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /chat — chat index (no pipe selected)
pub async fn chat_index(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    let pipes = fetch_pipes(&state.db, &user.user_id).await;

    // If the user has exactly one web pipe, redirect directly to it
    if pipes.len() == 1 {
        return axum::response::Redirect::to(&format!("{}/chat/{}", base_path, pipes[0].id))
            .into_response();
    }

    render_template(ChatTemplate {
        active_nav: "chat",
        is_admin: user.is_admin,
        base_path,
        selected_pipe_id: String::new(),
        selected_pipe: None,
        threads: vec![],
        selected_thread_id: String::new(),
        messages: vec![],
        pipes,
        has_more_messages: false,
        oldest_message_ts: String::new(),
    })
}

/// GET /chat/{pipe_id} — pipe selected, show thread list
pub async fn chat_pipe(
    user: AuthUser,
    Path(pipe_id): Path<Uuid>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    let pipe_id_str = pipe_id.to_string();
    let pipes = fetch_pipes(&state.db, &user.user_id).await;
    let threads = fetch_threads(&state.db, &pipe_id_str, &user.user_id, &user.language).await;
    let selected = pipes.iter().find(|p| p.id == pipe_id_str).cloned();

    render_template(ChatTemplate {
        active_nav: "chat",
        is_admin: user.is_admin,
        base_path,
        selected_pipe_id: pipe_id_str,
        selected_pipe: selected,
        threads,
        selected_thread_id: String::new(),
        messages: vec![],
        pipes,
        has_more_messages: false,
        oldest_message_ts: String::new(),
    })
}

/// GET /chat/{pipe_id}/t/{thread_id} — specific thread selected
pub async fn chat_thread(
    user: AuthUser,
    Path((pipe_id, thread_id)): Path<(Uuid, Uuid)>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    let pipe_id_str = pipe_id.to_string();
    let thread_id_str = thread_id.to_string();
    let pipes = fetch_pipes(&state.db, &user.user_id).await;
    let threads = fetch_threads(&state.db, &pipe_id_str, &user.user_id, &user.language).await;
    let selected = pipes.iter().find(|p| p.id == pipe_id_str).cloned();
    let (messages, has_more_messages, oldest_message_ts) =
        fetch_thread_messages(&state, &thread_id_str, &user.language, &user.user_id, None).await;

    render_template(ChatTemplate {
        active_nav: "chat",
        is_admin: user.is_admin,
        base_path,
        selected_pipe_id: pipe_id_str,
        selected_pipe: selected,
        threads,
        selected_thread_id: thread_id_str,
        messages,
        pipes,
        has_more_messages,
        oldest_message_ts: oldest_message_ts.unwrap_or_default(),
    })
}

/// POST /chat/{pipe_id}/t/new — create a new thread and redirect to it
pub async fn chat_new_thread(
    user: AuthUser,
    Path(pipe_id): Path<Uuid>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    let pipe_id_str = pipe_id.to_string();

    // Determine the agent for this pipe
    let default_agent_id = sqlx::query!(
        "SELECT default_agent_id FROM pipes WHERE id = ?",
        pipe_id_str
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .and_then(|r| r.default_agent_id)
    .unwrap_or_else(|| "default".to_string());
    let force_new_session = state
        .agents
        .load()
        .get(&default_agent_id)
        .map(|a| a.session.requires_thread_isolation())
        .unwrap_or(false);

    match thread_mgr::create_thread(
        &state.db,
        &pipe_id_str,
        &user.user_id,
        &default_agent_id,
        force_new_session,
        None,
    )
    .await
    {
        Ok(thread) => {
            let url = format!("{}/chat/{}/t/{}", base_path, pipe_id_str, thread.id);
            Redirect::to(&url).into_response()
        }
        Err(e) => {
            error!("Failed to create thread: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// POST /chat/{pipe_id}/t/{thread_id}/delete — delete a thread and its messages
pub async fn chat_delete_thread(
    user: AuthUser,
    Path((pipe_id, thread_id)): Path<(Uuid, Uuid)>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    let pipe_id_str = pipe_id.to_string();
    let thread_id_str = thread_id.to_string();
    let user_id = user.user_id.clone();
    let thread_user_id = user_id.clone();
    let pipe_user_id = user_id.clone();

    let thread_exists = sqlx::query!(
        r#"SELECT t.id
           FROM threads t
           JOIN pipes p ON p.id = t.pipe_id
           WHERE t.id = ? AND t.pipe_id = ? AND t.user_id = ? AND p.user_id = ? AND p.transport = 'web'
           LIMIT 1"#,
        thread_id_str,
        pipe_id_str,
        thread_user_id,
        pipe_user_id,
    )
    .fetch_optional(&state.db)
    .await;

    let Ok(Some(_)) = thread_exists else {
        return Redirect::to(&format!("{}/chat/{}", base_path, pipe_id_str)).into_response();
    };

    let mut tx = match state.db.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            error!("Failed to start transaction for thread delete: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let persisted_artifacts =
        match crate::agents::artifacts::list_thread_artifact_files(&mut tx, &thread_id_str).await {
            Ok(rows) => rows,
            Err(e) => {
                error!("Failed to load persisted artifacts for thread {thread_id_str}: {e}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };

    if let Err(e) = sqlx::query!(
        "DELETE FROM pending_tool_approvals WHERE thread_id = ?",
        thread_id_str
    )
    .execute(&mut *tx)
    .await
    {
        error!("Failed to delete pending approvals for thread {thread_id_str}: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if let Err(e) = sqlx::query!(
        "DELETE FROM thread_reply_guids WHERE thread_id = ?",
        thread_id_str
    )
    .execute(&mut *tx)
    .await
    {
        error!("Failed to delete reply guid mappings for thread {thread_id_str}: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if let Err(e) =
        crate::agents::artifacts::delete_thread_artifacts_rows(&mut tx, &thread_id_str).await
    {
        error!("Failed to delete persisted artifacts for thread {thread_id_str}: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if let Err(e) = sqlx::query!("DELETE FROM messages WHERE thread_id = ?", thread_id_str)
        .execute(&mut *tx)
        .await
    {
        error!("Failed to delete messages for thread {thread_id_str}: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if let Err(e) = sqlx::query!(
        "DELETE FROM threads WHERE id = ? AND pipe_id = ? AND user_id = ?",
        thread_id_str,
        pipe_id_str,
        user_id,
    )
    .execute(&mut *tx)
    .await
    {
        error!("Failed to delete thread {thread_id_str}: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if let Err(e) = tx.commit().await {
        error!("Failed to commit thread delete for {thread_id_str}: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let artifact_ids = persisted_artifacts
        .iter()
        .map(|a| a.artifact_id.clone())
        .collect::<Vec<_>>();
    crate::agents::artifacts::remove_ids(&state.artifacts, &artifact_ids).await;
    crate::agents::artifacts::delete_artifact_files(&persisted_artifacts).await;

    Redirect::to(&format!("{}/chat/{}", base_path, pipe_id_str)).into_response()
}

/// POST /chat/{pipe_id}/t/{thread_id}/send — returns HTMX fragment: user bubble + streaming assistant bubble
pub async fn chat_send(
    user: AuthUser,
    Path((pipe_id, thread_id)): Path<(Uuid, Uuid)>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    multipart: Multipart,
) -> Response {
    let (text, inbound_attachments) = match parse_chat_send_payload_multipart(multipart).await {
        Ok(payload) => payload,
        Err(status) => return status.into_response(),
    };
    if text.is_empty() && inbound_attachments.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let pipe_id_str = pipe_id.to_string();
    let thread_id_str = thread_id.to_string();

    // ── Slash command interception ───────────────────────────────────────────
    // If the message starts with '/', try to handle it as a command instead
    // of routing to the agent engine.
    if inbound_attachments.is_empty() {
        if let Some(cmd) = commands::parse_command(&text) {
            let ctx = commands::CommandContext {
                state: &state,
                user_id: &user.user_id,
                pipe_id: &pipe_id_str,
                language: &user.language,
            };
            let result = commands::dispatch(cmd, ctx).await;

            // Persist both the user command and the response as messages
            let _ = sqlx::query!(
                "INSERT INTO messages (id, pipe_id, session_id, thread_id, role, content) VALUES (?, ?, '', ?, 'user', ?)",
                pipe_id_str, pipe_id_str, thread_id_str, text,
            ).execute(&state.db).await;
            let reply_id = uuid::Uuid::new_v4().to_string();
            let _ = sqlx::query!(
                "INSERT INTO messages (id, pipe_id, session_id, thread_id, role, content) VALUES (?, ?, '', ?, 'assistant', ?)",
                reply_id, pipe_id_str, thread_id_str, result.text,
            ).execute(&state.db).await;

            let escaped_input = html_escape(&text);
            let escaped_reply = html_escape(&result.text);
            let html = format!(
                r#"<div class="flex justify-end">
  <div class="max-w-[72%] bg-gradient-to-br from-coral/20 to-amber/10 border border-coral/20 rounded-2xl rounded-br-md px-4 py-2.5">
    <p class="text-sm leading-relaxed whitespace-pre-wrap break-words text-txt-heading">{escaped_input}</p>
  </div>
</div>
<div class="flex justify-start">
  <div class="max-w-[72%] bg-surface-raised border border-edge rounded-2xl rounded-bl-md px-4 py-2.5">
    <div class="text-sm leading-relaxed break-words markdown-body md-source">{escaped_reply}</div>
  </div>
</div>"#,
                escaped_input = escaped_input,
                escaped_reply = escaped_reply,
            );
            return Html(html).into_response();
        }
    }

    // Create a Notify so the background task waits for the SSE client to connect
    let stream_id = Uuid::new_v4().to_string();
    let notify = Arc::new(Notify::new());
    {
        // Insert a placeholder sender; the SSE endpoint will replace it
        let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
        let mut streams = state.active_streams.lock().await;
        streams.insert(stream_id.clone(), tx);
    }

    // Store the notify so the SSE endpoint can signal readiness
    {
        let mut notifies = state.stream_notifies.lock().await;
        notifies.insert(stream_id.clone(), notify.clone());
    }

    let has_inbound_attachments = !inbound_attachments.is_empty();
    let image_preview_html = render_user_image_preview_html(&inbound_attachments);

    // Kick off LLM in background — use the logged-in user's identity
    tokio::spawn(process_message(
        state.clone(),
        pipe_id_str.clone(),
        thread_id_str.clone(),
        user.user_id.clone(),
        text.clone(),
        inbound_attachments,
        stream_id.clone(),
        notify,
    ));

    let bp = &base_path;
    let text_for_bubble = if text.trim().is_empty() && has_inbound_attachments {
        crate::i18n::t(&user.language, "chat.image_only_sent").to_string()
    } else {
        text.clone()
    };
    let html = format!(
        r#"<div class="flex justify-end">
  <div class="max-w-[72%] bg-gradient-to-br from-coral/20 to-amber/10 border border-coral/20 rounded-2xl rounded-br-md px-4 py-2.5">
    <p class="text-sm leading-relaxed whitespace-pre-wrap break-words text-txt-heading">{escaped_text}</p>
    {image_preview_html}
  </div>
</div>
<div class="flex justify-start" id="stream-wrap-{stream_id}" style="display:none">
  <div class="max-w-[72%] bg-surface-raised border border-edge rounded-2xl rounded-bl-md px-4 py-2.5">
    <div class="text-sm leading-relaxed break-words markdown-body streaming" id="stream-{stream_id}"
         hx-ext="sse"
         sse-connect="{base_path}/chat/{pipe_id}/stream/{stream_id}"
         sse-swap="token"
         hx-swap="beforeend">
    </div>
  </div>
</div>"#,
        escaped_text = html_escape(&text_for_bubble),
        image_preview_html = image_preview_html,
        base_path = bp,
        pipe_id = pipe_id_str,
        stream_id = stream_id,
    );

    Html(html).into_response()
}

/// GET /chat/{pipe_id}/stream/{stream_id} — SSE: stream LLM tokens to browser
pub async fn chat_stream(
    Path((_pipe_id, stream_id)): Path<(Uuid, String)>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Sse<impl futures::Stream<Item = Result<axum::response::sse::Event, Infallible>>> {
    let (bridge_tx, bridge_rx) = mpsc::channel::<LoopEvent>(64);
    {
        let mut streams = state.active_streams.lock().await;
        streams.insert(stream_id.clone(), bridge_tx);
    }

    // Signal that the SSE endpoint is ready to receive tokens
    {
        let mut notifies = state.stream_notifies.lock().await;
        if let Some(notify) = notifies.remove(&stream_id) {
            notify.notify_one();
        }
    }

    let bp = base_path;
    let sid = stream_id.clone();
    let confirmations = state.pending_confirmations.clone();
    let sse_stream = ReceiverStream::new(bridge_rx).map(move |event| {
        let data = match event {
            LoopEvent::Token(t) => {
                // Send raw token wrapped in a script that appends to the streaming element
                let escaped = html_escape(&t)
                    .replace('\\', "\\\\")
                    .replace('\'', "\\'")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r");
                format!(
                    r#"<script>
(function(){{
  var el = document.getElementById('stream-{sid}') || document.querySelector('[id^="cont-"].streaming');
  if(el) appendStreamToken(el, '{tok}');
  var msgs = document.getElementById('messages');
  if(msgs) msgs.scrollTop = msgs.scrollHeight;
}})();
</script>"#,
                    sid = sid,
                    tok = escaped,
                )
            }
            LoopEvent::CapabilityStatus(s) => {
                format!(
                    r#"<script>
(function(){{
  var msgs = document.getElementById('messages');
  var statusDiv = document.createElement('div');
  statusDiv.className = 'flex justify-start';
  statusDiv.innerHTML = '<div class="tool-status active"><svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M12 1v4m0 14v4m-8.66-13H7.34m9.32 0h4m-14.14 5.66l2.83-2.83m7.07-7.07l2.83-2.83M4.22 4.22l2.83 2.83m7.07 7.07l2.83 2.83"/></svg><span>{status}</span></div>';
  msgs.appendChild(statusDiv);
  msgs.scrollTop = msgs.scrollHeight;
}})();
</script>"#,
                    status = html_escape(&s),
                )
            }
            LoopEvent::ConfirmationRequired(req) => {
                let confirmation_id = Uuid::new_v4().to_string();
                let args_pretty = serde_json::to_string_pretty(&req.arguments)
                    .unwrap_or_else(|_| req.arguments.to_string());

                if let Ok(mut pending) = confirmations.try_lock() {
                    pending.insert(confirmation_id.clone(), req.reply);
                } else {
                    warn!("Failed to lock pending_confirmations; confirmation will be auto-denied");
                }

                build_confirmation_html(
                    &sid,
                    &confirmation_id,
                    &req.tool_display_name,
                    req.approval_message.as_deref(),
                    &args_pretty,
                    &bp,
                )
            }
            LoopEvent::Done => {
                format!(r#"<script>finalizeStream('{sid}');</script>"#, sid = sid)
            }
            LoopEvent::Error(e) => {
                let escaped = html_escape(&e).replace('\'', "\\'").replace('\n', "\\n");
                format!(
                    r#"<script>
(function(){{
  var el = document.getElementById('stream-{sid}') || document.querySelector('[id^="cont-"].streaming');
  if(el) {{
    el.classList.remove('streaming');
    el.innerHTML += '<div class="mt-2 px-3 py-2 bg-red-950/50 border border-red-800/50 rounded-lg text-red-300 text-xs">[Error: {err}]</div>';
  }}
}})();
</script>"#,
                    sid = sid,
                    err = escaped,
                )
            }
            LoopEvent::ApprovalQueued {
                tool_display_name,
                approval_message,
                approval_id,
            } => {
                format!(
                    r#"<script>
(function(){{
  var msgs = document.getElementById('messages');
  var statusDiv = document.createElement('div');
  statusDiv.className = 'flex justify-start';
  var label = t('chat.confirm.queued').replace('{{tool}}', '{display}').replace('{{id}}', '{id}');
  statusDiv.innerHTML = '<div class="tool-status active"><svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M12 1v4m0 14v4m-8.66-13H7.34m9.32 0h4m-14.14 5.66l2.83-2.83m7.07-7.07l2.83-2.83M4.22 4.22l2.83 2.83m7.07 7.07l2.83 2.83"/></svg><span>' + label + '</span></div>{message_html}';
  msgs.appendChild(statusDiv);
  msgs.scrollTop = msgs.scrollHeight;
}})();
</script>"#,
                    display = html_escape(&tool_display_name),
                    message_html = approval_message.as_deref().map(render_confirmation_message_html).unwrap_or_default(),
                    id = html_escape(&approval_id),
                )
            }
        };
        Ok::<_, Infallible>(axum::response::sse::Event::default().event("token").data(data))
    });

    Sse::new(sse_stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

/// POST /chat/confirm/{confirmation_id}?approved=true|false
///
/// Resolves a pending tool-call confirmation. Called by the HTMX buttons
/// rendered by the SSE stream when a `requires_confirmation` tool is invoked.
pub async fn chat_confirm(
    Path(confirmation_id): Path<String>,
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let approved = params.get("approved").map(|v| v == "true").unwrap_or(false);

    let sender = {
        let mut pending = state.pending_confirmations.lock().await;
        pending.remove(&confirmation_id)
    };

    match sender {
        Some(tx) => {
            let _ = tx.send(approved);
            if approved {
                Html(format!(
                    r#"<div class="flex justify-start"><div class="tool-status" style="color:#6ee7b7;border-color:rgba(16,185,129,0.3);background:rgba(6,95,70,0.15)"><svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"/></svg><span data-i18n="chat.confirm.approved">Approved</span></div></div>"#
                )).into_response()
            } else {
                Html(format!(
                    r#"<div class="flex justify-start"><div class="tool-status" style="color:#fca5a5;border-color:rgba(220,38,38,0.3);background:rgba(127,29,29,0.15)"><svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg><span data-i18n="chat.confirm.denied">Denied</span></div></div>"#
                )).into_response()
            }
        }
        None => {
            Html(r#"<div class="flex justify-start"><div class="tool-status"><span data-i18n="chat.confirm.expired">Confirmation expired or already handled.</span></div></div>"#.to_string())
                .into_response()
        }
    }
}

// ── Feedback (thumbs up/down) ─────────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct FeedbackForm {
    pub rating: i64,
}

/// HTMX: record thumbs up/down feedback on the latest turn in a thread.
pub async fn chat_feedback(
    user: AuthUser,
    Path((_pipe_id, thread_id)): Path<(String, String)>,
    State(state): State<AppState>,
    Form(form): Form<FeedbackForm>,
) -> Response {
    // Resolve the agent for this thread
    let agent_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT s.agent_id FROM sessions s \
         JOIN threads t ON t.session_id = s.id \
         WHERE t.id = ?",
    )
    .bind(&thread_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .flatten();

    let Some(agent_id) = agent_id else {
        return Html(
            r#"<span class="text-xs text-txt-muted" data-i18n="improvement.feedback.thanks">Thanks!</span>"#,
        )
        .into_response();
    };

    let _ = crate::agents::improvement::rate_turn(
        &state.db,
        &agent_id,
        &user.user_id,
        &thread_id,
        form.rating,
    )
    .await;

    Html(
        r#"<span class="text-xs text-txt-muted" data-i18n="improvement.feedback.thanks">Thanks!</span>"#,
    )
    .into_response()
}

// ── Pagination: load older messages (HTMX fragment) ─────────────────────────

/// GET /chat/{pipe_id}/t/{thread_id}/older?before=<timestamp>
/// Returns an HTML fragment of older messages + a new load-more trigger if more exist.
pub async fn chat_older_messages(
    user: AuthUser,
    Path((pipe_id, thread_id)): Path<(Uuid, Uuid)>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let pipe_id_str = pipe_id.to_string();
    let thread_id_str = thread_id.to_string();
    let before = params.get("before").map(|s| s.as_str());

    let Some(before_ts) = before else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let (messages, has_more, oldest_ts) =
        fetch_thread_messages(&state, &thread_id_str, &user.language, &user.user_id, Some(before_ts)).await;

    // Build HTML fragment wrapped in a new load-older-wrap so subsequent
    // pages can target it again.  The outerHTML swap replaces the old wrapper.
    let mut html = String::from(r#"<div id="load-older-wrap">"#);

    if has_more {
        if let Some(ref ts) = oldest_ts {
            let encoded_ts = ts.replace('&', "&amp;").replace('"', "&quot;");
            html.push_str(&format!(
                r##"<div id="load-older" hx-get="{base_path}/chat/{pipe_id}/t/{thread_id}/older?before={ts}" hx-trigger="revealed" hx-target="#load-older-wrap" hx-swap="outerHTML" class="flex justify-center py-2"><button class="text-xs text-txt-muted hover:text-coral transition-colors cursor-pointer" data-i18n="chat.load_older">Load older messages</button></div>"##,
                base_path = base_path,
                pipe_id = pipe_id_str,
                thread_id = thread_id_str,
                ts = encoded_ts,
            ));
        }
    }

    for msg in &messages {
        html.push_str(&render_message_html(msg));
    }

    html.push_str("</div>");
    Html(html).into_response()
}

fn render_message_html(msg: &MessageView) -> String {
    if msg.role == "user" {
        let images_html = if msg.image_urls.is_empty() {
            String::new()
        } else {
            let imgs: Vec<String> = msg.image_urls.iter().map(|url| {
                format!(r#"<img src="{}" alt="image" class="w-[110px] h-[82px] object-cover rounded-lg border border-coral/30 bg-surface-input" />"#, html_escape(url))
            }).collect();
            format!(r#"<div class="mt-2 flex flex-wrap gap-2">{}</div>"#, imgs.join(""))
        };
        let content_html = if msg.content.is_empty() {
            String::new()
        } else {
            format!(r#"<p class="text-sm leading-relaxed whitespace-pre-wrap break-words text-txt-heading">{}</p>"#, html_escape(&msg.content))
        };
        format!(
            r#"<div class="flex justify-end"><div class="max-w-[72%] bg-gradient-to-br from-coral/20 to-amber/10 border border-coral/20 rounded-2xl rounded-br-md px-4 py-2.5">{content}{images}<p class="text-[10px] text-txt-muted mt-1.5">{ts}</p></div></div>"#,
            content = content_html,
            images = images_html,
            ts = html_escape(&msg.created_at),
        )
    } else if msg.role == "tool" {
        format!(
            r#"<div class="flex justify-start"><details class="tool-result max-w-[72%]"><summary class="tool-result-summary"><svg class="w-3.5 h-3.5 flex-shrink-0" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="4 17 10 11 4 5"/><line x1="12" y1="19" x2="20" y2="19"/></svg><span>Tool result</span><svg class="tool-result-chevron w-3 h-3 flex-shrink-0 ml-auto" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="6 9 12 15 18 9"/></svg></summary><div class="tool-result-body"><pre class="tool-result-pre"><code>{content}</code></pre></div></details></div>"#,
            content = html_escape(&msg.content),
        )
    } else if !msg.tool_calls.is_empty() {
        let calls_html: Vec<String> = msg.tool_calls.iter().map(|tc| {
            let result_part = if tc.result.is_empty() {
                String::new()
            } else {
                format!(
                    r#"<svg class="tool-result-chevron w-3 h-3 flex-shrink-0 ml-auto" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="6 9 12 15 18 9"/></svg></summary><div class="tool-result-body"><pre class="tool-result-pre"><code>{}</code></pre></div>"#,
                    html_escape(&tc.result)
                )
            };
            let chevron_or_close = if tc.result.is_empty() {
                "</summary>".to_string()
            } else {
                result_part
            };
            format!(
                r#"<details class="tool-call-item"><summary class="tool-call-summary"><svg class="w-3.5 h-3.5 flex-shrink-0" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M12 1v4m0 14v4m-8.66-13H7.34m9.32 0h4m-14.14 5.66l2.83-2.83m7.07-7.07l2.83-2.83M4.22 4.22l2.83 2.83m7.07 7.07l2.83 2.83"/></svg><span>{name}</span>{rest}</details>"#,
                name = html_escape(&tc.name),
                rest = chevron_or_close,
            )
        }).collect();
        format!(
            r#"<div class="flex justify-start"><div class="tool-calls-group">{}</div></div>"#,
            calls_html.join("")
        )
    } else {
        // Assistant message
        format!(
            r#"<div class="flex justify-start"><div class="max-w-[72%] bg-surface-raised border border-edge rounded-2xl rounded-bl-md px-4 py-2.5"><div class="text-sm leading-relaxed break-words markdown-body md-source">{content}</div><p class="text-[10px] text-txt-muted mt-1.5">{ts}</p></div></div>"#,
            content = html_escape(&msg.content),
            ts = html_escape(&msg.created_at),
        )
    }
}

// ── Background processing ─────────────────────────────────────────────────────

async fn process_message(
    state: AppState,
    pipe_id: String,
    thread_id: String,
    user_id: String,
    text: String,
    inbound_attachments: Vec<InboundAttachmentInput>,
    stream_id: String,
    notify: Arc<Notify>,
) {
    let process_start = std::time::Instant::now();

    // Wait for the SSE endpoint to connect and register its sender (with timeout)
    tokio::select! {
        _ = notify.notified() => {}
        _ = tokio::time::sleep(Duration::from_secs(5)) => {
            error!(stream_id = %stream_id, "SSE client did not connect within 5s");
        }
    }

    let sse_wait_ms = process_start.elapsed().as_millis();
    if sse_wait_ms > 100 {
        warn!(stream_id = %stream_id, sse_wait_ms, "Slow SSE client connection");
    }

    // Fetch pipe's default agent for routing
    let default_agent_id = sqlx::query!("SELECT default_agent_id FROM pipes WHERE id = ?", pipe_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.default_agent_id);

    // Resolve @mention routing (same as webhook/iMessage adapters)
    let agents_snapshot = state.agents.load();
    let user_agents =
        crate::agents::access::visible_agents(&state.db, &user_id, &agents_snapshot).await;
    let (agent_id, effective_text) =
        agent_router::route(&text, default_agent_id.as_deref(), &user_agents);

    // Retrieve the SSE sender registered by chat_stream
    let tx = match state.active_streams.lock().await.get(&stream_id).cloned() {
        Some(t) => t,
        None => {
            // SSE client may not have connected yet — create a throwaway sender
            let (t, _) = mpsc::channel::<LoopEvent>(1);
            t
        }
    };

    let params = TurnParams {
        pipe_id,
        user_id: user_id.clone(),
        agent_id: Some(agent_id),
        message: effective_text,
        thread_id: Some(thread_id),
        chain_depth: 0,
        channel_kind: ChannelKind::Interactive,
        skip_user_persist: false,
        enable_streaming: true,
        inbound_attachments,
        delegation_trace: Vec::new(),
        location_context: None,
    };

    tracing::debug!(
        setup_ms = process_start.elapsed().as_millis(),
        "process_message setup complete, starting turn"
    );

    match run_turn(&state, params, tx.clone()).await {
        Ok(output) => {
            if !output.attachments.is_empty() {
                // Web chat uses a relative URL (just the base path) so the
                // browser resolves it against its own origin — avoids
                // internal Docker hostnames leaking into download links.
                let web_base = state.config.base_path().to_string();
                let attachments = crate::agents::artifacts::to_outbound_attachments(
                    &state.artifacts,
                    &output.attachments,
                    &user_id,
                    &web_base,
                    &state.config.encryption_key,
                    false,
                )
                .await;
                if !attachments.is_empty() {
                    let lang = crate::i18n::user_language(&state.db, &user_id).await;
                    let mut lines = Vec::new();
                    lines.push(String::new());
                    lines.push(crate::i18n::t(&lang, "artifact.generated_files").to_string());
                    for a in attachments {
                        if let Some(url) = a.download_url {
                            lines.push(format!("- [{}]({})", a.filename, url));
                        } else {
                            lines.push(format!("- {}", a.filename));
                        }
                    }
                    let _ = tx.send(LoopEvent::Token(lines.join("\n"))).await;
                }
            }
        }
        Err(e) => {
            error!("Agent turn failed: {e}");
            let lang = crate::i18n::user_language(&state.db, &user_id).await;
            let error_text = crate::i18n::t(&lang, "error.agent_turn_failed").to_string();
            let _ = tx.send(LoopEvent::Error(error_text)).await;
            let _ = tx.send(LoopEvent::Done).await;
        }
    }

    tracing::debug!(
        total_ms = process_start.elapsed().as_millis(),
        "process_message complete"
    );

    state.active_streams.lock().await.remove(&stream_id);
}

async fn parse_chat_send_payload_multipart(
    mut multipart: Multipart,
) -> Result<(String, Vec<InboundAttachmentInput>), StatusCode> {
    let mut text = String::new();
    let mut images: Vec<InboundAttachmentInput> = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?
    {
        let name = field.name().unwrap_or_default().to_string();
        if name == "text" {
            text = field.text().await.map_err(|_| StatusCode::BAD_REQUEST)?;
            continue;
        }
        let is_image_field = name == "image" || name == "image[]";
        if !is_image_field {
            let _ = field.bytes().await;
            continue;
        }

        let declared_mime = field
            .content_type()
            .map(|v| v.to_string())
            .unwrap_or_default();
        let filename = field.file_name().map(|v| v.to_string());
        let mime_type = resolve_image_mime_type(filename.as_deref(), &declared_mime);
        let Some(mime_type) = mime_type else {
            let _ = field.bytes().await;
            continue;
        };
        let filename = filename.unwrap_or_else(|| {
            let ext = mime_type.split('/').nth(1).unwrap_or("img");
            format!("upload-{}.{}", Uuid::new_v4(), ext)
        });
        let data = field.bytes().await.map_err(|_| StatusCode::BAD_REQUEST)?;
        if data.is_empty() {
            continue;
        }
        if data.len() > MAX_WEB_CHAT_IMAGE_BYTES {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }

        images.push(InboundAttachmentInput {
            filename,
            mime_type,
            data: data.to_vec(),
        });
    }

    Ok((text.trim().to_string(), images))
}

fn resolve_image_mime_type(filename: Option<&str>, declared_mime: &str) -> Option<String> {
    if declared_mime.starts_with("image/") {
        return Some(declared_mime.to_string());
    }

    let ext = filename.and_then(|f| f.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()))?;
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        "heic" => "image/heic",
        "heif" => "image/heif",
        _ => return None,
    };
    Some(mime.to_string())
}

async fn user_message_for_display(
    state: &AppState,
    user_id: &str,
    raw: &str,
    image_only_fallback: &str,
) -> (String, Vec<String>) {
    let (text, artifact_ids) = split_user_artifact_block(raw);
    if artifact_ids.is_empty() {
        return (raw.to_string(), Vec::new());
    }

    let base = state.config.base_path().to_string();
    let mut urls = Vec::new();
    for id in artifact_ids {
        let mut exists = crate::agents::artifacts::get_for_user(&state.artifacts, &id, user_id)
            .await
            .is_some();
        if !exists {
            exists =
                crate::agents::artifacts::persisted_exists_for_user(&state.db, &id, user_id).await;
        }
        if !exists {
            continue;
        }
        if let Some(url) = crate::agents::artifacts::generate_download_url(
            &base,
            &state.config.encryption_key,
            &id,
            user_id,
        ) {
            urls.push(url);
        }
    }
    let content = if text.trim().is_empty() {
        image_only_fallback.to_string()
    } else {
        text
    };
    (content, urls)
}

fn split_user_artifact_block(raw: &str) -> (String, Vec<String>) {
    let marker = "\n\nAttached image artifacts:\n";
    let Some((prefix, rest)) = raw.split_once(marker) else {
        return (raw.to_string(), Vec::new());
    };

    let artifact_ids = rest
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if !trimmed.starts_with("- artifact_id: ") {
                return None;
            }
            let value = trimmed.trim_start_matches("- artifact_id: ");
            let id = value.split(" | ").next().unwrap_or("").trim();
            if id.is_empty() {
                None
            } else {
                Some(id.to_string())
            }
        })
        .collect::<Vec<_>>();

    (prefix.to_string(), artifact_ids)
}

fn render_user_image_preview_html(images: &[InboundAttachmentInput]) -> String {
    if images.is_empty() {
        return String::new();
    }

    let mut out = String::from(r#"<div class="mt-2 flex flex-wrap gap-2">"#);
    for img in images {
        if !img.mime_type.starts_with("image/") {
            continue;
        }
        let data_url = format!(
            "data:{};base64,{}",
            img.mime_type,
            base64::engine::general_purpose::STANDARD.encode(&img.data)
        );
        out.push_str(&format!(
            r#"<img src="{src}" alt="{alt}" class="w-[110px] h-[82px] object-cover rounded-lg border border-coral/30 bg-surface-input" />"#,
            src = html_escape(&data_url),
            alt = html_escape(&img.filename),
        ));
    }
    out.push_str("</div>");
    out
}

fn build_confirmation_html(
    sid: &str,
    cid: &str,
    tool: &str,
    approval_message: Option<&str>,
    args: &str,
    base_path: &str,
) -> String {
    let bp = base_path;
    let tool_esc = html_escape(tool);
    let approval_message_esc = approval_message.map(html_escape);
    let args_esc = html_escape(args).replace('\n', "\\n").replace('\r', "");
    let mut s = String::new();
    s.push_str("<script>\n(function(){\n");
    s.push_str("  var el = document.getElementById('stream-");
    s.push_str(sid);
    s.push_str("') || document.querySelector('[id^=\"cont-\"].streaming');\n");
    s.push_str("  if(el) el.classList.remove('streaming');\n");
    s.push_str("  var msgs = document.getElementById('messages');\n");
    s.push_str("  var cardWrap = document.createElement('div');\n");
    s.push_str("  cardWrap.className = 'flex justify-start';\n");
    s.push_str("  cardWrap.id = 'confirm-");
    s.push_str(cid);
    s.push_str("';\n");
    // Build innerHTML using JS string concatenation; use t() for i18n
    s.push_str("  var h = '';\n");
    s.push_str("  h += '<div class=\"confirm-card\">';\n");
    s.push_str("  h += '<div class=\"flex items-center gap-2 text-sm font-medium text-amber-300 mb-2\">';\n");
    s.push_str("  h += '<svg class=\"w-4 h-4\" viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><path d=\"M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z\"/><line x1=\"12\" y1=\"9\" x2=\"12\" y2=\"13\"/><line x1=\"12\" y1=\"17\" x2=\"12.01\" y2=\"17\"/></svg>';\n");
    s.push_str("  h += ' ' + t('chat.confirm.title') + '</div>';\n");
    s.push_str("  h += '<p class=\"text-xs text-txt-muted mb-1\">' + t('chat.confirm.tool') + ' <code class=\"bg-surface-input px-1.5 py-0.5 rounded text-brand-300 text-xs\">");
    s.push_str(&tool_esc);
    s.push_str("</code></p>';\n");
    if let Some(message) = approval_message_esc {
        s.push_str("  h += '<p class=\"text-sm text-txt-heading mb-3 whitespace-pre-wrap\">");
        s.push_str(&message.replace('\'', "\\'"));
        s.push_str("</p>';\n");
    }
    s.push_str("  h += '<pre>");
    s.push_str(&args_esc);
    s.push_str("</pre>';\n");
    s.push_str("  h += '<div class=\"confirm-actions\">';\n");
    s.push_str(&format!("  h += '<button hx-post=\"{}/chat/confirm/", bp));
    s.push_str(cid);
    s.push_str("?approved=true\" hx-target=\"#confirm-");
    s.push_str(cid);
    s.push_str("\" hx-swap=\"outerHTML\" class=\"btn-approve\">' + t('chat.confirm.approve') + '</button>';\n");
    s.push_str(&format!("  h += '<button hx-post=\"{}/chat/confirm/", bp));
    s.push_str(cid);
    s.push_str("?approved=false\" hx-target=\"#confirm-");
    s.push_str(cid);
    s.push_str(
        "\" hx-swap=\"outerHTML\" class=\"btn-deny\">' + t('chat.confirm.deny') + '</button>';\n",
    );
    s.push_str("  h += '</div></div>';\n");
    s.push_str("  cardWrap.innerHTML = h;\n");
    s.push_str("  msgs.appendChild(cardWrap);\n");
    s.push_str("  htmx.process(cardWrap);\n");
    s.push_str("  msgs.scrollTop = msgs.scrollHeight;\n");
    s.push_str("})();\n</script>");
    s
}

fn render_confirmation_message_html(message: &str) -> String {
    format!(
        "<div class=\\\"mt-2 max-w-[72%] text-sm text-txt-heading whitespace-pre-wrap\\\">{}</div>",
        html_escape(message)
    )
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
