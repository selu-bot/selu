use axum::{
    Json,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, Sse},
    routing::{delete, get, post},
    Router,
};
use chrono::{Duration, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio::sync::{Notify, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::agents::engine::{ChannelKind, InboundAttachmentInput, TurnParams, run_turn};
use crate::agents::router as agent_router;
use crate::agents::thread as thread_mgr;
use crate::llm::tool_loop::LoopEvent;
use crate::state::AppState;

const SESSION_TTL_DAYS: i64 = 30;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/mobile/login", post(mobile_login))
        .route("/api/mobile/redeem", post(redeem_setup_token))
        .route("/api/mobile/pipes", get(list_pipes))
        .route("/api/mobile/pipes/{pipe_id}/threads", get(list_threads).post(create_thread))
        .route("/api/mobile/pipes/{pipe_id}/threads/{thread_id}", delete(delete_thread))
        .route("/api/mobile/pipes/{pipe_id}/threads/{thread_id}/messages", get(list_messages))
        .route(
            "/api/mobile/pipes/{pipe_id}/threads/{thread_id}/send",
            post(send_message).layer(DefaultBodyLimit::max(6 * 1024 * 1024)),
        )
        .route("/api/mobile/pipes/{pipe_id}/stream/{stream_id}", get(stream_response))
        .route("/api/mobile/approvals/{confirmation_id}", post(resolve_approval))
        .route("/api/mobile/artifacts/{artifact_id}", get(get_artifact))
}

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    session_id: String,
    user_id: String,
    username: String,
    display_name: String,
    is_admin: bool,
    expires_at: String,
}

#[derive(Serialize)]
struct PipeResponse {
    id: String,
    user_id: String,
    name: String,
    transport: String,
    outbound_url: String,
    default_agent_id: Option<String>,
    active: bool,
    created_at: String,
}

#[derive(Serialize)]
struct ThreadResponse {
    id: String,
    pipe_id: String,
    title: Option<String>,
    status: String,
    created_at: String,
    last_activity_at: Option<String>,
}

#[derive(Serialize)]
struct MessageResponse {
    id: String,
    role: String,
    content: String,
    created_at: String,
    tool_calls: Option<Vec<ToolCallResponse>>,
}

#[derive(Serialize)]
struct ToolCallResponse {
    name: String,
    result: Option<String>,
}

#[derive(Deserialize)]
struct SendRequest {
    text: String,
    image: Option<ImagePayload>,
    metadata: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ImagePayload {
    data_base64: String,
    mime_type: String,
    filename: String,
}

#[derive(Serialize)]
struct SendResponse {
    stream_id: String,
    thread_id: String,
}

#[derive(Deserialize)]
struct ApprovalQuery {
    approved: Option<bool>,
}

// ── Auth helper ──────────────────────────────────────────────────────────────

struct MobileUser {
    user_id: String,
    #[allow(dead_code)]
    username: String,
    #[allow(dead_code)]
    is_admin: bool,
    language: String,
}

async fn extract_mobile_user(headers: &HeaderMap, db: &sqlx::SqlitePool) -> Result<MobileUser, StatusCode> {
    let session_id = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|c| {
                let c = c.trim();
                c.strip_prefix("selu_session=").map(|v| v.to_string())
            })
        })
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let row = sqlx::query!(
        r#"SELECT ws.user_id, u.username, u.is_admin, u.language
           FROM web_sessions ws
           JOIN users u ON u.id = ws.user_id
           WHERE ws.id = ? AND ws.expires_at > datetime('now')"#,
        session_id
    )
    .fetch_optional(db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .ok_or(StatusCode::UNAUTHORIZED)?;

    Ok(MobileUser {
        user_id: row.user_id,
        username: row.username,
        is_admin: row.is_admin != 0,
        language: row.language,
    })
}

// ── POST /api/mobile/login ───────────────────────────────────────────────────

async fn mobile_login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> impl IntoResponse {
    use argon2::{Argon2, password_hash::{PasswordHash, PasswordVerifier}};

    let username = req.username.trim().to_string();

    let user = match sqlx::query!(
        "SELECT id, username, display_name, password_hash, is_admin FROM users WHERE username = ?",
        username
    )
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(u)) => u,
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    };

    let hash = match PasswordHash::new(&user.password_hash) {
        Ok(h) => h,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if Argon2::default()
        .verify_password(req.password.as_bytes(), &hash)
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let session_id = Uuid::new_v4().to_string();
    let user_id = user.id.unwrap_or_default();
    let expires_at = (Utc::now() + Duration::days(SESSION_TTL_DAYS))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    if let Err(e) = sqlx::query!(
        "INSERT INTO web_sessions (id, user_id, expires_at) VALUES (?, ?, ?)",
        session_id,
        user_id,
        expires_at
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to create mobile session: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    Json(LoginResponse {
        session_id,
        user_id,
        username: user.username,
        display_name: user.display_name,
        is_admin: user.is_admin != 0,
        expires_at,
    })
    .into_response()
}

// ── GET /api/mobile/pipes ────────────────────────────────────────────────────

async fn list_pipes(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    let rows = sqlx::query!(
        "SELECT id, user_id, name, transport, outbound_url, default_agent_id, active, created_at
         FROM pipes WHERE user_id = ? AND active = 1 ORDER BY created_at",
        user.user_id
    )
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let pipes: Vec<PipeResponse> = rows
                .into_iter()
                .map(|r| PipeResponse {
                    id: r.id.unwrap_or_default(),
                    user_id: r.user_id,
                    name: r.name,
                    transport: r.transport,
                    outbound_url: r.outbound_url,
                    default_agent_id: r.default_agent_id,
                    active: r.active != 0,
                    created_at: r.created_at,
                })
                .collect();
            Json(pipes).into_response()
        }
        Err(e) => {
            error!("Failed to list pipes: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── GET /api/mobile/pipes/{pipe_id}/threads ──────────────────────────────────

async fn list_threads(
    Path(pipe_id): Path<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    let pipe_id_str = pipe_id.to_string();
    let rows = sqlx::query!(
        r#"SELECT t.id, t.pipe_id, t.title, t.status, t.created_at,
                  (SELECT MAX(created_at) FROM messages WHERE thread_id = t.id) as "last_activity_at?: String"
           FROM threads t
           WHERE t.pipe_id = ? AND t.user_id = ?
           ORDER BY COALESCE(
               (SELECT MAX(created_at) FROM messages WHERE thread_id = t.id),
               t.created_at
           ) DESC
           LIMIT 50"#,
        pipe_id_str,
        user.user_id,
    )
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let threads: Vec<ThreadResponse> = rows
                .into_iter()
                .map(|r| ThreadResponse {
                    id: r.id.unwrap_or_default(),
                    pipe_id: r.pipe_id.clone(),
                    title: r.title,
                    status: r.status,
                    created_at: r.created_at,
                    last_activity_at: r.last_activity_at,
                })
                .collect();
            Json(threads).into_response()
        }
        Err(e) => {
            error!("Failed to list threads: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── POST /api/mobile/pipes/{pipe_id}/threads ─────────────────────────────────

async fn create_thread(
    Path(pipe_id): Path<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    let pipe_id_str = pipe_id.to_string();

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

    let force_new = state
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
        force_new,
        None,
    )
    .await
    {
        Ok(thread) => Json(ThreadResponse {
            id: thread.id.to_string(),
            pipe_id: pipe_id_str,
            title: thread.title,
            status: thread.status.to_string(),
            created_at: thread.created_at.to_string(),
            last_activity_at: None,
        })
        .into_response(),
        Err(e) => {
            error!("Failed to create thread: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── DELETE /api/mobile/pipes/{pipe_id}/threads/{thread_id} ───────────────────

async fn delete_thread(
    Path((pipe_id, thread_id)): Path<(Uuid, Uuid)>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    let pipe_id_str = pipe_id.to_string();
    let thread_id_str = thread_id.to_string();

    // Verify ownership
    let exists = sqlx::query!(
        "SELECT t.id FROM threads t WHERE t.id = ? AND t.pipe_id = ? AND t.user_id = ?",
        thread_id_str,
        pipe_id_str,
        user.user_id,
    )
    .fetch_optional(&state.db)
    .await;

    if matches!(exists, Ok(None) | Err(_)) {
        return StatusCode::NOT_FOUND.into_response();
    }

    // Delete messages then thread
    let _ = sqlx::query!("DELETE FROM messages WHERE thread_id = ?", thread_id_str)
        .execute(&state.db)
        .await;
    let _ = sqlx::query!(
        "DELETE FROM threads WHERE id = ? AND user_id = ?",
        thread_id_str,
        user.user_id,
    )
    .execute(&state.db)
    .await;

    StatusCode::NO_CONTENT.into_response()
}

// ── GET /api/mobile/pipes/{pipe_id}/threads/{thread_id}/messages ─────────────

async fn list_messages(
    Path((_pipe_id, thread_id)): Path<(Uuid, Uuid)>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let _user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    let thread_id_str = thread_id.to_string();
    let rows = sqlx::query!(
        "SELECT id, role, content, created_at, tool_calls_json, tool_call_id
         FROM messages WHERE thread_id = ? ORDER BY created_at LIMIT 200",
        thread_id_str
    )
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let mut messages = Vec::new();
            let mut tool_call_map: std::collections::HashMap<String, (usize, usize)> =
                std::collections::HashMap::new();

            for r in &rows {
                if r.role == "tool" {
                    if let Some(tc_id) = &r.tool_call_id {
                        if let Some(&(msg_idx, tc_idx)) = tool_call_map.get(tc_id) {
                            if let Some(tcs) = &mut messages[msg_idx] {
                                if let MessageResponse { tool_calls: Some(calls), .. } = tcs {
                                    if tc_idx < calls.len() {
                                        calls[tc_idx].result = Some(r.content.clone());
                                    }
                                }
                            }
                            continue;
                        }
                    }
                }

                let mut tool_calls = Vec::new();
                if let Some(json_str) = &r.tool_calls_json {
                    if let Ok(calls) = serde_json::from_str::<Vec<serde_json::Value>>(json_str) {
                        let msg_idx = messages.len();
                        for (tc_idx, call) in calls.iter().enumerate() {
                            let name = call["name"].as_str().unwrap_or("tool").to_string();
                            let id = call["id"].as_str().unwrap_or_default().to_string();
                            tool_calls.push(ToolCallResponse {
                                name,
                                result: None,
                            });
                            if !id.is_empty() {
                                tool_call_map.insert(id, (msg_idx, tc_idx));
                            }
                        }
                    }
                }

                messages.push(Some(MessageResponse {
                    id: r.id.clone().unwrap_or_default(),
                    role: r.role.clone(),
                    content: r.content.clone(),
                    created_at: r.created_at.clone(),
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                }));
            }

            let result: Vec<MessageResponse> = messages.into_iter().flatten().collect();
            Json(result).into_response()
        }
        Err(e) => {
            error!("Failed to list messages: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── POST /api/mobile/pipes/{pipe_id}/threads/{thread_id}/send ────────────────

async fn send_message(
    Path((pipe_id, thread_id)): Path<(Uuid, Uuid)>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SendRequest>,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    let pipe_id_str = pipe_id.to_string();
    let thread_id_str = thread_id.to_string();
    let stream_id = Uuid::new_v4().to_string();

    // Register SSE stream
    let notify = Arc::new(Notify::new());
    {
        let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
        let mut streams = state.active_streams.lock().await;
        streams.insert(stream_id.clone(), tx);
    }
    {
        let mut notifies = state.stream_notifies.lock().await;
        notifies.insert(stream_id.clone(), notify.clone());
    }

    // Parse image attachment
    let mut inbound_attachments = Vec::new();
    if let Some(img) = req.image {
        if let Ok(data) = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            &img.data_base64,
        ) {
            inbound_attachments.push(InboundAttachmentInput {
                filename: img.filename,
                mime_type: img.mime_type,
                data,
            });
        }
    }

    // Extract location context from metadata (if present)
    let location_context = req
        .metadata
        .as_ref()
        .and_then(|m| m.get("location"))
        .and_then(|loc| {
            let lat = loc.get("latitude")?.as_f64()?;
            let lng = loc.get("longitude")?.as_f64()?;
            let place_name = loc
                .get("place_name")
                .and_then(|v| v.as_str())
                .map(String::from);
            let mut ctx = format!("Latitude: {lat}, Longitude: {lng}");
            if let Some(name) = place_name {
                ctx = format!("{name} ({ctx})");
            }
            Some(ctx)
        });

    // Spawn background processing
    let bg_state = state.clone();
    let bg_stream_id = stream_id.clone();
    let bg_user_id = user.user_id.clone();
    let text = req.text.clone();

    tokio::spawn(async move {
        // Wait for SSE client
        info!(stream_id = %bg_stream_id, "Waiting for mobile SSE client to connect");
        tokio::select! {
            _ = notify.notified() => {
                info!(stream_id = %bg_stream_id, "Mobile SSE client connected");
            }
            _ = tokio::time::sleep(StdDuration::from_secs(10)) => {
                warn!(stream_id = %bg_stream_id, "Mobile SSE client did not connect within 10s");
            }
        }

        let default_agent_id = sqlx::query!(
            "SELECT default_agent_id FROM pipes WHERE id = ?",
            pipe_id_str
        )
        .fetch_optional(&bg_state.db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.default_agent_id);

        let agents_snapshot = bg_state.agents.load();
        let user_agents =
            crate::agents::access::visible_agents(&bg_state.db, &bg_user_id, &agents_snapshot)
                .await;
        let (agent_id, effective_text) =
            agent_router::route(&text, default_agent_id.as_deref(), &user_agents);

        let tx = match bg_state
            .active_streams
            .lock()
            .await
            .get(&bg_stream_id)
            .cloned()
        {
            Some(t) => {
                info!(stream_id = %bg_stream_id, "Found active stream channel");
                t
            }
            None => {
                warn!(stream_id = %bg_stream_id, "No active stream channel found, events will be lost");
                let (t, _) = mpsc::channel::<LoopEvent>(1);
                t
            }
        };

        let params = TurnParams {
            pipe_id: pipe_id_str,
            user_id: bg_user_id.clone(),
            agent_id: Some(agent_id),
            message: effective_text,
            thread_id: Some(thread_id_str),
            chain_depth: 0,
            channel_kind: ChannelKind::Interactive,
            skip_user_persist: false,
            enable_streaming: true,
            inbound_attachments,
            delegation_trace: Vec::new(),
            location_context,
        };

        match run_turn(&bg_state, params, tx.clone()).await {
            Ok(output) => {
                if !output.attachments.is_empty() {
                    let web_base = bg_state.config.base_path().to_string();
                    let attachments = crate::agents::artifacts::to_outbound_attachments(
                        &bg_state.artifacts,
                        &output.attachments,
                        &bg_user_id,
                        &web_base,
                        &bg_state.config.encryption_key,
                        false,
                    )
                    .await;
                    if !attachments.is_empty() {
                        let mut lines = vec![String::new()];
                        for a in attachments {
                            if let Some(url) = a.download_url {
                                lines.push(format!("[{}]({})", a.filename, url));
                            }
                        }
                        let _ = tx.send(LoopEvent::Token(lines.join("\n"))).await;
                    }
                }
            }
            Err(e) => {
                error!("Mobile agent turn failed: {e}");
                let _ = tx.send(LoopEvent::Error(e.to_string())).await;
                let _ = tx.send(LoopEvent::Done).await;
            }
        }

        bg_state
            .active_streams
            .lock()
            .await
            .remove(&bg_stream_id);
    });

    Json(SendResponse {
        stream_id,
        thread_id: thread_id.to_string(),
    })
    .into_response()
}

// ── GET /api/mobile/pipes/{pipe_id}/stream/{stream_id} ───────────────────────

async fn stream_response(
    Path((_pipe_id, stream_id)): Path<(Uuid, String)>,
    State(state): State<AppState>,
) -> Sse<impl futures::Stream<Item = Result<axum::response::sse::Event, Infallible>>> {
    info!(stream_id = %stream_id, "Mobile SSE stream endpoint hit");
    let (bridge_tx, bridge_rx) = mpsc::channel::<LoopEvent>(64);
    {
        let mut streams = state.active_streams.lock().await;
        streams.insert(stream_id.clone(), bridge_tx);
    }

    // Signal readiness
    {
        let mut notifies = state.stream_notifies.lock().await;
        if let Some(notify) = notifies.remove(&stream_id) {
            info!(stream_id = %stream_id, "Signaling SSE readiness to background task");
            notify.notify_one();
        } else {
            warn!(stream_id = %stream_id, "No notify found for stream - background task may have already timed out");
        }
    }

    let log_stream_id = stream_id.clone();
    let confirmations = state.pending_confirmations.clone();
    let sse_stream = ReceiverStream::new(bridge_rx).map(move |event| {
        let (event_type, data) = match event {
            LoopEvent::Token(t) => ("token", t),
            LoopEvent::CapabilityStatus(s) => ("tool_status", s),
            LoopEvent::Done => ("done", String::new()),
            LoopEvent::Error(e) => ("error", e),
            LoopEvent::ConfirmationRequired(req) => {
                let cid = Uuid::new_v4().to_string();
                let args = serde_json::to_string(&req.arguments).unwrap_or_default();
                if let Ok(mut pending) = confirmations.try_lock() {
                    pending.insert(cid.clone(), req.reply);
                }
                let payload = serde_json::json!({
                    "confirmation_id": cid,
                    "tool_name": req.tool_display_name,
                    "approval_message": req.approval_message,
                    "arguments": args,
                });
                ("confirmation_required", payload.to_string())
            }
            LoopEvent::ApprovalQueued {
                tool_display_name,
                approval_id,
                ..
            } => {
                let payload = serde_json::json!({
                    "tool_name": tool_display_name,
                    "approval_id": approval_id,
                });
                ("approval_queued", payload.to_string())
            }
        };

        info!(stream_id = %log_stream_id, event_type, "Sending SSE event to mobile client");
        Ok::<_, Infallible>(
            axum::response::sse::Event::default()
                .event(event_type)
                .data(data),
        )
    });

    Sse::new(sse_stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(StdDuration::from_secs(15))
            .text("ping"),
    )
}

// ── POST /api/mobile/approvals/{confirmation_id} ─────────────────────────────

async fn resolve_approval(
    Path(confirmation_id): Path<String>,
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<ApprovalQuery>,
) -> impl IntoResponse {
    let approved = params.approved.unwrap_or(false);

    let sender = {
        let mut pending = state.pending_confirmations.lock().await;
        pending.remove(&confirmation_id)
    };

    match sender {
        Some(tx) => {
            let _ = tx.send(approved);
            Json(serde_json::json!({ "status": if approved { "approved" } else { "denied" } }))
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ── GET /api/mobile/artifacts/{artifact_id} ─────────────────────────────────

async fn get_artifact(
    Path(artifact_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let _user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    // Try in-memory store first, then persisted
    let artifact = if let Some(a) =
        crate::agents::artifacts::get_by_id(&state.artifacts, &artifact_id).await
    {
        a
    } else {
        match crate::agents::artifacts::get_persisted_by_id(&state.db, &artifact_id).await {
            Ok(Some(p)) => crate::agents::artifacts::StoredArtifact {
                user_id: p.user_id,
                session_id: String::new(),
                filename: p.filename,
                mime_type: p.mime_type,
                data: p.data,
                created_at: std::time::Instant::now(),
            },
            _ => return StatusCode::NOT_FOUND.into_response(),
        }
    };

    (
        StatusCode::OK,
        [
            ("content-type", artifact.mime_type.as_str()),
            ("cache-control", "private, max-age=3600"),
        ],
        artifact.data,
    )
        .into_response()
}

// ── POST /api/mobile/redeem ──────────────────────────────────────────────────
// Exchange a one-time setup token (from QR code) for a session.

#[derive(Deserialize)]
struct RedeemRequest {
    token: String,
}

async fn redeem_setup_token(
    State(state): State<AppState>,
    Json(req): Json<RedeemRequest>,
) -> impl IntoResponse {
    // Look up the token and verify it hasn't expired
    let user_id: Option<String> = match sqlx::query_scalar(
        "SELECT user_id FROM mobile_setup_tokens WHERE token = ? AND expires_at > datetime('now') AND used = 0",
    )
    .bind(&req.token)
    .fetch_optional(&state.db)
    .await
    {
        Ok(uid) => uid,
        Err(e) => {
            error!("Failed to look up setup token: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let Some(token_user_id) = user_id else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    // Mark the token as used
    let _ = sqlx::query("UPDATE mobile_setup_tokens SET used = 1 WHERE token = ?")
        .bind(&req.token)
        .execute(&state.db)
        .await;

    // Look up the user
    let user = match sqlx::query!(
        "SELECT id, username, display_name, is_admin FROM users WHERE id = ?",
        token_user_id
    )
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(u)) => u,
        _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    // Create a long-lived session
    let session_id = Uuid::new_v4().to_string();
    let user_id = user.id.unwrap_or_default();
    let expires_at = (Utc::now() + Duration::days(SESSION_TTL_DAYS))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    if let Err(e) = sqlx::query!(
        "INSERT INTO web_sessions (id, user_id, expires_at) VALUES (?, ?, ?)",
        session_id,
        user_id,
        expires_at
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to create session from setup token: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    Json(LoginResponse {
        session_id,
        user_id,
        username: user.username,
        display_name: user.display_name,
        is_admin: user.is_admin != 0,
        expires_at,
    })
    .into_response()
}
