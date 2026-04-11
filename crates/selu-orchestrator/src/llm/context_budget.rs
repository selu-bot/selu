//! Context-window budget trimmer.
//!
//! Estimates token usage for a `Vec<ChatMessage>` and trims it so the total
//! stays within the provider's context limit, leaving room for the response.

use tracing::{debug, info};

use crate::llm::provider::{ChatMessage, LlmProvider, MessageContent};

/// Tokens reserved for the model's response (matches the hard-coded max_tokens
/// sent to Anthropic / Bedrock).
const RESPONSE_RESERVE: usize = 16_384;

/// Safety margin so we don't cut it too close to the provider's hard limit.
const SAFETY_MARGIN: usize = 2_048;

/// Individual tool-result messages longer than this (in estimated tokens) are
/// truncated.  12 288 tokens ≈ ~50 KB of text — generous for most tool output.
const PER_TOOL_RESULT_CAP: usize = 12_288;

/// Rough token estimate: 1 token ≈ 4 characters.
/// Conservative for English + JSON; slightly over-counts, which is safer than
/// under-counting.
fn estimate_tokens(text: &str) -> usize {
    // Ceiling division so even a 1-char string counts as 1 token.
    (text.len() + 3) / 4
}

/// Estimate tokens for a single message (all content variants).
fn message_tokens(msg: &ChatMessage) -> usize {
    let content_tokens = match &msg.content {
        MessageContent::Text(s) => estimate_tokens(s),
        MessageContent::Parts(parts) => parts
            .iter()
            .map(|p| {
                let text_tok = p.text.as_deref().map(estimate_tokens).unwrap_or(0);
                // Base64-encoded images: the token cost is dominated by the
                // vision encoder, not the base64 length.  Use a fixed 1000-token
                // estimate per image (close to reality for most providers).
                let img_tok = if p.image_base64.is_some() { 1_000 } else { 0 };
                text_tok + img_tok
            })
            .sum(),
    };

    // Per-message overhead: role field, separators, tool_call metadata.
    let overhead = 4 + msg.tool_calls.len() * 20;

    // Tool calls carry their own argument payloads.
    let tc_tokens: usize = msg
        .tool_calls
        .iter()
        .map(|tc| estimate_tokens(&tc.arguments.to_string()) + estimate_tokens(&tc.name) + 10)
        .sum();

    content_tokens + tc_tokens + overhead
}

/// Trim `messages` so the total estimated token count fits within the
/// provider's context window (minus response reserve and safety margin).
///
/// Strategy (applied in order):
/// 1. Truncate individual tool-result messages that exceed `PER_TOOL_RESULT_CAP`.
/// 2. If still over budget, drop the oldest non-system messages one by one,
///    ensuring tool_use / tool_result pairs are dropped together.
pub fn trim_to_budget(messages: &mut Vec<ChatMessage>, provider: &dyn LlmProvider) {
    let max_input = provider
        .max_context_tokens()
        .saturating_sub(RESPONSE_RESERVE)
        .saturating_sub(SAFETY_MARGIN);

    if max_input == 0 {
        return;
    }

    // ── Phase 1: truncate oversized tool results ─────────────────────────
    let mut phase1_truncated = 0usize;
    let char_cap = PER_TOOL_RESULT_CAP * 4; // convert token cap back to chars
    for msg in messages.iter_mut() {
        if msg.tool_call_id.is_some() {
            if let MessageContent::Text(ref mut text) = msg.content {
                if text.len() > char_cap {
                    phase1_truncated += 1;
                    text.truncate(char_cap);
                    text.push_str("\n\n[… truncated to fit context window]");
                }
            }
        }
    }

    // ── Phase 2: drop oldest non-system messages if still over budget ────
    let total: usize = messages.iter().map(message_tokens).sum();
    if total <= max_input {
        if phase1_truncated > 0 {
            info!(
                phase1_truncated,
                estimated_tokens = total,
                budget = max_input,
                "Context budget: truncated oversized tool results, now within budget"
            );
        }
        return;
    }

    // Find the boundary between system messages and history.
    // System messages are always at the front and must be kept.
    let history_start = messages
        .iter()
        .position(|m| m.role != "system")
        .unwrap_or(messages.len());

    // Build a set of tool_call IDs present in assistant messages so we can
    // drop tool_result messages together with their tool_use when removing
    // old history.
    let mut to_drop = 0usize;
    let mut freed = 0usize;
    let overshoot = total - max_input;

    // Walk from oldest history message forward, marking messages for removal.
    // We track tool_call IDs of dropped assistant messages so their paired
    // tool_results are also dropped.
    let mut dropped_tc_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let history = &messages[history_start..];

    for msg in history {
        if freed >= overshoot {
            break;
        }
        freed += message_tokens(msg);
        to_drop += 1;

        // If this is an assistant message with tool calls, record the IDs
        // so we also drop their tool_results even if they come later.
        for tc in &msg.tool_calls {
            dropped_tc_ids.insert(tc.id.clone());
        }
    }

    // Also drop any tool_result messages whose tool_use was dropped but that
    // sit beyond the to_drop boundary (orphaned results).
    if !dropped_tc_ids.is_empty() {
        let remaining = &messages[history_start + to_drop..];
        let mut extra_drop_indices: Vec<usize> = Vec::new();
        for (i, msg) in remaining.iter().enumerate() {
            if let Some(ref tc_id) = msg.tool_call_id {
                if dropped_tc_ids.contains(tc_id) {
                    extra_drop_indices.push(history_start + to_drop + i);
                    freed += message_tokens(msg);
                }
            }
        }
        // Remove from back to front so indices stay valid.
        for &idx in extra_drop_indices.iter().rev() {
            messages.remove(idx);
        }
    }

    // Remove the contiguous oldest block.
    if to_drop > 0 {
        messages.drain(history_start..history_start + to_drop);
    }

    let final_tokens: usize = messages.iter().map(message_tokens).sum();
    info!(
        phase1_truncated,
        phase2_dropped = to_drop,
        freed_tokens = freed,
        final_tokens,
        budget = max_input,
        "Context budget: trimmed history to fit within provider limit"
    );

    debug!(
        remaining_messages = messages.len(),
        system_messages = history_start,
        "Context budget trimming complete"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::provider::{
        ChatMessage, ChunkStream, LlmProvider, LlmResponse, StreamChunk, ToolSpec,
    };
    use anyhow::Result;
    use async_trait::async_trait;

    #[test]
    fn no_trim_when_within_budget() {
        struct BigProvider;
        #[async_trait]
        impl LlmProvider for BigProvider {
            fn id(&self) -> &str {
                "big"
            }
            fn max_context_tokens(&self) -> usize {
                200_000
            }
            async fn chat(
                &self,
                _: &[ChatMessage],
                _: &[ToolSpec],
                _: f32,
            ) -> Result<LlmResponse> {
                Ok(LlmResponse::Text(String::new()))
            }
            async fn chat_stream(
                &self,
                _: &[ChatMessage],
                _: &[ToolSpec],
                _: f32,
            ) -> Result<ChunkStream> {
                Ok(Box::pin(futures::stream::iter(vec![Ok(StreamChunk::Done)])))
            }
        }

        let mut msgs = vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("Hello"),
            ChatMessage::assistant("Hi there!"),
        ];
        let original_len = msgs.len();
        trim_to_budget(&mut msgs, &BigProvider);
        assert_eq!(msgs.len(), original_len);
    }

    #[test]
    fn trims_oldest_history_when_over_budget() {
        let mut msgs = vec![
            ChatMessage::system("system"),
            ChatMessage::user("old message 1"),
            ChatMessage::assistant("old reply 1"),
            ChatMessage::user("old message 2"),
            ChatMessage::assistant("old reply 2"),
            // This huge message blows the budget (8000 chars ≈ 2000 tokens)
            ChatMessage::user(&"x".repeat(8000)),
        ];
        // SmallProvider has 1000 token budget, minus 16384+2048 reserves =
        // saturates to 0, but let's use a slightly bigger provider.
        struct TightProvider;
        #[async_trait]
        impl LlmProvider for TightProvider {
            fn id(&self) -> &str {
                "tight"
            }
            fn max_context_tokens(&self) -> usize {
                // 20K tokens total → ~1500 available after reserves
                20_000
            }
            async fn chat(
                &self,
                _: &[ChatMessage],
                _: &[ToolSpec],
                _: f32,
            ) -> Result<LlmResponse> {
                Ok(LlmResponse::Text(String::new()))
            }
            async fn chat_stream(
                &self,
                _: &[ChatMessage],
                _: &[ToolSpec],
                _: f32,
            ) -> Result<ChunkStream> {
                Ok(Box::pin(futures::stream::iter(vec![Ok(StreamChunk::Done)])))
            }
        }

        let original_len = msgs.len();
        trim_to_budget(&mut msgs, &TightProvider);
        // Should have dropped some old messages but kept system + newest
        assert!(msgs.len() < original_len);
        assert_eq!(msgs[0].role, "system");
    }

    #[test]
    fn truncates_oversized_tool_result() {
        struct BigProvider;
        #[async_trait]
        impl LlmProvider for BigProvider {
            fn id(&self) -> &str {
                "big"
            }
            fn max_context_tokens(&self) -> usize {
                200_000
            }
            async fn chat(
                &self,
                _: &[ChatMessage],
                _: &[ToolSpec],
                _: f32,
            ) -> Result<LlmResponse> {
                Ok(LlmResponse::Text(String::new()))
            }
            async fn chat_stream(
                &self,
                _: &[ChatMessage],
                _: &[ToolSpec],
                _: f32,
            ) -> Result<ChunkStream> {
                Ok(Box::pin(futures::stream::iter(vec![Ok(StreamChunk::Done)])))
            }
        }

        // 60K chars → ~15K tokens, exceeds PER_TOOL_RESULT_CAP of 12288
        let huge_result = "a".repeat(60_000);
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::tool_result("call_1", &huge_result),
        ];

        trim_to_budget(&mut msgs, &BigProvider);

        if let MessageContent::Text(ref text) = msgs[1].content {
            // Should be truncated to ~49152 chars + truncation marker
            assert!(text.len() < 60_000);
            assert!(text.contains("[… truncated to fit context window]"));
        } else {
            panic!("expected text content");
        }
    }
}
