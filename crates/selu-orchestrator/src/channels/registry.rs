/// Channel-agnostic outbound message abstraction.
///
/// Each transport (BlueBubbles, webhook, etc.) registers a `ChannelSender`
/// implementation keyed by `pipe_id`. The approval queue and other
/// subsystems that need to send out-of-band messages (approval prompts,
/// notifications) call `ChannelRegistry::send()` without knowing which
/// transport backs the pipe.
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::debug;

// ── Trait ──────────────────────────────────────────────────────────────────────

/// Abstraction over a messaging channel that can send messages into a thread.
///
/// Implementations must be `Send + Sync` so they can be shared across
/// tasks via the registry.
#[async_trait]
pub trait ChannelSender: Send + Sync {
    /// Send a text message into a conversation thread.
    ///
    /// `thread_id`     — Selu's internal thread UUID  
    /// `text`          — message body to send  
    /// `reply_to_guid` — optional external message GUID to reply-to
    ///                    (e.g. BlueBubbles `selectedMessageGuid`)  
    ///
    /// Returns the external GUID of the sent message, if the transport
    /// provides one. This is used to update `last_reply_guid` on the
    /// thread so future incoming replies are correctly matched.
    async fn send_message(
        &self,
        thread_id: &str,
        text: &str,
        reply_to_guid: Option<&str>,
    ) -> Result<Option<String>>;
}

// ── Registry ──────────────────────────────────────────────────────────────────

/// Central registry of per-pipe channel senders.
///
/// Adapters register their sender when starting up (e.g. the BlueBubbles
/// poller registers in `start_all`). The registry is stored on `AppState`
/// and accessible everywhere.
#[derive(Clone)]
pub struct ChannelRegistry {
    senders: Arc<RwLock<HashMap<String, Arc<dyn ChannelSender>>>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self {
            senders: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a channel sender for a pipe.
    ///
    /// Replaces any previously registered sender for the same pipe_id
    /// (e.g. when a BlueBubbles adapter is restarted).
    pub async fn register(&self, pipe_id: &str, sender: Arc<dyn ChannelSender>) {
        self.senders
            .write()
            .await
            .insert(pipe_id.to_string(), sender);
        debug!(pipe_id = %pipe_id, "Channel sender registered");
    }

    /// Remove the channel sender for a pipe.
    #[allow(dead_code)]
    pub async fn deregister(&self, pipe_id: &str) {
        self.senders.write().await.remove(pipe_id);
        debug!(pipe_id = %pipe_id, "Channel sender deregistered");
    }

    /// Send a message into a thread via the channel registered for `pipe_id`.
    ///
    /// Returns `Err` if no sender is registered for the pipe.
    pub async fn send(
        &self,
        pipe_id: &str,
        thread_id: &str,
        text: &str,
        reply_to_guid: Option<&str>,
    ) -> Result<Option<String>> {
        let senders = self.senders.read().await;
        let sender = senders
            .get(pipe_id)
            .ok_or_else(|| anyhow!("No channel sender registered for pipe '{}'", pipe_id))?
            .clone();
        // Drop the lock before the async send
        drop(senders);

        sender.send_message(thread_id, text, reply_to_guid).await
    }

    /// Check whether a sender is registered for a given pipe.
    #[allow(dead_code)]
    pub async fn has_sender(&self, pipe_id: &str) -> bool {
        self.senders.read().await.contains_key(pipe_id)
    }
}
