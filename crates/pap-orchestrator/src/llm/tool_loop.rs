use anyhow::Result;
use futures::StreamExt;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use super::provider::{ChatMessage, LlmProvider, LlmResponse, StreamChunk, ToolSpec};

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
/// `confirmation_tools` contains the namespaced names of tools that require
/// explicit user approval before dispatch (e.g. `"pim-api__send_email"`).
/// When the set is empty, no confirmation is ever requested.
pub async fn run_loop(
    provider: Arc<dyn LlmProvider>,
    mut messages: Vec<ChatMessage>,
    tools: Vec<ToolSpec>,
    temperature: f32,
    tx: LoopSender,
    confirmation_tools: HashSet<String>,
    tool_dispatcher: impl Fn(String, serde_json::Value) -> futures::future::BoxFuture<'static, Result<String>> + Send + Sync + 'static,
) -> Result<String> {
    let max_iterations = 10;

    for iteration in 0..max_iterations {
        debug!(iteration, "Tool loop iteration");

        // ── Non-streaming call to get a complete response ─────────────────────
        // This reliably returns either Text or ToolCalls with full arguments.
        let response = provider.chat(&messages, &tools, temperature).await?;

        match response {
            LlmResponse::Text(text) => {
                // Final text response — stream it to the user.
                // If this is iteration 0 (no tool calls happened), try streaming
                // for real-time token delivery. Otherwise, send the already-
                // obtained text as a single token.
                if iteration == 0 {
                    // No tool calls yet — re-request with streaming for live tokens.
                    // The response should be the same text (same messages, no tools triggered).
                    let mut stream = provider.chat_stream(&messages, &tools, temperature).await?;
                    let mut streamed_text = String::new();

                    while let Some(chunk) = stream.next().await {
                        match chunk? {
                            StreamChunk::Text(t) => {
                                streamed_text.push_str(&t);
                                let _ = tx.send(LoopEvent::Token(t)).await;
                            }
                            StreamChunk::ToolCallStart(_) => {
                                // Unexpected tool call during streaming — fall back to
                                // the text we already got from the non-streaming call.
                                break;
                            }
                            StreamChunk::Done => break,
                        }
                    }

                    // Use streamed text if we got some, otherwise fall back to
                    // the non-streaming response
                    let final_text = if streamed_text.is_empty() {
                        let _ = tx.send(LoopEvent::Token(text.clone())).await;
                        text
                    } else {
                        streamed_text
                    };

                    let _ = tx.send(LoopEvent::Done).await;
                    return Ok(final_text);
                } else {
                    // After tool calls — send the text we already have
                    let _ = tx.send(LoopEvent::Token(text.clone())).await;
                    let _ = tx.send(LoopEvent::Done).await;
                    return Ok(text);
                }
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

                    // ── Confirmation gate ─────────────────────────────────────
                    if confirmation_tools.contains(&call.name) {
                        let (confirm_tx, confirm_rx) = oneshot::channel::<bool>();
                        let req = ConfirmationRequest {
                            tool_name: call.name.clone(),
                            arguments: call.arguments.clone(),
                            reply: confirm_tx,
                        };
                        let _ = tx.send(LoopEvent::ConfirmationRequired(req)).await;

                        let approved = confirm_rx.await.unwrap_or(false);
                        if !approved {
                            warn!(tool = %call.name, "Tool call denied by user");
                            messages.push(ChatMessage::tool_result(
                                &call.id,
                                "The user denied this action. Do NOT retry it. \
                                 Acknowledge the denial and ask how they'd like to proceed."
                                    .to_string(),
                            ));
                            continue;
                        }
                        info!(tool = %call.name, "Tool call approved by user");
                    }

                    // Dispatch the tool call
                    let result = tool_dispatcher(call.name.clone(), call.arguments.clone()).await
                        .unwrap_or_else(|e| format!("Tool error: {}", e));

                    debug!(tool = %call.name, "Tool call completed");
                    messages.push(ChatMessage::tool_result(&call.id, result));
                }
                // Continue loop — next iteration will get the final response
            }
        }
    }

    let _ = tx.send(LoopEvent::Error("Max tool iterations reached".into())).await;
    let _ = tx.send(LoopEvent::Done).await;
    Ok(String::new())
}
