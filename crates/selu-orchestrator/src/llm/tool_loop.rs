use anyhow::Result;
use futures::StreamExt;
use futures::future::join_all;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

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

    /// User's policy says "block" (or no policy is set — secure default).
    /// The string is a denial message for the LLM.
    Blocked(String),

    /// User's policy says "ask" and the caller is on an interactive channel
    /// (web chat with an active SSE stream). The tool loop should prompt
    /// the user via `LoopEvent::ConfirmationRequired`.
    NeedsConfirmation,

    /// User's policy says "ask" and the caller is on a non-interactive
    /// threaded channel (iMessage, webhook). An approval prompt has already
    /// been sent to the user. The tool loop should wait on the receiver
    /// with the given timeout.
    Queued {
        approval_id: String,
        receiver: oneshot::Receiver<bool>,
    },
}

// ── Loop events ───────────────────────────────────────────────────────────────

/// A pending confirmation request sent to the caller for approval.
#[derive(Debug)]
pub struct ConfirmationRequest {
    /// The tool name the LLM wants to call (namespaced, e.g. `"pim-api__send_email"`)
    pub tool_name: String,
    /// The arguments the LLM passed to the tool
    pub arguments: serde_json::Value,
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
        tool_name: String,
        approval_id: String,
    },
    /// Final complete text response
    Done,
    /// An error occurred
    Error(String),
}

// LoopEvent is no longer Clone because of the oneshot sender -- callers that
// need to forward events should destructure and re-wrap as needed.

pub type LoopSender = mpsc::Sender<LoopEvent>;

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
    tx: LoopSender,
    tool_dispatcher: impl Fn(
        String,
        serde_json::Value,
        bool,
    ) -> futures::future::BoxFuture<'static, Result<ToolDispatchResult>>
    + Send
    + Sync
    + 'static,
) -> Result<String> {
    let loop_start = Instant::now();
    let mut iteration: u32 = 0;

    loop {
        debug!(iteration, "Tool loop iteration");

        // ── State for accumulating tool calls from the stream ─────────────
        // Each entry: (id, name, accumulated_arguments_json)
        let mut pending_tool_calls: Vec<(String, String, String)> = Vec::new();
        let mut streamed_text = String::new();
        let mut stream_ok = true;

        // ── Stream the response ──────────────────────────────────────────
        match provider.chat_stream(&messages, &tools, temperature).await {
            Ok(mut stream) => {
                let stream_start = Instant::now();
                let mut got_first_token = false;

                while let Some(chunk) = stream.next().await {
                    match chunk? {
                        StreamChunk::Text(t) => {
                            if !got_first_token {
                                debug!(
                                    iteration,
                                    elapsed_ms = stream_start.elapsed().as_millis(),
                                    "First token received"
                                );
                                got_first_token = true;
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
                                debug!(
                                    iteration,
                                    elapsed_ms = stream_start.elapsed().as_millis(),
                                    "First token received (tool call)"
                                );
                                got_first_token = true;
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
                            if let Some(name) = name {
                                pending_tool_calls[index].1 = name;
                            }
                            pending_tool_calls[index].2.push_str(&arguments_delta);
                        }
                        StreamChunk::Done => {
                            debug!(
                                iteration,
                                elapsed_ms = stream_start.elapsed().as_millis(),
                                text_len = streamed_text.len(),
                                tool_calls = pending_tool_calls.len(),
                                "Stream complete"
                            );
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                warn!(iteration, "Streaming failed, falling back: {e}");
                stream_ok = false;
            }
        }

        // ── Handle the streamed result ───────────────────────────────────

        if stream_ok && pending_tool_calls.is_empty() {
            // Pure text response — we're done
            if !streamed_text.is_empty() {
                debug!(
                    total_ms = loop_start.elapsed().as_millis(),
                    iterations = iteration + 1,
                    text_len = streamed_text.len(),
                    "Tool loop complete (streamed)"
                );
                let _ = tx.send(LoopEvent::Done).await;
                return Ok(streamed_text);
            }
            // Stream completed but returned no text and no tool calls — fall through to non-streaming
            debug!(
                iteration,
                "Stream returned no content, falling back to non-streaming"
            );
        }

        // ── Build ToolCall objects from stream deltas (or fall back) ─────

        let calls: Vec<ToolCall> = if !pending_tool_calls.is_empty() {
            pending_tool_calls
                .into_iter()
                .filter(|(_, name, _)| !name.is_empty())
                .map(|(id, name, args_json)| {
                    let arguments: serde_json::Value = if args_json.trim().is_empty() {
                        // Tool calls with no arguments (e.g. tools that take no params):
                        // the LLM may emit contentBlockStart but no contentBlockDelta,
                        // leaving the accumulated string empty. Default to `{}`.
                        serde_json::json!({})
                    } else {
                        serde_json::from_str(&args_json).unwrap_or_else(|e| {
                            warn!(
                                tool = %name,
                                raw_len = args_json.len(),
                                error = %e,
                                raw_start = &args_json[..args_json.len().min(200)],
                                "Failed to parse streamed tool args, wrapping as object"
                            );
                            // Wrap in an object so providers that require JSON objects
                            // (e.g. Bedrock's toolUse.input) don't reject the message.
                            serde_json::json!({ "raw_input": args_json })
                        })
                    };
                    ToolCall {
                        id: if id.is_empty() {
                            format!("call_{}", uuid::Uuid::new_v4())
                        } else {
                            id
                        },
                        name,
                        arguments,
                    }
                })
                .collect()
        } else {
            // Fallback: streaming failed entirely or returned nothing useful.
            // Use a non-streaming call as last resort.
            let llm_start = Instant::now();
            let response = provider.chat(&messages, &tools, temperature).await?;
            debug!(
                iteration,
                elapsed_ms = llm_start.elapsed().as_millis(),
                "LLM non-streaming fallback completed"
            );

            match response {
                LlmResponse::Text(text) => {
                    let _ = tx.send(LoopEvent::Token(text.clone())).await;
                    let _ = tx.send(LoopEvent::Done).await;
                    debug!(
                        total_ms = loop_start.elapsed().as_millis(),
                        iterations = iteration + 1,
                        "Tool loop complete (non-streaming fallback)"
                    );
                    return Ok(text);
                }
                LlmResponse::ToolCalls(c) => c,
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

        // Send all status events upfront so the UI shows activity immediately
        for call in &calls {
            info!(tool = %call.name, "LLM requesting tool call");
            let status = format!("Using {}...", call.name);
            let _ = tx.send(LoopEvent::CapabilityStatus(status)).await;
        }

        // Dispatch all tool calls in parallel. The dispatcher checks policy
        // and invokes the tool (for Allow) or returns NeedsConfirmation/Queued
        // immediately (for Ask) — so parallel dispatch is safe.
        let dispatch_start = Instant::now();
        let dispatch_futures: Vec<_> = calls
            .iter()
            .map(|call| tool_dispatcher(call.name.clone(), call.arguments.clone(), false))
            .collect();
        let dispatch_results = join_all(dispatch_futures).await;

        debug!(
            count = calls.len(),
            elapsed_ms = dispatch_start.elapsed().as_millis(),
            "Parallel tool dispatch complete"
        );

        // Process results: Done/Blocked are pushed immediately, while
        // NeedsConfirmation/Queued are collected for sequential handling
        // (they require user interaction which is inherently serial).
        struct DeferredConfirmation<'a> {
            call: &'a ToolCall,
        }
        struct DeferredQueued<'a> {
            call: &'a ToolCall,
            approval_id: String,
            receiver: oneshot::Receiver<bool>,
        }
        let mut deferred_confirmations: Vec<DeferredConfirmation<'_>> = Vec::new();
        let mut deferred_queued: Vec<DeferredQueued<'_>> = Vec::new();

        for (call, result) in calls.iter().zip(dispatch_results) {
            let dispatch_result = result.unwrap_or_else(|e| {
                warn!(tool = %call.name, "Tool dispatch failed: {e}");
                ToolDispatchResult::Done(format!("Tool error: {}", e))
            });

            match dispatch_result {
                ToolDispatchResult::Done(result) => {
                    debug!(
                        tool = %call.name,
                        "Tool call completed"
                    );
                    messages.push(ChatMessage::tool_result(&call.id, result));
                }

                ToolDispatchResult::Blocked(msg) => {
                    warn!(tool = %call.name, "Tool call blocked by policy");
                    messages.push(ChatMessage::tool_result(&call.id, msg));
                }

                ToolDispatchResult::NeedsConfirmation => {
                    deferred_confirmations.push(DeferredConfirmation { call });
                }

                ToolDispatchResult::Queued {
                    approval_id,
                    receiver,
                } => {
                    deferred_queued.push(DeferredQueued {
                        call,
                        approval_id,
                        receiver,
                    });
                }
            }
        }

        // Handle interactive confirmations sequentially (user approves one at a time)
        for deferred in deferred_confirmations {
            let call = deferred.call;
            let (confirm_tx, confirm_rx) = oneshot::channel::<bool>();
            let req = ConfirmationRequest {
                tool_name: call.name.clone(),
                arguments: call.arguments.clone(),
                reply: confirm_tx,
            };
            let _ = tx.send(LoopEvent::ConfirmationRequired(req)).await;

            let approved = confirm_rx.await.unwrap_or(false);
            if !approved {
                warn!(tool = %call.name, "Tool call denied by user (interactive)");
                messages.push(ChatMessage::tool_result(
                    &call.id,
                    "The user denied this action. Do NOT retry it. \
                     Acknowledge the denial and ask how they'd like to proceed."
                        .to_string(),
                ));
                continue;
            }
            info!(tool = %call.name, "Tool call approved by user (interactive)");

            // Re-dispatch — the user already approved, so the
            // dispatcher should skip the policy check and invoke directly.
            let result = tool_dispatcher(call.name.clone(), call.arguments.clone(), true)
                .await
                .unwrap_or_else(|e| {
                    warn!(tool = %call.name, "Tool dispatch failed after confirmation: {e}");
                    ToolDispatchResult::Done(format!("Tool error: {}", e))
                });

            match result {
                ToolDispatchResult::Done(r) => {
                    debug!(tool = %call.name, "Tool call completed after confirmation");
                    messages.push(ChatMessage::tool_result(&call.id, r));
                }
                _ => {
                    messages.push(ChatMessage::tool_result(
                        &call.id,
                        "Internal error: tool dispatch failed after approval.".to_string(),
                    ));
                }
            }
        }

        // Handle async approvals sequentially (non-interactive channels)
        for deferred in deferred_queued {
            let call = deferred.call;
            let _ = tx
                .send(LoopEvent::ApprovalQueued {
                    tool_name: call.name.clone(),
                    approval_id: deferred.approval_id.clone(),
                })
                .await;

            info!(
                tool = %call.name,
                approval_id = %deferred.approval_id,
                "Waiting for async tool approval"
            );

            let approved = match deferred.receiver.await {
                Ok(v) => v,
                Err(_) => false, // sender dropped
            };

            if !approved {
                warn!(tool = %call.name, "Tool call denied (async approval)");
                messages.push(ChatMessage::tool_result(
                    &call.id,
                    "The user did not approve this action. Do NOT retry it. \
                     Acknowledge that the action was not approved and ask how they'd like to proceed."
                        .to_string(),
                ));
                continue;
            }

            info!(tool = %call.name, "Tool call approved (async)");

            // Re-dispatch after approval
            let result = tool_dispatcher(call.name.clone(), call.arguments.clone(), true)
                .await
                .unwrap_or_else(|e| {
                    warn!(tool = %call.name, "Tool dispatch failed after async approval: {e}");
                    ToolDispatchResult::Done(format!("Tool error: {}", e))
                });

            match result {
                ToolDispatchResult::Done(r) => {
                    debug!(tool = %call.name, "Tool call completed after async approval");
                    messages.push(ChatMessage::tool_result(&call.id, r));
                }
                _ => {
                    messages.push(ChatMessage::tool_result(
                        &call.id,
                        "Internal error: tool dispatch failed after approval.".to_string(),
                    ));
                }
            }
        }
        iteration += 1;
        // Continue loop — next iteration will stream the post-tool response
    }
}
