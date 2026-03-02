use arc_swap::ArcSwap;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex, Notify};

use crate::agents::loader::AgentDefinition;
use crate::capabilities::CapabilityEngine;
use crate::channels::ChannelRegistry;
use crate::config::AppConfig;
use crate::events::EventBus;
use crate::llm::registry::ProviderCache;
use crate::llm::tool_loop::LoopSender;
use crate::permissions::CredentialStore;

/// Agent map type alias: values are `Arc<AgentDefinition>` so that cloning
/// the map (or individual entries) only copies cheap `Arc` pointers.
pub type AgentMap = HashMap<String, Arc<AgentDefinition>>;

/// Shared application state threaded through all Axum handlers
#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub config: Arc<AppConfig>,
    /// Agent definitions loaded from disk at startup (refreshable).
    ///
    /// Uses `ArcSwap` for lock-free reads: readers call `agents.load()`
    /// which returns an `Arc<AgentMap>` via an atomic pointer load — no
    /// mutex, no allocation, no `.await`. Writers (rare: marketplace
    /// install/uninstall) use `agents.store()` or `rcu()`.
    pub agents: Arc<ArcSwap<AgentMap>>,
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
    /// LLM provider cache: avoids repeated DB lookups and key decryption
    pub provider_cache: ProviderCache,
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
        // Wrap each AgentDefinition in Arc for cheap cloning
        let arc_agents: AgentMap = agents.into_iter().map(|(k, v)| (k, Arc::new(v))).collect();
        Self {
            db,
            config: Arc::new(config),
            agents: Arc::new(ArcSwap::from_pointee(arc_agents)),
            active_streams: Arc::new(Mutex::new(HashMap::new())),
            stream_notifies: Arc::new(Mutex::new(HashMap::new())),
            pending_confirmations: Arc::new(Mutex::new(HashMap::new())),
            pending_approvals: Arc::new(Mutex::new(HashMap::new())),
            capabilities,
            channel_registry,
            credentials,
            events,
            provider_cache: ProviderCache::new(),
        }
    }
}
