/// Shared agent invocation engine.
///
/// Centralises the session -> context -> LLM -> persist flow so that
/// inbound webhooks, web-chat, and event-fanout reactions all behave
/// identically without duplicating code.
use anyhow::Result;
use base64::Engine;
use futures::FutureExt;
use futures::future::BoxFuture;
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Duration, MissedTickBehavior};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use selu_core::types::Role;

use crate::agents::loader::{AgentDefinition, RoutingMode};
use crate::agents::{
    context, delegation, localization, memory, router as agent_router, session as session_mgr,
    storage, thread as thread_mgr,
};
use crate::capabilities::build_tool_specs;
use crate::capabilities::discovery::{hydrate_dynamic_tools_for_agent, load_discovered_tools};
use crate::capabilities::manifest::CapabilityManifest;
use crate::llm::image_normalizer::normalize_messages_for_provider;
use crate::llm::provider::{ChatMessage, ContentPart, LlmResponse, MessageContent};
use crate::llm::registry::load_provider;
use crate::llm::tool_loop::{LoopEvent, LoopOutput, LoopSender, ToolDispatchResult, run_loop};
use crate::permissions::approval_queue;
use crate::permissions::tool_policy::{
    self, BUILTIN_CAPABILITY_ID, BUILTIN_DELEGATE, BUILTIN_IMAGE_EDIT, BUILTIN_IMAGE_GENERATE,
    BUILTIN_MEMORY_FORGET, BUILTIN_MEMORY_LIST, BUILTIN_MEMORY_REMEMBER, BUILTIN_MEMORY_SEARCH,
    BUILTIN_SET_REMINDER, BUILTIN_SET_SCHEDULE, BUILTIN_STORE_DELETE, BUILTIN_STORE_GET,
    BUILTIN_STORE_LIST, BUILTIN_STORE_SET, BUILTIN_SUBMIT_FEEDBACK, ToolPolicy,
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
    /// Enable token streaming for this turn.
    /// Selu keeps streaming enabled for all turns to minimize tool-call latency.
    /// Non-interactive callers can pass `noop_sender()` to ignore stream events.
    pub enable_streaming: bool,
    /// Optional normalized inbound attachments to attach to this user turn.
    pub inbound_attachments: Vec<InboundAttachmentInput>,
    /// Delegation ancestry (agent IDs) carried across nested delegated turns.
    /// Used for generic delegation-cycle prevention.
    pub delegation_trace: Vec<String>,
    /// Optional pre-formatted location context string injected into the system
    /// prompt so the LLM knows the user's current location (mobile only).
    pub location_context: Option<String>,
}

pub struct TurnOutput {
    pub reply_text: String,
    pub attachments: Vec<crate::agents::artifacts::ArtifactRef>,
}

pub struct InboundAttachmentInput {
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

/// Run one agent turn end-to-end:
///   1. Route to the correct agent
///   2. Open / resume a session
///   3. Persist the user message
///   4. Build context window + tool specs
///   5. Run the tool loop (capability dispatch + built-ins + `delegate_to_agent`)
///   6. Persist the assistant reply
///   7. Extract personality facts (background)
///
/// Streaming tokens are sent to `tx`. Pass a throwaway channel for
/// non-streaming callers (inbound, fanout).
///
/// Returns the assistant reply text.
pub async fn run_turn(state: &AppState, params: TurnParams, tx: LoopSender) -> Result<TurnOutput> {
    let turn_start = Instant::now();
    let turn_id = Uuid::new_v4().to_string();
    let TurnParams {
        pipe_id,
        user_id,
        agent_id,
        message,
        thread_id,
        chain_depth,
        channel_kind,
        skip_user_persist,
        enable_streaming,
        inbound_attachments,
        delegation_trace,
        location_context,
    } = params;
    // Only treat the message as an explicit confirmation when it comes from the
    // actual user (chain_depth == 0).  Delegation messages are LLM-generated
    // instructions — they often contain words like "send" + "pdf" that would
    // false-positive as user confirmations, bypassing ask-policy tools.
    let explicit_turn_confirmation = chain_depth == 0 && is_explicit_confirmation_message(&message);
    info!(
        turn_id = %turn_id,
        pipe_id = %pipe_id,
        user_id = %user_id,
        requested_agent_id = ?agent_id,
        has_thread_id = thread_id.is_some(),
        chain_depth,
        streaming = enable_streaming,
        explicit_turn_confirmation,
        delegation_trace_len = delegation_trace.len(),
        "Turn started"
    );

    // ── Route to agent ────────────────────────────────────────────────────────
    let route_start = Instant::now();
    let agents_snapshot = state.agents.load();
    let user_agents =
        crate::agents::access::visible_agents(&state.db, &user_id, &agents_snapshot).await;
    let (resolved_agent_id, effective_text) = {
        if let Some(id) = agent_id {
            (id, message.clone())
        } else {
            agent_router::route(&message, None, &user_agents)
        }
    };
    let route_ms = route_start.elapsed().as_millis();

    let agent = match user_agents
        .get(&resolved_agent_id)
        .or_else(|| user_agents.get("default"))
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
    let mut current_delegation_trace = delegation_trace;
    if current_delegation_trace.last().map(|id| id.as_str()) != Some(agent.id.as_str()) {
        current_delegation_trace.push(agent.id.clone());
    }
    // Drop the full snapshot early — we only need `agent` from here on.
    // Keep it alive only for delegation check below.
    debug!(
        agent_id = %agent.id,
        route_ms,
        "Agent routing complete"
    );

    // ── Session + Persist user message (parallelized) ─────────────────────────
    let db_start = Instant::now();

    // Open session and persist user message in parallel.
    // Both are independent operations that hit the DB.
    let session_fut = session_mgr::open_session(
        &state.db,
        &pipe_id,
        &user_id,
        &agent.id,
        thread_id.as_deref(),
    );

    let session = session_fut.await?;
    let session_id = session.id.to_string();
    let before_artifacts = crate::agents::artifacts::list_session_artifacts(
        &state.artifacts,
        &user_id,
        &session_id,
        256,
    )
    .await;
    let before_ids: HashSet<String> = before_artifacts
        .into_iter()
        .map(|a| a.artifact_id)
        .collect();
    let session_open_ms = db_start.elapsed().as_millis();
    debug!(session_ms = session_open_ms, "Session opened");

    // Persist normalized inbound attachments as session artifacts first,
    // then persist the user message with stable artifact references.
    let mut inbound_attachment_refs = Vec::new();
    for attachment in inbound_attachments {
        match crate::agents::artifacts::store_inbound_attachment(
            &state.artifacts,
            &user_id,
            &session_id,
            &attachment.filename,
            &attachment.mime_type,
            attachment.data,
        )
        .await
        {
            Ok(stored) => inbound_attachment_refs.push(stored),
            Err(e) => warn!("Failed to store inbound attachment: {e}"),
        }
    }
    let inbound_attachment_ids: HashSet<String> = inbound_attachment_refs
        .iter()
        .map(|r| r.artifact_id.clone())
        .collect();
    let persist_thread_artifacts = if let Some(tid) = thread_id.as_deref() {
        if is_web_pipe_transport(&state.db, &pipe_id, &user_id).await {
            crate::agents::artifacts::persist_refs_for_thread(
                &state.db,
                &state.artifacts,
                &state.config.database.url,
                tid,
                &user_id,
                &inbound_attachment_refs,
            )
            .await;
            true
        } else {
            false
        }
    } else {
        false
    };
    let effective_user_text =
        normalize_user_text_for_inbound_attachments(&effective_text, &inbound_attachment_refs);
    // Build multimodal parts AFTER normalizing the text so the artifact_id
    // metadata is included in the text content part.  Without this the LLM
    // sees the image but has no artifact_id to pass to image_edit.
    let multimodal_user_parts = build_multimodal_user_parts(
        &state.artifacts,
        &user_id,
        &effective_user_text,
        &inbound_attachment_refs,
    )
    .await;
    let user_language = crate::i18n::user_language(&state.db, &user_id).await;

    // Persist user message (fire-and-forget timing)
    // Skip for schedule-triggered turns — the prompt should not show as a
    // user bubble; the executor persists a "status" note instead.
    let user_persist_start = Instant::now();
    if !skip_user_persist {
        let msg_id = Uuid::new_v4().to_string();
        let user_role = Role::User.to_string();
        let now_ms = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3f")
            .to_string();
        if let Err(e) = sqlx::query!(
            "INSERT INTO messages (id, pipe_id, session_id, thread_id, role, content, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            msg_id, pipe_id, session_id, thread_id, user_role, effective_user_text, now_ms
        )
        .execute(&state.db)
        .await
        {
            error!("Failed to persist user message: {e}");
        }
    }
    let user_persist_ms = user_persist_start.elapsed().as_millis();

    // ── Partition agents into inlined vs delegated ──────────────────────────────
    // Inlined agents' tools are exposed directly on this agent's tool list,
    // eliminating the delegation round-trip. Delegated agents keep the full
    // delegate_to_agent flow with their own LLM context.
    let (inlined_manifests, delegated_agents, cap_owner) = partition_agents(&agent, &user_agents);

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

    let runtime_settings = crate::agents::runtime_limits::resolve_effective_runtime_settings(
        &state.db, &user_id, &agent.id,
    )
    .await
    .unwrap_or_else(
        |_| crate::agents::runtime_limits::EffectiveRuntimeSettings {
            autonomy_level: crate::agents::runtime_limits::AutonomyLevel::Medium,
            use_advanced_limits: false,
            max_tool_loop_iterations: crate::agents::runtime_limits::limits_for_autonomy(
                crate::agents::runtime_limits::AutonomyLevel::Medium,
            )
            .max_tool_loop_iterations,
            max_delegation_hops: crate::agents::runtime_limits::limits_for_autonomy(
                crate::agents::runtime_limits::AutonomyLevel::Medium,
            )
            .max_delegation_hops,
            agent_default_level: crate::agents::runtime_limits::AutonomyLevel::Medium,
            agent_default_limits: crate::agents::runtime_limits::limits_for_autonomy(
                crate::agents::runtime_limits::AutonomyLevel::Medium,
            ),
        },
    );
    let max_tool_loop_iterations = runtime_settings.max_tool_loop_iterations;
    let max_delegation_hops = runtime_settings.max_delegation_hops;

    let context_fut = async {
        let start = Instant::now();
        let result = context::build(
            &state.db,
            &agent,
            &pipe_id,
            &session_id,
            thread_id.as_deref(),
            &user_id,
            &user_language,
            &effective_user_text,
            &inlined_manifests,
            delegated_ref,
            location_context.as_deref(),
        )
        .await;
        (start.elapsed().as_millis(), result)
    };

    let provider_fut = async {
        let start = Instant::now();
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
        Ok::<_, anyhow::Error>((start.elapsed().as_millis(), provider, resolved.temperature))
    };

    // Run context build and provider load in parallel
    let (context_result, provider_result) = tokio::join!(context_fut, provider_fut);

    let (context_ms, mut messages) = {
        let (ms, result) = context_result;
        (ms, result?)
    };
    let (provider_ms, provider, temperature) = provider_result?;
    if let Some(parts) = multimodal_user_parts {
        inject_multimodal_user_message(&mut messages, &effective_user_text, parts);
    }
    // For delegated turns the user message was not persisted (skip_user_persist),
    // so context::build won't find it in history.  Append it now so the LLM sees
    // the delegation request and the conversation ends with a user message (required
    // by Bedrock / Converse API).
    if skip_user_persist && !effective_user_text.is_empty() {
        messages.push(ChatMessage::user(&effective_user_text));
    }
    normalize_messages_for_provider(&mut messages, provider.as_ref());
    crate::llm::context_budget::trim_to_budget(&mut messages, provider.as_ref());
    let prep_ms = prep_start.elapsed().as_millis();

    if context_ms > 5_000 {
        warn!(context_ms, "Slow context build");
    }
    if provider_ms > 5_000 {
        warn!(provider_ms, "Slow provider load");
    }
    debug!(
        prep_ms,
        context_ms,
        provider_ms,
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
    tool_specs.push(storage::store_get_tool_spec());
    tool_specs.push(storage::store_set_tool_spec());
    tool_specs.push(storage::store_delete_tool_spec());
    tool_specs.push(storage::store_list_tool_spec());
    tool_specs.push(memory::remember_tool_spec());
    tool_specs.push(memory::forget_tool_spec());
    tool_specs.push(memory::search_tool_spec());
    tool_specs.push(memory::list_tool_spec());
    tool_specs.push(crate::schedules::set_schedule_tool_spec());
    tool_specs.push(crate::schedules::set_reminder_tool_spec());
    tool_specs.push(crate::web::feedback::submit_feedback_tool_spec());

    // Image tools: only exposed when the agent has an image model configured.
    let image_model_for_tools =
        crate::agents::model::resolve_image_model(&state.db, &agent.id).await;
    if matches!(image_model_for_tools, Ok(Some(_))) {
        tool_specs.push(crate::agents::image_tools::generate_tool_spec());
        tool_specs.push(crate::agents::image_tools::edit_tool_spec());
    }

    // Delegation follows an orchestrator/worker model:
    // only the default orchestrator on a root turn gets the delegation tool.
    let is_root_orchestrator_turn = agent.id == "default" && current_delegation_trace.len() <= 1;
    if is_root_orchestrator_turn
        && chain_depth < max_delegation_hops
        && !delegated_agents.is_empty()
    {
        tool_specs.push(delegation::tool_spec(&agent.id, &delegated_agents));
    }
    let tool_names: Vec<String> = tool_specs.iter().map(|t| t.name.clone()).collect();
    info!(
        agent_id = %agent.id,
        tool_count = tool_names.len(),
        tool_names = ?tool_names,
        "Agent tools available for turn"
    );

    // ── Dispatcher ────────────────────────────────────────────────────────────
    let cap_engine = state.capabilities.clone();
    let cred_store = state.credentials.clone();
    // Merge the agent's own capability manifests with inlined manifests so the
    // dispatcher can resolve and invoke tools from both sources.
    let mut cap_manifests = own_manifests;
    cap_manifests.extend(hydrated_inlined);
    let sid = session_id.clone();
    let parent_thread_id = thread_id.clone();
    let tid = thread_id.clone().unwrap_or_default();
    let uid = user_id.clone();
    let src_agent_id = agent.id.clone();
    let known_agents = user_agents.clone();
    let cap_owner_map = cap_owner;
    let status_agents = user_agents.clone();
    let status_cap_owner = cap_owner_map.clone();
    let status_agent = agent.clone();
    let dispatcher_agent = agent.clone();
    let delegation_state = state.clone();
    let delegation_pipe = pipe_id.clone();
    let dispatcher_state = state.clone();
    let dispatcher_channel = channel_kind.clone();
    let dispatcher_pipe = pipe_id.clone();
    let dispatcher_tx = tx.clone();
    let user_language_pref = user_language.clone();
    let user_language_for_status = user_language.clone();
    let delegation_trace_for_dispatch = current_delegation_trace.clone();
    let delegated_attachments = Arc::new(Mutex::new(
        Vec::<crate::agents::artifacts::ArtifactRef>::new(),
    ));
    let delegated_attachments_for_dispatch = delegated_attachments.clone();

    debug!(
        setup_ms = turn_start.elapsed().as_millis(),
        "Turn setup complete, entering tool loop"
    );

    let tool_loop_start = Instant::now();
    let loop_output = run_loop(
        provider,
        messages,
        tool_specs,
        temperature,
        max_tool_loop_iterations,
        tx.clone(),
        enable_streaming,
        user_language.clone(),
        move |tool_name| {
            // Delegation emits its own "Asking <agent>..." status in
            // dispatch_delegation, so return empty to suppress the generic one.
            if tool_name == delegation::TOOL_NAME {
                return String::new();
            }
            if let Some((cap_id, bare_tool)) = tool_name.split_once("__") {
                let owner_agent = status_cap_owner
                    .get(cap_id)
                    .and_then(|owner_id| status_agents.get(owner_id))
                    .map(|owner| owner.as_ref())
                    .unwrap_or(&status_agent);
                return localization::localized_tool_display_name(
                    owner_agent,
                    cap_id,
                    bare_tool,
                    &humanize_identifier(bare_tool),
                    &user_language_for_status,
                );
            }
            humanize_identifier(tool_name)
        },
        move |name, args, approved| {
            let engine = cap_engine.clone();
            let store = cred_store.clone();
            let manifests = cap_manifests.clone();
            let session = sid.clone();
            let thread = tid.clone();
            let user = uid.clone();
            let agent_id_copy = src_agent_id.clone();
            let current_agent = dispatcher_agent.clone();
            let agent_registry = known_agents.clone();
            let owners = cap_owner_map.clone();
            let del_state = delegation_state.clone();
            let del_pipe = delegation_pipe.clone();
            let state = dispatcher_state.clone();
            let channel = dispatcher_channel.clone();
            let pipe = dispatcher_pipe.clone();
            let del_tx = dispatcher_tx.clone();
            let parent_thread_id = parent_thread_id.clone();
            let preferred_language = user_language_pref.clone();
            let delegated_attachments = delegated_attachments_for_dispatch.clone();
            let user_confirmed_this_turn = explicit_turn_confirmation;
            let delegation_trace_for_call = delegation_trace_for_dispatch.clone();

            Box::pin(async move {
                let invoke_args = strip_internal_tool_args(&args);

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
                        &current_agent,
                        &preferred_language,
                        BUILTIN_CAPABILITY_ID,
                        builtin_store_name,
                        &name,
                        &args,
                        &channel,
                        &session,
                        &pipe,
                        &agent_id_copy,
                        approved,
                        user_confirmed_this_turn,
                        false,
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

                // ── Built-in tools: unified BM25 memory (user-scoped) ────
                if let Some(builtin_memory_name) = match name.as_str() {
                    "memory_remember" => Some(BUILTIN_MEMORY_REMEMBER),
                    "memory_forget" => Some(BUILTIN_MEMORY_FORGET),
                    "memory_search" => Some(BUILTIN_MEMORY_SEARCH),
                    "memory_list" => Some(BUILTIN_MEMORY_LIST),
                    _ => None,
                } {
                    let result = check_policy_and_dispatch(
                        &state,
                        &user,
                        &current_agent,
                        &preferred_language,
                        BUILTIN_CAPABILITY_ID,
                        builtin_memory_name,
                        &name,
                        &args,
                        &channel,
                        &session,
                        &pipe,
                        &agent_id_copy,
                        approved,
                        user_confirmed_this_turn,
                        false,
                        || {
                            let db = state.db.clone();
                            let agent_id = agent_id_copy.clone();
                            let user = user.clone();
                            let args = invoke_args.clone();
                            let tool = name.clone();
                            Box::pin(async move {
                                match tool.as_str() {
                                    "memory_remember" => {
                                        memory::dispatch_remember(&db, &agent_id, &user, &args)
                                            .await
                                    }
                                    "memory_forget" => {
                                        memory::dispatch_forget(&db, &user, &args).await
                                    }
                                    "memory_search" => {
                                        memory::dispatch_search(&db, &user, &args).await
                                    }
                                    "memory_list" => {
                                        memory::dispatch_list(&db, &user, &args).await
                                    }
                                    _ => unreachable!(),
                                }
                            })
                        },
                    )
                    .await;
                    return result;
                }

                // ── Built-in tool: set_schedule ─────────────────────
                if name == "set_schedule" {
                    let result = check_policy_and_dispatch(
                        &state,
                        &user,
                        &current_agent,
                        &preferred_language,
                        BUILTIN_CAPABILITY_ID,
                        BUILTIN_SET_SCHEDULE,
                        &name,
                        &args,
                        &channel,
                        &session,
                        &pipe,
                        &agent_id_copy,
                        approved,
                        user_confirmed_this_turn,
                        false,
                        || {
                            let db = state.db.clone();
                            let user = user.clone();
                            let pipe = pipe.clone();
                            let agent_id = agent_id_copy.clone();
                            let args = invoke_args.clone();
                            Box::pin(async move {
                                crate::schedules::dispatch_set_schedule(
                                    &db, &user, &pipe, &agent_id, &args,
                                )
                                .await
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
                        &current_agent,
                        &preferred_language,
                        BUILTIN_CAPABILITY_ID,
                        BUILTIN_SET_REMINDER,
                        &name,
                        &args,
                        &channel,
                        &session,
                        &pipe,
                        &agent_id_copy,
                        approved,
                        user_confirmed_this_turn,
                        false,
                        || {
                            let db = state.db.clone();
                            let user = user.clone();
                            let pipe = pipe.clone();
                            let agent_id = agent_id_copy.clone();
                            let args = invoke_args.clone();
                            Box::pin(async move {
                                crate::schedules::dispatch_set_reminder(
                                    &db, &user, &pipe, &agent_id, &args,
                                )
                                .await
                            })
                        },
                    )
                    .await;
                    return result;
                }

                // ── Built-in tool: submit_feedback ───────────────────
                if name == "submit_feedback" {
                    let result = check_policy_and_dispatch(
                        &state,
                        &user,
                        &current_agent,
                        &preferred_language,
                        BUILTIN_CAPABILITY_ID,
                        BUILTIN_SUBMIT_FEEDBACK,
                        &name,
                        &args,
                        &channel,
                        &session,
                        &pipe,
                        &agent_id_copy,
                        approved,
                        user_confirmed_this_turn,
                        false,
                        || {
                            let marketplace_url = state.config.marketplace_url.clone();
                            let db = state.db.clone();
                            let args = invoke_args.clone();
                            Box::pin(async move {
                                let args_str = args.to_string();
                                crate::web::feedback::dispatch_submit_feedback(
                                    &marketplace_url,
                                    &db,
                                    &args_str,
                                )
                                .await
                            })
                        },
                    )
                    .await;
                    return result;
                }

                // ── Built-in tools: image generation / editing ─────────
                if let Some(builtin_image_name) = match name.as_str() {
                    "image_generate" => Some(BUILTIN_IMAGE_GENERATE),
                    "image_edit" => Some(BUILTIN_IMAGE_EDIT),
                    _ => None,
                } {
                    let result = check_policy_and_dispatch(
                        &state,
                        &user,
                        &current_agent,
                        &preferred_language,
                        BUILTIN_CAPABILITY_ID,
                        builtin_image_name,
                        &name,
                        &args,
                        &channel,
                        &session,
                        &pipe,
                        &agent_id_copy,
                        approved,
                        user_confirmed_this_turn,
                        false,
                        || {
                            let db = state.db.clone();
                            let credentials = state.credentials.clone();
                            let artifact_store = state.artifacts.clone();
                            let agent_id = agent_id_copy.clone();
                            let user = user.clone();
                            let session = session.clone();
                            let args = invoke_args.clone();
                            let tool = name.clone();
                            Box::pin(async move {
                                match tool.as_str() {
                                    "image_generate" => {
                                        crate::agents::image_tools::dispatch_generate(
                                            &db,
                                            &credentials,
                                            &artifact_store,
                                            &agent_id,
                                            &user,
                                            &session,
                                            &args,
                                        )
                                        .await
                                    }
                                    "image_edit" => {
                                        crate::agents::image_tools::dispatch_edit(
                                            &db,
                                            &credentials,
                                            &artifact_store,
                                            &agent_id,
                                            &user,
                                            &session,
                                            &args,
                                        )
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

                if name == delegation::TOOL_NAME {
                    // Check policy for built-in delegate_to_agent
                    let result = check_policy_and_dispatch(
                        &state,
                        &user,
                        &current_agent,
                        &preferred_language,
                        BUILTIN_CAPABILITY_ID,
                        BUILTIN_DELEGATE,
                        &name,
                        &args,
                        &channel,
                        &session,
                        &pipe,
                        &agent_id_copy,
                        approved,
                        user_confirmed_this_turn,
                        false,
                        || {
                            let del_state = del_state.clone();
                            let del_pipe = del_pipe.clone();
                            let source_agent_id = agent_id_copy.clone();
                            let user = user.clone();
                            let args = invoke_args.clone();
                            let session = session.clone();
                            let chain_depth = chain_depth;
                            let del_channel = channel.clone();
                            let del_tx = del_tx.clone();
                            let parent_thread_id = parent_thread_id.clone();
                            let delegated_attachments = delegated_attachments.clone();
                            let delegation_trace = delegation_trace_for_call.clone();
                            Box::pin(async move {
                                let output = dispatch_delegation(
                                    del_state,
                                    source_agent_id,
                                    del_pipe,
                                    user,
                                    args,
                                    session,
                                    chain_depth,
                                    del_channel,
                                    del_tx,
                                    parent_thread_id,
                                    delegation_trace,
                                )
                                .await?;
                                if !output.attachments.is_empty() {
                                    let mut captured = delegated_attachments.lock().await;
                                    captured.extend(output.attachments.clone());
                                }
                                // Append artifact references to the tool result so
                                // the orchestrator LLM can pass them to the next
                                // delegation step (e.g. email delivery).
                                let mut result_text = output.reply_text;
                                if !output.attachments.is_empty() {
                                    result_text.push_str("\n\n[Artifacts returned by this agent:");
                                    for att in &output.attachments {
                                        result_text.push_str(&format!(
                                            "\n- artifact_id: {} | filename: {} | mime_type: {} | size: {} bytes",
                                            att.artifact_id, att.filename, att.mime_type, att.size_bytes
                                        ));
                                    }
                                    result_text.push(']');
                                }
                                Ok(result_text)
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
                // Approval labels/messages should also come from the owning
                // agent's locale bundle when a capability is inlined.
                let localization_agent = owners
                    .get(&cap_id)
                    .and_then(|owner_id| agent_registry.get(owner_id))
                    .map(|owner| owner.as_ref())
                    .unwrap_or(&current_agent);

                let result = check_policy_and_dispatch(
                    &state,
                    &user,
                    localization_agent,
                    &preferred_language,
                    &cap_id,
                    &bare_tool,
                    &name,
                    &args,
                    &channel,
                    &session,
                    &pipe,
                    policy_agent,
                    approved,
                    user_confirmed_this_turn,
                    false,
                    || {
                        let engine = engine.clone();
                        let manifests = manifests.clone();
                        let name = name.clone();
                        let args = invoke_args.clone();
                        let session = session.clone();
                        let thread = thread.clone();
                        let user = user.clone();
                        let store = store.clone();
                        let db = state.db.clone();
                        let artifacts = state.artifacts.clone();
                        let cap_id_for_touch = cap_id.clone();
                        let policy_agent_id = policy_agent.clone();
                        Box::pin(async move {
                            if let Some(manifest) = manifests.get(&cap_id_for_touch) {
                                if let Err(e) = crate::capabilities::touch_workspace_activity(
                                    &db, manifest, &session,
                                )
                                .await
                                {
                                    warn!(
                                        capability_id = %cap_id_for_touch,
                                        session_id = %session,
                                        "Failed to touch workspace activity: {e}"
                                    );
                                }
                            }

                            let prepared_args =
                                crate::agents::artifacts::prepare_attachment_artifacts_for_capability(
                                    &artifacts,
                                    &user,
                                    &session,
                                    &args,
                                    |artifact| {
                                        let engine = engine.clone();
                                        let manifests = manifests.clone();
                                        let cap_id = cap_id_for_touch.clone();
                                        let session = session.clone();
                                        let user = user.clone();
                                        let policy_agent_id = policy_agent_id.clone();
                                        let db = db.clone();
                                        let filename = artifact.filename.clone();
                                        let mime_type = artifact.mime_type.clone();
                                        let data = artifact.data.clone();
                                        async move {
                                            engine
                                                .upload_input_artifact(
                                                    &manifests,
                                                    &cap_id,
                                                    &session,
                                                    &user,
                                                    &policy_agent_id,
                                                    &db,
                                                    &filename,
                                                    &mime_type,
                                                    &data,
                                                )
                                                .await
                                        }
                                    },
                                )
                                .await?;

                            let result = engine
                                .invoke(
                                    &manifests,
                                    &name,
                                    prepared_args,
                                    &session,
                                    &thread,
                                    &user,
                                    &policy_agent_id,
                                    &store,
                                    &db,
                                )
                                .await?;

                            crate::agents::artifacts::capture_artifacts_from_result(
                                &artifacts,
                                &user,
                                &session,
                                &result,
                                |capability_artifact_id| {
                                    let engine = engine.clone();
                                    let manifests = manifests.clone();
                                    let cap_id = cap_id_for_touch.clone();
                                    let session = session.clone();
                                    let user = user.clone();
                                    let policy_agent_id = policy_agent_id.clone();
                                    let db = db.clone();
                                    let capability_artifact_id = capability_artifact_id.to_string();
                                    async move {
                                        engine
                                            .download_output_artifact(
                                                &manifests,
                                                &cap_id,
                                                &session,
                                                &user,
                                                &policy_agent_id,
                                                &db,
                                                &capability_artifact_id,
                                            )
                                            .await
                                    }
                                },
                            )
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
    let LoopOutput {
        reply,
        tool_messages,
        iterations_used,
        elapsed_secs,
    } = loop_output;
    let tool_loop_ms = tool_loop_start.elapsed().as_millis();

    info!(
        iterations_used,
        elapsed_secs,
        tool_messages_count = tool_messages.len(),
        loop_ms = tool_loop_ms,
        "Tool loop complete, persisting reply"
    );

    // ── Persist tool interaction messages ─────────────────────────────────────
    // These are the assistant-with-tool-calls and tool-result messages
    // exchanged during the agentic loop. Persisting them lets context.rs
    // reconstruct the full tool interaction sequence for future turns.
    //
    // Compaction: when the orchestrator used delegation, only persist the
    // delegation tool call/result pairs and drop intermediate non-delegation
    // tool interactions. This prevents multi-hop chains from filling the
    // 50-message context window with internal orchestrator reasoning.
    let persist_start = Instant::now();
    let has_delegation = tool_messages.iter().any(|m| {
        m.tool_calls
            .iter()
            .any(|tc| tc.name == crate::agents::delegation::TOOL_NAME)
    });
    let delegation_call_ids: std::collections::HashSet<String> = if has_delegation {
        tool_messages
            .iter()
            .flat_map(|m| {
                m.tool_calls
                    .iter()
                    .filter(|tc| tc.name == crate::agents::delegation::TOOL_NAME)
                    .map(|tc| tc.id.clone())
            })
            .collect()
    } else {
        std::collections::HashSet::new()
    };
    let messages_to_persist: Vec<&ChatMessage> = if has_delegation {
        tool_messages
            .iter()
            .filter(|msg| {
                // Keep assistant messages that contain at least one delegation tool call
                if !msg.tool_calls.is_empty() {
                    return msg
                        .tool_calls
                        .iter()
                        .any(|tc| tc.name == crate::agents::delegation::TOOL_NAME);
                }
                // Keep tool results that correspond to a delegation call
                if let Some(ref tcid) = msg.tool_call_id {
                    return delegation_call_ids.contains(tcid);
                }
                false
            })
            .collect()
    } else {
        tool_messages.iter().collect()
    };
    if has_delegation {
        info!(
            total_tool_messages = tool_messages.len(),
            persisted_tool_messages = messages_to_persist.len(),
            "Compacted tool messages (delegation-only)"
        );
    }
    for msg in &messages_to_persist {
        let msg_id = Uuid::new_v4().to_string();
        let role_str = msg.role.as_str();
        let content_str = msg.content.as_text();
        let tool_call_id = msg.tool_call_id.as_deref();
        let tool_calls_json = if msg.tool_calls.is_empty() {
            None
        } else {
            serde_json::to_string(&msg.tool_calls).ok()
        };
        let now_ms = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3f")
            .to_string();
        if let Err(e) = sqlx::query(
            "INSERT INTO messages (id, pipe_id, session_id, thread_id, role, content, tool_call_id, tool_calls_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&msg_id)
        .bind(&pipe_id)
        .bind(&session_id)
        .bind(&thread_id)
        .bind(role_str)
        .bind(content_str)
        .bind(tool_call_id)
        .bind(tool_calls_json.as_deref())
        .bind(&now_ms)
        .execute(&state.db)
        .await
        {
            error!("Failed to persist tool message: {e}");
        }
    }

    // ── Persist assistant reply ───────────────────────────────────────────────
    if !reply.is_empty() {
        let reply_id = Uuid::new_v4().to_string();
        let asst_role = Role::Assistant.to_string();
        let now_ms = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3f")
            .to_string();
        if let Err(e) = sqlx::query!(
            "INSERT INTO messages (id, pipe_id, session_id, thread_id, role, content, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            reply_id, pipe_id, session_id, thread_id, asst_role, reply, now_ms
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

        // ── Self-improvement: record turn signal (best-effort, non-blocking) ─
        {
            let db = state.db.clone();
            let creds = state.credentials.clone();
            let uid = user_id.clone();
            let aid = agent.id.clone();
            let tid = thread_id.clone();
            let preview = effective_text.chars().take(200).collect::<String>();

            // Count tool calls and failures from tool_messages
            let mut tc_count: i64 = 0;
            let mut tc_failures: i64 = 0;
            let mut tools_used: Vec<String> = Vec::new();
            for msg in &tool_messages {
                for tc in &msg.tool_calls {
                    tc_count += 1;
                    tools_used.push(tc.name.clone());
                }
                // Count structured tool failures (set at dispatch time)
                if msg.tool_call_id.is_some() && msg.is_error {
                    tc_failures += 1;
                }
            }
            tools_used.sort();
            tools_used.dedup();

            let iters = iterations_used as i64;
            tokio::spawn(async move {
                let data = crate::agents::improvement::TurnSignalData {
                    agent_id: aid,
                    user_id: uid,
                    thread_id: tid,
                    user_message_preview: Some(preview),
                    tool_calls_count: tc_count,
                    tool_failures_count: tc_failures,
                    tool_loop_iterations: iters,
                    agent_tools_used: tools_used,
                };
                if let Err(e) =
                    crate::agents::improvement::process_turn_signal(&db, &creds, data, false).await
                {
                    debug!("Turn signal recording failed (non-fatal): {e}");
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
    let persist_ms = persist_start.elapsed().as_millis();

    info!(
        total_ms = turn_start.elapsed().as_millis(),
        agent_id = %agent.id,
        "Agent turn complete"
    );

    let artifacts_scan_start = Instant::now();
    let after_artifacts = crate::agents::artifacts::list_session_artifacts(
        &state.artifacts,
        &user_id,
        &session_id,
        256,
    )
    .await;
    let artifacts_scan_ms = artifacts_scan_start.elapsed().as_millis();
    let new_attachments = after_artifacts
        .into_iter()
        .filter(|a| {
            !before_ids.contains(&a.artifact_id) && !inbound_attachment_ids.contains(&a.artifact_id)
        })
        .collect::<Vec<_>>();
    let delegated = delegated_attachments.lock().await.clone();
    let mut merged_attachments = new_attachments;
    for attachment in delegated {
        if merged_attachments
            .iter()
            .all(|a| a.artifact_id != attachment.artifact_id)
        {
            merged_attachments.push(attachment);
        }
    }
    if persist_thread_artifacts {
        if let Some(tid) = thread_id.as_deref() {
            crate::agents::artifacts::persist_refs_for_thread(
                &state.db,
                &state.artifacts,
                &state.config.database.url,
                tid,
                &user_id,
                &merged_attachments,
            )
            .await;
        }
    }

    Ok(TurnOutput {
        reply_text: reply,
        attachments: merged_attachments,
    })
    .map(|output| {
        info!(
            turn_id = %turn_id,
            agent_id = %agent.id,
            session_id = %session_id,
            route_ms,
            session_open_ms,
            user_persist_ms,
            prep_ms,
            context_ms,
            provider_ms,
            tool_loop_ms,
            persist_ms,
            artifacts_scan_ms,
            total_ms = turn_start.elapsed().as_millis(),
            reply_len = output.reply_text.len(),
            attachments = output.attachments.len(),
            "Turn timing breakdown"
        );
        output
    })
}

async fn is_web_pipe_transport(db: &sqlx::SqlitePool, pipe_id: &str, user_id: &str) -> bool {
    let Ok(row) = sqlx::query!(
        "SELECT transport FROM pipes WHERE id = ? AND user_id = ? LIMIT 1",
        pipe_id,
        user_id
    )
    .fetch_optional(db)
    .await
    else {
        return false;
    };

    row.map(|r| r.transport == "web").unwrap_or(false)
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
    agent: &AgentDefinition,
    user_language: &str,
    capability_id: &str,
    tool_name: &str,
    namespaced_name: &str,
    args: &serde_json::Value,
    channel: &ChannelKind,
    session_id: &str,
    _pipe_id: &str,
    agent_id: &str,
    approved: bool,
    user_confirmed_this_turn: bool,
    terminal_on_success: bool,
    invoke_fn: F,
) -> Result<ToolDispatchResult>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    // If the user already approved this call, skip policy lookup and invoke.
    if approved {
        let invoke_start = Instant::now();
        let result = invoke_fn().await.unwrap_or_else(|e| {
            warn!(tool = %namespaced_name, "Pre-approved tool invocation failed: {e}");
            format!("Tool error: {}", e)
        });
        let invoke_ms = invoke_start.elapsed().as_millis();
        if invoke_ms > 10_000 {
            warn!(
                tool = %namespaced_name,
                invoke_ms,
                "Slow tool invocation (pre-approved)"
            );
        }
        info!(
            tool = %namespaced_name,
            policy = "pre_approved",
            invoke_ms,
            terminal_on_success,
            "Tool dispatch timing"
        );
        return Ok(if terminal_on_success {
            ToolDispatchResult::DoneAndReturn(result)
        } else {
            ToolDispatchResult::Done(result)
        });
    }

    let policy = tool_policy::get_effective_policy(&state.db, user_id, agent_id, capability_id, tool_name)
        .await
        .unwrap_or_else(|e| {
            warn!(tool = %namespaced_name, "Failed to look up tool policy (defaulting to blocked): {e}");
            None
        });

    let fallback_tool_label = humanize_identifier(tool_name);
    let localized_tool_label = localization::localized_approval_label(
        agent,
        capability_id,
        tool_name,
        &fallback_tool_label,
        user_language,
    );
    let approval_message = extract_approval_message(args, user_language).or_else(|| {
        localization::localized_approval_message(agent, capability_id, tool_name, user_language)
    });

    match policy {
        Some(ToolPolicy::Allow) => {
            let invoke_start = Instant::now();
            let result = invoke_fn().await.unwrap_or_else(|e| {
                warn!(tool = %namespaced_name, "Tool invocation failed: {e}");
                format!("Tool error: {}", e)
            });
            let invoke_ms = invoke_start.elapsed().as_millis();
            if invoke_ms > 10_000 {
                warn!(
                    tool = %namespaced_name,
                    invoke_ms,
                    "Slow tool invocation"
                );
            }
            info!(
                tool = %namespaced_name,
                policy = "allow",
                invoke_ms,
                terminal_on_success,
                "Tool dispatch timing"
            );
            Ok(if terminal_on_success {
                ToolDispatchResult::DoneAndReturn(result)
            } else {
                ToolDispatchResult::Done(result)
            })
        }

        Some(ToolPolicy::Block) | None => {
            info!(
                tool = %namespaced_name,
                policy = if policy.is_none() {
                    "implicit_block"
                } else {
                    "block"
                },
                "Tool dispatch timing"
            );
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
                    info!(
                        tool = %namespaced_name,
                        policy = "ask_interactive",
                        "Tool dispatch timing"
                    );
                    // The tool loop will prompt via SSE
                    Ok(ToolDispatchResult::NeedsConfirmation {
                        tool_display_name: localized_tool_label,
                        approval_message,
                    })
                }

                ChannelKind::ThreadedNonInteractive {
                    pipe_id: chan_pipe,
                    thread_id: chan_thread,
                } => {
                    if user_confirmed_this_turn {
                        let invoke_start = Instant::now();
                        let result = invoke_fn().await.unwrap_or_else(|e| {
                            warn!(tool = %namespaced_name, "Tool invocation failed after user-turn confirmation: {e}");
                            format!("Tool error: {}", e)
                        });
                        let invoke_ms = invoke_start.elapsed().as_millis();
                        info!(
                            tool = %namespaced_name,
                            policy = "ask_threaded_user_turn_confirmed",
                            invoke_ms,
                            terminal_on_success,
                            "Tool dispatch timing"
                        );
                        return Ok(if terminal_on_success {
                            ToolDispatchResult::DoneAndReturn(result)
                        } else {
                            ToolDispatchResult::Done(result)
                        });
                    }

                    let queue_start = Instant::now();
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

                    // Build a human-readable approval prompt
                    let prompt_text = build_approval_prompt(
                        user_language,
                        &localized_tool_label,
                        approval_message.clone(),
                        args,
                    );

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
                        .send(
                            chan_pipe,
                            chan_thread,
                            &crate::channels::ChannelMessage {
                                text: prompt_text,
                                attachments: Vec::new(),
                            },
                            reply_to.as_deref(),
                        )
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
                    info!(
                        tool = %namespaced_name,
                        policy = "ask_threaded_queued",
                        queue_ms = queue_start.elapsed().as_millis(),
                        approval_id = %approval_id,
                        "Tool dispatch timing"
                    );

                    Ok(ToolDispatchResult::Queued {
                        approval_id,
                        receiver: orx,
                        tool_display_name: localized_tool_label,
                        approval_message,
                    })
                }

                ChannelKind::NonInteractive => {
                    info!(
                        tool = %namespaced_name,
                        policy = "ask_non_interactive_blocked",
                        "Tool dispatch timing"
                    );
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
fn build_approval_prompt(
    lang: &str,
    tool_label: &str,
    approval_message: Option<String>,
    args: &serde_json::Value,
) -> String {
    let public_args = strip_internal_tool_args(args);

    let mut prompt = String::new();
    prompt.push_str(crate::i18n::t(lang, "approval.confirm_header"));
    prompt.push('\n');

    if let Some(message) = approval_message {
        prompt.push_str(&message);
    } else {
        prompt.push_str(&crate::i18n::t_with_tool(
            lang,
            "approval.action",
            tool_label,
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
const INTERNAL_APPROVAL_MESSAGE_I18N_KEY: &str = "_approval_message_i18n";

fn strip_internal_tool_args(args: &serde_json::Value) -> serde_json::Value {
    let mut sanitized = args.clone();
    if let Some(obj) = sanitized.as_object_mut() {
        obj.remove(INTERNAL_APPROVAL_MESSAGE_KEY);
        obj.remove(INTERNAL_APPROVAL_MESSAGE_I18N_KEY);
    }
    sanitized
}

fn extract_approval_message(args: &serde_json::Value, lang: &str) -> Option<String> {
    if let Some(map) = args
        .as_object()
        .and_then(|obj| obj.get(INTERNAL_APPROVAL_MESSAGE_I18N_KEY))
        .and_then(|value| value.as_object())
    {
        for candidate in localization::language_candidates(lang, "en") {
            if let Some(message) = map.get(&candidate).and_then(|value| value.as_str()) {
                let collapsed = message.split_whitespace().collect::<Vec<_>>().join(" ");
                let trimmed = collapsed.trim();
                if !trimmed.is_empty() {
                    return Some(truncate_for_approval(trimmed));
                }
            }
        }
    }

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
        let truncated: String = text.chars().take(77).collect();
        format!("{truncated}...")
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
    source_agent_id: String,
    pipe_id: String,
    user_id: String,
    args: serde_json::Value,
    session_id: String,
    chain_depth: i32,
    channel_kind: ChannelKind,
    parent_tx: LoopSender,
    parent_thread_id: Option<String>,
    delegation_trace: Vec<String>,
) -> BoxFuture<'static, Result<TurnOutput>> {
    async move {
        let (target_agent_id, mut message) = delegation::parse_args(&args)?;
        if delegation_trace
            .iter()
            .any(|agent_id| agent_id == &target_agent_id)
        {
            let mut path = delegation_trace.join(" -> ");
            if !path.is_empty() {
                path.push_str(" -> ");
            }
            path.push_str(&target_agent_id);
            return Err(anyhow::anyhow!(
                "Delegation cycle detected ({path}). Do NOT delegate again. Use the tools available in this agent to complete the request."
            ));
        }
        // Validate that the target agent exists in the user's visible agents.
        // This catches hallucinated or corrupted agent IDs early with a clear
        // error message, preventing blind retries by the orchestrator LLM.
        let agents_snapshot = state.agents.load();
        let user_agents =
            crate::agents::access::visible_agents(&state.db, &user_id, &agents_snapshot).await;
        if !user_agents.contains_key(&target_agent_id) {
            let available: Vec<&str> = user_agents.keys().map(|k| k.as_str()).collect();
            return Err(anyhow::anyhow!(
                "Agent '{}' does not exist. Available agents: {}. Do NOT retry with the same agent_id.",
                target_agent_id,
                available.join(", ")
            ));
        }
        drop(agents_snapshot);

        let contains_artifact_ref =
            message.contains("artifact_id") || message.contains("attachments");
        if !contains_artifact_ref {
            let refs = crate::agents::artifacts::list_session_artifacts(
                &state.artifacts,
                &user_id,
                &session_id,
                3,
            )
            .await;
            if !refs.is_empty() {
                let mut lines = Vec::new();
                lines.push(
                    "Available artifact references from this session (use artifact_id when needed):"
                        .to_string(),
                );
                for r in &refs {
                    lines.push(format!(
                        "- artifact_id: {} | filename: {} | mime_type: {} | size_bytes: {}",
                        r.artifact_id, r.filename, r.mime_type, r.size_bytes
                    ));
                }
                message.push_str("\n\n");
                message.push_str(&lines.join("\n"));
                info!(
                    from = %source_agent_id,
                    to = %target_agent_id,
                    injected_count = refs.len(),
                    "Injected session artifact references into delegation payload"
                );
            }
        }

        // Look up the target agent's display name for a better status label
        let target_display_name = state
            .agents
            .load()
            .get(&target_agent_id)
            .map(|a| a.name.clone())
            .unwrap_or_else(|| humanize_identifier(&target_agent_id));

        // Send a localized "Asking <agent>..." status to the parent stream
        let lang = crate::i18n::user_language(&state.db, &user_id).await;
        let delegation_status =
            crate::i18n::t_with_agent(&lang, "chat.tool.delegating", &target_display_name);
        let _ = parent_tx.send(LoopEvent::CapabilityStatus(delegation_status)).await;

        info!(
            from = %source_agent_id,
            to = %target_agent_id,
            "Delegating to specialist agent"
        );
        info!(
            from = %source_agent_id,
            to = %target_agent_id,
            chain_depth,
            msg_len = message.len(),
            contains_artifact_ref = (message.contains("artifact_id") || message.contains("attachments")),
            "Delegation payload metadata"
        );
        debug!(
            from = %source_agent_id,
            to = %target_agent_id,
            chain_depth,
            msg_len = message.len(),
            contains_artifact_ref,
            message_preview = %truncate_for_log(&message, 220),
            "Delegation payload prepared"
        );

        // Always inherit the parent thread id for delegation so specialist
        // turns remain in the same conversation context. This keeps follow-up
        // confirmations (e.g. "Ja") routed to the active specialist flow.
        let delegated_thread_id = parent_thread_id.clone();
        let mut next_delegation_trace = delegation_trace.clone();
        if next_delegation_trace
            .last()
            .map(|id| id.as_str())
            != Some(source_agent_id.as_str())
        {
            next_delegation_trace.push(source_agent_id.clone());
        }

        let params = TurnParams {
            pipe_id,
            user_id,
            agent_id: Some(target_agent_id.clone()),
            message,
            thread_id: delegated_thread_id,
            chain_depth: chain_depth + 1,
            channel_kind,
            skip_user_persist: true,
            enable_streaming: true,
            inbound_attachments: Vec::new(),
            delegation_trace: next_delegation_trace,
            location_context: None,
        };

        // Create a filtered sender that forwards only confirmation events
        // (needed for interactive Ask-policy approval) while discarding
        // text tokens and Done signals (which would cause duplicate output).
        let parent_status_tx = parent_tx.clone();
        let filtered_tx = confirmation_only_sender(parent_tx);
        let mut delegated_turn = Box::pin(run_turn(&state, params, filtered_tx));
        let mut heartbeat = tokio::time::interval(Duration::from_secs(25));
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Skip the immediate first tick so we only send progress updates
        // after a real wait period.
        heartbeat.tick().await;
        let output = loop {
            tokio::select! {
                res = &mut delegated_turn => break res?,
                _ = heartbeat.tick() => {
                    let progress =
                        crate::i18n::t_with_agent(&lang, "chat.tool.delegating_progress", &target_display_name);
                    let _ = parent_status_tx
                        .send(LoopEvent::CapabilityStatus(progress))
                        .await;
                }
            }
        };
        let reply = output.reply_text.clone();
        info!(
            from = %source_agent_id,
            to = %target_agent_id,
            reply_len = reply.len(),
            reply_contains_artifact_ref = (reply.contains("artifact_id") || reply.contains("attachments")),
            "Delegation reply metadata"
        );
        debug!(
            from = %source_agent_id,
            to = %target_agent_id,
            reply_len = reply.len(),
            reply_contains_artifact_ref = (reply.contains("artifact_id") || reply.contains("attachments")),
            reply_preview = %truncate_for_log(&reply, 220),
            "Delegation reply received"
        );

        Ok(output)
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
                LoopEvent::Token(_) | LoopEvent::Done => {}
                // Forward errors so the parent stream can surface delegation
                // failures instead of silently swallowing them.
                LoopEvent::Error(_) => {
                    tracing::warn!("Delegated agent emitted error event, forwarding to parent");
                    let _ = parent_tx.send(event).await;
                }
            }
        }
    });

    tx
}

/// Create a throwaway channel for callers that don't stream tokens.
///
/// The receiver is immediately dropped, so every `send()` will return `Err`
/// (which callers ignore with `let _ =`).  A buffer of 16 avoids blocking
/// before the first send discovers the closed receiver.
pub fn noop_sender() -> LoopSender {
    let (tx, _) = mpsc::channel::<LoopEvent>(16);
    tx
}

fn truncate_for_log(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn is_explicit_confirmation_message(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    if normalized.is_empty() {
        return false;
    }
    let collapsed = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    let word_count = collapsed.split_whitespace().count();

    // Bare confirmations — only match when the message is very short (≤3 words)
    // to avoid false positives when the user says "ok" at the start of a longer
    // message that is actually a new instruction, not a confirmation.
    if word_count <= 3 {
        let explicit_yes = [
            "yes",
            "yeah",
            "yep",
            "ok",
            "okay",
            "ja",
            "j",
            "klar",
            "mach das",
            "mach es",
            "sende es",
            "bitte senden",
            "send it",
            "send now",
            "go ahead",
        ];
        if explicit_yes.contains(&collapsed.as_str()) {
            return true;
        }
    }

    // Compound send-intent checks — these are specific enough to allow on
    // longer messages too.
    let has_email = collapsed.contains("email") || collapsed.contains("e-mail");
    let has_send_verb = [
        "send",
        "sende",
        "senden",
        "absenden",
        "verschick",
        "verschicken",
    ]
    .iter()
    .any(|k| collapsed.contains(k));
    let has_delivery_object = ["pdf", "anhang", "attachment", "dokument", "datei", "file"]
        .iter()
        .any(|k| collapsed.contains(k));

    (has_send_verb && has_email) || (has_send_verb && has_delivery_object)
}

fn normalize_user_text_for_inbound_attachments(
    raw_text: &str,
    attachments: &[crate::agents::artifacts::ArtifactRef],
) -> String {
    if attachments.is_empty() {
        return raw_text.to_string();
    }

    let mut out = String::new();
    let trimmed = raw_text.trim();
    if trimmed.is_empty() {
        out.push_str(
            "User sent image attachment(s) without accompanying text. \
Interpret what is visible and respond helpfully. \
If you infer a date/event from the image, ask whether the user wants it added to the calendar.",
        );
    } else {
        out.push_str(trimmed);
    }

    out.push_str("\n\nAttached image artifacts:\n");
    for r in attachments {
        out.push_str(&format!(
            "- artifact_id: {} | filename: {} | mime_type: {} | size_bytes: {}\n",
            r.artifact_id, r.filename, r.mime_type, r.size_bytes
        ));
    }
    out.trim_end().to_string()
}

async fn build_multimodal_user_parts(
    store: &crate::agents::artifacts::ArtifactStore,
    user_id: &str,
    message_text: &str,
    refs: &[crate::agents::artifacts::ArtifactRef],
) -> Option<Vec<ContentPart>> {
    if refs.is_empty() {
        return None;
    }

    let mut parts = Vec::new();
    if !message_text.trim().is_empty() {
        parts.push(ContentPart::text(message_text));
    }

    for r in refs {
        let Some(stored) =
            crate::agents::artifacts::get_for_user(store, &r.artifact_id, user_id).await
        else {
            continue;
        };
        if !stored.mime_type.starts_with("image/") {
            continue;
        }
        parts.push(ContentPart::image_base64(
            stored.mime_type,
            base64::engine::general_purpose::STANDARD.encode(stored.data),
        ));
    }

    if parts.is_empty() { None } else { Some(parts) }
}

fn inject_multimodal_user_message(
    messages: &mut Vec<ChatMessage>,
    expected_text: &str,
    parts: Vec<ContentPart>,
) {
    let replacement = MessageContent::Parts(parts.clone());
    if let Some(msg) = messages.iter_mut().rev().find(|m| {
        m.role == "user" && matches!(&m.content, MessageContent::Text(t) if t == expected_text)
    }) {
        msg.content = replacement;
        return;
    }
    messages.push(ChatMessage {
        role: "user".to_string(),
        content: MessageContent::Parts(parts),
        tool_call_id: None,
        tool_calls: Vec::new(),
        is_error: false,
    });
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
        let prompt = build_approval_prompt("de", "Delete multiple items from list", None, &args);

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
        let prompt = build_approval_prompt(
            "de",
            "Delete multiple items from list",
            extract_approval_message(&args, "de"),
            &args,
        );

        assert!(prompt.contains("Ich moechte Spuelmaschinensalz von deiner Liste loeschen."));
        assert!(!prompt.contains("Aktion:"));
        assert!(!prompt.contains("_approval_message"));
        assert!(prompt.contains("Item names: Spulmaschinensalz"));
    }

    #[test]
    fn strip_internal_tool_args_removes_approval_message() {
        let args = json!({
            "_approval_message": "friendly",
            "_approval_message_i18n": { "de": "freundlich" },
            "itemNames": ["A"]
        });
        let stripped = strip_internal_tool_args(&args);

        assert!(stripped.get("_approval_message").is_none());
        assert!(stripped.get("_approval_message_i18n").is_none());
        assert_eq!(stripped.get("itemNames").unwrap(), &json!(["A"]));
    }

    #[test]
    fn extract_approval_message_prefers_localized_variant() {
        let args = json!({
            "_approval_message": "Delete the item.",
            "_approval_message_i18n": {
                "de": "Ich moechte den Artikel loeschen."
            }
        });

        assert_eq!(
            extract_approval_message(&args, "de").as_deref(),
            Some("Ich moechte den Artikel loeschen.")
        );
    }

    #[test]
    fn explicit_confirmation_detects_short_yes() {
        assert!(is_explicit_confirmation_message("Ja"));
        assert!(is_explicit_confirmation_message("send it"));
    }

    #[test]
    fn explicit_confirmation_detects_send_requests() {
        assert!(is_explicit_confirmation_message(
            "Die PDF bitte per email an jan@example.com senden"
        ));
        assert!(is_explicit_confirmation_message("Please send this PDF now"));
        assert!(!is_explicit_confirmation_message(
            "Kannst du eine PDF erstellen?"
        ));
    }
}
