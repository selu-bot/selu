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

pub struct OpenAiProvider {
    client: Client,
    api_key: String,
    base_url: String,
    default_model: String,
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
            api_key: api_key.into(),
            base_url: "https://api.openai.com".into(),
            default_model: if model_str.is_empty() { "gpt-4o".into() } else { model_str },
        }
    }

    /// Construct an Ollama-compatible provider (OpenAI-compatible API)
    pub fn ollama(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .pool_max_idle_per_host(4)
                .pool_idle_timeout(std::time::Duration::from_secs(90))
                .build()
                .expect("Failed to build HTTP client"),
            api_key: "ollama".into(), // Ollama doesn't need a real key
            base_url: base_url.into(),
            default_model: model.into(),
        }
    }
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
                        .map(|p| json!({"type": p.part_type, "text": p.text}))
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
        if self.base_url.contains("openai.com") { "openai" } else { "ollama" }
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        temperature: f32,
    ) -> Result<LlmResponse> {
        debug!(provider = self.id(), "Sending chat request");

        let mut body = json!({
            "model": self.default_model,
            "messages": messages_to_openai(messages),
            "temperature": temperature,
        });

        if !tools.is_empty() {
            body["tools"] = json!(tools_to_openai(tools));
        }

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
        struct FunctionCall { name: String, arguments: String }
        #[derive(Deserialize)]
        struct OaiToolCall { id: String, function: FunctionCall }
        #[derive(Deserialize)]
        struct Message {
            content: Option<String>,
            tool_calls: Option<Vec<OaiToolCall>>,
        }
        #[derive(Deserialize)]
        struct Choice { message: Message }
        #[derive(Deserialize)]
        struct Resp { choices: Vec<Choice> }

        let parsed: Resp = resp.json().await?;
        let choice = parsed.choices.into_iter().next()
            .ok_or_else(|| anyhow!("No choices in response"))?;

        if let Some(tool_calls) = choice.message.tool_calls {
            let calls = tool_calls
                .into_iter()
                .map(|tc| {
                    let args: Value = serde_json::from_str(&tc.function.arguments)
                        .unwrap_or(json!({}));
                    ToolCall { id: tc.id, name: tc.function.name, arguments: args }
                })
                .collect();
            return Ok(LlmResponse::ToolCalls(calls));
        }

        Ok(LlmResponse::Text(
            choice.message.content.unwrap_or_default(),
        ))
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        temperature: f32,
    ) -> Result<ChunkStream> {
        let mut body = json!({
            "model": self.default_model,
            "messages": messages_to_openai(messages),
            "temperature": temperature,
            "stream": true,
        });

        if !tools.is_empty() {
            body["tools"] = json!(tools_to_openai(tools));
        }

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

        let stream = resp.bytes_stream().eventsource().filter_map(|event| async move {
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
