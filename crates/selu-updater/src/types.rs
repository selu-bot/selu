use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct CheckRequest {
    pub request_id: String,
    pub channel: String,
    pub target_tag: String,
    pub target_digest: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplyRequest {
    pub request_id: String,
    pub channel: String,
    pub target_tag: String,
    pub target_digest: String,
    #[serde(default)]
    pub target_version: String,
    #[serde(default)]
    pub target_build: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RollbackRequest {
    pub request_id: String,
    pub channel: String,
    pub rollback_tag: String,
    pub rollback_digest: String,
    #[serde(default)]
    pub rollback_version: String,
    #[serde(default)]
    pub rollback_build: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnsureWhatsappBridgeRequest {
    pub request_id: String,
    pub channel: String,
    pub inbound_url: String,
    pub inbound_token: String,
    #[serde(default)]
    pub outbound_auth: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StopWhatsappBridgeRequest {
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WhatsappBridgeStatusResponse {
    pub running: bool,
    pub connection_state: Option<String>,
    pub requires_qr: bool,
    pub qr_data_url: Option<String>,
    pub jid: Option<String>,
    pub last_error: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhatsappBridgeChat {
    pub sender_ref: String,
    pub chat_ref: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WhatsappBridgeChatsResponse {
    pub running: bool,
    pub connection_state: Option<String>,
    pub chats: Vec<WhatsappBridgeChat>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AckResponse {
    pub accepted: bool,
    pub job_id: Option<String>,
    pub status: Option<String>,
    pub progress_key: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    pub status: String,
    pub progress_key: Option<String>,
    pub message: Option<String>,
    pub job_id: Option<String>,
    pub installed_tag: Option<String>,
    pub installed_digest: Option<String>,
    pub installed_version: Option<String>,
    pub installed_build: Option<String>,
    pub previous_tag: Option<String>,
    pub previous_digest: Option<String>,
    pub previous_version: Option<String>,
    pub previous_build: Option<String>,
}
