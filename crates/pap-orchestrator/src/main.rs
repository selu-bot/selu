use anyhow::Result;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod state;
mod persistence;
mod pipes;
mod agents;
mod bluebubbles;
mod capabilities;
mod events;
mod llm;
mod permissions;
mod web;
mod api;

use state::AppState;
use capabilities::{
    egress_proxy,
    runner::CapabilityRunner,
    CapabilityEngine,
};
use permissions::{store::parse_key, CredentialStore};
use events::{EventBus, fanout};
use agents::memory::{MemoryStore, EmbeddingConfig};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pap_orchestrator=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cfg = config::AppConfig::load()?;
    info!("Starting PAP orchestrator on {}:{}", cfg.server.host, cfg.server.port);

    let db = persistence::db::connect(&cfg.database.url).await?;

    let agent_defs = agents::loader::load_all(&cfg.agents_dir).await?;
    info!("Loaded {} agent(s)", agent_defs.len());

    // ── Credential store ──────────────────────────────────────────────────────
    let enc_key = parse_key(&cfg.encryption_key)?;
    let cred_store = CredentialStore::new(db.clone(), enc_key);

    // ── Capability subsystem ──────────────────────────────────────────────────
    let egress_registry = egress_proxy::new_registry();
    let egress_addr: std::net::SocketAddr = cfg.egress_proxy_addr.parse()
        .unwrap_or_else(|_| "0.0.0.0:8888".parse().unwrap());
    let proxy_registry = egress_registry.clone();
    tokio::spawn(async move {
        if let Err(e) = egress_proxy::run_proxy(egress_addr, proxy_registry).await {
            tracing::error!("Egress proxy error: {e}");
        }
    });

    let runner = CapabilityRunner::new(egress_registry, &cfg.egress_proxy_addr)?;
    runner.ensure_networks().await?;
    let cap_engine = CapabilityEngine::new(runner);

    // ── Event bus + fanout ────────────────────────────────────────────────────
    let (event_bus, fanout_rx) = EventBus::new(db.clone());

    // ── Semantic memory ───────────────────────────────────────────────────────
    let memory = if !cfg.embedding.api_key.is_empty() {
        info!("Semantic memory enabled (embedding model: {})", cfg.embedding.model);
        Some(MemoryStore::new(
            db.clone(),
            EmbeddingConfig {
                api_key: cfg.embedding.api_key.clone(),
                base_url: cfg.embedding.base_url.clone(),
                model: cfg.embedding.model.clone(),
            },
        ))
    } else {
        info!("Semantic memory disabled (no PAP__EMBEDDING__API_KEY set)");
        None
    };

    // ── Build shared state ────────────────────────────────────────────────────
    let state = AppState::new(db, cfg.clone(), agent_defs, cap_engine, cred_store, event_bus, memory);

    // Start fanout after state is built (needs AppState)
    fanout::start(state.clone(), fanout_rx, cfg.max_chain_depth);

    // ── BlueBubbles adapters ──────────────────────────────────────────────────
    // Start all configured BlueBubbles polling adapters
    bluebubbles::adapter::start_all(state.clone()).await;

    // ── Background tasks ──────────────────────────────────────────────────────

    // Session idle cleanup: runs every 5 minutes
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            // Get the minimum idle timeout across all agents (or use 30 as default)
            let agents = cleanup_state.agents.read().await;
            let min_idle = agents.values()
                .map(|a| a.session.idle_timeout_minutes)
                .min()
                .unwrap_or(30);
            drop(agents);

            match agents::session::close_idle_sessions(&cleanup_state.db, min_idle).await {
                Ok(ref idled) if !idled.is_empty() => {
                    info!("Closed {} idle session(s) (threshold: {} min)", idled.len(), min_idle);
                    // Stop capability containers for each session that was just idled
                    for session_id in idled {
                        cleanup_state.capabilities.close_session(session_id).await;
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::error!("Idle session cleanup error: {e}"),
            }
        }
    });

    // Workspace volume TTL cleanup: runs every 15 minutes
    let ttl_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(900));
        loop {
            interval.tick().await;
            if let Err(e) = capabilities::cleanup_expired_workspaces(&ttl_state).await {
                tracing::error!("Workspace TTL cleanup error: {e}");
            }
        }
    });

    // Web session cleanup: runs every hour
    let ws_db = state.db.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            let _ = sqlx::query!("DELETE FROM web_sessions WHERE expires_at < datetime('now')")
                .execute(&ws_db)
                .await;
        }
    });

    let app = axum::Router::new()
        .merge(web::router(state.clone()))
        .merge(api::router(state.clone()))
        .merge(pipes::inbound::router())
        .with_state(state);

    let addr: std::net::SocketAddr = format!("{}:{}", cfg.server.host, cfg.server.port).parse()?;
    info!("Listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
