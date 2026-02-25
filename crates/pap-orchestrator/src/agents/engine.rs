/// Shared agent invocation engine.
///
/// Centralises the session -> context -> LLM -> persist flow so that
/// inbound webhooks, web-chat, and event-fanout reactions all behave
/// identically without duplicating code.
use anyhow::Result;
use futures::future::BoxFuture;
use futures::FutureExt;
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use uuid::Uuid;

use pap_core::types::Role;

use crate::agents::{context, delegation, router as agent_router, session as session_mgr};
use crate::capabilities::build_tool_specs;
use crate::llm::{registry::load_provider, tool_loop::{run_loop, LoopEvent, LoopSender}};
use crate::state::AppState;

/// Parameters for a single agent turn.
pub struct TurnParams {
    pub pipe_id: String,
    pub user_id: String,
    /// If `None`, routing falls back to the pipe's default agent.
    pub agent_id: Option<String>,
    /// The user-visible message text (already stripped of any @mention prefix).
    pub message: String,
    /// Thread ID for this conversation thread. If `None`, messages are unthreaded
    /// (backward compatible with web chat and delegation).
    pub thread_id: Option<String>,
    /// Chain depth for loop prevention (0 for direct messages, +1 per fanout hop).
    pub chain_depth: i32,
}

/// Run one agent turn end-to-end:
///   1. Route to the correct agent
///   2. Open / resume a session
///   3. Persist the user message
///   4. Build context window + tool specs
///   5. Run the tool loop (capability dispatch + `emit_event` + `delegate_to_agent`)
///   6. Persist the assistant reply
///   7. Store embeddings for semantic memory
///
/// Streaming tokens are sent to `tx`. Pass a throwaway channel for
/// non-streaming callers (inbound, fanout).
///
/// Returns the assistant reply text.
pub async fn run_turn(state: &AppState, params: TurnParams, tx: LoopSender) -> Result<String> {
    let TurnParams { pipe_id, user_id, agent_id, message, thread_id, chain_depth } = params;

    // ── Route to agent ────────────────────────────────────────────────────────
    let agents_snapshot = state.agents.read().await.clone();
    let (resolved_agent_id, effective_text) = {
        if let Some(id) = agent_id {
            (id, message.clone())
        } else {
            agent_router::route(&message, None, &agents_snapshot)
        }
    };

    let agent = match agents_snapshot
        .get(&resolved_agent_id)
        .or_else(|| agents_snapshot.get("default"))
        .cloned()
    {
        Some(a) => a,
        None => {
            let _ = tx.send(LoopEvent::Error(format!("Agent '{}' not found", resolved_agent_id))).await;
            return Err(anyhow::anyhow!("Agent '{}' not found", resolved_agent_id));
        }
    };

    // ── Session ───────────────────────────────────────────────────────────────
    let session = session_mgr::open_session(&state.db, &pipe_id, &user_id, &agent.id).await?;
    let session_id = session.id.to_string();

    // ── Persist user message ──────────────────────────────────────────────────
    let msg_id = Uuid::new_v4().to_string();
    let user_role = Role::User.to_string();
    if let Err(e) = sqlx::query!(
        "INSERT INTO messages (id, pipe_id, session_id, thread_id, role, content) VALUES (?, ?, ?, ?, ?, ?)",
        msg_id, pipe_id, session_id, thread_id, user_role, effective_text
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to persist user message: {e}");
    }

    // ── Store user message embedding (best-effort, non-blocking) ──────────────
    if let Some(ref memory) = state.memory {
        let mem = memory.clone();
        let mid = msg_id.clone();
        let sid = session_id.clone();
        let pid = pipe_id.clone();
        let txt = effective_text.clone();
        tokio::spawn(async move {
            if let Err(e) = mem.store_message(&mid, &sid, &pid, &txt).await {
                debug!("Failed to store user message embedding: {e}");
            }
        });
    }

    // ── Context + provider (model resolved from DB) ────────────────────────────
    let messages = context::build(
        &state.db,
        &agent,
        &pipe_id,
        &session_id,
        thread_id.as_deref(),
        state.memory.as_deref(),
        &effective_text,
        Some(&agents_snapshot),
    ).await?;

    let resolved = crate::agents::model::resolve_model(&state.db, &agent.id).await?;
    let provider = load_provider(&state.db, &resolved.provider_id, &resolved.model_id, &state.credentials).await?;

    // ── Tool specs: capability tools + built-in emit_event + delegation ───────
    let (mut tool_specs, confirmation_tools) = build_tool_specs(&agent.capability_manifests);
    tool_specs.push(crate::events::emit_event_tool_spec());

    // Only add delegation tool if there are other agents to delegate to
    let has_delegates = agents_snapshot.keys().any(|id| id != &agent.id);
    if has_delegates {
        tool_specs.push(delegation::tool_spec(&agent.id, &agents_snapshot));
    }

    // ── Dispatcher ────────────────────────────────────────────────────────────
    let cap_engine = state.capabilities.clone();
    let cred_store = state.credentials.clone();
    let event_bus = state.events.clone();
    let cap_manifests = agent.capability_manifests.clone();
    let sid = session_id.clone();
    let uid = user_id.clone();
    let src_agent_id = agent.id.clone();
    let delegation_state = state.clone();
    let delegation_pipe = pipe_id.clone();

    let reply = run_loop(
        provider,
        messages,
        tool_specs,
        resolved.temperature,
        tx.clone(),
        confirmation_tools,
        move |name, args| {
            let engine = cap_engine.clone();
            let store = cred_store.clone();
            let bus = event_bus.clone();
            let manifests = cap_manifests.clone();
            let session = sid.clone();
            let user = uid.clone();
            let agent_id_copy = src_agent_id.clone();
            let del_state = delegation_state.clone();
            let del_pipe = delegation_pipe.clone();
            Box::pin(async move {
                if name == "emit_event" {
                    crate::events::dispatch_emit_event(
                        &bus, &session, &agent_id_copy, &args, chain_depth,
                    ).await
                } else if name == delegation::TOOL_NAME {
                    dispatch_delegation(
                        del_state, del_pipe, user.clone(), args, chain_depth,
                    ).await
                } else {
                    engine.invoke(&manifests, &name, args, &session, &user, &store).await
                }
            })
        },
    )
    .await?;

    // ── Persist assistant reply ───────────────────────────────────────────────
    if !reply.is_empty() {
        let reply_id = Uuid::new_v4().to_string();
        let asst_role = Role::Assistant.to_string();
        if let Err(e) = sqlx::query!(
            "INSERT INTO messages (id, pipe_id, session_id, thread_id, role, content) VALUES (?, ?, ?, ?, ?, ?)",
            reply_id, pipe_id, session_id, thread_id, asst_role, reply
        )
        .execute(&state.db)
        .await
        {
            error!("Failed to persist assistant reply: {e}");
        }

        // Store assistant reply embedding (best-effort)
        if let Some(ref memory) = state.memory {
            let mem = memory.clone();
            let rid = reply_id.clone();
            let sid = session_id.clone();
            let pid = pipe_id.clone();
            let rtxt = reply.clone();
            tokio::spawn(async move {
                if let Err(e) = mem.store_message(&rid, &sid, &pid, &rtxt).await {
                    debug!("Failed to store assistant reply embedding: {e}");
                }
            });
        }
    }

    Ok(reply)
}

/// Dispatch a `delegate_to_agent` tool call by running a nested agent turn.
///
/// The delegated turn runs non-streaming (the orchestrator agent will relay
/// the response), using the same pipe and user but targeting the specialist
/// agent directly.
///
/// Returns a `BoxFuture` (rather than being `async fn`) so that the recursive
/// `run_turn` call satisfies the `Send` bound required by the tool dispatcher.
fn dispatch_delegation(
    state: AppState,
    pipe_id: String,
    user_id: String,
    args: serde_json::Value,
    chain_depth: i32,
) -> BoxFuture<'static, Result<String>> {
    async move {
        let (target_agent_id, message) = delegation::parse_args(&args)?;

        info!(
            from = "orchestrator",
            to = %target_agent_id,
            "Delegating to specialist agent"
        );

        let params = TurnParams {
            pipe_id,
            user_id,
            agent_id: Some(target_agent_id),
            message,
            thread_id: None, // Delegation gets its own context, no thread
            chain_depth: chain_depth + 1,
        };

        // Run the delegated turn with a noop sender — we capture the reply text
        // and return it as the tool result to the orchestrator.
        let reply = run_turn(&state, params, noop_sender()).await?;

        Ok(reply)
    }.boxed()
}

/// Create a throwaway channel for callers that don't stream tokens.
pub fn noop_sender() -> LoopSender {
    let (tx, _) = mpsc::channel::<LoopEvent>(1);
    tx
}
