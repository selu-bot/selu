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
        r#"<div class="message user">{escaped_text}</div>
<div class="message assistant streaming" id="stream-{stream_id}"
     hx-ext="sse"
     sse-connect="/chat/{pipe_id}/stream/{stream_id}"
     sse-swap="token"
     hx-swap="beforeend"></div>"#,
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
            LoopEvent::Token(t) => format!(r#"<span>{}</span>"#, html_escape(&t)),
            LoopEvent::CapabilityStatus(s) => {
                format!(r#"</div><div class="message status">{}</div><div class="message assistant streaming" id="cont-{}">"#,
                    html_escape(&s), sid)
            }
            LoopEvent::ConfirmationRequired(req) => {
                // Stash the oneshot sender so the confirm endpoint can resolve it
                let confirmation_id = Uuid::new_v4().to_string();
                let tool_display = req.tool_name.split("__").last().unwrap_or(&req.tool_name).to_string();
                let args_pretty = serde_json::to_string_pretty(&req.arguments)
                    .unwrap_or_else(|_| req.arguments.to_string());

                // Store sender — must use try_lock since we're in a sync map closure.
                // This is safe because the mutex is uncontended (confirmations are rare).
                if let Ok(mut pending) = confirmations.try_lock() {
                    pending.insert(confirmation_id.clone(), req.reply);
                } else {
                    warn!("Failed to lock pending_confirmations; confirmation will be auto-denied");
                    // The oneshot sender is dropped here, which resolves to Err on the
                    // receiver side, treated as denial.
                }

                format!(
                    "</div><div class=\"message confirmation\" id=\"confirm-{cid}\">\
                     <p><strong>Confirmation required:</strong> <code>{tool}</code></p>\
                     <pre>{args}</pre>\
                     <div class=\"confirm-actions\">\
                       <button hx-post=\"/chat/confirm/{cid}?approved=true\" \
                               hx-target=\"#confirm-{cid}\" \
                               hx-swap=\"outerHTML\" \
                               class=\"btn btn-approve\">Approve</button>\
                       <button hx-post=\"/chat/confirm/{cid}?approved=false\" \
                               hx-target=\"#confirm-{cid}\" \
                               hx-swap=\"outerHTML\" \
                               class=\"btn btn-deny\">Deny</button>\
                     </div></div>\
                     <div class=\"message assistant streaming\" id=\"cont-{cid}\">",
                    cid = confirmation_id,
                    tool = html_escape(&tool_display),
                    args = html_escape(&args_pretty),
                )
            }
            LoopEvent::Done => {
                format!(r#"<script>document.getElementById('stream-{}').classList.remove('streaming')</script>"#, sid)
            }
            LoopEvent::Error(e) => {
                format!(r#"<span style="color:#fc8181">[Error: {}]</span>"#, html_escape(&e))
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
            let status = if approved { "Approved" } else { "Denied" };
            let class = if approved { "status approved" } else { "status denied" };
            Html(format!(
                r#"<div class="message {class}">Action {status}.</div>"#,
                class = class,
                status = status,
            )).into_response()
        }
        None => {
            Html(r#"<div class="message status">Confirmation expired or already handled.</div>"#.to_string())
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

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
}
