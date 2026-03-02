use anyhow::Result;
use futures::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use super::provider::{ChatMessage, LlmProvider, LlmResponse, StreamChunk, ToolSpec};

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
        timeout: Duration,
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
    ApprovalQueued { tool_name: String, approval_id: String },
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
/// Uses non-streaming calls to detect and dispatch tool calls, then
/// streams the final text response for real-time token delivery.
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
    tool_dispatcher: impl Fn(String, serde_json::Value, bool) -> futures::future::BoxFuture<'static, Result<ToolDispatchResult>> + Send + Sync + 'static,
) -> Result<String> {
    let max_iterations = 10;
    let loop_start = Instant::now();

    for iteration in 0..max_iterations {
        debug!(iteration, "Tool loop iteration");
        let iter_start = Instant::now();

        if iteration == 0 {
            // ── First iteration: stream directly for real-time token delivery ──
            // This eliminates the previous double-call pattern where we first
            // made a non-streaming call, then re-requested with streaming.
            let stream_start = Instant::now();
            let mut stream = provider.chat_stream(&messages, &tools, temperature).await?;
            debug!(
                elapsed_ms = stream_start.elapsed().as_millis(),
                "LLM stream connection established (iteration 0)"
            );

            let mut streamed_text = String::new();
            let mut tool_call_detected = false;

            while let Some(chunk) = stream.next().await {
                match chunk? {
                    StreamChunk::Text(t) => {
                        streamed_text.push_str(&t);
                        let _ = tx.send(LoopEvent::Token(t)).await;
                    }
                    StreamChunk::ToolCallStart => {
                        // LLM wants to call tools — we need to fall back to a
                        // non-streaming call to get complete tool call arguments.
                        tool_call_detected = true;
                        debug!("Tool call detected during streaming, falling back to non-streaming");
                        break;
                    }
                    StreamChunk::Done => break,
                }
            }

            if !tool_call_detected {
                // Pure text response — we already streamed it, we're done!
                let final_text = if streamed_text.is_empty() {
                    // Edge case: stream returned nothing, fall back to non-streaming
                    debug!("Stream returned no text, falling back to non-streaming call");
                    let response = provider.chat(&messages, &tools, temperature).await?;
                    match response {
                        LlmResponse::Text(text) => {
                            let _ = tx.send(LoopEvent::Token(text.clone())).await;
                            text
                        }
                        LlmResponse::ToolCalls(_) => {
                            // Unexpected — should have been caught by streaming.
                            // Fall through to tool handling below by continuing.
                            String::new()
                        }
                    }
                } else {
                    streamed_text
                };

                if !final_text.is_empty() {
                    debug!(
                        elapsed_ms = iter_start.elapsed().as_millis(),
                        total_ms = loop_start.elapsed().as_millis(),
                        text_len = final_text.len(),
                        "Streaming text response complete (no tool calls)"
                    );
                    let _ = tx.send(LoopEvent::Done).await;
                    return Ok(final_text);
                }
            }

            // Tool call detected — fall through to non-streaming call to get
            // complete tool arguments (streaming doesn't give us full args).
            debug!("Falling back to non-streaming call for tool arguments");
        }

        // ── Non-streaming call for tool iterations or tool fallback ────────
        let llm_start = Instant::now();
        let response = provider.chat(&messages, &tools, temperature).await?;
        debug!(
            iteration,
            elapsed_ms = llm_start.elapsed().as_millis(),
            "LLM non-streaming call completed"
        );

        match response {
            LlmResponse::Text(text) => {
                if iteration == 0 {
                    // We already streamed partial text — send whatever the
                    // non-streaming call returned as the complete response.
                    let _ = tx.send(LoopEvent::Token(text.clone())).await;
                } else {
                    // After tool calls — stream the final response
                    let stream_start = Instant::now();
                    let stream_result = provider.chat_stream(&messages, &tools, temperature).await;
                    match stream_result {
                        Ok(mut stream) => {
                            debug!(
                                elapsed_ms = stream_start.elapsed().as_millis(),
                                "LLM stream connection established (post-tool)"
                            );
                            let mut streamed_text = String::new();
                            while let Some(chunk) = stream.next().await {
                                match chunk? {
                                    StreamChunk::Text(t) => {
                                        streamed_text.push_str(&t);
                                        let _ = tx.send(LoopEvent::Token(t)).await;
                                    }
                                    StreamChunk::ToolCallStart | StreamChunk::Done => break,
                                }
                            }
                            // Use streamed text if available, otherwise the non-streaming text
                            let final_text = if streamed_text.is_empty() {
                                let _ = tx.send(LoopEvent::Token(text.clone())).await;
                                text
                            } else {
                                streamed_text
                            };
                            debug!(
                                total_ms = loop_start.elapsed().as_millis(),
                                iterations = iteration + 1,
                                "Tool loop complete (streamed post-tool response)"
                            );
                            let _ = tx.send(LoopEvent::Done).await;
                            return Ok(final_text);
                        }
                        Err(e) => {
                            // Streaming failed — send the non-streaming text we already have
                            warn!("Post-tool streaming failed, using non-streaming text: {e}");
                            let _ = tx.send(LoopEvent::Token(text.clone())).await;
                        }
                    }
                }
                debug!(
                    total_ms = loop_start.elapsed().as_millis(),
                    iterations = iteration + 1,
                    "Tool loop complete"
                );
                let _ = tx.send(LoopEvent::Done).await;
                return Ok(text);
            }
            LlmResponse::ToolCalls(calls) => {
                // Add the assistant's tool call message with the actual
                // ToolCall data so providers can emit proper toolUse blocks.
                let summary = calls.iter().map(|c| format!("[calling {}]", c.name)).collect::<Vec<_>>().join(", ");
                messages.push(ChatMessage::assistant_with_tool_calls(summary, calls.clone()));

                for call in &calls {
                    let status = format!("Using {}...", call.name);
                    info!(tool = %call.name, "LLM requesting tool call");
                    let _ = tx.send(LoopEvent::CapabilityStatus(status)).await;

                    // ── Dispatch with policy enforcement ──────────────────────
                    let dispatch_start = Instant::now();
                    let dispatch_result = tool_dispatcher(call.name.clone(), call.arguments.clone(), false)
                        .await
                        .unwrap_or_else(|e| ToolDispatchResult::Done(format!("Tool error: {}", e)));

                    match dispatch_result {
                        ToolDispatchResult::Done(result) => {
                            debug!(
                                tool = %call.name,
                                elapsed_ms = dispatch_start.elapsed().as_millis(),
                                "Tool call completed"
                            );
                            messages.push(ChatMessage::tool_result(&call.id, result));
                        }

                        ToolDispatchResult::Blocked(msg) => {
                            warn!(tool = %call.name, "Tool call blocked by policy");
                            messages.push(ChatMessage::tool_result(&call.id, msg));
                        }

                        ToolDispatchResult::NeedsConfirmation => {
                            // Interactive channel — prompt via SSE
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
                                .unwrap_or_else(|e| ToolDispatchResult::Done(format!("Tool error: {}", e)));

                            match result {
                                ToolDispatchResult::Done(r) => {
                                    debug!(tool = %call.name, "Tool call completed after confirmation");
                                    messages.push(ChatMessage::tool_result(&call.id, r));
                                }
                                _ => {
                                    // Shouldn't happen after approval — treat as error
                                    messages.push(ChatMessage::tool_result(
                                        &call.id,
                                        "Internal error: tool dispatch failed after approval.".to_string(),
                                    ));
                                }
                            }
                        }

                        ToolDispatchResult::Queued { approval_id, receiver, timeout } => {
                            // Non-interactive threaded channel — wait for async approval
                            let _ = tx.send(LoopEvent::ApprovalQueued {
                                tool_name: call.name.clone(),
                                approval_id: approval_id.clone(),
                            }).await;

                            info!(
                                tool = %call.name,
                                approval_id = %approval_id,
                                timeout_secs = timeout.as_secs(),
                                "Waiting for async tool approval"
                            );

                            let approved = match tokio::time::timeout(timeout, receiver).await {
                                Ok(Ok(v)) => v,
                                Ok(Err(_)) => false, // sender dropped
                                Err(_) => {
                                    warn!(tool = %call.name, "Async approval timed out");
                                    false
                                }
                            };

                            if !approved {
                                warn!(tool = %call.name, "Tool call denied (async approval)");
                                messages.push(ChatMessage::tool_result(
                                    &call.id,
                                    "The user did not approve this action in time. Do NOT retry it. \
                                     Acknowledge that the action was not approved and ask how they'd like to proceed."
                                        .to_string(),
                                ));
                                continue;
                            }

                            info!(tool = %call.name, "Tool call approved (async)");

                            // Re-dispatch after approval
                            let result = tool_dispatcher(call.name.clone(), call.arguments.clone(), true)
                                .await
                                .unwrap_or_else(|e| ToolDispatchResult::Done(format!("Tool error: {}", e)));

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
                    }
                }
                // Continue loop — next iteration will get the final response
            }
        }
    }

    let _ = tx.send(LoopEvent::Error("Max tool iterations reached".into())).await;
    let _ = tx.send(LoopEvent::Done).await;
    Ok(String::new())
}
