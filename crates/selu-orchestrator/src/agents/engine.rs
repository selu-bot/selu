/// Shared agent invocation engine.
///
/// Centralises the session -> context -> LLM -> persist flow so that
/// inbound webhooks, web-chat, and event-fanout reactions all behave
/// identically without duplicating code.
use anyhow::Result;
use futures::FutureExt;
use futures::future::BoxFuture;
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use selu_core::types::Role;

use crate::agents::loader::{AgentDefinition, RoutingMode};
use crate::agents::{
    context, delegation, router as agent_router, session as session_mgr, storage,
    thread as thread_mgr,
};
use crate::capabilities::build_tool_specs;
use crate::capabilities::discovery::{hydrate_dynamic_tools_for_agent, load_discovered_tools};
use crate::capabilities::manifest::CapabilityManifest;
use crate::llm::provider::{ChatMessage, LlmResponse};
use crate::llm::registry::load_provider;
use crate::llm::tool_loop::{LoopEvent, LoopSender, ToolDispatchResult, run_loop};
use crate::permissions::approval_queue;
use crate::permissions::tool_policy::{
    self, BUILTIN_CAPABILITY_ID, BUILTIN_DELEGATE, BUILTIN_EMIT_EVENT, BUILTIN_SET_REMINDER,
    BUILTIN_STORE_DELETE, BUILTIN_STORE_GET, BUILTIN_STORE_LIST, BUILTIN_STORE_SET, ToolPolicy,
};
use crate::state::AppState;

// ── Channel kind ──────────────────────────────────────────────────────────────

/// Describes the calling context for tool-policy enforcement.
///
/// Callers set this on `TurnParams` so the dispatcher knows how to handle
/// "ask" policies:
///   - `Interactive` → prompt via SSE (web chat)
///   - `ThreadedNonInteractive` → queue async approval via channel sender
///   - `NonInteractive` → treat "ask" as "block" (no user to ask)
#[derive(Debug, Clone)]
pub enum ChannelKind {
    /// Web chat — user is watching an SSE stream and can approve inline.
    Interactive,
    /// iMessage, webhook, etc. — there is a thread and a registered
    /// `ChannelSender`, but the user is not watching a live stream.
    ThreadedNonInteractive { pipe_id: String, thread_id: String },
    /// Event fanout, delegation, or other machine-to-machine paths.
    /// No user interaction possible; "ask" is treated as "block".
    NonInteractive,
}

// ── Turn parameters ───────────────────────────────────────────────────────────

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
    /// Channel type — determines how "ask" policies are handled.
    pub channel_kind: ChannelKind,
    /// When true, the user message is NOT persisted as a "user" role row.
    /// Used by the schedule executor: the prompt is still sent to the LLM
    /// but should not appear as a user bubble in chat.
    pub skip_user_persist: bool,
}

/// Run one agent turn end-to-end:
///   1. Route to the correct agent
///   2. Open / resume a session
///   3. Persist the user message
///   4. Build context window + tool specs
///   5. Run the tool loop (capability dispatch + `emit_event` + `delegate_to_agent`)
///   6. Persist the assistant reply
///   7. Extract personality facts (background)
///
/// Streaming tokens are sent to `tx`. Pass a throwaway channel for
/// non-streaming callers (inbound, fanout).
///
/// Returns the assistant reply text.
pub async fn run_turn(state: &AppState, params: TurnParams, tx: LoopSender) -> Result<String> {
    let turn_start = Instant::now();
    let TurnParams {
        pipe_id,
        user_id,
        agent_id,
        message,
        thread_id,
        chain_depth,
        channel_kind,
        skip_user_persist,
    } = params;

    // ── Route to agent ────────────────────────────────────────────────────────
    let route_start = Instant::now();
    let agents_snapshot = state.agents.load();
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
            let _ = tx
                .send(LoopEvent::Error(format!(
                    "Agent '{}' not found",
                    resolved_agent_id
                )))
                .await;
            return Err(anyhow::anyhow!("Agent '{}' not found", resolved_agent_id));
        }
    };
    // Drop the full snapshot early — we only need `agent` from here on.
    // Keep it alive only for delegation check below.
    debug!(
        agent_id = %agent.id,
        route_ms = route_start.elapsed().as_millis(),
        "Agent routing complete"
    );

    // ── Session + Persist user message (parallelized) ─────────────────────────
    let db_start = Instant::now();

    // Open session and persist user message in parallel.
    // Both are independent operations that hit the DB.
    let session_fut = session_mgr::open_session(&state.db, &pipe_id, &user_id, &agent.id);

    let session = session_fut.await?;
    let session_id = session.id.to_string();
    debug!(
        session_ms = db_start.elapsed().as_millis(),
        "Session opened"
    );

    // Persist user message (fire-and-forget timing)
    // Skip for schedule-triggered turns — the prompt should not show as a
    // user bubble; the executor persists a "status" note instead.
    if !skip_user_persist {
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
    }

    // ── Partition agents into inlined vs delegated ──────────────────────────────
    // Inlined agents' tools are exposed directly on this agent's tool list,
    // eliminating the delegation round-trip. Delegated agents keep the full
    // delegate_to_agent flow with their own LLM context.
    let (inlined_manifests, delegated_agents, cap_owner) =
        partition_agents(&agent, &agents_snapshot);

    // ── Context + provider (parallelized) ─────────────────────────────────────
    // These operations are independent and can run concurrently:
    //   1. Build context window (needs session_id, hits DB for personality + history)
    //   2. Resolve model + load provider (hits DB for agent model + provider key)
    let prep_start = Instant::now();

    let delegated_ref = if delegated_agents.is_empty() {
        None
    } else {
        Some(&delegated_agents)
    };

    let user_language = crate::i18n::user_language(&state.db, &user_id).await;

    let context_fut = context::build(
        &state.db,
        &agent,
        &pipe_id,
        &session_id,
        thread_id.as_deref(),
        &user_id,
        &user_language,
        &effective_text,
        &inlined_manifests,
        delegated_ref,
    );

    let provider_fut = async {
        let resolved = crate::agents::model::resolve_model(&state.db, &agent.id).await?;
        let provider = state
            .provider_cache
            .get_or_load(
                &state.db,
                &resolved.provider_id,
                &resolved.model_id,
                &state.credentials,
            )
            .await?;
        Ok::<_, anyhow::Error>((provider, resolved.temperature))
    };

    // Run context build and provider load in parallel
    let (context_result, provider_result) = tokio::join!(context_fut, provider_fut);

    let messages = context_result?;
    let (provider, temperature) = provider_result?;

    debug!(
        prep_ms = prep_start.elapsed().as_millis(),
        context_msgs = messages.len(),
        "Context + provider ready"
    );

    // ── Tool specs: own capabilities + inlined + built-ins + delegation ───────
    let mut own_manifests = agent.capability_manifests.clone();
    if let Err(e) = hydrate_dynamic_tools_for_agent(&state.db, &agent.id, &mut own_manifests).await
    {
        warn!(agent_id = %agent.id, "Failed to hydrate dynamic tools: {e}");
    }

    let mut hydrated_inlined = inlined_manifests.clone();
    for (cap_id, manifest) in &mut hydrated_inlined {
        if manifest.tool_source != crate::capabilities::manifest::ToolSource::Dynamic {
            continue;
        }
        let owner_agent_id = cap_owner
            .get(cap_id)
            .map(|s| s.as_str())
            .unwrap_or_else(|| agent.id.as_str());
        match load_discovered_tools(&state.db, owner_agent_id, cap_id).await {
            Ok(tools) => {
                manifest.tools = tools;
            }
            Err(e) => {
                warn!(
                    agent_id = %agent.id,
                    capability_id = %cap_id,
                    owner_agent_id = %owner_agent_id,
                    "Failed to load discovered tools for inlined capability: {e}"
                );
            }
        }
    }

    let mut tool_specs = build_tool_specs(&own_manifests);
    tool_specs.extend(build_tool_specs(&hydrated_inlined));
    tool_specs.push(crate::events::emit_event_tool_spec());
    tool_specs.push(storage::store_get_tool_spec());
    tool_specs.push(storage::store_set_tool_spec());
    tool_specs.push(storage::store_delete_tool_spec());
    tool_specs.push(storage::store_list_tool_spec());
    tool_specs.push(crate::schedules::set_reminder_tool_spec());

    // Only add delegation tool if there are agents that still need delegation
    if !delegated_agents.is_empty() {
        tool_specs.push(delegation::tool_spec(&agent.id, &delegated_agents));
    }

    // ── Dispatcher ────────────────────────────────────────────────────────────
    let cap_engine = state.capabilities.clone();
    let cred_store = state.credentials.clone();
    let event_bus = state.events.clone();
    // Merge the agent's own capability manifests with inlined manifests so the
    // dispatcher can resolve and invoke tools from both sources.
    let mut cap_manifests = own_manifests;
    cap_manifests.extend(hydrated_inlined);
    let sid = session_id.clone();
    let tid = thread_id.clone().unwrap_or_default();
    let uid = user_id.clone();
    let src_agent_id = agent.id.clone();
    let cap_owner_map = cap_owner;
    let delegation_state = state.clone();
    let delegation_pipe = pipe_id.clone();
    let dispatcher_state = state.clone();
    let dispatcher_channel = channel_kind.clone();
    let dispatcher_pipe = pipe_id.clone();
    let dispatcher_tx = tx.clone();

    debug!(
        setup_ms = turn_start.elapsed().as_millis(),
        "Turn setup complete, entering tool loop"
    );

    let reply = run_loop(
        provider,
        messages,
        tool_specs,
        temperature,
        tx.clone(),
        move |name, args, approved| {
            let engine = cap_engine.clone();
            let store = cred_store.clone();
            let bus = event_bus.clone();
            let manifests = cap_manifests.clone();
            let session = sid.clone();
            let thread = tid.clone();
            let user = uid.clone();
            let agent_id_copy = src_agent_id.clone();
            let owners = cap_owner_map.clone();
            let del_state = delegation_state.clone();
            let del_pipe = delegation_pipe.clone();
            let state = dispatcher_state.clone();
            let channel = dispatcher_channel.clone();
            let pipe = dispatcher_pipe.clone();
            let del_tx = dispatcher_tx.clone();

            Box::pin(async move {
                let invoke_args = strip_internal_tool_args(&args);

                // ── Built-in tools: emit_event, delegate_to_agent ─────────
                if name == "emit_event" {
                    // Check policy for built-in emit_event
                    let result = check_policy_and_dispatch(
                        &state,
                        &user,
                        BUILTIN_CAPABILITY_ID,
                        BUILTIN_EMIT_EVENT,
                        &name,
                        &args,
                        &channel,
                        &session,
                        &pipe,
                        &agent_id_copy,
                        approved,
                        || {
                            let bus = bus.clone();
                            let session = session.clone();
                            let agent_id = agent_id_copy.clone();
                            let args = invoke_args.clone();
                            let chain_depth = chain_depth;
                            Box::pin(async move {
                                crate::events::dispatch_emit_event(
                                    &bus,
                                    &session,
                                    &agent_id,
                                    &args,
                                    chain_depth,
                                )
                                .await
                            })
                        },
                    )
                    .await;
                    return result;
                }

                // ── Built-in tools: persistent agent storage ─────────────
                if let Some(builtin_store_name) = match name.as_str() {
                    "store_get" => Some(BUILTIN_STORE_GET),
                    "store_set" => Some(BUILTIN_STORE_SET),
                    "store_delete" => Some(BUILTIN_STORE_DELETE),
                    "store_list" => Some(BUILTIN_STORE_LIST),
                    _ => None,
                } {
                    let result = check_policy_and_dispatch(
                        &state,
                        &user,
                        BUILTIN_CAPABILITY_ID,
                        builtin_store_name,
                        &name,
                        &args,
                        &channel,
                        &session,
                        &pipe,
                        &agent_id_copy,
                        approved,
                        || {
                            let db = state.db.clone();
                            let agent_id = agent_id_copy.clone();
                            let user = user.clone();
                            let args = invoke_args.clone();
                            let tool = name.clone();
                            Box::pin(async move {
                                match tool.as_str() {
                                    "store_get" => {
                                        storage::dispatch_store_get(&db, &agent_id, &user, &args)
                                            .await
                                    }
                                    "store_set" => {
                                        storage::dispatch_store_set(&db, &agent_id, &user, &args)
                                            .await
                                    }
                                    "store_delete" => {
                                        storage::dispatch_store_delete(&db, &agent_id, &user, &args)
                                            .await
                                    }
                                    "store_list" => {
                                        storage::dispatch_store_list(&db, &agent_id, &user, &args)
                                            .await
                                    }
                                    _ => unreachable!(),
                                }
                            })
                        },
                    )
                    .await;
                    return result;
                }

                // ── Built-in tool: set_reminder ─────────────────────
                if name == "set_reminder" {
                    let result = check_policy_and_dispatch(
                        &state,
                        &user,
                        BUILTIN_CAPABILITY_ID,
                        BUILTIN_SET_REMINDER,
                        &name,
                        &args,
                        &channel,
                        &session,
                        &pipe,
                        &agent_id_copy,
                        approved,
                        || {
                            let db = state.db.clone();
                            let user = user.clone();
                            let pipe = pipe.clone();
                            let args = invoke_args.clone();
                            Box::pin(async move {
                                crate::schedules::dispatch_set_reminder(&db, &user, &pipe, &args)
                                    .await
                            })
                        },
                    )
                    .await;
                    return result;
                }

                if name == delegation::TOOL_NAME {
                    // Check policy for built-in delegate_to_agent
                    let result = check_policy_and_dispatch(
                        &state,
                        &user,
                        BUILTIN_CAPABILITY_ID,
                        BUILTIN_DELEGATE,
                        &name,
                        &args,
                        &channel,
                        &session,
                        &pipe,
                        &agent_id_copy,
                        approved,
                        || {
                            let del_state = del_state.clone();
                            let del_pipe = del_pipe.clone();
                            let user = user.clone();
                            let args = invoke_args.clone();
                            let chain_depth = chain_depth;
                            let del_channel = channel.clone();
                            let del_tx = del_tx.clone();
                            Box::pin(async move {
                                dispatch_delegation(
                                    del_state,
                                    del_pipe,
                                    user,
                                    args,
                                    chain_depth,
                                    del_channel,
                                    del_tx,
                                )
                                .await
                            })
                        },
                    )
                    .await;
                    return result;
                }

                // ── Capability tools ──────────────────────────────────────
                // Parse namespaced tool name to get capability_id and bare tool name
                let (cap_id, bare_tool) = if let Some((c, t)) = name.split_once("__") {
                    (c.to_string(), t.to_string())
                } else {
                    // Shouldn't happen with properly namespaced tools
                    (name.clone(), name.clone())
                };

                // For inlined tools, resolve the policy against the agent
                // that owns the capability, not the host agent.
                let policy_agent = owners.get(&cap_id).unwrap_or(&agent_id_copy);

                let result = check_policy_and_dispatch(
                    &state,
                    &user,
                    &cap_id,
                    &bare_tool,
                    &name,
                    &args,
                    &channel,
                    &session,
                    &pipe,
                    policy_agent,
                    approved,
                    || {
                        let engine = engine.clone();
                        let manifests = manifests.clone();
                        let name = name.clone();
                        let args = invoke_args.clone();
                        let session = session.clone();
                        let thread = thread.clone();
                        let user = user.clone();
                        let store = store.clone();
                        Box::pin(async move {
                            engine
                                .invoke(&manifests, &name, args, &session, &thread, &user, &store)
                                .await
                        })
                    },
                )
                .await;

                result
            })
        },
    )
    .await?;

    debug!(
        loop_ms = turn_start.elapsed().as_millis(),
        "Tool loop complete, persisting reply"
    );

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

        // ── Personality extraction (best-effort, non-blocking) ────────────────
        {
            let db = state.db.clone();
            let creds = state.credentials.clone();
            let uid = user_id.clone();
            let agent_id = agent.id.clone();
            let conv_msg = effective_text.clone();
            let conv_reply = reply.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::agents::personality::extract_from_conversation(
                    &db,
                    &creds,
                    &uid,
                    &agent_id,
                    &conv_msg,
                    &conv_reply,
                )
                .await
                {
                    debug!("Personality extraction failed (non-fatal): {e}");
                }
            });
        }

        // ── Auto-generate thread title (first reply only) ────────────────────
        if let Some(ref tid) = thread_id {
            let title_needed = sqlx::query("SELECT title FROM threads WHERE id = ?")
                .bind(tid.as_str())
                .fetch_optional(&state.db)
                .await
                .ok()
                .flatten()
                .map(|r| {
                    r.try_get::<Option<String>, _>("title")
                        .ok()
                        .flatten()
                        .is_none()
                })
                .unwrap_or(false);

            if title_needed {
                let db = state.db.clone();
                let creds = state.credentials.clone();
                let tid = tid.clone();
                let msg = effective_text.clone();
                let agent_id = agent.id.clone();
                tokio::spawn(async move {
                    if let Err(e) = generate_thread_title(&db, &creds, &tid, &msg, &agent_id).await
                    {
                        debug!("Failed to generate thread title: {e}");
                    }
                });
            }
        }
    }

    info!(
        total_ms = turn_start.elapsed().as_millis(),
        agent_id = %agent.id,
        "Agent turn complete"
    );

    Ok(reply)
}

// ── Policy check + dispatch helper ────────────────────────────────────────────

/// Check the user's tool policy and either invoke the tool, block it,
/// or return a confirmation/queued result.
///
/// When `approved` is `true` the caller has already obtained user consent
/// (e.g. via the interactive confirmation flow).  In that case the policy
/// check is skipped and the tool is invoked directly.
///
/// `invoke_fn` is a closure that actually executes the tool. It is only
/// called when the policy is `Allow` or the call was pre-approved.
async fn check_policy_and_dispatch<F, Fut>(
    state: &AppState,
    user_id: &str,
    capability_id: &str,
    tool_name: &str,
    namespaced_name: &str,
    args: &serde_json::Value,
    channel: &ChannelKind,
    session_id: &str,
    _pipe_id: &str,
    agent_id: &str,
    approved: bool,
    invoke_fn: F,
) -> Result<ToolDispatchResult>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    // If the user already approved this call, skip policy lookup and invoke.
    if approved {
        let result = invoke_fn().await.unwrap_or_else(|e| {
            warn!(tool = %namespaced_name, "Pre-approved tool invocation failed: {e}");
            format!("Tool error: {}", e)
        });
        return Ok(ToolDispatchResult::Done(result));
    }

    let policy = tool_policy::get_effective_policy(&state.db, user_id, agent_id, capability_id, tool_name)
        .await
        .unwrap_or_else(|e| {
            warn!(tool = %namespaced_name, "Failed to look up tool policy (defaulting to blocked): {e}");
            None
        });

    match policy {
        Some(ToolPolicy::Allow) => {
            let result = invoke_fn().await.unwrap_or_else(|e| {
                warn!(tool = %namespaced_name, "Tool invocation failed: {e}");
                format!("Tool error: {}", e)
            });
            Ok(ToolDispatchResult::Done(result))
        }

        Some(ToolPolicy::Block) | None => {
            let msg = if policy.is_none() {
                format!(
                    "The tool '{}' is not configured in your security policy. \
                     Ask the user to update their tool permissions for this agent.",
                    namespaced_name
                )
            } else {
                format!(
                    "The tool '{}' is blocked by the user's security policy. \
                     Do NOT retry this tool call. Inform the user that this action \
                     is blocked and suggest they update their tool permissions if they \
                     want to allow it.",
                    namespaced_name
                )
            };
            Ok(ToolDispatchResult::Blocked(msg))
        }

        Some(ToolPolicy::Ask) => {
            match channel {
                ChannelKind::Interactive => {
                    // The tool loop will prompt via SSE
                    Ok(ToolDispatchResult::NeedsConfirmation)
                }

                ChannelKind::ThreadedNonInteractive {
                    pipe_id: chan_pipe,
                    thread_id: chan_thread,
                } => {
                    // Queue an async approval
                    let args_json = serde_json::to_string(args).unwrap_or_default();
                    let tool_call_id = Uuid::new_v4().to_string();

                    let approval_id = approval_queue::create_pending(
                        &state.db,
                        user_id,
                        session_id,
                        chan_thread,
                        chan_pipe,
                        agent_id,
                        capability_id,
                        tool_name,
                        &args_json,
                        &tool_call_id,
                    )
                    .await?;

                    // Look up user language for the approval prompt
                    let lang = crate::i18n::user_language(&state.db, user_id).await;

                    // Build a human-readable approval prompt
                    let prompt_text = build_approval_prompt(&lang, namespaced_name, args);

                    // Send the approval prompt via the channel
                    // Look up the thread's origin_message_ref for reply-to
                    let reply_to = sqlx::query(
                        "SELECT origin_message_ref, last_reply_guid FROM threads WHERE id = ?",
                    )
                    .bind(chan_thread)
                    .fetch_optional(&state.db)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|r| {
                        let last_reply: Option<String> =
                            r.try_get("last_reply_guid").ok().flatten();
                        let origin: Option<String> = r.try_get("origin_message_ref").ok().flatten();
                        last_reply.or(origin)
                    });

                    match state
                        .channel_registry
                        .send(chan_pipe, chan_thread, &prompt_text, reply_to.as_deref())
                        .await
                    {
                        Ok(sent_guid) => {
                            // Update thread's last_reply_guid so the user's
                            // inline reply routes back to this thread
                            if let Some(ref guid) = sent_guid {
                                let _ = thread_mgr::update_reply_guid(
                                    &state.db,
                                    chan_thread,
                                    chan_pipe,
                                    guid,
                                )
                                .await;
                            }
                        }
                        Err(e) => {
                            warn!(
                                pipe_id = %chan_pipe,
                                "Failed to send approval prompt via channel: {e}"
                            );
                            // Fall back to blocking the tool
                            return Ok(ToolDispatchResult::Blocked(format!(
                                "Could not send approval prompt to user: {}. \
                                 The tool call was not executed.",
                                e
                            )));
                        }
                    }

                    // Store the oneshot in AppState for the interception handler
                    let (otx, orx) = tokio::sync::oneshot::channel::<bool>();
                    {
                        let mut pending = state.pending_approvals.lock().await;
                        pending.insert(approval_id.clone(), otx);
                    }

                    Ok(ToolDispatchResult::Queued {
                        approval_id,
                        receiver: orx,
                        timeout: std::time::Duration::from_secs(
                            approval_queue::APPROVAL_TIMEOUT_SECS as u64,
                        ),
                    })
                }

                ChannelKind::NonInteractive => {
                    // No user to ask — treat as block
                    Ok(ToolDispatchResult::Blocked(format!(
                        "The tool '{}' requires user approval but this is a non-interactive \
                         channel. The tool call was not executed. Do NOT retry.",
                        namespaced_name
                    )))
                }
            }
        }
    }
}

/// Build a human-readable approval prompt for non-interactive channels.
/// Uses the server-side i18n module to translate the prompt into the
/// user's preferred language.
fn build_approval_prompt(lang: &str, tool_name: &str, args: &serde_json::Value) -> String {
    let display_name = tool_name.split("__").last().unwrap_or(tool_name);
    let tool_label = humanize_identifier(display_name);
    let user_message = extract_approval_message(args);
    let public_args = strip_internal_tool_args(args);

    let mut prompt = String::new();
    prompt.push_str(crate::i18n::t(lang, "approval.confirm_header"));
    prompt.push('\n');

    if let Some(message) = user_message {
        prompt.push_str(&message);
    } else {
        prompt.push_str(&crate::i18n::t_with_tool(
            lang,
            "approval.action",
            &tool_label,
        ));
    }

    // Add key argument values for context
    if let Some(obj) = public_args.as_object() {
        let relevant: Vec<String> = obj
            .iter()
            .filter(|(_, v)| !v.is_null())
            .take(3) // limit to avoid massive prompts
            .map(|(k, v)| {
                let key = humanize_identifier(k);
                let value = format_approval_value(v);
                format!("{key}: {value}")
            })
            .collect();

        if !relevant.is_empty() {
            prompt.push('\n');
            prompt.push_str(crate::i18n::t(lang, "approval.details"));
            prompt.push('\n');
            for item in &relevant {
                prompt.push_str("- ");
                prompt.push_str(item);
                prompt.push('\n');
            }
        }
    }

    prompt.push('\n');
    prompt.push_str(crate::i18n::t(lang, "approval.reply_to_approve"));
    prompt
}

const INTERNAL_APPROVAL_MESSAGE_KEY: &str = "_approval_message";

fn strip_internal_tool_args(args: &serde_json::Value) -> serde_json::Value {
    let mut sanitized = args.clone();
    if let Some(obj) = sanitized.as_object_mut() {
        obj.remove(INTERNAL_APPROVAL_MESSAGE_KEY);
    }
    sanitized
}

fn extract_approval_message(args: &serde_json::Value) -> Option<String> {
    let raw = args
        .as_object()
        .and_then(|obj| obj.get(INTERNAL_APPROVAL_MESSAGE_KEY))
        .and_then(|v| v.as_str())?;

    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_for_approval(trimmed))
}

fn format_approval_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => truncate_for_approval(s),
        serde_json::Value::Array(items) => {
            let all_strings: Option<Vec<&str>> = items.iter().map(|v| v.as_str()).collect();
            if let Some(values) = all_strings {
                truncate_for_approval(&values.join(", "))
            } else {
                truncate_for_approval(&value.to_string())
            }
        }
        _ => truncate_for_approval(&value.to_string()),
    }
}

fn truncate_for_approval(text: &str) -> String {
    if text.len() > 80 {
        format!("{}...", &text[..77])
    } else {
        text.to_string()
    }
}

fn humanize_identifier(identifier: &str) -> String {
    let mut out = String::new();
    let mut prev_was_lower_or_digit = false;

    for ch in identifier.chars() {
        if ch == '_' || ch == '-' {
            if !out.ends_with(' ') {
                out.push(' ');
            }
            prev_was_lower_or_digit = false;
            continue;
        }

        if ch.is_ascii_uppercase() && prev_was_lower_or_digit && !out.ends_with(' ') {
            out.push(' ');
        }

        out.push(ch.to_ascii_lowercase());
        prev_was_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }

    let normalized = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return identifier.to_string();
    }

    let mut chars = normalized.chars();
    let first = chars
        .next()
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or_default();
    format!("{}{}", first, chars.collect::<String>())
}

/// Generate a short title for a thread by asking the LLM.
///
/// Called as a background task after the first assistant reply in a thread.
/// Fails silently — title generation is cosmetic and non-essential.
async fn generate_thread_title(
    db: &sqlx::SqlitePool,
    creds: &crate::permissions::store::CredentialStore,
    thread_id: &str,
    first_message: &str,
    agent_id: &str,
) -> Result<()> {
    let resolved = crate::agents::model::resolve_model(db, agent_id).await?;
    let provider = load_provider(db, &resolved.provider_id, &resolved.model_id, creds).await?;

    let messages = vec![
        ChatMessage::system(
            "Generate a very short title (3-5 words) for a conversation that starts with the following message. \
             Return ONLY the title, nothing else. No quotes, no punctuation at the end.",
        ),
        ChatMessage::user(first_message),
    ];

    let response = provider.chat(&messages, &[], 0.3).await?;

    let title = match response {
        LlmResponse::Text(t) => t.trim().trim_matches('"').trim_matches('\'').to_string(),
        _ => return Ok(()), // Unexpected tool call response, skip
    };

    if !title.is_empty() && title.len() < 100 {
        thread_mgr::set_title(db, thread_id, &title).await?;
        debug!(thread_id = %thread_id, title = %title, "Generated thread title");
    }

    Ok(())
}

// ── Agent routing partition ───────────────────────────────────────────────────

/// Partition installed agents into **inlined** and **delegated** sets relative
/// to the current agent.
///
/// - **Inlined**: The agent's capability manifests are merged into the current
///   agent's tool list. The LLM can call the tools directly — no delegation
///   round-trip.
/// - **Delegated**: The agent appears in the `delegate_to_agent` registry and
///   gets its own LLM context when invoked.
///
/// The current agent itself is excluded from both sets (it can't delegate to
/// or inline itself).
fn partition_agents(
    current_agent: &AgentDefinition,
    all_agents: &HashMap<String, Arc<AgentDefinition>>,
) -> (
    HashMap<String, CapabilityManifest>,
    HashMap<String, Arc<AgentDefinition>>,
    HashMap<String, String>,
) {
    let mut inlined_manifests = HashMap::new();
    let mut delegated_agents = HashMap::new();
    // Map from capability_id -> source agent_id so that inlined tools
    // resolve policies against the agent that owns them, not the host.
    let mut cap_owner: HashMap<String, String> = HashMap::new();

    for (id, agent) in all_agents {
        if id == &current_agent.id {
            continue;
        }
        if agent.effective_routing() == RoutingMode::Inline {
            // Merge this agent's capability manifests into the inlined set
            for (cap_id, manifest) in &agent.capability_manifests {
                inlined_manifests.insert(cap_id.clone(), manifest.clone());
                cap_owner.insert(cap_id.clone(), id.clone());
            }
        } else {
            delegated_agents.insert(id.clone(), agent.clone());
        }
    }

    (inlined_manifests, delegated_agents, cap_owner)
}

/// Dispatch a `delegate_to_agent` tool call by running a nested agent turn.
///
/// The delegated turn uses a **confirmation-only sender** — streamed text
/// tokens are discarded to avoid the duplicate-response problem (where the
/// user would see both the delegated agent's raw stream AND the
/// orchestrator's summary).  However, `ConfirmationRequired` events are
/// forwarded to the parent's real SSE channel so that interactive Ask-policy
/// approvals still work.
///
/// Returns a `BoxFuture` (rather than being `async fn`) so that the recursive
/// `run_turn` call satisfies the `Send` bound required by the tool dispatcher.
fn dispatch_delegation(
    state: AppState,
    pipe_id: String,
    user_id: String,
    args: serde_json::Value,
    chain_depth: i32,
    channel_kind: ChannelKind,
    parent_tx: LoopSender,
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
            channel_kind,
            skip_user_persist: false,
        };

        // Create a filtered sender that forwards only confirmation events
        // (needed for interactive Ask-policy approval) while discarding
        // text tokens and Done signals (which would cause duplicate output).
        let filtered_tx = confirmation_only_sender(parent_tx);
        let reply = run_turn(&state, params, filtered_tx).await?;

        Ok(reply)
    }
    .boxed()
}

/// Create a sender that forwards only `ConfirmationRequired` and
/// `ApprovalQueued` events to the parent, discarding tokens and other
/// streaming events.  This lets delegated agents use Ask-policy tools
/// without causing duplicate streamed output.
fn confirmation_only_sender(parent_tx: LoopSender) -> LoopSender {
    let (tx, mut rx) = mpsc::channel::<LoopEvent>(32);

    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                // Forward confirmation/approval events — these are essential
                // for the interactive Ask flow to work.
                LoopEvent::ConfirmationRequired(_) | LoopEvent::ApprovalQueued { .. } => {
                    let _ = parent_tx.send(event).await;
                }
                // Forward capability status so the user sees "Using pim..."
                LoopEvent::CapabilityStatus(_) => {
                    let _ = parent_tx.send(event).await;
                }
                // Discard tokens and Done to prevent duplicate streaming.
                LoopEvent::Token(_) | LoopEvent::Done | LoopEvent::Error(_) => {}
            }
        }
    });

    tx
}

/// Create a throwaway channel for callers that don't stream tokens.
pub fn noop_sender() -> LoopSender {
    let (tx, _) = mpsc::channel::<LoopEvent>(1);
    tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn humanize_identifier_splits_camel_case() {
        assert_eq!(
            humanize_identifier("deleteMultipleItemsFromList"),
            "Delete multiple items from list"
        );
        assert_eq!(humanize_identifier("list_uuid"), "List uuid");
    }

    #[test]
    fn approval_prompt_uses_friendly_text_and_values() {
        let args = json!({
            "itemNames": ["Spulmaschinensalz"],
            "listUuid": "d6018602-2aa7-4f26-bbb6-c25065b86ca0"
        });
        let prompt = build_approval_prompt("de", "deleteMultipleItemsFromList", &args);

        assert!(prompt.contains("Bevor ich weitermache"));
        assert!(prompt.contains("Aktion: Delete multiple items from list"));
        assert!(prompt.contains("Item names: Spulmaschinensalz"));
        assert!(prompt.contains("List uuid: d6018602-2aa7-4f26-bbb6-c25065b86ca0"));
    }

    #[test]
    fn approval_prompt_prefers_llm_message_and_hides_internal_field() {
        let args = json!({
            "_approval_message": "Ich moechte Spuelmaschinensalz von deiner Liste loeschen.",
            "itemNames": ["Spulmaschinensalz"],
            "listUuid": "d6018602-2aa7-4f26-bbb6-c25065b86ca0"
        });
        let prompt = build_approval_prompt("de", "deleteMultipleItemsFromList", &args);

        assert!(prompt.contains("Ich moechte Spuelmaschinensalz von deiner Liste loeschen."));
        assert!(!prompt.contains("Aktion:"));
        assert!(!prompt.contains("_approval_message"));
        assert!(prompt.contains("Item names: Spulmaschinensalz"));
    }

    #[test]
    fn strip_internal_tool_args_removes_approval_message() {
        let args = json!({
            "_approval_message": "friendly",
            "itemNames": ["A"]
        });
        let stripped = strip_internal_tool_args(&args);

        assert!(stripped.get("_approval_message").is_none());
        assert_eq!(stripped.get("itemNames").unwrap(), &json!(["A"]));
    }
}
