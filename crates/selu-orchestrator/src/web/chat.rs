use askama::Template;
use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response, Sse},
};
use futures::StreamExt;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, warn};
use uuid::Uuid;

use crate::agents::engine::{ChannelKind, TurnParams, run_turn};
use crate::agents::router as agent_router;
use crate::agents::thread as thread_mgr;
use crate::commands;
use crate::llm::tool_loop::LoopEvent;
use crate::state::AppState;
use crate::web::BasePath;
use crate::web::auth::AuthUser;

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
}

#[derive(Debug, Clone)]
pub struct MessageView {
    pub role: String,
    pub content: String,
    pub created_at: String,
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

async fn fetch_threads(db: &sqlx::SqlitePool, pipe_id: &str, user_id: &str) -> Vec<ThreadView> {
    sqlx::query!(
        r#"SELECT t.id, t.pipe_id, t.title, t.status, t.created_at,
                  (SELECT content FROM messages WHERE thread_id = t.id ORDER BY created_at ASC LIMIT 1) as first_msg
           FROM threads t
           WHERE t.pipe_id = ? AND t.user_id = ?
           ORDER BY t.created_at DESC"#,
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
        ThreadView {
            id: r.id.unwrap_or_default(),
            title,
            status: r.status,
        }
    })
    .collect()
}

async fn fetch_thread_messages(db: &sqlx::SqlitePool, thread_id: &str) -> Vec<MessageView> {
    sqlx::query!(
        "SELECT role, content, created_at FROM messages WHERE thread_id = ? ORDER BY created_at LIMIT 100",
        thread_id
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| MessageView {
        role: r.role,
        content: r.content,
        created_at: r.created_at,
    })
    .collect()
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
    let threads = fetch_threads(&state.db, &pipe_id_str, &user.user_id).await;
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
    let threads = fetch_threads(&state.db, &pipe_id_str, &user.user_id).await;
    let selected = pipes.iter().find(|p| p.id == pipe_id_str).cloned();
    let messages = fetch_thread_messages(&state.db, &thread_id_str).await;

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

/// POST /chat/{pipe_id}/t/{thread_id}/send — returns HTMX fragment: user bubble + streaming assistant bubble
pub async fn chat_send(
    user: AuthUser,
    Path((pipe_id, thread_id)): Path<(Uuid, Uuid)>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let text = match form
        .get("text")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(t) => t,
        None => return StatusCode::BAD_REQUEST.into_response(),
    };

    let pipe_id_str = pipe_id.to_string();
    let thread_id_str = thread_id.to_string();

    // ── Slash command interception ───────────────────────────────────────────
    // If the message starts with '/', try to handle it as a command instead
    // of routing to the agent engine.
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

    // Kick off LLM in background — use the logged-in user's identity
    tokio::spawn(process_message(
        state.clone(),
        pipe_id_str.clone(),
        thread_id_str.clone(),
        user.user_id.clone(),
        text.clone(),
        stream_id.clone(),
        notify,
    ));

    let bp = &base_path;
    let html = format!(
        r#"<div class="flex justify-end">
  <div class="max-w-[72%] bg-gradient-to-br from-coral/20 to-amber/10 border border-coral/20 rounded-2xl rounded-br-md px-4 py-2.5">
    <p class="text-sm leading-relaxed whitespace-pre-wrap break-words text-txt-heading">{escaped_text}</p>
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
        escaped_text = html_escape(&text),
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
                let tool_display = req.tool_name.split("__").last().unwrap_or(&req.tool_name).to_string();
                let args_pretty = serde_json::to_string_pretty(&req.arguments)
                    .unwrap_or_else(|_| req.arguments.to_string());

                if let Ok(mut pending) = confirmations.try_lock() {
                    pending.insert(confirmation_id.clone(), req.reply);
                } else {
                    warn!("Failed to lock pending_confirmations; confirmation will be auto-denied");
                }

                build_confirmation_html(&sid, &confirmation_id, &tool_display, &args_pretty, &bp)
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
            LoopEvent::ApprovalQueued { tool_name, approval_id } => {
                let display = tool_name.split("__").last().unwrap_or(&tool_name);
                format!(
                    r#"<script>
(function(){{
  var msgs = document.getElementById('messages');
  var statusDiv = document.createElement('div');
  statusDiv.className = 'flex justify-start';
  var label = t('chat.confirm.queued').replace('{{tool}}', '{display}').replace('{{id}}', '{id}');
  statusDiv.innerHTML = '<div class="tool-status active"><svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M12 1v4m0 14v4m-8.66-13H7.34m9.32 0h4m-14.14 5.66l2.83-2.83m7.07-7.07l2.83-2.83M4.22 4.22l2.83 2.83m7.07 7.07l2.83 2.83"/></svg><span>' + label + '</span></div>';
  msgs.appendChild(statusDiv);
  msgs.scrollTop = msgs.scrollHeight;
}})();
</script>"#,
                    display = html_escape(display),
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

// ── Background processing ─────────────────────────────────────────────────────

async fn process_message(
    state: AppState,
    pipe_id: String,
    thread_id: String,
    user_id: String,
    text: String,
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
    let (agent_id, effective_text) =
        agent_router::route(&text, default_agent_id.as_deref(), &agents_snapshot);

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
        user_id,
        agent_id: Some(agent_id),
        message: effective_text,
        thread_id: Some(thread_id),
        chain_depth: 0,
        channel_kind: ChannelKind::Interactive,
        skip_user_persist: false,
    };

    tracing::debug!(
        setup_ms = process_start.elapsed().as_millis(),
        "process_message setup complete, starting turn"
    );

    if let Err(e) = run_turn(&state, params, tx).await {
        error!("Agent turn failed: {e}");
    }

    tracing::debug!(
        total_ms = process_start.elapsed().as_millis(),
        "process_message complete"
    );

    state.active_streams.lock().await.remove(&stream_id);
}

fn build_confirmation_html(
    sid: &str,
    cid: &str,
    tool: &str,
    args: &str,
    base_path: &str,
) -> String {
    let bp = base_path;
    let tool_esc = html_escape(tool);
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

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
