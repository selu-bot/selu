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

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SidecarProtectedImageRef {
    pub image_ref: String,
    pub owner: String,
    pub state: String,
    #[serde(default)]
    pub retain_until: Option<String>,
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
    #[serde(default)]
    pub storage_metadata_ready: bool,
    #[serde(default)]
    pub storage_metadata_generated_at: Option<String>,
    #[serde(default)]
    pub storage_cleanup_block_reason: Option<String>,
    #[serde(default)]
    pub managed_repositories: Vec<String>,
    #[serde(default)]
    pub protected_image_refs: Vec<SidecarProtectedImageRef>,
}

#[cfg(test)]
mod tests {
    use super::SidecarStatusResponse;

    #[test]
    fn old_updater_status_defaults_storage_metadata_to_not_ready() {
        let status: SidecarStatusResponse = serde_json::from_str(
            r#"{
            "status":"idle","progress_key":null,"message":null,"job_id":null,
            "installed_tag":null,"installed_digest":null,"installed_version":null,
            "installed_build":null,"previous_tag":null,"previous_digest":null,
            "previous_version":null,"previous_build":null
        }"#,
        )
        .expect("legacy status should remain decodable");

        assert!(!status.storage_metadata_ready);
        assert!(status.storage_metadata_generated_at.is_none());
        assert!(status.managed_repositories.is_empty());
        assert!(status.protected_image_refs.is_empty());
    }
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
