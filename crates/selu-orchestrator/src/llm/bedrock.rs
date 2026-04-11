/// Amazon Bedrock Converse API provider.
///
/// Uses the Bedrock Converse endpoint with Bearer token authentication.
/// Endpoint: POST https://bedrock-runtime.{region}.amazonaws.com/model/{model-id}/converse
///
/// The Bedrock Converse API has its own message format:
///   - Messages have `role` and `content` (array of content blocks)
///   - Content blocks: `{"text": "..."}` or `{"toolUse": {...}}` or `{"toolResult": {...}}`
///   - Tools are declared in a `toolConfig` object
///   - System prompts go in a separate `system` array
///
/// Streaming uses /converse-stream and returns AWS Event Stream binary frames.
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use aws_smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder};
use aws_smithy_types::event_stream::HeaderValue;
use base64::Engine;
use bytes::BytesMut;
use futures::{StreamExt, stream};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, trace, warn};

use super::provider::{
    ChatMessage, ChunkStream, ImageConstraints, LlmProvider, LlmResponse, MessageContent,
    StreamChunk, ToolCall, ToolSpec,
};

const BEDROCK_MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

pub struct BedrockProvider {
    client: Client,
    api_key: String,
    region: String,
    model_id: String,
}

impl BedrockProvider {
    pub fn new(
        api_key: impl Into<String>,
        region: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        let model = model_id.into();
        Self {
            client: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .pool_max_idle_per_host(4)
                .pool_idle_timeout(std::time::Duration::from_secs(90))
                .build()
                .expect("Failed to build HTTP client"),
            api_key: api_key.into(),
            region: region.into(),
            model_id: if model.is_empty() {
                "us.anthropic.claude-sonnet-4-20250514-v1:0".into()
            } else {
                model
            },
        }
    }

    fn converse_url(&self) -> String {
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/converse",
            self.region, self.model_id
        )
    }

    fn converse_stream_url(&self) -> String {
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/converse-stream",
            self.region, self.model_id
        )
    }
}

fn sanitize_bedrock_input_schema(schema: &Value) -> Value {
    let mut out = schema.clone();
    if let Some(obj) = out.as_object_mut() {
        let had_any_of = obj.remove("anyOf").is_some();
        let had_one_of = obj.remove("oneOf").is_some();
        let had_all_of = obj.remove("allOf").is_some();
        if had_any_of || had_one_of || had_all_of {
            warn!(
                had_any_of,
                had_one_of,
                had_all_of,
                "Bedrock tool input schema does not support top-level anyOf/oneOf/allOf; stripped unsupported keys"
            );
        }
    }
    out
}

fn bedrock_image_format(media_type: &str) -> &str {
    match media_type
        .split('/')
        .nth(1)
        .unwrap_or("png")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "jpeg",
        "png" => "png",
        "gif" => "gif",
        "webp" => "webp",
        _ => "png",
    }
}

fn estimate_base64_decoded_len(data: &str) -> usize {
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return 0;
    }
    let padding = trimmed
        .as_bytes()
        .iter()
        .rev()
        .take_while(|&&b| b == b'=')
        .count();
    (trimmed.len() / 4) * 3usize - padding.min(2)
}

/// Build the Bedrock Converse request body from our internal message types.
fn build_converse_body(messages: &[ChatMessage], tools: &[ToolSpec], temperature: f32) -> Value {
    // Extract system prompts
    let system: Vec<Value> = messages
        .iter()
        .filter(|m| m.role == "system")
        .map(|m| json!({"text": m.content.as_text()}))
        .collect();

    // Convert messages to Bedrock format, merging consecutive tool-result
    // messages into a single "user" message (Bedrock requires all toolResult
    // blocks for one turn to live in one message).
    let non_system: Vec<&ChatMessage> = messages.iter().filter(|m| m.role != "system").collect();

    let mut bedrock_messages: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < non_system.len() {
        let m = non_system[i];

        if m.role == "assistant" && !m.tool_calls.is_empty() {
            // Assistant message with tool calls → emit toolUse content blocks
            let mut content_blocks: Vec<Value> = Vec::new();
            let text = m.content.as_text();
            if !text.is_empty() {
                content_blocks.push(json!({"text": text}));
            }
            for tc in &m.tool_calls {
                content_blocks.push(json!({
                    "toolUse": {
                        "toolUseId": tc.id,
                        "name": tc.name,
                        "input": tc.arguments
                    }
                }));
            }
            bedrock_messages.push(json!({"role": "assistant", "content": content_blocks}));
        } else if m.role == "tool" {
            // Collect all consecutive tool-result messages into one "user" message
            let mut tool_result_blocks: Vec<Value> = Vec::new();
            while i < non_system.len() && non_system[i].role == "tool" {
                let tr = non_system[i];
                tool_result_blocks.push(json!({
                    "toolResult": {
                        "toolUseId": tr.tool_call_id.as_deref().unwrap_or("unknown"),
                        "content": [{"text": tr.content.as_text()}]
                    }
                }));
                i += 1;
            }
            bedrock_messages.push(json!({"role": "user", "content": tool_result_blocks}));
            continue; // skip the i += 1 at bottom since we advanced i in the inner loop
        } else {
            // Plain user or assistant text message
            let content = match &m.content {
                MessageContent::Text(t) => json!([{"text": t}]),
                MessageContent::Parts(parts) => {
                    let blocks: Vec<Value> = parts
                        .iter()
                        .filter_map(|p| {
                            if p.part_type == "text" {
                                return p.text.as_ref().map(|t| json!({"text": t}));
                            }
                            if let Some(data) = p.image_base64.as_ref() {
                                let media_type = p.media_type.as_deref().unwrap_or("image/png");
                                let image_size = base64::engine::general_purpose::STANDARD
                                    .decode(data)
                                    .map(|bytes| bytes.len())
                                    .unwrap_or_else(|_| estimate_base64_decoded_len(data));
                                if image_size > BEDROCK_MAX_IMAGE_BYTES {
                                    warn!(
                                        image_size,
                                        max_bytes = BEDROCK_MAX_IMAGE_BYTES,
                                        "Skipping oversized image for Bedrock request"
                                    );
                                    return None;
                                }
                                return Some(json!({
                                    "image": {
                                        "format": bedrock_image_format(media_type),
                                        "source": { "bytes": data }
                                    }
                                }));
                            }
                            p.image_url
                                .as_ref()
                                .map(|url| json!({"text": format!("Image URL: {}", url)}))
                        })
                        .collect();
                    json!(blocks)
                }
            };
            bedrock_messages.push(json!({"role": &m.role, "content": content}));
        }

        i += 1;
    }

    let mut body = json!({
        "messages": bedrock_messages,
        "inferenceConfig": {
            "temperature": temperature,
            "maxTokens": 16384,
        }
    });

    if !system.is_empty() {
        body["system"] = json!(system);
    }

    if !tools.is_empty() {
        let tool_defs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "toolSpec": {
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": {
                            "json": sanitize_bedrock_input_schema(&t.parameters)
                        }
                    }
                })
            })
            .collect();
        body["toolConfig"] = json!({"tools": tool_defs});
    }

    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::provider::{ChatMessage, ContentPart, MessageContent};

    #[test]
    fn estimates_base64_length() {
        assert_eq!(estimate_base64_decoded_len(""), 0);
        assert_eq!(estimate_base64_decoded_len("TQ=="), 1);
        assert_eq!(estimate_base64_decoded_len("TWE="), 2);
        assert_eq!(estimate_base64_decoded_len("TWFu"), 3);
    }

    #[test]
    fn omits_oversized_images_from_bedrock_payload() {
        let too_big_b64 = "A".repeat(((BEDROCK_MAX_IMAGE_BYTES + 1).div_ceil(3)) * 4);
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: MessageContent::Parts(vec![ContentPart::image_base64(
                "image/png",
                too_big_b64,
            )]),
            tool_call_id: None,
            tool_calls: vec![],
            is_error: false,
        }];
        let body = build_converse_body(&messages, &[], 0.2);
        let content = &body["messages"][0]["content"];
        assert_eq!(content.as_array().map(|a| a.len()), Some(0));
    }
}

#[async_trait]
impl LlmProvider for BedrockProvider {
    fn id(&self) -> &str {
        "bedrock"
    }

    fn max_context_tokens(&self) -> usize {
        200_000
    }

    fn image_constraints(&self) -> Option<ImageConstraints> {
        Some(ImageConstraints {
            max_image_bytes: BEDROCK_MAX_IMAGE_BYTES,
        })
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        temperature: f32,
    ) -> Result<LlmResponse> {
        let body = build_converse_body(messages, tools, temperature);
        debug!(provider = "bedrock", model = %self.model_id, "Sending converse request");

        let resp = self
            .client
            .post(self.converse_url())
            .bearer_auth(&self.api_key)
            .header("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(120))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Bedrock API error {}: {}", status, text));
        }

        // Parse Bedrock Converse response
        #[derive(Deserialize)]
        struct ToolUseBlock {
            #[serde(rename = "toolUseId")]
            tool_use_id: String,
            name: String,
            input: Value,
        }

        let resp_json: Value = resp.json().await?;

        // Extract content blocks from output.message.content
        let content_blocks = resp_json["output"]["message"]["content"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let mut tool_calls = Vec::new();
        let mut text_parts = Vec::new();

        for block in &content_blocks {
            if let Some(text) = block["text"].as_str() {
                text_parts.push(text.to_string());
            }
            if let Some(tool_use) = block.get("toolUse") {
                if let Ok(tc) = serde_json::from_value::<ToolUseBlock>(tool_use.clone()) {
                    tool_calls.push(ToolCall {
                        id: tc.tool_use_id,
                        name: tc.name,
                        arguments: tc.input,
                    });
                }
            }
        }

        if !tool_calls.is_empty() {
            return Ok(LlmResponse::ToolCalls(tool_calls));
        }

        Ok(LlmResponse::Text(text_parts.join("")))
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        temperature: f32,
    ) -> Result<ChunkStream> {
        let body = build_converse_body(messages, tools, temperature);
        debug!(provider = "bedrock", model = %self.model_id, "Sending converse-stream request");

        let resp = self
            .client
            .post(self.converse_stream_url())
            .bearer_auth(&self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Bedrock stream API error {}: {}", status, text));
        }

        // Bedrock converse-stream returns AWS Event Stream binary frames.
        // Each frame contains headers (:event-type, :message-type, :content-type)
        // and a JSON payload. We use aws-smithy-eventstream to parse the binary
        // framing and extract text deltas, tool call deltas, and message stops.
        //
        // Tool calls arrive as:
        //   contentBlockStart  → { start: { toolUse: { toolUseId, name } } }
        //   contentBlockDelta  → { delta: { toolUse: { input: "<json-fragment>" } } }
        //   ...more deltas...
        //   contentBlockStop   → (marks end of this content block)
        //   messageStop        → (marks end of the entire message)
        //
        // We track a running tool-call index so the tool loop can reassemble
        // the complete arguments from the incremental deltas.
        let mut decoder = MessageFrameDecoder::new();
        let mut buf = BytesMut::new();
        let mut tool_call_index: usize = 0;

        let stream = resp.bytes_stream().flat_map(move |chunk| {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    warn!("Bedrock stream chunk error: {e}");
                    return stream::iter(Vec::new());
                }
            };

            buf.extend_from_slice(&chunk);

            // Decode ALL complete frames from the buffer and collect them.
            // A single HTTP chunk may contain multiple event-stream frames,
            // and we must emit every actionable event — not just the first.
            let mut events: Vec<Result<StreamChunk>> = Vec::new();

            loop {
                match decoder.decode_frame(&mut buf) {
                    Ok(DecodedFrame::Complete(msg)) => {
                        // Extract :event-type header
                        let event_type = msg
                            .headers()
                            .iter()
                            .find(|h| h.name().as_str() == ":event-type")
                            .and_then(|h| match h.value() {
                                HeaderValue::String(s) => Some(s.as_str().to_string()),
                                _ => None,
                            });

                        // Check for exceptions
                        let msg_type = msg
                            .headers()
                            .iter()
                            .find(|h| h.name().as_str() == ":message-type")
                            .and_then(|h| match h.value() {
                                HeaderValue::String(s) => Some(s.as_str().to_string()),
                                _ => None,
                            });

                        if msg_type.as_deref() == Some("exception") {
                            let payload_str = String::from_utf8_lossy(msg.payload().as_ref());
                            warn!(
                                event_type = ?event_type,
                                "Bedrock stream exception: {payload_str}"
                            );
                            continue;
                        }

                        // Parse JSON payload
                        let payload: Value = match serde_json::from_slice(msg.payload().as_ref()) {
                            Ok(v) => v,
                            Err(_) => {
                                trace!(event_type = ?event_type, "Skipping non-JSON event frame");
                                continue; // Try next frame
                            }
                        };

                        match event_type.as_deref() {
                            Some("contentBlockDelta") => {
                                // Text delta
                                if let Some(text) = payload["delta"]["text"].as_str() {
                                    events.push(Ok(StreamChunk::Text(text.to_string())));
                                    continue;
                                }
                                // Tool-use input delta (JSON fragment)
                                if let Some(input_val) = payload["delta"]["toolUse"].get("input") {
                                    let input_frag = match input_val {
                                        Value::String(s) => s.clone(),
                                        // Some Bedrock models/providers emit tool input
                                        // as structured JSON instead of a string fragment.
                                        // Serialize to preserve content for the tool loop.
                                        other => other.to_string(),
                                    };
                                    events.push(Ok(StreamChunk::ToolCallDelta {
                                        index: tool_call_index.saturating_sub(1),
                                        id: None,
                                        name: None,
                                        arguments_delta: input_frag,
                                    }));
                                    continue;
                                }
                                continue;
                            }
                            Some("contentBlockStart") => {
                                if let Some(tool_use) = payload["start"].get("toolUse") {
                                    let name = tool_use["name"].as_str().map(|s| s.to_string());
                                    let id = tool_use["toolUseId"].as_str().map(|s| s.to_string());
                                    if name.is_some() {
                                        let idx = tool_call_index;
                                        tool_call_index += 1;
                                        events.push(Ok(StreamChunk::ToolCallDelta {
                                            index: idx,
                                            id,
                                            name,
                                            arguments_delta: String::new(),
                                        }));
                                    }
                                }
                                continue;
                            }
                            Some("messageStop") => {
                                events.push(Ok(StreamChunk::Done));
                                break;
                            }
                            _ => {
                                // messageStart, contentBlockStop, metadata, etc. — skip
                                continue;
                            }
                        }
                    }
                    Ok(DecodedFrame::Incomplete) => {
                        // Need more data — wait for next chunk
                        break;
                    }
                    Err(e) => {
                        warn!("Bedrock event stream decode error: {e}");
                        break;
                    }
                }
            }

            stream::iter(events)
        });

        Ok(Box::pin(stream))
    }
}
