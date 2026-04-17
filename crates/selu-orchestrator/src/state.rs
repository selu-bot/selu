use arc_swap::{ArcSwap, ArcSwapOption};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, Notify, oneshot};

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

#[derive(Debug, Clone)]
pub struct AgentUpdateJob {
    pub owner_user_id: String,
    pub agent_id: String,
    pub agent_name: String,
    pub target_version: String,
    pub progress: u8,
    pub message_key: String,
    pub done: bool,
    pub success: bool,
    pub redirect_to: Option<String>,
    pub error_key: Option<String>,
    pub updated_at: Instant,
}

/// Shared application state threaded through all Axum handlers
#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub config: Arc<AppConfig>,
    /// Optional admin-configured public origin override, persisted in SQLite
    /// and applied immediately without restart.
    pub public_origin_override: Arc<ArcSwapOption<String>>,
    /// URL path prefix for reverse-proxy sub-path deployments (e.g. "/selu").
    /// Empty string when not configured.  Computed once at startup from config.
    pub base_path: String,
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
    /// Maps thread_id → stream_id for active mobile agent runs.
    /// Used by the mobile active-stream endpoint so clients can reconnect.
    pub thread_active_streams: Arc<Mutex<HashMap<String, String>>>,
    /// Maps thread_id → last tool-status text for active agent runs.
    /// Allows clients reconnecting mid-stream to immediately show what the
    /// agent is doing before the next SSE event arrives.
    pub thread_last_status: Arc<Mutex<HashMap<String, String>>>,
    /// Pending tool-call confirmations keyed by confirmation_id.
    /// The oneshot sender is resolved when the user approves or denies.
    pub pending_confirmations: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    /// Pending asynchronous tool-call approvals keyed by approval_id.
    /// Used for non-interactive channels (iMessage, webhooks) where the
    /// user approves by replying to a thread rather than clicking a button.
    pub pending_approvals: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    /// In-flight and recent agent update jobs keyed by update job id.
    pub agent_update_jobs: Arc<Mutex<HashMap<String, AgentUpdateJob>>>,
    /// Capability engine: manages Docker container lifecycle + gRPC dispatch
    pub capabilities: CapabilityEngine,
    /// Channel-agnostic outbound message registry.
    /// Adapters register their `ChannelSender` here at startup.
    pub channel_registry: ChannelRegistry,
    /// Encrypted credential store
    pub credentials: CredentialStore,
    /// Event bus: persists + broadcasts agent events for fanout
    #[allow(dead_code)]
    pub events: EventBus,
    /// LLM provider cache: avoids repeated DB lookups and key decryption
    pub provider_cache: ProviderCache,
    /// Session-scoped artifact payloads (e.g. generated PDFs) referenced by ID
    pub artifacts: crate::agents::artifacts::ArtifactStore,
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
        let base_path = config.base_path().to_string();
        // Wrap each AgentDefinition in Arc for cheap cloning
        let arc_agents: AgentMap = agents.into_iter().map(|(k, v)| (k, Arc::new(v))).collect();
        Self {
            db,
            config: Arc::new(config),
            public_origin_override: Arc::new(ArcSwapOption::from(None)),
            base_path,
            agents: Arc::new(ArcSwap::from_pointee(arc_agents)),
            active_streams: Arc::new(Mutex::new(HashMap::new())),
            stream_notifies: Arc::new(Mutex::new(HashMap::new())),
            thread_active_streams: Arc::new(Mutex::new(HashMap::new())),
            thread_last_status: Arc::new(Mutex::new(HashMap::new())),
            pending_confirmations: Arc::new(Mutex::new(HashMap::new())),
            pending_approvals: Arc::new(Mutex::new(HashMap::new())),
            agent_update_jobs: Arc::new(Mutex::new(HashMap::new())),
            capabilities,
            channel_registry,
            credentials,
            events,
            provider_cache: ProviderCache::new(),
            artifacts: crate::agents::artifacts::new_store(),
        }
    }

    pub fn public_origin(&self) -> String {
        if let Some(origin) = self.public_origin_override.load_full() {
            return origin.trim_end_matches('/').to_string();
        }
        if let Some(origin) = self.config.external_url.clone() {
            let origin = origin.trim().trim_end_matches('/').to_string();
            if !origin.is_empty() {
                return origin;
            }
        }
        format!("http://localhost:{}", self.config.server.port)
    }

    pub fn public_base_url(&self) -> String {
        let origin = self.public_origin();
        if self.base_path.is_empty() {
            origin
        } else {
            format!("{}{}", origin, self.base_path)
        }
    }
}
