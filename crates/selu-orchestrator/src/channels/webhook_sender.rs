/// `ChannelSender` implementation for generic webhook pipes.
///
/// Sends out-of-band messages (approval prompts, notifications) via the
/// pipe's registered outbound URL using the same mechanism as regular
/// outbound replies.
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tracing::info;

use crate::channels::{ChannelMessage, ChannelSender};

/// A `ChannelSender` that POSTs messages to a pipe's outbound URL.
pub struct WebhookSender {
    pub outbound_url: String,
    pub outbound_auth: Option<String>,
    pub recipient_ref: String,
    http: Client,
}

#[derive(Debug, Deserialize)]
struct WebhookSendResponse {
    message_ref: Option<String>,
}

impl WebhookSender {
    pub fn new(outbound_url: String, outbound_auth: Option<String>, recipient_ref: String) -> Self {
        Self {
            outbound_url,
            outbound_auth,
            recipient_ref,
            http: Client::new(),
        }
    }
}

#[async_trait]
impl ChannelSender for WebhookSender {
    async fn send_message(
        &self,
        thread_id: &str,
        message: &ChannelMessage,
        reply_to_guid: Option<&str>,
    ) -> Result<Option<String>> {
        let envelope = json!({
            "recipient_ref": self.recipient_ref,
            "text": message.text,
            "thread_id": thread_id,
            "reply_to_message_ref": reply_to_guid,
            "attachments": if message.attachments.is_empty() { serde_json::Value::Null } else { serde_json::to_value(&message.attachments).unwrap_or(serde_json::Value::Null) },
            "metadata": {
                "type": "approval_prompt"
            }
        });

        info!(
            url = %self.outbound_url,
            thread_id = %thread_id,
            reply_to_guid = ?reply_to_guid,
            "Sending approval prompt via webhook"
        );

        let mut req = self.http.post(&self.outbound_url).json(&envelope);

        if let Some(ref auth) = self.outbound_auth {
            req = req.header("Authorization", auth);
        }

        let resp = req.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Webhook send failed with status {}: {}", status, body);
        }

        let parsed = resp
            .json::<WebhookSendResponse>()
            .await
            .ok()
            .and_then(|body| body.message_ref)
            .filter(|value| !value.trim().is_empty());

        Ok(parsed)
    }
}
