#![allow(dead_code)]

/// Event system: in-memory broadcast bus backed by the `agent_events` DB table.
///
/// Agents call the built-in `emit_event` tool during a session.
/// The bus persists the event and broadcasts it to any running fanout task.
/// The fanout task matches events against per-user subscriptions and fires reactions.
pub mod cel;
pub mod fanout;
pub mod nl_to_cel;

use anyhow::{Context as _, Result};
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::llm::provider::ToolSpec;
use selu_core::types::AgentEvent;

/// Channel capacity -- how many unprocessed events can queue before senders see lag.
const BUS_CAPACITY: usize = 256;
/// Default max chain depth (can be overridden via config).
const DEFAULT_MAX_CHAIN_DEPTH: i32 = 3;

/// Clone-cheap handle to the event bus (wraps a broadcast sender + DB pool).
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<AgentEvent>,
    db: SqlitePool,
}

impl EventBus {
    pub fn new(db: SqlitePool) -> (Self, broadcast::Receiver<AgentEvent>) {
        let (tx, rx) = broadcast::channel(BUS_CAPACITY);
        (Self { tx, db }, rx)
    }

    /// Persist and broadcast an event.
    /// Non-fatal: a send error (no active receivers) is logged but not propagated.
    pub async fn emit(&self, event: AgentEvent) -> Result<()> {
        // Persist first so the event survives even if fanout is down
        let id = event.id.to_string();
        let session_id = event.source_session_id.to_string();
        let payload_json =
            serde_json::to_string(&event.payload).context("Failed to serialise event payload")?;
        let chain_depth = event.chain_depth;

        sqlx::query!(
            r#"INSERT INTO agent_events
               (id, source_session_id, source_agent_id, event_type, payload_json, chain_depth)
               VALUES (?, ?, ?, ?, ?, ?)"#,
            id,
            session_id,
            event.source_agent_id,
            event.event_type,
            payload_json,
            chain_depth,
        )
        .execute(&self.db)
        .await
        .context("Failed to persist agent event")?;

        // Broadcast to fanout (best-effort)
        if let Err(e) = self.tx.send(event) {
            debug!("Event bus: no active receivers ({})", e);
        }

        Ok(())
    }

    /// Subscribe to the broadcast stream. The returned receiver will lag-drop
    /// events if it falls behind by more than `BUS_CAPACITY`.
    #[allow(dead_code)]
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }
}

// ── Built-in tool: emit_event ─────────────────────────────────────────────────

/// The `ToolSpec` injected into every agent's tool list so the LLM can emit events.
pub fn emit_event_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "emit_event".to_string(),
        description: "Emit a named event that can trigger subscriptions and invoke other agents. \
             Use this to notify the user, start workflows, or chain agent actions."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "event_type": {
                    "type": "string",
                    "description": "Dot-separated event identifier, e.g. 'calendar.reminder' or 'home.alert'"
                },
                "payload": {
                    "type": "object",
                    "description": "Arbitrary JSON data attached to the event"
                }
            },
            "required": ["event_type", "payload"]
        }),
    }
}

/// Called by the tool dispatcher when the LLM invokes `emit_event`.
pub async fn dispatch_emit_event(
    bus: &EventBus,
    session_id: &str,
    agent_id: &str,
    args: &Value,
    chain_depth: i32,
) -> Result<String> {
    let event_type = args["event_type"]
        .as_str()
        .context("emit_event: missing 'event_type' string")?
        .to_string();

    let payload = args
        .get("payload")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));

    if chain_depth >= DEFAULT_MAX_CHAIN_DEPTH {
        warn!(session = %session_id, event_type = %event_type, "Dropping emit_event: chain depth limit reached");
        return Ok(
            serde_json::json!({"ok": false, "reason": "chain depth limit reached"}).to_string(),
        );
    }

    let event = AgentEvent {
        id: Uuid::new_v4(),
        source_session_id: Uuid::parse_str(session_id).unwrap_or_else(|_| Uuid::new_v4()),
        source_agent_id: agent_id.to_string(),
        event_type: event_type.clone(),
        payload,
        chain_depth,
        emitted_at: chrono::Utc::now(),
        processed_at: None,
    };

    let event_id = event.id.to_string();
    bus.emit(event).await?;

    debug!(session = %session_id, event_type = %event_type, "Event emitted");
    Ok(serde_json::json!({"ok": true, "event_id": event_id}).to_string())
}
