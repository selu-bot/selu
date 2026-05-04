use anyhow::{Result, anyhow};
use futures::StreamExt;
use futures::future::join_all;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, timeout};
use tracing::{debug, info, warn};

use super::image_normalizer::normalize_messages_for_provider;
use super::provider::{ChatMessage, LlmProvider, LlmResponse, StreamChunk, ToolCall, ToolSpec};

// ── Tool dispatch result ──────────────────────────────────────────────────────

/// The result of a tool dispatch attempt, returned by the dispatcher closure.
///
/// This replaces the previous `Result<String>` return type and the separate
/// `confirmation_tools` set, unifying policy enforcement into the dispatch path.
pub enum ToolDispatchResult {
    /// Tool executed successfully (or failed with a tool-level error).
    /// The string is the tool result to feed back to the LLM.
    Done(String),
    /// Tool executed successfully and should complete the turn immediately
    /// (skip another LLM round-trip).
    DoneAndReturn(String),

    /// User's policy says "block" (or no policy is set — secure default).
    /// The string is a denial message for the LLM.
    Blocked(String),

    /// User's policy says "ask" and the caller is on an interactive channel
    /// (web chat with an active SSE stream). The tool loop should prompt
    /// the user via `LoopEvent::ConfirmationRequired`.
    NeedsConfirmation {
        tool_display_name: String,
        approval_message: Option<String>,
    },

    /// User's policy says "ask" and the caller is on a non-interactive
    /// threaded channel (iMessage, webhook). An approval prompt has already
    /// been sent to the user. The tool loop should wait on the receiver
    /// with the given timeout.
    Queued {
        approval_id: String,
        receiver: oneshot::Receiver<bool>,
        tool_display_name: String,
        approval_message: Option<String>,
    },
}

// ── Loop events ───────────────────────────────────────────────────────────────

/// A pending confirmation request sent to the caller for approval.
#[derive(Debug)]
pub struct ConfirmationRequest {
    /// Localized tool label for user-facing approval UI.
    pub tool_display_name: String,
    /// The arguments the LLM passed to the tool
    pub arguments: serde_json::Value,
    /// Optional localized explanation from the agent for why approval is needed.
    pub approval_message: Option<String>,
    /// Send `true` to approve, `false` to deny. If the sender is dropped, the
    /// call is treated as denied.
    pub reply: oneshot::Sender<bool>,
}

/// Status updates sent back to the caller during the agentic loop
#[derive(Debug)]
pub enum LoopEvent {
    /// A text token (streamed)
    Token(String),
    /// The LLM is invoking a capability
    CapabilityStatus(String), // e.g. "Using google-calendar..."
    /// A tool call requires explicit user confirmation before dispatch.
    /// The receiver must send `true` (approve) or `false` (deny) on the
    /// enclosed oneshot channel.
    ConfirmationRequired(ConfirmationRequest),
    /// A tool call has been queued for async approval on a non-interactive
    /// channel. The caller can use this to update the UI.
    ApprovalQueued {
        tool_display_name: String,
        approval_message: Option<String>,
        approval_id: String,
    },
    /// A tool interaction message was added to the conversation.
    /// Used for incremental persistence so reconnecting clients see
    /// in-progress tool calls without waiting for the turn to complete.
    ToolMessage(ChatMessage),
    /// Artifacts produced during this turn (delivered before Done).
    Artifacts(Vec<crate::agents::artifacts::ArtifactRef>),
    /// Final complete text response
    Done,
    /// An error occurred
    Error(String),
}

// LoopEvent is no longer Clone because of the oneshot sender -- callers that
// need to forward events should destructure and re-wrap as needed.

pub type LoopSender = mpsc::Sender<LoopEvent>;

/// Output from the agentic tool loop containing the final reply text and any
/// tool interaction messages that were exchanged during the loop. The caller
/// can persist the `tool_messages` so that future conversation history includes
/// the full tool call/result sequence.
pub struct LoopOutput {
    pub reply: String,
    /// Tool interaction messages (assistant-with-tool-calls + tool results)
    /// added during the loop, in chronological order.
    pub tool_messages: Vec<ChatMessage>,
    /// Number of loop iterations consumed.
    pub iterations_used: u32,
    /// Wall-clock seconds elapsed during the loop.
    pub elapsed_secs: f64,
}

/// Runs the agentic tool-call loop.
///
/// Every iteration streams the LLM response in real-time. Text deltas are
/// forwarded immediately as `LoopEvent::Token`. Tool-call arguments are
/// accumulated from `ToolCallDelta` chunks during the stream, then dispatched
/// when the stream completes.
///
/// This means both pure-text and tool-call responses use a **single** streaming
/// call per iteration — no redundant non-streaming fallback.
///
/// The `tool_dispatcher` closure is responsible for:
///   1. Checking the user's tool policy (unless `approved` is `true`)
///   2. If `Allow` (or `approved`): invoking the tool and returning `ToolDispatchResult::Done`
///   3. If `Block` / no policy: returning `ToolDispatchResult::Blocked`
///   4. If `Ask` + interactive: returning `ToolDispatchResult::NeedsConfirmation`
///   5. If `Ask` + threaded non-interactive: queuing an approval and returning
///      `ToolDispatchResult::Queued`
///
/// The third parameter (`approved: bool`) is `true` when the user has already
/// approved this specific call via the confirmation flow.  In that case the
/// dispatcher should skip the policy check and invoke the tool directly.
pub async fn run_loop(
    provider: Arc<dyn LlmProvider>,
    mut messages: Vec<ChatMessage>,
    tools: Vec<ToolSpec>,
    temperature: f32,
    max_iterations: u32,
    tx: LoopSender,
    enable_streaming: bool,
    status_language: String,
    tool_status_labeler: impl Fn(&str) -> String + Send + Sync + 'static,
    tool_dispatcher: impl Fn(
        String,
        serde_json::Value,
        bool,
    ) -> futures::future::BoxFuture<'static, Result<ToolDispatchResult>>
    + Send
    + Sync
    + 'static,
) -> Result<LoopOutput> {
    let loop_start = Instant::now();
    let initial_message_count = messages.len();
    let mut iteration: u32 = 0;

    loop {
        if max_iterations > 0 && iteration >= max_iterations {
            return Err(anyhow!(
                "Tool loop exceeded {} iterations without completion",
                max_iterations
            ));
        }
        let iteration_start = Instant::now();
        info!(
            iteration,
            message_count = messages.len(),
            tool_count = tools.len(),
            streaming = enable_streaming,
            "Tool loop iteration started — calling LLM"
        );
        normalize_messages_for_provider(&mut messages, provider.as_ref());
        super::context_budget::trim_to_budget(&mut messages, provider.as_ref());

        // Tell the user we're waiting for the model
        let thinking_status = crate::i18n::t(&status_language, "chat.thinking").to_string();
        let _ = tx.send(LoopEvent::CapabilityStatus(thinking_status)).await;

        // ── State for accumulating tool calls from the stream ─────────────
        // Each entry: (id, name, accumulated_arguments_json)
        let mut pending_tool_calls: Vec<(String, String, String)> = Vec::new();
        // Track which tool-call indices already had a CapabilityStatus
        // emitted during streaming, so we don't duplicate at dispatch time.
        let mut status_sent: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut streamed_text = String::new();
        let mut stream_ok = true;
        let mut stream_reason = "completed";

        // ── Stream the response ──────────────────────────────────────────
        if !enable_streaming {
            info!(iteration, "Streaming disabled, using non-streaming call");
            stream_ok = false;
            stream_reason = "disabled";
        } else {
            let stream_req_start = Instant::now();
            info!(iteration, "Opening streaming connection to LLM");
            match provider.chat_stream(&messages, &tools, temperature).await {
                Ok(mut stream) => {
                    let stream_start = Instant::now();
                    let mut got_first_token = false;
                    let mut first_token_ms: Option<u128> = None;
                    let mut last_progress_log = Instant::now();

                    loop {
                        // Choose timeout based on stream state:
                        //  - No tool calls yet → full stream timeout (45s)
                        //  - Have tool calls but JSON is still structurally
                        //    incomplete (unclosed braces) → full stream timeout
                        //    (the LLM is still generating a large argument)
                        //  - Have tool calls with structurally complete JSON →
                        //    short idle timeout to catch the missing Done signal
                        //    without waiting the full 45s
                        let chunk_timeout = STREAM_CHUNK_TIMEOUT_SECS;

                        let next = timeout(Duration::from_secs(chunk_timeout), stream.next()).await;
                        let Some(chunk) = (match next {
                            Ok(item) => item,
                            Err(_) => {
                                warn!(
                                    iteration,
                                    pending_tool_calls = pending_tool_calls.len(),
                                    "Streaming response timed out waiting for next chunk, falling back to non-streaming"
                                );
                                stream_ok = false;
                                stream_reason = "chunk_timeout";
                                break;
                            }
                        }) else {
                            break;
                        };

                        match chunk? {
                            StreamChunk::Text(t) => {
                                if !got_first_token {
                                    let elapsed = stream_start.elapsed().as_millis();
                                    info!(
                                        iteration,
                                        elapsed_ms = elapsed,
                                        "First text token received from LLM"
                                    );
                                    got_first_token = true;
                                    first_token_ms = Some(elapsed);
                                }
                                streamed_text.push_str(&t);
                                let _ = tx.send(LoopEvent::Token(t)).await;
                            }
                            StreamChunk::ToolCallDelta {
                                index,
                                id,
                                name,
                                arguments_delta,
                            } => {
                                if !got_first_token {
                                    let elapsed = stream_start.elapsed().as_millis();
                                    info!(
                                        iteration,
                                        elapsed_ms = elapsed,
                                        "First token received (tool call delta)"
                                    );
                                    got_first_token = true;
                                    first_token_ms = Some(elapsed);
                                }
                                // Grow the vec if needed
                                while pending_tool_calls.len() <= index {
                                    pending_tool_calls.push((
                                        String::new(),
                                        String::new(),
                                        String::new(),
                                    ));
                                }
                                if let Some(id) = id {
                                    pending_tool_calls[index].0 = id;
                                }
                                if let Some(ref tool_name) = name {
                                    // Emit a status event as soon as we know the tool name,
                                    // so the UI shows "Preparing X..." immediately — not just
                                    // after all arguments are fully accumulated.
                                    // The labeler returns empty for tools that handle their
                                    // own status (e.g. delegation emits "Asking <agent>...").
                                    if pending_tool_calls[index].1.is_empty() {
                                        info!(
                                            iteration,
                                            tool = %tool_name,
                                            "LLM chose tool, streaming arguments"
                                        );
                                        let status_label = tool_status_labeler(tool_name);
                                        if !status_label.is_empty() {
                                            let status = crate::i18n::t_with_tool(
                                                &status_language,
                                                "chat.tool.preparing",
                                                &status_label,
                                            );
                                            let _ =
                                                tx.send(LoopEvent::CapabilityStatus(status)).await;
                                            status_sent.insert(index);
                                        }
                                    }
                                }
                                if let Some(name) = name {
                                    pending_tool_calls[index].1 = name;
                                }
                                pending_tool_calls[index].2.push_str(&arguments_delta);

                                // Periodic progress logging every 10s so operators can
                                // see the model is still generating large tool arguments
                                if last_progress_log.elapsed().as_secs() >= 10 {
                                    let total_args_bytes: usize =
                                        pending_tool_calls.iter().map(|(_, _, a)| a.len()).sum();
                                    info!(
                                        iteration,
                                        elapsed_s = stream_start.elapsed().as_secs(),
                                        total_args_bytes,
                                        tool = %pending_tool_calls[index].1,
                                        "Still streaming tool arguments"
                                    );
                                    last_progress_log = Instant::now();
                                }
                            }
                            StreamChunk::Done => {
                                stream_reason = "completed";
                                info!(
                                    iteration,
                                    elapsed_ms = stream_start.elapsed().as_millis(),
                                    text_len = streamed_text.len(),
                                    tool_calls = pending_tool_calls.len(),
                                    "LLM stream complete"
                                );
                                break;
                            }
                        }
                    }
                    info!(
                        iteration,
                        stream_request_ms = stream_req_start.elapsed().as_millis(),
                        stream_duration_ms = stream_start.elapsed().as_millis(),
                        first_token_ms = ?first_token_ms,
                        stream_ok,
                        reason = stream_reason,
                        streamed_text_len = streamed_text.len(),
                        streamed_tool_calls = pending_tool_calls.len(),
                        "Tool loop stream phase complete"
                    );
                }
                Err(e) => {
                    warn!(iteration, "Streaming failed, falling back: {e}");
                    stream_ok = false;
                    stream_reason = "provider_stream_error";
                }
            }
        }

        // ── Handle the streamed result ───────────────────────────────────

        if stream_ok && pending_tool_calls.is_empty() {
            // Pure text response — we're done
            if !streamed_text.is_empty() {
                info!(
                    iteration,
                    iteration_ms = iteration_start.elapsed().as_millis(),
                    total_ms = loop_start.elapsed().as_millis(),
                    text_len = streamed_text.len(),
                    "Tool loop complete — returning streamed text"
                );
                // Done is sent by the engine after artifact computation.
                return Ok(LoopOutput {
                    reply: streamed_text,
                    tool_messages: messages[initial_message_count..].to_vec(),
                    iterations_used: iteration,
                    elapsed_secs: loop_start.elapsed().as_secs_f64(),
                });
            }
            // Stream completed but returned no text and no tool calls — fall through to non-streaming
            debug!(
                iteration,
                "Stream returned no content, falling back to non-streaming"
            );
        }

        // ── Build ToolCall objects from stream deltas (or fall back) ─────

        let calls: Vec<ToolCall> = if !pending_tool_calls.is_empty() {
            let mut parsed_calls: Vec<ToolCall> = Vec::new();
            let mut parse_failed = false;

            for (id, name, args_json) in pending_tool_calls
                .into_iter()
                .filter(|(_, name, _)| !name.is_empty())
            {
                let arguments: serde_json::Value = if args_json.trim().is_empty() {
                    // Tool calls with no arguments (e.g. tools that take no params):
                    // the LLM may emit contentBlockStart but no contentBlockDelta,
                    // leaving the accumulated string empty. Default to `{}`.
                    serde_json::json!({})
                } else {
                    match parse_streamed_tool_args(&args_json) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(
                                tool = %name,
                                raw_len = args_json.len(),
                                error = %e,
                                "Failed to parse streamed tool args, retrying with non-streaming call"
                            );
                            parse_failed = true;
                            break;
                        }
                    }
                };

                parsed_calls.push(ToolCall {
                    id: if id.is_empty() {
                        format!("call_{}", uuid::Uuid::new_v4())
                    } else {
                        id
                    },
                    name,
                    arguments,
                });
            }

            // Guard against partially streamed args where JSON is syntactically valid
            // but missing required fields from the tool schema.
            if !parse_failed {
                for call in &parsed_calls {
                    if missing_required_tool_args(call, &tools) {
                        warn!(
                            tool = %call.name,
                            args = %call.arguments,
                            "Streamed tool args missing required fields, retrying with non-streaming call"
                        );
                        parse_failed = true;
                        break;
                    }
                }
            }

            if !parse_failed {
                parsed_calls
            } else {
                // Streamed tool-call args can arrive incomplete with some providers.
                // Fall back to one non-streaming round rather than dispatching malformed args.
                info!(
                    iteration,
                    "Streamed tool args incomplete, falling back to non-streaming LLM call"
                );
                let llm_start = Instant::now();
                let response = fallback_chat(&provider, &messages, &tools, temperature).await?;
                let fallback_ms = llm_start.elapsed().as_millis();
                debug!(
                    iteration,
                    elapsed_ms = fallback_ms,
                    "LLM non-streaming fallback completed"
                );
                info!(
                    iteration,
                    fallback_ms,
                    reason = "parse_streamed_args_failed",
                    "Tool loop fallback chat complete"
                );

                match response {
                    LlmResponse::Text(text) => {
                        info!(iteration, fallback_ms, "LLM returned text via fallback");
                        let _ = tx.send(LoopEvent::Token(text.clone())).await;
                        return Ok(LoopOutput {
                            reply: text,
                            tool_messages: messages[initial_message_count..].to_vec(),
                            iterations_used: iteration,
                            elapsed_secs: loop_start.elapsed().as_secs_f64(),
                        });
                    }
                    LlmResponse::ToolCalls(c) => {
                        info!(
                            iteration,
                            fallback_ms,
                            tool_calls = c.len(),
                            "LLM returned tool calls via fallback"
                        );
                        c
                    }
                }
            }
        } else {
            // Fallback: streaming failed entirely or returned nothing useful.
            // Use a non-streaming call as last resort.
            info!(
                iteration,
                reason = stream_reason,
                "Using non-streaming LLM call"
            );
            let llm_start = Instant::now();
            let response = if enable_streaming {
                fallback_chat(&provider, &messages, &tools, temperature).await?
            } else {
                provider.chat(&messages, &tools, temperature).await?
            };
            let fallback_ms = llm_start.elapsed().as_millis();
            info!(
                iteration,
                fallback_ms,
                reason = stream_reason,
                "Non-streaming LLM call complete"
            );

            match response {
                LlmResponse::Text(text) => {
                    info!(iteration, fallback_ms, "LLM returned text (non-streaming)");
                    let _ = tx.send(LoopEvent::Token(text.clone())).await;
                    return Ok(LoopOutput {
                        reply: text,
                        tool_messages: messages[initial_message_count..].to_vec(),
                        iterations_used: iteration,
                        elapsed_secs: loop_start.elapsed().as_secs_f64(),
                    });
                }
                LlmResponse::ToolCalls(c) => {
                    info!(
                        iteration,
                        fallback_ms,
                        tool_calls = c.len(),
                        "LLM returned tool calls (non-streaming)"
                    );
                    c
                }
            }
        };

        // ── Dispatch tool calls (parallel where possible) ─────────────

        // Add the assistant's tool call message with the actual
        // ToolCall data so providers can emit proper toolUse blocks.
        let summary = calls
            .iter()
            .map(|c| format!("[calling {}]", c.name))
            .collect::<Vec<_>>()
            .join(", ");
        messages.push(ChatMessage::assistant_with_tool_calls(
            summary,
            calls.clone(),
        ));
        let _ = tx
            .send(LoopEvent::ToolMessage(messages.last().unwrap().clone()))
            .await;

        // Send status events for tools that didn't already get one during streaming.
        // Tools that got an early streaming status (tracked in `status_sent`) are
        // skipped to avoid duplicate UI entries. Tools with an empty label (e.g.
        // delegation) handle their own status elsewhere.
        for (idx, call) in calls.iter().enumerate() {
            let mut arg_keys: Vec<String> = call
                .arguments
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            arg_keys.sort();
            let content_len = call
                .arguments
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.len())
                .unwrap_or(0);
            let files_count = call
                .arguments
                .get("files")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            info!(
                tool = %call.name,
                arg_keys = ?arg_keys,
                content_len,
                files_count,
                "LLM requesting tool call"
            );
            if !status_sent.contains(&idx) {
                let status_label = tool_status_labeler(&call.name);
                if !status_label.is_empty() {
                    let status = crate::i18n::t_with_tool(
                        &status_language,
                        "chat.tool.using",
                        &status_label,
                    );
                    let _ = tx.send(LoopEvent::CapabilityStatus(status)).await;
                }
            }
        }

        // Validate tool args against schema before dispatch. This catches
        // semantically invalid payloads (wrong types/shapes) and lets the LLM
        // self-correct on the next loop turn.
        let mut dispatch_calls: Vec<ToolCall> = Vec::new();
        for call in &calls {
            if let Some(validation_err) = validate_tool_call_args_against_schema(call, &tools) {
                warn!(
                    tool = %call.name,
                    args = %call.arguments,
                    validation_error = %validation_err,
                    "Tool args failed schema validation, skipping dispatch"
                );
                messages.push(ChatMessage::tool_error(
                    &call.id,
                    format!(
                        "Tool argument validation failed for '{}': {}. Regenerate this tool call with a valid argument shape.",
                        call.name, validation_err
                    ),
                ));
                let _ = tx
                    .send(LoopEvent::ToolMessage(messages.last().unwrap().clone()))
                    .await;
            } else {
                dispatch_calls.push(call.clone());
            }
        }

        if dispatch_calls.is_empty() {
            info!(
                iteration,
                "All tool calls were invalid after schema validation; continuing loop for regeneration"
            );
            iteration += 1;
            continue;
        }

        // Dispatch all valid tool calls in parallel. The dispatcher checks policy
        // and invokes the tool (for Allow) or returns NeedsConfirmation/Queued
        // immediately (for Ask) — so parallel dispatch is safe.
        //
        // No per-dispatch timeout: tools like coding agents or PDF generation
        // can legitimately run for minutes. Safety comes from
        // the iteration cap, stream-level timeouts, and external
        // cancellation by the caller.
        let tool_names: Vec<&str> = dispatch_calls.iter().map(|c| c.name.as_str()).collect();
        info!(
            iteration,
            tools = ?tool_names,
            "Dispatching {} tool call(s)",
            dispatch_calls.len()
        );
        let dispatch_start = Instant::now();
        let dispatch_futures: Vec<_> = dispatch_calls
            .iter()
            .map(|call| tool_dispatcher(call.name.clone(), call.arguments.clone(), false))
            .collect();
        let dispatch_results = join_all(dispatch_futures).await;
        let dispatch_ms = dispatch_start.elapsed().as_millis();

        info!(
            iteration,
            tool_calls = dispatch_calls.len(),
            dispatch_ms,
            "Tool dispatch phase complete"
        );

        // Process results: Done/Blocked are pushed immediately, while
        // NeedsConfirmation/Queued are collected for sequential handling
        // (they require user interaction which is inherently serial).
        struct DeferredConfirmation<'a> {
            call: &'a ToolCall,
            tool_display_name: String,
            approval_message: Option<String>,
        }
        struct DeferredQueued<'a> {
            call: &'a ToolCall,
            approval_id: String,
            receiver: oneshot::Receiver<bool>,
            tool_display_name: String,
            approval_message: Option<String>,
        }
        let mut deferred_confirmations: Vec<DeferredConfirmation<'_>> = Vec::new();
        let mut deferred_queued: Vec<DeferredQueued<'_>> = Vec::new();

        let mut terminal_reply: Option<String> = None;
        // When multiple tools are dispatched in parallel, downgrade
        // DoneAndReturn → Done so the LLM sees every result before
        // the loop exits. This prevents orphaned tool_result messages
        // that would otherwise be persisted but never consumed.
        let force_non_terminal = dispatch_calls.len() > 1;

        for (call, result) in dispatch_calls.iter().zip(dispatch_results) {
            match result {
                Err(e) => {
                    warn!(tool = %call.name, "Tool dispatch failed: {e}");
                    messages.push(ChatMessage::tool_error(
                        &call.id,
                        format!("Tool error: {}", e),
                    ));
                    let _ = tx
                        .send(LoopEvent::ToolMessage(messages.last().unwrap().clone()))
                        .await;
                    continue;
                }
                Ok(dispatch_result) => match dispatch_result {
                    ToolDispatchResult::Done(result) => {
                        info!(
                            tool = %call.name,
                            "Tool call completed"
                        );
                        messages.push(ChatMessage::tool_result(&call.id, result));
                        let _ = tx
                            .send(LoopEvent::ToolMessage(messages.last().unwrap().clone()))
                            .await;
                    }
                    ToolDispatchResult::DoneAndReturn(result) if force_non_terminal => {
                        info!(
                            tool = %call.name,
                            "Tool call completed (terminal downgraded — parallel batch)"
                        );
                        messages.push(ChatMessage::tool_result(&call.id, result));
                        let _ = tx
                            .send(LoopEvent::ToolMessage(messages.last().unwrap().clone()))
                            .await;
                    }
                    ToolDispatchResult::DoneAndReturn(result) => {
                        info!(tool = %call.name, "Tool call completed (terminal)");
                        if terminal_reply.is_none() {
                            terminal_reply = Some(result);
                        }
                    }

                    ToolDispatchResult::Blocked(msg) => {
                        warn!(tool = %call.name, "Tool call blocked by policy");
                        messages.push(ChatMessage::tool_error(&call.id, msg));
                        let _ = tx
                            .send(LoopEvent::ToolMessage(messages.last().unwrap().clone()))
                            .await;
                    }

                    ToolDispatchResult::NeedsConfirmation {
                        tool_display_name,
                        approval_message,
                    } => {
                        deferred_confirmations.push(DeferredConfirmation {
                            call,
                            tool_display_name,
                            approval_message,
                        });
                    }

                    ToolDispatchResult::Queued {
                        approval_id,
                        receiver,
                        tool_display_name,
                        approval_message,
                    } => {
                        deferred_queued.push(DeferredQueued {
                            call,
                            approval_id,
                            receiver,
                            tool_display_name,
                            approval_message,
                        });
                    }
                }, // match dispatch_result
            } // Ok(dispatch_result)
        }

        // Handle interactive confirmations sequentially (user approves one at a time)
        for deferred in deferred_confirmations {
            let call = deferred.call;
            let (confirm_tx, confirm_rx) = oneshot::channel::<bool>();
            let req = ConfirmationRequest {
                tool_display_name: deferred.tool_display_name,
                arguments: call.arguments.clone(),
                approval_message: deferred.approval_message,
                reply: confirm_tx,
            };
            let _ = tx.send(LoopEvent::ConfirmationRequired(req)).await;

            let approved =
                match timeout(Duration::from_secs(CONFIRMATION_TIMEOUT_SECS), confirm_rx).await {
                    Ok(Ok(v)) => v,
                    Ok(Err(_)) => false, // sender dropped
                    Err(_) => {
                        warn!(
                            tool = %call.name,
                            timeout_s = CONFIRMATION_TIMEOUT_SECS,
                            "Interactive confirmation timed out"
                        );
                        false
                    }
                };
            if !approved {
                warn!(tool = %call.name, "Tool call denied by user (interactive)");
                messages.push(ChatMessage::tool_error(
                    &call.id,
                    "The user denied this action. Do NOT retry it. \
                     Acknowledge the denial and ask how they'd like to proceed."
                        .to_string(),
                ));
                let _ = tx
                    .send(LoopEvent::ToolMessage(messages.last().unwrap().clone()))
                    .await;
                continue;
            }
            info!(tool = %call.name, "Tool call approved by user (interactive)");

            // Re-dispatch — the user already approved, so the
            // dispatcher should skip the policy check and invoke directly.
            let result = tool_dispatcher(call.name.clone(), call.arguments.clone(), true).await;

            match result {
                Err(e) => {
                    warn!(tool = %call.name, "Tool dispatch failed after confirmation: {e}");
                    messages.push(ChatMessage::tool_error(
                        &call.id,
                        format!("Tool error: {}", e),
                    ));
                    let _ = tx
                        .send(LoopEvent::ToolMessage(messages.last().unwrap().clone()))
                        .await;
                }
                Ok(ToolDispatchResult::Done(r)) => {
                    info!(tool = %call.name, "Tool call completed after confirmation");
                    messages.push(ChatMessage::tool_result(&call.id, r));
                    let _ = tx
                        .send(LoopEvent::ToolMessage(messages.last().unwrap().clone()))
                        .await;
                }
                Ok(ToolDispatchResult::DoneAndReturn(r)) => {
                    info!(
                        tool = %call.name,
                        "Tool call completed after confirmation (terminal)"
                    );
                    if terminal_reply.is_none() {
                        terminal_reply = Some(r);
                    }
                }
                _ => {
                    messages.push(ChatMessage::tool_error(
                        &call.id,
                        "Internal error: tool dispatch failed after approval.".to_string(),
                    ));
                    let _ = tx
                        .send(LoopEvent::ToolMessage(messages.last().unwrap().clone()))
                        .await;
                }
            }
        }

        // Handle async approvals sequentially (non-interactive channels)
        for deferred in deferred_queued {
            let call = deferred.call;
            let _ = tx
                .send(LoopEvent::ApprovalQueued {
                    tool_display_name: deferred.tool_display_name,
                    approval_message: deferred.approval_message,
                    approval_id: deferred.approval_id.clone(),
                })
                .await;

            info!(
                tool = %call.name,
                approval_id = %deferred.approval_id,
                "Waiting for async tool approval"
            );

            let approved = match timeout(
                Duration::from_secs(CONFIRMATION_TIMEOUT_SECS),
                deferred.receiver,
            )
            .await
            {
                Ok(Ok(v)) => v,
                Ok(Err(_)) => false, // sender dropped
                Err(_) => {
                    warn!(
                        tool = %call.name,
                        approval_id = %deferred.approval_id,
                        timeout_s = CONFIRMATION_TIMEOUT_SECS,
                        "Async approval timed out"
                    );
                    false
                }
            };

            if !approved {
                warn!(tool = %call.name, "Tool call denied (async approval)");
                messages.push(ChatMessage::tool_error(
                    &call.id,
                    "The user did not approve this action. Do NOT retry it. \
                     Acknowledge that the action was not approved and ask how they'd like to proceed."
                        .to_string(),
                ));
                let _ = tx
                    .send(LoopEvent::ToolMessage(messages.last().unwrap().clone()))
                    .await;
                continue;
            }

            info!(tool = %call.name, "Tool call approved (async)");

            // Re-dispatch after approval
            let result = tool_dispatcher(call.name.clone(), call.arguments.clone(), true).await;

            match result {
                Err(e) => {
                    warn!(tool = %call.name, "Tool dispatch failed after async approval: {e}");
                    messages.push(ChatMessage::tool_error(
                        &call.id,
                        format!("Tool error: {}", e),
                    ));
                    let _ = tx
                        .send(LoopEvent::ToolMessage(messages.last().unwrap().clone()))
                        .await;
                }
                Ok(ToolDispatchResult::Done(r)) => {
                    info!(tool = %call.name, "Tool call completed after async approval");
                    messages.push(ChatMessage::tool_result(&call.id, r));
                    let _ = tx
                        .send(LoopEvent::ToolMessage(messages.last().unwrap().clone()))
                        .await;
                }
                Ok(ToolDispatchResult::DoneAndReturn(r)) => {
                    info!(
                        tool = %call.name,
                        "Tool call completed after async approval (terminal)"
                    );
                    if terminal_reply.is_none() {
                        terminal_reply = Some(r);
                    }
                }
                _ => {
                    messages.push(ChatMessage::tool_error(
                        &call.id,
                        "Internal error: tool dispatch failed after approval.".to_string(),
                    ));
                    let _ = tx
                        .send(LoopEvent::ToolMessage(messages.last().unwrap().clone()))
                        .await;
                }
            }
        }

        if let Some(reply) = terminal_reply {
            let _ = tx.send(LoopEvent::Token(reply.clone())).await;
            info!(
                iteration,
                iteration_ms = iteration_start.elapsed().as_millis(),
                total_ms = loop_start.elapsed().as_millis(),
                "Tool loop complete — terminal tool result"
            );
            return Ok(LoopOutput {
                reply,
                tool_messages: messages[initial_message_count..].to_vec(),
                iterations_used: iteration,
                elapsed_secs: loop_start.elapsed().as_secs_f64(),
            });
        }
        info!(
            iteration,
            iteration_ms = iteration_start.elapsed().as_millis(),
            "Tool loop iteration done, continuing to next iteration"
        );
        iteration += 1;
        // Continue loop — next iteration will stream the post-tool response
    }
}

const STREAM_CHUNK_TIMEOUT_SECS: u64 = 45;
/// How long to wait for user confirmation before treating as denied.
/// Generous because the user may be reading the approval prompt on their phone.
const CONFIRMATION_TIMEOUT_SECS: u64 = 600;

/// Non-streaming fallback for when streaming fails or produces incomplete data.
/// No artificial timeout — the provider's HTTP client handles connection-level
/// timeouts. Large content generation (PDF, coding agents) can legitimately
/// take minutes.
async fn fallback_chat(
    provider: &Arc<dyn LlmProvider>,
    messages: &[ChatMessage],
    tools: &[ToolSpec],
    temperature: f32,
) -> Result<LlmResponse> {
    provider.chat(messages, tools, temperature).await
}

fn parse_streamed_tool_args(args_json: &str) -> Result<serde_json::Value> {
    if args_json.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json) {
        return Ok(v);
    }

    // Providers can occasionally truncate streamed JSON args at EOF.
    // Repair by appending missing closing braces/brackets.
    let repaired = append_missing_json_closers(args_json);
    serde_json::from_str::<serde_json::Value>(&repaired).map_err(|e| anyhow!(e))
}

fn append_missing_json_closers(input: &str) -> String {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for c in input.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string && c == '\\' {
            escaped = true;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match c {
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                if stack.last() == Some(&c) {
                    stack.pop();
                }
            }
            _ => {}
        }
    }

    let mut out = input.to_string();
    while let Some(ch) = stack.pop() {
        out.push(ch);
    }
    out
}

/// Check if a parsed tool call is missing required fields defined in the tool's
/// JSON Schema.  Returns `true` when at least one required field is absent from
/// the arguments object, which indicates the streamed JSON was syntactically
/// valid but semantically incomplete (e.g. `{"title":"Hello"}` when `count` is
/// also required).
fn missing_required_tool_args(call: &ToolCall, tools: &[ToolSpec]) -> bool {
    let spec = match tools.iter().find(|t| t.name == call.name) {
        Some(s) => s,
        None => return false, // Unknown tool — can't validate, let dispatch handle it
    };

    let required = match spec.parameters.get("required").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return false, // No required fields in schema
    };

    let args_obj = match call.arguments.as_object() {
        Some(o) => o,
        None => return !required.is_empty(), // Non-object args but required fields exist
    };

    for field in required {
        if let Some(name) = field.as_str() {
            // Treat both missing and explicit null as absent
            if !args_obj.contains_key(name) || args_obj[name].is_null() {
                debug!(
                    tool = %call.name,
                    missing_field = %name,
                    "Required tool argument missing from streamed args"
                );
                return true;
            }
        }
    }

    false
}

fn validate_tool_call_args_against_schema(call: &ToolCall, tools: &[ToolSpec]) -> Option<String> {
    let spec = tools.iter().find(|t| t.name == call.name)?;
    validate_value_against_schema(&call.arguments, &spec.parameters, "$")
}

fn validate_value_against_schema(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    path: &str,
) -> Option<String> {
    let expected_type = schema.get("type").and_then(|v| v.as_str());
    if let Some(t) = expected_type {
        let type_ok = match t {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "number" => value.is_number(),
            _ => true,
        };
        if !type_ok {
            return Some(format!("{path} expected {t}"));
        }
    }

    if value.is_object() {
        let obj = value.as_object().expect("checked above");
        if let Some(required) = schema.get("required").and_then(|v| v.as_array()) {
            for field in required {
                if let Some(name) = field.as_str() {
                    if !obj.contains_key(name) || obj[name].is_null() {
                        return Some(format!("{path}.{name} is required"));
                    }
                }
            }
        }
        if let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) {
            for (key, child_schema) in properties {
                let Some(child_value) = obj.get(key) else {
                    continue;
                };
                if child_value.is_null() {
                    continue;
                }
                let child_path = format!("{path}.{key}");
                if let Some(err) =
                    validate_value_against_schema(child_value, child_schema, &child_path)
                {
                    return Some(err);
                }
            }
        }
    }

    if value.is_array() {
        if let Some(item_schema) = schema.get("items") {
            let arr = value.as_array().expect("checked above");
            for (idx, item) in arr.iter().enumerate() {
                let item_path = format!("{path}[{idx}]");
                if let Some(err) = validate_value_against_schema(item, item_schema, &item_path) {
                    return Some(err);
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::parse_streamed_tool_args;
    use super::*;

    #[test]
    fn parses_complete_json_args() {
        let v = parse_streamed_tool_args(r#"{"title":"Hello","count":1}"#).unwrap();
        assert_eq!(v["title"], "Hello");
    }

    #[test]
    fn repairs_truncated_json_args() {
        let v = parse_streamed_tool_args(r#"{"title":"Hello","opts":{"a":1"#).unwrap();
        assert_eq!(v["opts"]["a"], 1);
    }

    fn make_tool_spec(name: &str, required: &[&str]) -> ToolSpec {
        let required_json: Vec<serde_json::Value> =
            required.iter().map(|s| serde_json::json!(s)).collect();
        ToolSpec {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({
                "type": "object",
                "required": required_json,
                "properties": {}
            }),
        }
    }

    #[test]
    fn detects_missing_required_args() {
        let tools = vec![make_tool_spec("send_email", &["to", "subject", "body"])];
        let call = ToolCall {
            id: "c1".to_string(),
            name: "send_email".to_string(),
            arguments: serde_json::json!({"to": "a@b.com"}),
        };
        assert!(missing_required_tool_args(&call, &tools));
    }

    #[test]
    fn passes_when_all_required_present() {
        let tools = vec![make_tool_spec("send_email", &["to", "subject", "body"])];
        let call = ToolCall {
            id: "c1".to_string(),
            name: "send_email".to_string(),
            arguments: serde_json::json!({"to": "a@b.com", "subject": "hi", "body": "hello"}),
        };
        assert!(!missing_required_tool_args(&call, &tools));
    }

    #[test]
    fn unknown_tool_passes_validation() {
        let tools = vec![make_tool_spec("send_email", &["to"])];
        let call = ToolCall {
            id: "c1".to_string(),
            name: "unknown_tool".to_string(),
            arguments: serde_json::json!({}),
        };
        assert!(!missing_required_tool_args(&call, &tools));
    }

    #[test]
    fn null_required_arg_treated_as_missing() {
        let tools = vec![make_tool_spec("send_email", &["to"])];
        let call = ToolCall {
            id: "c1".to_string(),
            name: "send_email".to_string(),
            arguments: serde_json::json!({"to": null}),
        };
        assert!(missing_required_tool_args(&call, &tools));
    }
}
