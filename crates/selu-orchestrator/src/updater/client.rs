use anyhow::{Context, Result, anyhow};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::AppConfig;
use crate::updater::types::{
    SidecarAck, SidecarApplyRequest, SidecarCheckRequest, SidecarEnsureWhatsappBridgeRequest,
    SidecarRollbackRequest, SidecarStatusResponse, SidecarStopWhatsappBridgeRequest,
    SidecarWhatsappBridgeChatsResponse, SidecarWhatsappBridgeStatusResponse,
};

const UPDATER_SECRET_HEADER: &str = "X-Selu-Updater-Secret";

#[derive(Clone)]
pub struct SidecarUpdaterClient {
    http: reqwest::Client,
    base_url: String,
}

impl SidecarUpdaterClient {
    pub fn from_config(config: &AppConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        if !config.updater_shared_secret.trim().is_empty() {
            let value = HeaderValue::from_str(config.updater_shared_secret.trim())
                .context("Invalid updater shared secret header value")?;
            headers.insert(UPDATER_SECRET_HEADER, value);
        }

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(
                config.updater_request_timeout_secs,
            ))
            .build()
            .context("Failed to build updater HTTP client")?;

        Ok(Self {
            http,
            base_url: config.updater_url.trim_end_matches('/').to_string(),
        })
    }

    pub async fn check(&self, payload: &SidecarCheckRequest) -> Result<SidecarAck> {
        self.post_json("/update/check", payload).await
    }

    pub async fn apply(&self, payload: &SidecarApplyRequest) -> Result<SidecarAck> {
        self.post_json("/update/apply", payload).await
    }

    pub async fn rollback(&self, payload: &SidecarRollbackRequest) -> Result<SidecarAck> {
        self.post_json("/update/rollback", payload).await
    }

    pub async fn status(&self) -> Result<SidecarStatusResponse> {
        self.get_json("/update/status").await
    }

    pub async fn ensure_whatsapp_bridge(
        &self,
        payload: &SidecarEnsureWhatsappBridgeRequest,
    ) -> Result<SidecarAck> {
        self.post_json("/sidecars/whatsapp/ensure", payload).await
    }

    pub async fn whatsapp_bridge_status(&self) -> Result<SidecarWhatsappBridgeStatusResponse> {
        self.get_json("/sidecars/whatsapp/status").await
    }

    pub async fn whatsapp_bridge_chats(
        &self,
        query: &str,
    ) -> Result<SidecarWhatsappBridgeChatsResponse> {
        let encoded = urlencoding::encode(query.trim());
        self.get_json(&format!("/sidecars/whatsapp/chats?q={encoded}"))
            .await
    }

    pub async fn stop_whatsapp_bridge(
        &self,
        payload: &SidecarStopWhatsappBridgeRequest,
    ) -> Result<SidecarAck> {
        self.post_json("/sidecars/whatsapp/stop", payload).await
    }

    async fn get_json<TResp>(&self, path: &str) -> Result<TResp>
    where
        TResp: DeserializeOwned,
    {
        let url = format!("{}{}", self.base_url, path);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("Failed to call updater endpoint {}", path))?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Updater endpoint {} returned {}",
                path,
                response.status()
            ));
        }

        response
            .json::<TResp>()
            .await
            .context("Invalid updater response payload")
    }

    async fn post_json<TReq, TResp>(&self, path: &str, payload: &TReq) -> Result<TResp>
    where
        TReq: Serialize + ?Sized,
        TResp: DeserializeOwned,
    {
        let url = format!("{}{}", self.base_url, path);
        let response = self
            .http
            .post(&url)
            .json(payload)
            .send()
            .await
            .with_context(|| format!("Failed to call updater endpoint {}", path))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if let Ok(ack) = serde_json::from_str::<SidecarAck>(&body) {
                if let Some(msg) = ack.message {
                    return Err(anyhow!(
                        "Updater endpoint {} returned {}: {}",
                        path,
                        status,
                        msg
                    ));
                }
            }
            return Err(anyhow!("Updater endpoint {} returned {}", path, status));
        }

        response
            .json::<TResp>()
            .await
            .context("Invalid updater response payload")
    }
}
