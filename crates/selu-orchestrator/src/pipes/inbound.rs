use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use std::sync::Arc;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::agents::thread as thread_mgr;
use crate::agents::{
    engine::{ChannelKind, TurnParams, noop_sender, run_turn},
    router as agent_router,
};
use crate::channels::WebhookSender;
use crate::permissions::approval_queue;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/pipes/{pipe_id}/inbound", post(handle_inbound))
}

/// Resolve a sender_ref to a user_id via the user_sender_refs table.
/// Returns None if the sender is not registered for this pipe (message should be ignored).
async fn resolve_sender(
    db: &sqlx::SqlitePool,
    pipe_id: &str,
    sender_ref: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query!(
        "SELECT user_id FROM user_sender_refs WHERE pipe_id = ? AND sender_ref = ?",
        pipe_id,
        sender_ref,
    )
    .fetch_optional(db)
    .await?;

    Ok(row.map(|r| r.user_id))
}

/// POST /api/pipes/{pipe_id}/inbound
async fn handle_inbound(
    Path(pipe_id): Path<Uuid>,
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(envelope): axum::Json<selu_core::types::InboundEnvelope>,
) -> impl IntoResponse {
    let provided_token = match extract_bearer(&headers) {
        Some(t) => t,
        None => {
            warn!(pipe_id = %pipe_id, "Inbound: missing Authorization header");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    let pipe_id_str = pipe_id.to_string();

    let pipe = match sqlx::query!(
        "SELECT id, user_id, inbound_token, outbound_url, outbound_auth, default_agent_id, active
         FROM pipes WHERE id = ?",
        pipe_id_str
    )
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(p)) => p,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("DB error: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if pipe.active == 0 {
        return StatusCode::FORBIDDEN.into_response();
    }
    if pipe.inbound_token != provided_token {
        warn!(pipe_id = %pipe_id, "Inbound: invalid token");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // ── Sender resolution ─────────────────────────────────────────────────────
    // Check if this sender_ref maps to a known user for this pipe.
    // If no mapping exists, check if there are ANY mappings for this pipe.
    // If there are mappings but this sender isn't in them, reject.
    // If there are no mappings at all, fall back to pipe.user_id (backward compat).
    let resolved_user_id = match resolve_sender(&state.db, &pipe_id_str, &envelope.sender_ref).await
    {
        Ok(Some(uid)) => uid,
        Ok(None) => {
            // Check if there are any sender_ref mappings for this pipe
            let has_mappings = sqlx::query!(
                "SELECT COUNT(*) as cnt FROM user_sender_refs WHERE pipe_id = ?",
                pipe_id_str
            )
            .fetch_one(&state.db)
            .await
            .map(|r| r.cnt > 0)
            .unwrap_or(false);

            if has_mappings {
                // There are mappings, but this sender isn't in them. Reject.
                info!(
                    pipe_id = %pipe_id,
                    sender = %envelope.sender_ref,
                    "Inbound: unknown sender (not in allowed sender_refs), ignoring"
                );
                return StatusCode::OK.into_response(); // 200 so adapter doesn't retry
            } else {
                // No mappings configured: backward-compatible, use pipe owner
                pipe.user_id.clone()
            }
        }
        Err(e) => {
            error!("Failed to resolve sender: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    info!(
        pipe_id = %pipe_id,
        sender = %envelope.sender_ref,
        user_id = %resolved_user_id,
        "Inbound message (sender resolved)"
    );

    // ── Extract message refs from metadata ────────────────────────────────────
    // If the adapter doesn't supply a message_guid, generate a synthetic one
    // so that each webhook message gets its own thread (prevents the
    // `find_recent_active_thread` fallback from merging concurrent messages
    // into a single thread).
    let origin_message_ref = Some(
        envelope
            .metadata
            .as_ref()
            .and_then(|m: &serde_json::Value| m.get("message_guid"))
            .and_then(|v: &serde_json::Value| v.as_str())
            .map(|s: &str| s.to_string())
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
    );

    // reply_to_ref: if the adapter provides threading info, use it to
    // continue an existing thread (same as the BB adapter does).
    let reply_to_ref = envelope
        .metadata
        .as_ref()
        .and_then(|m: &serde_json::Value| m.get("reply_to_ref"))
        .and_then(|v: &serde_json::Value| v.as_str())
        .map(|s: &str| s.to_string());

    // ── Route to agent ────────────────────────────────────────────────────────
    let agents_snapshot = state.agents.load();
    let (agent_id, effective_text) = agent_router::route(
        &envelope.text,
        pipe.default_agent_id.as_deref(),
        &agents_snapshot,
    );

    let outbound_url = pipe.outbound_url.clone();
    let outbound_auth = pipe.outbound_auth.clone();
    let recipient_ref = envelope.sender_ref.clone();

    // Register a webhook channel sender for this pipe (if outbound is configured).
    // This allows the approval queue to send prompts back through the pipe.
    if !outbound_url.is_empty() {
        let sender = Arc::new(WebhookSender::new(
            outbound_url.clone(),
            outbound_auth.clone(),
            recipient_ref.clone(),
        ));
        state.channel_registry.register(&pipe_id_str, sender).await;
    }

    tokio::spawn(async move {
        // ── Find or create thread ─────────────────────────────────────────────
        // If the adapter provides a reply_to_ref, try to continue an existing
        // thread. Otherwise create a new one.
        let thread = match thread_mgr::find_or_create_thread(
            &state.db,
            &pipe_id_str,
            &resolved_user_id,
            &agent_id,
            origin_message_ref.as_deref(),
            reply_to_ref.as_deref(),
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                error!("Failed to find/create thread: {e}");
                return;
            }
        };

        let thread_id = thread.id.to_string();

        // ── Approval interception ─────────────────────────────────────────────
        // If this thread has a pending tool approval, the user's reply resolves
        // it instead of starting a new agent turn.
        if approval_queue::try_resolve_pending(&state, &thread_id).await
            || approval_queue::try_resolve_pending_for_user_pipe(
                &state,
                &resolved_user_id,
                &pipe_id_str,
            )
            .await
        {
            info!(thread_id = %thread_id, "Message consumed as tool approval response (webhook)");
            return;
        }

        // Determine channel kind based on whether outbound is configured.
        // Any pipe with an outbound URL can receive async tool-approval
        // prompts, so it qualifies as ThreadedNonInteractive even on the
        // first message (before reply_to_ref is available).
        let is_threaded = !outbound_url.is_empty();
        let channel_kind = if is_threaded {
            ChannelKind::ThreadedNonInteractive {
                pipe_id: pipe_id_str.clone(),
                thread_id: thread_id.clone(),
            }
        } else {
            ChannelKind::NonInteractive
        };

        let params = TurnParams {
            pipe_id: pipe_id_str,
            user_id: resolved_user_id.clone(),
            agent_id: Some(agent_id),
            message: effective_text,
            thread_id: Some(thread_id.clone()),
            chain_depth: 0,
            channel_kind,
            skip_user_persist: false,
        };

        let reply = run_turn(&state, params, noop_sender()).await;

        let reply_text = match reply {
            Ok(t) if !t.is_empty() => t,
            Ok(_) => {
                warn!("Agent turn returned empty reply");
                // Empty reply — send a fallback message so the user isn't
                // left staring at silence, then complete/keep the thread.
                if is_threaded {
                    let lang = crate::i18n::user_language(&state.db, &resolved_user_id).await;
                    let error_text = crate::i18n::t(&lang, "error.agent_turn_failed");
                    let sender = crate::pipes::outbound::OutboundSender::new();
                    let outbound = selu_core::types::OutboundEnvelope {
                        recipient_ref: recipient_ref.clone(),
                        text: error_text.to_string(),
                        thread_id: Some(thread_id.clone()),
                        reply_to_message_ref: thread.origin_message_ref.clone(),
                        metadata: None,
                    };
                    if let Err(send_err) = sender
                        .send(&outbound_url, outbound_auth.as_deref(), &outbound)
                        .await
                    {
                        error!("Failed to send fallback message to user: {send_err}");
                    }
                } else {
                    let _ = thread_mgr::complete_thread(&state.db, &thread_id).await;
                }
                return;
            }
            Err(e) => {
                error!("Agent turn failed: {e}");
                let _ = thread_mgr::fail_thread(&state.db, &thread_id).await;
                // Send a user-friendly error message if outbound is configured
                if is_threaded {
                    let lang = crate::i18n::user_language(&state.db, &resolved_user_id).await;
                    let error_text = crate::i18n::t(&lang, "error.agent_turn_failed");
                    let sender = crate::pipes::outbound::OutboundSender::new();
                    let outbound = selu_core::types::OutboundEnvelope {
                        recipient_ref: recipient_ref.clone(),
                        text: error_text.to_string(),
                        thread_id: Some(thread_id.clone()),
                        reply_to_message_ref: thread.origin_message_ref.clone(),
                        metadata: None,
                    };
                    if let Err(send_err) = sender
                        .send(&outbound_url, outbound_auth.as_deref(), &outbound)
                        .await
                    {
                        error!("Failed to send error message to user: {send_err}");
                    }
                }
                return;
            }
        };

        // ── Send outbound reply with thread correlation ───────────────────────
        let sender = crate::pipes::outbound::OutboundSender::new();
        let outbound = selu_core::types::OutboundEnvelope {
            recipient_ref,
            text: reply_text,
            thread_id: Some(thread_id.clone()),
            reply_to_message_ref: thread.origin_message_ref.clone(),
            metadata: None,
        };
        if let Err(e) = sender
            .send(&outbound_url, outbound_auth.as_deref(), &outbound)
            .await
        {
            error!("Outbound send failed: {e}");
        }

        // Only complete the thread immediately for non-threaded pipes
        // (no outbound URL). Threaded pipes stay active for multi-turn
        // conversations; the 48-hour idle cleanup handles eventual closure.
        if !is_threaded {
            let _ = thread_mgr::complete_thread(&state.db, &thread_id).await;
        }
    });

    StatusCode::OK.into_response()
}

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let auth = headers.get("Authorization")?.to_str().ok()?;
    auth.strip_prefix("Bearer ").map(|s| s.to_string())
}
