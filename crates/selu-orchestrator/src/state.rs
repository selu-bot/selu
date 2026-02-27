use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex, Notify, RwLock};

use crate::agents::loader::AgentDefinition;
use crate::capabilities::CapabilityEngine;
use crate::channels::ChannelRegistry;
use crate::config::AppConfig;
use crate::events::EventBus;
use crate::llm::tool_loop::LoopSender;
use crate::permissions::CredentialStore;

/// Shared application state threaded through all Axum handlers
#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub config: Arc<AppConfig>,
    /// Agent definitions loaded from disk at startup (refreshable)
    pub agents: Arc<RwLock<HashMap<String, AgentDefinition>>>,
    /// Active SSE streams keyed by stream_id
    pub active_streams: Arc<Mutex<HashMap<String, LoopSender>>>,
    /// Notify signals for SSE stream readiness (keyed by stream_id)
    pub stream_notifies: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
    /// Pending tool-call confirmations keyed by confirmation_id.
    /// The oneshot sender is resolved when the user approves or denies.
    pub pending_confirmations: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    /// Pending asynchronous tool-call approvals keyed by approval_id.
    /// Used for non-interactive channels (iMessage, webhooks) where the
    /// user approves by replying to a thread rather than clicking a button.
    pub pending_approvals: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    /// Capability engine: manages Docker container lifecycle + gRPC dispatch
    pub capabilities: CapabilityEngine,
    /// Channel-agnostic outbound message registry.
    /// Adapters register their `ChannelSender` here at startup.
    pub channel_registry: ChannelRegistry,
    /// Encrypted credential store
    pub credentials: CredentialStore,
    /// Event bus: persists + broadcasts agent events for fanout
    pub events: EventBus,
}

impl AppState {
    pub fn new(
        db: SqlitePool,
        config: AppConfig,
        agents: HashMap<String, AgentDefinition>,
        capabilities: CapabilityEngine,
        channel_registry: ChannelRegistry,
        credentials: CredentialStore,
        events: EventBus,
    ) -> Self {
        Self {
            db,
            config: Arc::new(config),
            agents: Arc::new(RwLock::new(agents)),
            active_streams: Arc::new(Mutex::new(HashMap::new())),
            stream_notifies: Arc::new(Mutex::new(HashMap::new())),
            pending_confirmations: Arc::new(Mutex::new(HashMap::new())),
            pending_approvals: Arc::new(Mutex::new(HashMap::new())),
            capabilities,
            channel_registry,
            credentials,
            events,
        }
    }
}
