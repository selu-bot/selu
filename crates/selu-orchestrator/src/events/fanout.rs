/// Fanout task: consumes events from the broadcast bus and fires reactions.
///
/// Runs as a background tokio task for the lifetime of the process.
/// For each event it:
///   1. Loads active subscriptions for (user_id, event_type) from the DB
///   2. Evaluates the CEL filter for each subscription
///   3. Dispatches matching subscriptions:
///      - `pipe_notification`  → POSTs a message to the pipe's outbound URL
///      - `agent_invocation`   → Opens a session and runs the named agent
///   4. Marks the event as processed
///
/// Loop prevention: skips `agent_invocation` if event.chain_depth >= 3.
use anyhow::{Context as _, Result};
use serde::Deserialize;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use selu_core::types::AgentEvent;

use crate::agents::engine::{noop_sender, run_turn, TurnParams};
use crate::events::cel;
use crate::pipes::outbound::OutboundSender;
use crate::state::AppState;

// Default max chain depth (used as fallback; actual value comes from config)
#[allow(dead_code)]
const MAX_CHAIN_DEPTH: i32 = 3;

/// Starts the fanout background task. The returned JoinHandle can be dropped --
/// the task keeps running as long as the broadcast channel has senders.
pub fn start(state: AppState, mut rx: broadcast::Receiver<AgentEvent>, max_chain_depth: i32) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Err(e) = process_event(&state, event, max_chain_depth).await {
                        error!("Fanout error: {e}");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Fanout lagged, dropped {} events", n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!("Event bus closed, fanout task exiting");
                    break;
                }
            }
        }
    })
}

async fn process_event(state: &AppState, event: AgentEvent, max_chain_depth: i32) -> Result<()> {
    debug!(event_type = %event.event_type, session = %event.source_session_id, "Processing event");

    // Determine which user this event belongs to via the source session
    let session_id_str = event.source_session_id.to_string();
    let session_row = sqlx::query!(
        "SELECT user_id FROM sessions WHERE id = ?",
        session_id_str
    )
    .fetch_optional(&state.db)
    .await
    .context("DB error looking up session for event")?;

    let user_id = match session_row {
        Some(r) => r.user_id,
        None => {
            warn!(session = %event.source_session_id, "Cannot fanout: session not found");
            mark_processed(state, &event.id.to_string()).await;
            return Ok(());
        }
    };

    // Load matching active subscriptions for this user + event type
    let subs = sqlx::query!(
        r#"SELECT id, reaction_type, reaction_config_json, filter_expression
           FROM event_subscriptions
           WHERE user_id = ? AND event_type = ? AND active = 1"#,
        user_id,
        event.event_type,
    )
    .fetch_all(&state.db)
    .await
    .context("DB error loading subscriptions")?;

    for sub in subs {
        // CEL filter evaluation
        match cel::matches(sub.filter_expression.as_deref(), &event) {
            Ok(false) => {
                debug!(sub_id = %sub.id.as_deref().unwrap_or("?"), "Subscription filtered out");
                continue;
            }
            Err(e) => {
                warn!(sub_id = %sub.id.as_deref().unwrap_or("?"), "CEL filter error: {e}");
                continue;
            }
            Ok(true) => {}
        }

        let sub_id = sub.id.as_deref().unwrap_or("?").to_string();
        let result = match sub.reaction_type.as_str() {
            "pipe_notification" => {
                dispatch_notification(state, &sub.reaction_config_json, &event).await
            }
            "agent_invocation" => {
                if event.chain_depth >= max_chain_depth {
                    warn!(sub_id = %sub_id, depth = event.chain_depth, "Skipping agent_invocation: chain depth limit");
                    continue;
                }
                dispatch_agent_invocation(state, &sub.reaction_config_json, &event, &user_id).await
            }
            other => {
                warn!(sub_id = %sub_id, "Unknown reaction type '{}'", other);
                continue;
            }
        };

        if let Err(e) = result {
            error!(sub_id = %sub_id, "Reaction dispatch failed: {e}");
        }
    }

    mark_processed(state, &event.id.to_string()).await;
    Ok(())
}

// ── Reactions ─────────────────────────────────────────────────────────────────

/// Config for `pipe_notification` reactions.
#[derive(Debug, Deserialize)]
struct NotificationConfig {
    pipe_id: String,
    /// Optional message template. Supports `{event_type}` and `{payload}` placeholders.
    #[serde(default = "default_notification_template")]
    message_template: String,
}

fn default_notification_template() -> String {
    "Event: {event_type}".to_string()
}

async fn dispatch_notification(
    state: &AppState,
    config_json: &str,
    event: &AgentEvent,
) -> Result<()> {
    let config: NotificationConfig = serde_json::from_str(config_json)
        .context("Invalid pipe_notification reaction config")?;

    let message = config.message_template
        .replace("{event_type}", &event.event_type)
        .replace("{payload}", &event.payload.to_string());

    // Look up the pipe's outbound URL
    let pipe = sqlx::query!(
        "SELECT outbound_url, outbound_auth FROM pipes WHERE id = ? AND active = 1",
        config.pipe_id
    )
    .fetch_optional(&state.db)
    .await
    .context("DB error loading pipe for notification")?
    .context("Pipe not found or inactive")?;

    let outbound = selu_core::types::OutboundEnvelope {
        recipient_ref: "notification".to_string(),
        text: message,
        thread_id: None,
        reply_to_message_ref: None,
        metadata: Some(serde_json::json!({
            "event_type": event.event_type,
            "event_id": event.id,
        })),
    };

    OutboundSender::new()
        .send(&pipe.outbound_url, pipe.outbound_auth.as_deref(), &outbound)
        .await
        .context("Failed to send pipe notification")?;

    info!(pipe_id = %config.pipe_id, event_type = %event.event_type, "Sent pipe notification");
    Ok(())
}

/// Config for `agent_invocation` reactions.
#[derive(Debug, Deserialize)]
struct AgentInvocationConfig {
    pipe_id: String,
    agent_id: String,
    /// Message sent to the agent. Supports `{event_type}` and `{payload}` placeholders.
    #[serde(default = "default_invocation_template")]
    message_template: String,
}

fn default_invocation_template() -> String {
    "Handle event {event_type}: {payload}".to_string()
}

async fn dispatch_agent_invocation(
    state: &AppState,
    config_json: &str,
    event: &AgentEvent,
    user_id: &str,
) -> Result<()> {
    let config: AgentInvocationConfig = serde_json::from_str(config_json)
        .context("Invalid agent_invocation reaction config")?;

    let message = config.message_template
        .replace("{event_type}", &event.event_type)
        .replace("{payload}", &event.payload.to_string());

    let params = TurnParams {
        pipe_id: config.pipe_id.clone(),
        user_id: user_id.to_string(),
        agent_id: Some(config.agent_id.clone()),
        message,
        thread_id: None, // Event fanout doesn't use threads
        chain_depth: event.chain_depth + 1,
    };

    // Clone for the log lines below (config is moved into the spawn)
    let log_pipe = config.pipe_id.clone();
    let log_agent = config.agent_id.clone();

    let state_clone = state.clone();
    tokio::spawn(async move {
        let tx = noop_sender();
        if let Err(e) = run_turn(&state_clone, params, tx).await {
            error!(
                pipe_id = %config.pipe_id,
                agent_id = %config.agent_id,
                "Agent invocation reaction failed: {e}"
            );
        }
    });

    info!(
        pipe_id = %log_pipe,
        agent_id = %log_agent,
        event_type = %event.event_type,
        chain_depth = event.chain_depth + 1,
        "Dispatched agent_invocation reaction"
    );
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn mark_processed(state: &AppState, event_id: &str) {
    let _ = sqlx::query!(
        "UPDATE agent_events SET processed_at = datetime('now') WHERE id = ?",
        event_id
    )
    .execute(&state.db)
    .await;
}
