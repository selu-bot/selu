use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response, Sse},
    Form,
};
use futures::StreamExt;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, warn};
use uuid::Uuid;

use crate::agents::engine::{run_turn, TurnParams};
use crate::agents::router as agent_router;
use crate::llm::tool_loop::LoopEvent;
use crate::state::AppState;
use crate::web::auth::AuthUser;

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PipeView {
    pub id: String,
    pub name: String,
    pub transport: String,
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
    pipes: Vec<PipeView>,
    selected_pipe_id: String,
    selected_pipe: Option<PipeView>,
    messages: Vec<MessageView>,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn fetch_pipes(db: &sqlx::SqlitePool) -> Vec<PipeView> {
    sqlx::query!("SELECT id, name, transport FROM pipes WHERE active = 1 ORDER BY created_at")
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| PipeView {
            id: r.id.unwrap_or_default(),
            name: r.name,
            transport: r.transport,
        })
        .collect()
}

async fn fetch_messages(db: &sqlx::SqlitePool, pipe_id: &str) -> Vec<MessageView> {
    sqlx::query!(
        "SELECT role, content, created_at FROM messages WHERE pipe_id = ? ORDER BY created_at LIMIT 100",
        pipe_id
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

pub async fn chat_index(_user: AuthUser, State(state): State<AppState>) -> Response {
    let pipes = fetch_pipes(&state.db).await;
    render_template(ChatTemplate {
        active_nav: "chat",
        selected_pipe_id: String::new(),
        selected_pipe: None,
        messages: vec![],
        pipes,
    })
}

pub async fn chat_pipe(
    _user: AuthUser,
    Path(pipe_id): Path<Uuid>,
    State(state): State<AppState>,
) -> Response {
    let pipe_id_str = pipe_id.to_string();
    let pipes = fetch_pipes(&state.db).await;
    let messages = fetch_messages(&state.db, &pipe_id_str).await;
    let selected = pipes.iter().find(|p| p.id == pipe_id_str).cloned();

    render_template(ChatTemplate {
        active_nav: "chat",
        selected_pipe_id: pipe_id_str,
        selected_pipe: selected,
        messages,
        pipes,
    })
}

/// POST /chat/{pipe_id}/send — returns HTMX fragment: user bubble + streaming assistant bubble
pub async fn chat_send(
    _user: AuthUser,
    Path(pipe_id): Path<Uuid>,
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let text = match form.get("text").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(t) => t,
        None => return StatusCode::BAD_REQUEST.into_response(),
    };

    let pipe_id_str = pipe_id.to_string();

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

    // Kick off LLM in background
    tokio::spawn(process_message(state.clone(), pipe_id_str.clone(), text.clone(), stream_id.clone(), notify));

    let html = format!(
        r#"<div class="flex justify-end">
  <div class="max-w-[72%] bg-brand-600/20 border border-brand-500/20 rounded-2xl rounded-br-md px-4 py-2.5">
    <p class="text-sm leading-relaxed whitespace-pre-wrap break-words">{escaped_text}</p>
  </div>
</div>
<div class="flex justify-start">
  <div class="max-w-[72%] bg-slate-800 border border-slate-700 rounded-2xl rounded-bl-md px-4 py-2.5">
    <div class="text-sm leading-relaxed break-words markdown-body streaming" id="stream-{stream_id}"
         hx-ext="sse"
         sse-connect="/chat/{pipe_id}/stream/{stream_id}"
         sse-swap="token"
         hx-swap="beforeend">
    </div>
  </div>
</div>"#,
        escaped_text = html_escape(&text),
        pipe_id = pipe_id_str,
        stream_id = stream_id,
    );

    Html(html).into_response()
}

/// GET /chat/{pipe_id}/stream/{stream_id} — SSE: stream LLM tokens to browser
pub async fn chat_stream(
    Path((_pipe_id, stream_id)): Path<(Uuid, String)>,
    State(state): State<AppState>,
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

                build_confirmation_html(&sid, &confirmation_id, &tool_display, &args_pretty)
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
                    r#"<div class="flex justify-start"><div class="tool-status" style="color:#6ee7b7;border-color:rgba(16,185,129,0.3);background:rgba(6,95,70,0.15)"><svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"/></svg><span>Approved</span></div></div>"#
                )).into_response()
            } else {
                Html(format!(
                    r#"<div class="flex justify-start"><div class="tool-status" style="color:#fca5a5;border-color:rgba(220,38,38,0.3);background:rgba(127,29,29,0.15)"><svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg><span>Denied</span></div></div>"#
                )).into_response()
            }
        }
        None => {
            Html(r#"<div class="flex justify-start"><div class="tool-status"><span>Confirmation expired or already handled.</span></div></div>"#.to_string())
                .into_response()
        }
    }
}

// ── Background processing ─────────────────────────────────────────────────────

async fn process_message(
    state: AppState,
    pipe_id: String,
    text: String,
    stream_id: String,
    notify: Arc<Notify>,
) {
    // Wait for the SSE endpoint to connect and register its sender (with timeout)
    tokio::select! {
        _ = notify.notified() => {}
        _ = tokio::time::sleep(Duration::from_secs(5)) => {
            error!(stream_id = %stream_id, "SSE client did not connect within 5s");
        }
    }

    let pipe = sqlx::query!(
        "SELECT user_id, default_agent_id FROM pipes WHERE id = ?", pipe_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let (user_id, default_agent_id) = match pipe {
        Some(p) => (p.user_id, p.default_agent_id),
        None => { error!(pipe_id = %pipe_id, "Pipe not found"); return; }
    };

    // Route to determine agent_id (for display purposes; run_turn also routes internally)
    let agents_snapshot = state.agents.read().await.clone();
    let (agent_id, effective_text) = agent_router::route(
        &text,
        default_agent_id.as_deref(),
        &agents_snapshot,
    );

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
        thread_id: None, // Web chat doesn't use threads (yet)
        chain_depth: 0,
    };

    if let Err(e) = run_turn(&state, params, tx).await {
        error!("Agent turn failed: {e}");
    }

    state.active_streams.lock().await.remove(&stream_id);
}

fn build_confirmation_html(sid: &str, cid: &str, tool: &str, args: &str) -> String {
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
    // Build innerHTML using JS string concatenation to avoid Rust raw string issues
    s.push_str("  var h = '';\n");
    s.push_str("  h += '<div class=\"confirm-card\">';\n");
    s.push_str("  h += '<div class=\"flex items-center gap-2 text-sm font-medium text-amber-300 mb-2\">';\n");
    s.push_str("  h += '<svg class=\"w-4 h-4\" viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><path d=\"M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z\"/><line x1=\"12\" y1=\"9\" x2=\"12\" y2=\"13\"/><line x1=\"12\" y1=\"17\" x2=\"12.01\" y2=\"17\"/></svg>';\n");
    s.push_str("  h += ' Confirmation required</div>';\n");
    s.push_str("  h += '<p class=\"text-xs text-slate-300 mb-1\">Tool: <code class=\"bg-slate-900 px-1.5 py-0.5 rounded text-brand-300 text-xs\">");
    s.push_str(&tool_esc);
    s.push_str("</code></p>';\n");
    s.push_str("  h += '<pre>");
    s.push_str(&args_esc);
    s.push_str("</pre>';\n");
    s.push_str("  h += '<div class=\"confirm-actions\">';\n");
    s.push_str("  h += '<button hx-post=\"/chat/confirm/");
    s.push_str(cid);
    s.push_str("?approved=true\" hx-target=\"#confirm-");
    s.push_str(cid);
    s.push_str("\" hx-swap=\"outerHTML\" class=\"btn-approve\">Approve</button>';\n");
    s.push_str("  h += '<button hx-post=\"/chat/confirm/");
    s.push_str(cid);
    s.push_str("?approved=false\" hx-target=\"#confirm-");
    s.push_str(cid);
    s.push_str("\" hx-swap=\"outerHTML\" class=\"btn-deny\">Deny</button>';\n");
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
