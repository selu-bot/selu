/// `ChannelSender` implementation for generic webhook pipes.
///
/// Sends out-of-band messages (approval prompts, notifications) via the
/// pipe's registered outbound URL using the same mechanism as regular
/// outbound replies.
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::json;
use tracing::info;

use crate::channels::ChannelSender;

/// A `ChannelSender` that POSTs messages to a pipe's outbound URL.
pub struct WebhookSender {
    pub outbound_url: String,
    pub outbound_auth: Option<String>,
    pub recipient_ref: String,
    http: Client,
}

impl WebhookSender {
    pub fn new(
        outbound_url: String,
        outbound_auth: Option<String>,
        recipient_ref: String,
    ) -> Self {
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
        text: &str,
        _reply_to_guid: Option<&str>,
    ) -> Result<Option<String>> {
        let envelope = json!({
            "recipient_ref": self.recipient_ref,
            "text": text,
            "thread_id": thread_id,
            "metadata": {
                "type": "approval_prompt"
            }
        });

        info!(
            url = %self.outbound_url,
            thread_id = %thread_id,
            "Sending approval prompt via webhook"
        );

        let mut req = self.http
            .post(&self.outbound_url)
            .json(&envelope);

        if let Some(ref auth) = self.outbound_auth {
            req = req.header("Authorization", auth);
        }

        let resp = req.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Webhook send failed with status {}: {}", status, body);
        }

        // Webhook pipes don't provide sent message GUIDs
        Ok(None)
    }
}
