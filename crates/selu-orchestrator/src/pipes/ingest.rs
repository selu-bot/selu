use crate::agents::engine::InboundAttachmentInput;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use selu_core::types::{InboundAttachment, InboundEnvelope};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, oneshot};
use tracing::{debug, warn};

const AGGREGATION_WINDOW: Duration = Duration::from_secs(2);


type AggregationKey = (String, String);

#[derive(Debug)]
struct PendingInbound {
    id: u64,
    envelope: InboundEnvelope,
    tx: oneshot::Sender<InboundEnvelope>,
}

static AGGREGATION_MAP: OnceLock<Mutex<HashMap<AggregationKey, PendingInbound>>> = OnceLock::new();
static AGGREGATION_SEQ: AtomicU64 = AtomicU64::new(1);

fn aggregation_map() -> &'static Mutex<HashMap<AggregationKey, PendingInbound>> {
    AGGREGATION_MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn conversation_ref(envelope: &InboundEnvelope) -> String {
    envelope
        .metadata
        .as_ref()
        .and_then(|m| m.get("chat_ref"))
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.to_string())
        .unwrap_or_else(|| envelope.sender_ref.clone())
}

pub async fn aggregate_inbound_envelope(
    pipe_id: &str,
    conversation_ref: &str,
    envelope: InboundEnvelope,
) -> Option<InboundEnvelope> {
    if !is_partial(&envelope) {
        return Some(envelope);
    }

    let key = (pipe_id.to_string(), conversation_ref.to_string());
    let original = envelope.clone();
    let (rx, pending_id) = {
        let mut lock = aggregation_map().lock().await;

        if let Some(existing) = lock.remove(&key) {
            if can_merge(&existing.envelope, &envelope) {
                let merged = merge_envelopes(existing.envelope, envelope);
                let _ = existing.tx.send(merged);
                return None;
            }
            let _ = existing.tx.send(existing.envelope);
        }

        let (tx, rx) = oneshot::channel::<InboundEnvelope>();
        let pending_id = AGGREGATION_SEQ.fetch_add(1, Ordering::Relaxed);
        lock.insert(
            key.clone(),
            PendingInbound {
                id: pending_id,
                envelope,
                tx,
            },
        );
        (rx, pending_id)
    };

    match tokio::time::timeout(AGGREGATION_WINDOW, rx).await {
        Ok(Ok(merged)) => Some(merged),
        _ => {
            let mut lock = aggregation_map().lock().await;
            if lock
                .get(&key)
                .map(|pending| pending.id == pending_id)
                .unwrap_or(false)
            {
                lock.remove(&key);
                Some(original)
            } else {
                None
            }
        }
    }
}

pub async fn load_inbound_attachment_inputs(
    envelope: &InboundEnvelope,
    http: &reqwest::Client,
) -> Vec<InboundAttachmentInput> {
    let mut out = Vec::new();
    for attachment in envelope.attachments.clone().unwrap_or_default() {
        if !attachment.mime_type.starts_with("image/") {
            debug!(mime_type = %attachment.mime_type, "Skipping non-image inbound attachment");
            continue;
        }

        match load_attachment_bytes(&attachment, http).await {
            Ok(data) => {
                out.push(InboundAttachmentInput {
                    filename: attachment_filename(&attachment),
                    mime_type: attachment.mime_type.clone(),
                    data,
                });
            }
            Err(e) => {
                warn!(
                    filename = %attachment.filename,
                    mime_type = %attachment.mime_type,
                    error = %e,
                    "Failed to load inbound attachment"
                );
            }
        }
    }
    out
}

fn is_partial(envelope: &InboundEnvelope) -> bool {
    let has_text = !envelope.text.trim().is_empty();
    let has_attachments = envelope
        .attachments
        .as_ref()
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    has_text ^ has_attachments
}

fn can_merge(a: &InboundEnvelope, b: &InboundEnvelope) -> bool {
    let a_text = !a.text.trim().is_empty();
    let b_text = !b.text.trim().is_empty();
    let a_media = a
        .attachments
        .as_ref()
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let b_media = b
        .attachments
        .as_ref()
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    (a_text && b_media) || (b_text && a_media)
}

fn merge_envelopes(a: InboundEnvelope, b: InboundEnvelope) -> InboundEnvelope {
    let text = match (a.text.trim(), b.text.trim()) {
        ("", "") => String::new(),
        ("", bt) => bt.to_string(),
        (at, "") => at.to_string(),
        (at, bt) => format!("{at}\n{bt}"),
    };

    let mut attachments = Vec::new();
    if let Some(existing) = a.attachments {
        attachments.extend(existing);
    }
    if let Some(existing) = b.attachments {
        attachments.extend(existing);
    }

    InboundEnvelope {
        sender_ref: if !b.sender_ref.trim().is_empty() {
            b.sender_ref
        } else {
            a.sender_ref
        },
        text,
        attachments: if attachments.is_empty() {
            None
        } else {
            Some(attachments)
        },
        metadata: b.metadata.or(a.metadata),
    }
}

async fn load_attachment_bytes(
    attachment: &InboundAttachment,
    http: &reqwest::Client,
) -> Result<Vec<u8>> {
    if let Some(data_base64) = attachment.data_base64.as_ref() {
        let decoded = STANDARD.decode(data_base64)?;
        if decoded.is_empty() {
            anyhow::bail!("inbound attachment payload is empty");
        }
        return Ok(decoded);
    }

    let Some(url) = attachment.download_url.as_ref() else {
        anyhow::bail!("inbound attachment missing data_base64 and download_url");
    };

    // Retry with back-off: adapters like BlueBubbles may fire the webhook
    // before the attachment transfer from the device is complete.
    const MAX_RETRIES: u32 = 3;
    const RETRY_DELAY: Duration = Duration::from_secs(2);
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            debug!(
                attempt = attempt,
                url = %url,
                "Retrying attachment download after delay"
            );
            tokio::time::sleep(RETRY_DELAY * attempt).await;
        }

        let response = match http.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(e.into());
                continue;
            }
        };

        if !response.status().is_success() {
            last_err = Some(anyhow::anyhow!(
                "attachment download failed with status {}",
                response.status()
            ));
            continue;
        }

        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                last_err = Some(e.into());
                continue;
            }
        };

        if bytes.is_empty() {
            last_err = Some(anyhow::anyhow!(
                "inbound attachment download returned empty payload"
            ));
            continue;
        }

        return Ok(bytes.to_vec());
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("attachment download failed after retries")))
}

fn attachment_filename(attachment: &InboundAttachment) -> String {
    if !attachment.filename.trim().is_empty() {
        return attachment.filename.clone();
    }
    let ext = attachment
        .mime_type
        .split('/')
        .nth(1)
        .filter(|v| !v.is_empty())
        .unwrap_or("bin");
    format!("image.{ext}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn merge_text_then_image_within_window() {
        let key_pipe = "p1";
        let key_ref = "chat-1";
        let first = InboundEnvelope {
            sender_ref: "s".to_string(),
            text: "hello".to_string(),
            attachments: None,
            metadata: None,
        };
        let second = InboundEnvelope {
            sender_ref: "s".to_string(),
            text: "".to_string(),
            attachments: Some(vec![InboundAttachment {
                filename: "a.jpg".to_string(),
                mime_type: "image/jpeg".to_string(),
                size_bytes: Some(12),
                download_url: None,
                data_base64: Some(STANDARD.encode(b"abc")),
            }]),
            metadata: None,
        };

        let t1 =
            tokio::spawn(async move { aggregate_inbound_envelope(key_pipe, key_ref, first).await });
        let t2 =
            tokio::spawn(async move { aggregate_inbound_envelope("p1", "chat-1", second).await });
        let r2 = t2.await.unwrap();
        let r1 = t1.await.unwrap();
        assert!(r2.is_none());
        let merged = r1.expect("first waiter should receive merged envelope");
        assert!(!merged.text.is_empty());
        assert!(merged.attachments.is_some());
    }

    #[tokio::test]
    async fn timeout_flushes_partial_message() {
        let env = InboundEnvelope {
            sender_ref: "s".to_string(),
            text: "".to_string(),
            attachments: Some(vec![InboundAttachment {
                filename: "a.jpg".to_string(),
                mime_type: "image/jpeg".to_string(),
                size_bytes: None,
                download_url: None,
                data_base64: Some(STANDARD.encode(b"abc")),
            }]),
            metadata: None,
        };

        let result = aggregate_inbound_envelope("p2", "chat-2", env).await;
        assert!(result.is_some());
    }
}
