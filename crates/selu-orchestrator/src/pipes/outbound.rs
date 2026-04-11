use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;
use tracing::{error, info};

use selu_core::types::OutboundEnvelope;

#[derive(Clone)]
pub struct OutboundSender {
    client: Client,
}

#[derive(Debug, Deserialize)]
struct OutboundSendResponse {
    message_ref: Option<String>,
}

impl OutboundSender {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    /// Send a message to an adapter's registered outbound URL
    pub async fn send(
        &self,
        outbound_url: &str,
        outbound_auth: Option<&str>,
        envelope: &OutboundEnvelope,
    ) -> Result<Option<String>> {
        info!(
            url = outbound_url,
            recipient = %envelope.recipient_ref,
            "Sending outbound message"
        );

        let mut req = self.client.post(outbound_url).json(envelope);

        if let Some(auth) = outbound_auth {
            req = req.header("Authorization", auth);
        }

        let resp = req.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            error!(
                url = outbound_url,
                status = %status,
                body = %body,
                "Outbound send failed"
            );
            anyhow::bail!("Outbound send failed with status {}: {}", status, body);
        }

        let parsed = resp
            .json::<OutboundSendResponse>()
            .await
            .ok()
            .and_then(|body| body.message_ref)
            .filter(|value| !value.trim().is_empty());

        Ok(parsed)
    }
}
