use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct SidecarCheckRequest {
    pub request_id: String,
    pub channel: String,
    pub target_tag: String,
    pub target_digest: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SidecarApplyRequest {
    pub request_id: String,
    pub channel: String,
    pub target_tag: String,
    pub target_digest: String,
    pub target_version: String,
    pub target_build: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SidecarRollbackRequest {
    pub request_id: String,
    pub channel: String,
    pub rollback_tag: String,
    pub rollback_digest: String,
    pub rollback_version: String,
    pub rollback_build: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SidecarEnsureWhatsappBridgeRequest {
    pub request_id: String,
    pub channel: String,
    pub inbound_url: String,
    pub inbound_token: String,
    pub outbound_auth: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SidecarStopWhatsappBridgeRequest {
    pub request_id: String,
    pub channel: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SidecarAck {
    pub accepted: bool,
    pub job_id: Option<String>,
    pub status: Option<String>,
    pub progress_key: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SidecarStatusResponse {
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

#[derive(Debug, Clone, Deserialize)]
pub struct SidecarWhatsappBridgeStatusResponse {
    pub running: bool,
    pub connection_state: Option<String>,
    pub requires_qr: bool,
    pub qr_data_url: Option<String>,
    pub jid: Option<String>,
    pub last_error: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SidecarWhatsappBridgeChat {
    pub sender_ref: String,
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SidecarWhatsappBridgeChatsResponse {
    pub running: bool,
    pub connection_state: Option<String>,
    pub chats: Vec<SidecarWhatsappBridgeChat>,
    pub message: Option<String>,
}
