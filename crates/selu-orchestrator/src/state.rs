use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex, Notify, RwLock};

use crate::agents::loader::AgentDefinition;
use crate::agents::memory::MemoryStore;
use crate::capabilities::CapabilityEngine;
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
    /// Capability engine: manages Docker container lifecycle + gRPC dispatch
    pub capabilities: CapabilityEngine,
    /// Encrypted credential store
    pub credentials: CredentialStore,
    /// Event bus: persists + broadcasts agent events for fanout
    pub events: EventBus,
    /// Semantic memory store (None if no embedding API key configured)
    pub memory: Option<Arc<MemoryStore>>,
}

impl AppState {
    pub fn new(
        db: SqlitePool,
        config: AppConfig,
        agents: HashMap<String, AgentDefinition>,
        capabilities: CapabilityEngine,
        credentials: CredentialStore,
        events: EventBus,
        memory: Option<MemoryStore>,
    ) -> Self {
        Self {
            db,
            config: Arc::new(config),
            agents: Arc::new(RwLock::new(agents)),
            active_streams: Arc::new(Mutex::new(HashMap::new())),
            stream_notifies: Arc::new(Mutex::new(HashMap::new())),
            pending_confirmations: Arc::new(Mutex::new(HashMap::new())),
            capabilities,
            credentials,
            events,
            memory: memory.map(Arc::new),
        }
    }
}
