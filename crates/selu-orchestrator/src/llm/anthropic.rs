use anyhow::{anyhow, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::debug;

use super::provider::{
    ChunkStream, ChatMessage, LlmProvider, LlmResponse, MessageContent, StreamChunk, ToolCall,
    ToolSpec,
};

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let model_str = model.into();
        Self {
            client: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .pool_max_idle_per_host(4)
                .pool_idle_timeout(std::time::Duration::from_secs(90))
                .build()
                .expect("Failed to build HTTP client"),
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com".into(),
            model: if model_str.is_empty() { "claude-sonnet-4-20250514".into() } else { model_str },
        }
    }
}

// ── Request / response shapes ─────────────────────────────────────────────────

fn build_request(
    messages: &[ChatMessage],
    tools: &[ToolSpec],
    model: &str,
    temperature: f32,
    stream: bool,
) -> Value {
    // Anthropic separates the system prompt from the messages array
    let system_prompt = messages
        .iter()
        .filter(|m| m.role == "system")
        .map(|m| m.content.as_text().to_string())
        .collect::<Vec<_>>()
        .join("\n\n");

    let msgs: Vec<Value> = {
        let non_system: Vec<&ChatMessage> = messages
            .iter()
            .filter(|m| m.role != "system")
            .collect();

        let mut result = Vec::new();
        let mut i = 0;
        while i < non_system.len() {
            let m = non_system[i];

            if m.role == "assistant" && !m.tool_calls.is_empty() {
                // Assistant message with tool calls → emit tool_use content blocks
                let mut content_blocks: Vec<Value> = Vec::new();
                let text = m.content.as_text();
                if !text.is_empty() {
                    content_blocks.push(json!({"type": "text", "text": text}));
                }
                for tc in &m.tool_calls {
                    content_blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": tc.arguments
                    }));
                }
                result.push(json!({"role": "assistant", "content": content_blocks}));
            } else if m.role == "tool" {
                // Collect all consecutive tool-result messages into one "user" message
                let mut tool_result_blocks: Vec<Value> = Vec::new();
                while i < non_system.len() && non_system[i].role == "tool" {
                    let tr = non_system[i];
                    tool_result_blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tr.tool_call_id,
                        "content": tr.content.as_text()
                    }));
                    i += 1;
                }
                result.push(json!({"role": "user", "content": tool_result_blocks}));
                continue; // skip i += 1 below
            } else {
                let content = match &m.content {
                    MessageContent::Text(t) => json!(t),
                    MessageContent::Parts(parts) => {
                        let p: Vec<Value> = parts
                            .iter()
                            .map(|p| json!({"type": p.part_type, "text": p.text}))
                            .collect();
                        json!(p)
                    }
                };
                result.push(json!({"role": m.role, "content": content}));
            }

            i += 1;
        }
        result
    };

    let tools_json: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        })
        .collect();

    let mut req = json!({
        "model": model,
        "max_tokens": 4096,
        "temperature": temperature,
        "messages": msgs,
        "stream": stream,
    });

    if !system_prompt.is_empty() {
        req["system"] = json!(system_prompt);
    }
    if !tools_json.is_empty() {
        req["tools"] = json!(tools_json);
    }

    req
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        temperature: f32,
    ) -> Result<LlmResponse> {
        let body = build_request(messages, tools, &self.model, temperature, false);
        debug!(provider = "anthropic", "Sending chat request");

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .timeout(std::time::Duration::from_secs(120))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Anthropic API error {}: {}", status, text));
        }

        #[derive(Deserialize)]
        struct ContentBlock {
            #[serde(rename = "type")]
            block_type: String,
            text: Option<String>,
            id: Option<String>,
            name: Option<String>,
            input: Option<Value>,
        }
        #[derive(Deserialize)]
        struct Resp {
            content: Vec<ContentBlock>,
        }

        let parsed: Resp = resp.json().await?;

        let tool_calls: Vec<ToolCall> = parsed
            .content
            .iter()
            .filter(|b| b.block_type == "tool_use")
            .map(|b| ToolCall {
                id: b.id.clone().unwrap_or_default(),
                name: b.name.clone().unwrap_or_default(),
                arguments: b.input.clone().unwrap_or(json!({})),
            })
            .collect();

        if !tool_calls.is_empty() {
            return Ok(LlmResponse::ToolCalls(tool_calls));
        }

        let text = parsed
            .content
            .iter()
            .filter(|b| b.block_type == "text")
            .filter_map(|b| b.text.as_deref())
            .collect::<Vec<_>>()
            .join("");

        Ok(LlmResponse::Text(text))
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        temperature: f32,
    ) -> Result<ChunkStream> {
        let body = build_request(messages, tools, &self.model, temperature, true);

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Anthropic API error {}: {}", status, text));
        }

        let mut tool_call_index: usize = 0;

        let stream = resp.bytes_stream().eventsource().filter_map(move |event| {
            let event = match event {
                Ok(e) => e,
                Err(_) => return std::future::ready(None),
            };
            let data: Value = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(_) => return std::future::ready(None),
            };
            let event_type = match data["type"].as_str() {
                Some(t) => t,
                None => return std::future::ready(None),
            };

            match event_type {
                "content_block_delta" => {
                    let delta_type = match data["delta"]["type"].as_str() {
                        Some(t) => t,
                        None => return std::future::ready(None),
                    };
                    match delta_type {
                        "text_delta" => {
                            let text = match data["delta"]["text"].as_str() {
                                Some(t) => t.to_string(),
                                None => return std::future::ready(None),
                            };
                            std::future::ready(Some(Ok(StreamChunk::Text(text))))
                        }
                        "input_json_delta" => {
                            let partial = data["delta"]["partial_json"]
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            std::future::ready(Some(Ok(StreamChunk::ToolCallDelta {
                                index: tool_call_index.saturating_sub(1),
                                id: None,
                                name: None,
                                arguments_delta: partial,
                            })))
                        }
                        _ => std::future::ready(None),
                    }
                }
                "content_block_start" => {
                    let block_type = match data["content_block"]["type"].as_str() {
                        Some(t) => t,
                        None => return std::future::ready(None),
                    };
                    if block_type == "tool_use" {
                        let name = data["content_block"]["name"].as_str().map(|s| s.to_string());
                        let id = data["content_block"]["id"].as_str().map(|s| s.to_string());
                        if name.is_some() {
                            let idx = tool_call_index;
                            tool_call_index += 1;
                            std::future::ready(Some(Ok(StreamChunk::ToolCallDelta {
                                index: idx,
                                id,
                                name,
                                arguments_delta: String::new(),
                            })))
                        } else {
                            std::future::ready(None)
                        }
                    } else {
                        std::future::ready(None)
                    }
                }
                "message_stop" => std::future::ready(Some(Ok(StreamChunk::Done))),
                _ => std::future::ready(None),
            }
        });

        Ok(Box::pin(stream))
    }
}
