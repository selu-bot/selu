use anyhow::{Result, anyhow};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use super::provider::{
    ChatMessage, ChunkStream, ImageConstraints, LlmProvider, LlmResponse, MessageContent,
    StreamChunk, ToolCall, ToolSpec,
};

pub struct OpenAiProvider {
    client: Client,
    provider_id: &'static str,
    api_key: String,
    base_url: String,
    default_model: String,
    /// When true, send `think: false` to suppress reasoning tokens and
    /// strip any residual `<think>…</think>` blocks from responses.
    strip_think_tags: bool,
}

impl OpenAiProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let model_str = model.into();
        Self {
            client: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .pool_max_idle_per_host(4)
                .pool_idle_timeout(std::time::Duration::from_secs(90))
                .build()
                .expect("Failed to build HTTP client"),
            provider_id: "openai",
            api_key: api_key.into(),
            base_url: "https://api.openai.com".into(),
            default_model: if model_str.is_empty() {
                "gpt-4o".into()
            } else {
                model_str
            },
            strip_think_tags: false,
        }
    }

    /// Construct a Pico AI Server provider (OpenAI-compatible API)
    pub fn pico(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .pool_max_idle_per_host(4)
                .pool_idle_timeout(std::time::Duration::from_secs(90))
                .build()
                .expect("Failed to build HTTP client"),
            provider_id: "pico",
            api_key: "pico".into(), // Pico doesn't need a real key
            base_url: base_url.into(),
            default_model: model.into(),
            strip_think_tags: true,
        }
    }

    /// Construct an xAI Grok provider (OpenAI-compatible API)
    pub fn grok(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let model_str = model.into();
        Self {
            client: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .pool_max_idle_per_host(4)
                .pool_idle_timeout(std::time::Duration::from_secs(90))
                .build()
                .expect("Failed to build HTTP client"),
            provider_id: "grok",
            api_key: api_key.into(),
            base_url: base_url.into(),
            default_model: if model_str.is_empty() {
                "grok-3".into()
            } else {
                model_str
            },
            strip_think_tags: false,
        }
    }

    fn build_body(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        temperature: f32,
        stream: bool,
    ) -> Value {
        let mut body = json!({
            "model": self.default_model,
            "messages": messages_to_openai(messages),
            "temperature": temperature,
        });

        if stream {
            body["stream"] = json!(true);
        }

        if !tools.is_empty() {
            body["tools"] = json!(tools_to_openai(tools));
        }

        // Disable reasoning/thinking for Pico models
        if self.strip_think_tags {
            body["think"] = json!(false);
        }

        body
    }
}

/// Strip `<think>...</think>` blocks from text (safety net for non-streaming).
fn strip_think_blocks(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        result.push_str(&rest[..start]);
        match rest[start..].find("</think>") {
            Some(end) => rest = &rest[start + end + 8..],
            None => return result,
        }
    }
    result.push_str(rest);
    result
}

fn messages_to_openai(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            let content = match &m.content {
                MessageContent::Text(t) => json!(t),
                MessageContent::Parts(parts) => {
                    let p: Vec<Value> = parts
                        .iter()
                        .filter_map(|p| {
                            if p.part_type == "text" {
                                return p.text.as_ref().map(|t| json!({"type": "text", "text": t}));
                            }

                            if let Some(data) = p.image_base64.as_ref() {
                                let media_type = p.media_type.as_deref().unwrap_or("image/png");
                                return Some(json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": format!("data:{};base64,{}", media_type, data)
                                    }
                                }));
                            }

                            p.image_url.as_ref().map(|url| {
                                json!({
                                    "type": "image_url",
                                    "image_url": { "url": url }
                                })
                            })
                        })
                        .collect();
                    json!(p)
                }
            };
            let mut msg = json!({"role": m.role, "content": content});
            if let Some(id) = &m.tool_call_id {
                msg["tool_call_id"] = json!(id);
            }
            // Emit tool_calls array for assistant messages that contain them
            if m.role == "assistant" && !m.tool_calls.is_empty() {
                let tc: Vec<Value> = m.tool_calls.iter().map(|tc| {
                    json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": tc.name,
                            "arguments": serde_json::to_string(&tc.arguments).unwrap_or_default()
                        }
                    })
                }).collect();
                msg["tool_calls"] = json!(tc);
            }
            msg
        })
        .collect()
}

fn tools_to_openai(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn id(&self) -> &str {
        self.provider_id
    }

    fn image_constraints(&self) -> Option<ImageConstraints> {
        let max_image_bytes = if self.provider_id == "openai" {
            20 * 1024 * 1024
        } else {
            5 * 1024 * 1024
        };
        Some(ImageConstraints { max_image_bytes })
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        temperature: f32,
    ) -> Result<LlmResponse> {
        debug!(provider = self.id(), "Sending chat request");

        let body = self.build_body(messages, tools, temperature, false);

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .timeout(std::time::Duration::from_secs(120))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("{} API error {}: {}", self.id(), status, text));
        }

        #[derive(Deserialize)]
        struct FunctionCall {
            name: String,
            arguments: String,
        }
        #[derive(Deserialize)]
        struct OaiToolCall {
            id: String,
            function: FunctionCall,
        }
        #[derive(Deserialize)]
        struct Message {
            content: Option<String>,
            tool_calls: Option<Vec<OaiToolCall>>,
        }
        #[derive(Deserialize)]
        struct Choice {
            message: Message,
        }
        #[derive(Deserialize)]
        struct Resp {
            choices: Vec<Choice>,
        }

        let parsed: Resp = resp.json().await?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No choices in response"))?;

        if let Some(tool_calls) = choice.message.tool_calls {
            let calls = tool_calls
                .into_iter()
                .map(|tc| {
                    let args: Value =
                        serde_json::from_str(&tc.function.arguments).unwrap_or(json!({}));
                    ToolCall {
                        id: tc.id,
                        name: tc.function.name,
                        arguments: args,
                    }
                })
                .collect();
            return Ok(LlmResponse::ToolCalls(calls));
        }

        let text = choice.message.content.unwrap_or_default();
        let text = if self.strip_think_tags {
            strip_think_blocks(&text)
        } else {
            text
        };
        Ok(LlmResponse::Text(text))
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        temperature: f32,
    ) -> Result<ChunkStream> {
        let body = self.build_body(messages, tools, temperature, true);

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("{} API error {}: {}", self.id(), status, text));
        }

        let stream = resp
            .bytes_stream()
            .eventsource()
            .filter_map(|event| async move {
                let event = event.ok()?;
                if event.data == "[DONE]" {
                    return Some(Ok(StreamChunk::Done));
                }
                let data: Value = serde_json::from_str(&event.data).ok()?;
                let delta = &data["choices"][0]["delta"];

                // Tool call delta
                if let Some(tool_calls) = delta["tool_calls"].as_array() {
                    if let Some(first) = tool_calls.first() {
                        let index = first["index"].as_u64().unwrap_or(0) as usize;
                        let id = first["id"].as_str().map(|s| s.to_string());
                        let name = first["function"]["name"].as_str().map(|s| s.to_string());
                        let args_delta = first["function"]["arguments"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        return Some(Ok(StreamChunk::ToolCallDelta {
                            index,
                            id,
                            name,
                            arguments_delta: args_delta,
                        }));
                    }
                    return None;
                }

                // Text delta
                if let Some(text) = delta["content"].as_str() {
                    return Some(Ok(StreamChunk::Text(text.to_string())));
                }

                None
            });

        Ok(Box::pin(stream))
    }
}
