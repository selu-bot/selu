use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Users ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub display_name: String,
    pub created_at: DateTime<Utc>,
}

// ── Pipes ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pipe {
    pub id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub transport: Transport,
    pub inbound_token: String,
    pub outbound_url: String,
    pub outbound_auth: Option<String>,
    pub default_agent_id: Option<String>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Webhook, // generic webhook (BlueBubbles, Baileys, etc.)
    Web,     // built-in web UI WebSocket
    Api,     // API key client
}

// ── Messages ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub pipe_id: Uuid,
    pub session_id: Option<Uuid>,
    pub role: Role,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::User => write!(f, "user"),
            Role::Assistant => write!(f, "assistant"),
            Role::System => write!(f, "system"),
            Role::Tool => write!(f, "tool"),
        }
    }
}

// ── Sessions ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: Uuid,
    pub pipe_id: Uuid,
    pub user_id: Uuid,
    pub agent_id: String,
    pub status: SessionStatus,
    pub workspace_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Idle,
    Closed,
}

// ── Threads ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: Uuid,
    pub pipe_id: Uuid,
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub status: ThreadStatus,
    /// Human-readable title for the UI sidebar (LLM-generated from first message).
    pub title: Option<String>,
    /// The adapter-side message reference that started this thread (e.g. BB message GUID).
    /// Used for reply-to correlation on outbound.
    pub origin_message_ref: Option<String>,
    /// The GUID of PAP's last outbound reply in this thread.
    /// Used to match incoming iMessage replies back to this thread.
    pub last_reply_guid: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum ThreadStatus {
    Active,
    Completed,
    Failed,
}

impl std::fmt::Display for ThreadStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThreadStatus::Active => write!(f, "active"),
            ThreadStatus::Completed => write!(f, "completed"),
            ThreadStatus::Failed => write!(f, "failed"),
        }
    }
}

// ── Sender References ────────────────────────────────────────────────────────

/// Maps an external sender identity (phone number, email, etc.) to a PAP user
/// on a per-pipe basis. Only mapped senders get responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSenderRef {
    pub id: Uuid,
    pub user_id: Uuid,
    pub pipe_id: Uuid,
    pub sender_ref: String,
    pub created_at: DateTime<Utc>,
}

// ── BlueBubbles Configuration ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlueBubblesConfig {
    pub id: Uuid,
    pub name: String,
    pub server_url: String,
    pub server_password: String,
    pub chat_guid: String,
    pub pipe_id: Uuid,
    pub poll_interval_ms: i64,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

// ── Inbound / Outbound message envelope ──────────────────────────────────────

/// Payload POSTed to /api/pipes/{id}/inbound by external adapters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundEnvelope {
    /// Sender identity as known by the adapter (phone number, username, etc.)
    pub sender_ref: String,
    /// Plain text content of the message
    pub text: String,
    /// Optional adapter-specific metadata
    pub metadata: Option<serde_json::Value>,
}

/// Payload the orchestrator POSTs to the adapter's outbound_url
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundEnvelope {
    /// Recipient identity as known by the adapter
    pub recipient_ref: String,
    /// Text to send
    pub text: String,
    /// Thread ID so the adapter can correlate replies
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Adapter-specific message GUID to reply to (e.g. for BB reply-to-message)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_ref: Option<String>,
    /// Optional metadata adapters may use
    pub metadata: Option<serde_json::Value>,
}

// ── Events ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub id: Uuid,
    pub source_session_id: Uuid,
    pub source_agent_id: String,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub chain_depth: i32,
    pub emitted_at: DateTime<Utc>,
    pub processed_at: Option<DateTime<Utc>>,
}

// ── Workspaces ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Uuid,
    pub session_id: Uuid,
    pub capability_id: String,
    pub status: WorkspaceStatus,
    pub ttl_hours: i32,
    pub created_at: DateTime<Utc>,
    pub suspended_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum WorkspaceStatus {
    Active,
    Suspended,
    Destroyed,
}
