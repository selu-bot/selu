use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

// ── Shared message / tool types ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String, // "system" | "user" | "assistant" | "tool"
    pub content: MessageContent,
    /// Only set for role = "tool" responses
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool calls requested by the assistant (role = "assistant" only).
    /// Providers use this to emit the correct toolUse / tool_use / tool_calls
    /// blocks so that subsequent toolResult messages are valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Structured error flag for tool results — set at dispatch time.
    /// Used for accurate failure counting (instead of substring matching on content).
    /// Skipped during serde so it doesn't affect DB persistence or LLM provider payloads.
    #[serde(skip)]
    pub is_error: bool,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: MessageContent::Text(content.into()),
            tool_call_id: None,
            tool_calls: vec![],
            is_error: false,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: MessageContent::Text(content.into()),
            tool_call_id: None,
            tool_calls: vec![],
            is_error: false,
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: MessageContent::Text(content.into()),
            tool_call_id: None,
            tool_calls: vec![],
            is_error: false,
        }
    }
    /// An assistant message that contains one or more tool calls.
    /// `text` may be empty if the assistant only emitted tool calls with no prose.
    pub fn assistant_with_tool_calls(text: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: MessageContent::Text(text.into()),
            tool_call_id: None,
            tool_calls: calls,
            is_error: false,
        }
    }
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: MessageContent::Text(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: vec![],
            is_error: false,
        }
    }
    /// A tool result that represents a failure (blocked, denied, dispatch error, etc.).
    pub fn tool_error(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: MessageContent::Text(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: vec![],
            is_error: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl MessageContent {
    pub fn as_text(&self) -> &str {
        match self {
            MessageContent::Text(s) => s,
            MessageContent::Parts(_) => "",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub part_type: String,
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            part_type: "text".to_string(),
            text: Some(text.into()),
            image_base64: None,
            media_type: None,
            image_url: None,
        }
    }

    pub fn image_base64(media_type: impl Into<String>, image_base64: impl Into<String>) -> Self {
        Self {
            part_type: "image".to_string(),
            text: None,
            image_base64: Some(image_base64.into()),
            media_type: Some(media_type.into()),
            image_url: None,
        }
    }
}

/// Tool definition exposed to the LLM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
}

/// A tool call the LLM wants to make
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Synchronous response from the LLM
#[derive(Debug, Clone)]
pub enum LlmResponse {
    Text(String),
    ToolCalls(Vec<ToolCall>),
}

/// A streaming token chunk
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// A text delta token
    Text(String),
    /// A tool-use input JSON delta for the tool call currently being built.
    /// Providers emit these as the LLM streams the arguments incrementally.
    ToolCallDelta {
        /// Index of the tool call in the current turn (0-based).
        index: usize,
        /// Set once at the start of a new tool-call block.
        id: Option<String>,
        /// Set once at the start of a new tool-call block.
        name: Option<String>,
        /// Incremental JSON fragment for the arguments.
        arguments_delta: String,
    },
    /// The stream has ended. If tool calls were being built, this signals
    /// that all deltas have been received and the caller can parse the
    /// accumulated argument strings.
    Done,
}

pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>;

#[derive(Debug, Clone, Copy)]
pub struct ImageConstraints {
    pub max_image_bytes: usize,
}

/// Provider-agnostic LLM interface
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &str;

    /// Maximum input context window in tokens for this provider/model.
    /// Used by the context budget trimmer to avoid exceeding the limit.
    /// Defaults to 128 000 (safe for most modern models).
    fn max_context_tokens(&self) -> usize {
        128_000
    }

    /// Optional provider-side image limits for multimodal requests.
    /// The shared image normalizer applies these constraints before dispatch.
    fn image_constraints(&self) -> Option<ImageConstraints> {
        None
    }

    /// Non-streaming chat
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        temperature: f32,
    ) -> Result<LlmResponse>;

    /// Streaming chat — yields StreamChunk items
    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        temperature: f32,
    ) -> Result<ChunkStream>;
}
