use anyhow::Result;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod agents;
mod api;
mod bluebubbles;
mod capabilities;
mod channels;
mod commands;
mod config;
mod events;
mod i18n;
mod llm;
mod permissions;
mod persistence;
mod pipes;
mod schedules;
mod state;
mod telegram;
mod web;

use capabilities::{CapabilityEngine, egress_proxy, runner::CapabilityRunner};
use channels::ChannelRegistry;
use events::{EventBus, fanout};
use permissions::{CredentialStore, store::parse_key};
use state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "selu_orchestrator=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cfg = config::AppConfig::load()?;
    info!(
        "Starting Selu orchestrator on {}:{}",
        cfg.server.host, cfg.server.port
    );

    let db = persistence::db::connect(&cfg.database.url).await?;

    // ── Ensure installed agents directory exists ──────────────────────────────
    tokio::fs::create_dir_all(&cfg.installed_agents_dir).await?;

    // ── Ensure bundled default agent has a DB row ─────────────────────────────
    agents::bundled::ensure_db_row(&db).await?;
    agents::bundled::ensure_global_policies(&db).await?;

    // ── Load installed agents from DB + filesystem ────────────────────────────
    let mut agent_defs = agents::loader::load_installed(&db, &cfg.installed_agents_dir).await?;

    // Always inject the bundled default agent (cannot be uninstalled)
    let default_agent = agents::bundled::default_agent();
    agent_defs.insert("default".to_string(), default_agent);

    info!(
        "Loaded {} agent(s) (including bundled default)",
        agent_defs.len()
    );

    // ── Credential store ──────────────────────────────────────────────────────
    let enc_key = parse_key(&cfg.encryption_key)?;
    let cred_store = CredentialStore::new(db.clone(), enc_key);

    // ── Capability subsystem ──────────────────────────────────────────────────
    let egress_registry = egress_proxy::new_registry();
    let egress_addr: std::net::SocketAddr = cfg
        .egress_proxy_addr
        .parse()
        .unwrap_or_else(|_| "0.0.0.0:8888".parse().unwrap());
    let proxy_registry = egress_registry.clone();

    // Egress log: channel + background drain task
    let (egress_log_tx, egress_log_rx) = egress_proxy::new_log_channel(1024);
    let log_db = persistence::db::connect(&cfg.database.url).await?;
    tokio::spawn(egress_proxy::drain_egress_log(log_db, egress_log_rx));

    tokio::spawn(async move {
        if let Err(e) = egress_proxy::run_proxy(egress_addr, proxy_registry, egress_log_tx).await {
            tracing::error!("Egress proxy error: {e}");
        }
    });

    let runner = CapabilityRunner::new(egress_registry, &cfg.egress_proxy_addr)?;
    runner.ensure_networks().await?;
    runner.cleanup_stale_containers().await?;
    let cap_engine = CapabilityEngine::new(runner);

    // ── Event bus + fanout ────────────────────────────────────────────────────
    let (event_bus, fanout_rx) = EventBus::new(db.clone());

    // ── Build shared state ────────────────────────────────────────────────────
    let channel_registry = ChannelRegistry::new();
    let state = AppState::new(
        db,
        cfg.clone(),
        agent_defs,
        cap_engine,
        channel_registry,
        cred_store,
        event_bus,
    );

    // Start fanout after state is built (needs AppState)
    fanout::start(state.clone(), fanout_rx, cfg.max_chain_depth);

    // ── BlueBubbles adapters ──────────────────────────────────────────────────
    // Register channel senders + auto-upgrade legacy polling configs to webhooks
    bluebubbles::adapter::init_all(state.clone()).await;

    // ── Telegram adapters ─────────────────────────────────────────────────────
    // Register channel senders + set webhooks with Telegram
    telegram::adapter::init_all(state.clone()).await;

    // ── Background tasks ──────────────────────────────────────────────────────

    // Session idle cleanup: runs every 5 minutes
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            // Get the minimum idle timeout across all agents (or use 30 as default)
            let agents = cleanup_state.agents.load();
            let min_idle = agents
                .values()
                .map(|a| a.session.idle_timeout_minutes)
                .min()
                .unwrap_or(30);
            drop(agents);

            match agents::session::close_idle_sessions(&cleanup_state.db, min_idle).await {
                Ok(ref idled) if !idled.is_empty() => {
                    info!(
                        "Closed {} idle session(s) (threshold: {} min)",
                        idled.len(),
                        min_idle
                    );
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

    // Thread idle cleanup: close threads with no activity for 48 hours
    let thread_db = state.db.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            match agents::thread::close_idle_threads(&thread_db, 48).await {
                Ok(count) if count > 0 => {
                    tracing::info!(count = count, "Closed idle threads (48h)");
                }
                Err(e) => tracing::error!("Thread idle cleanup error: {e}"),
                _ => {}
            }
        }
    });

    // Pending tool approval expiry: runs every 30 seconds
    let approval_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            if let Err(e) = permissions::approval_queue::expire_pending(&approval_state).await {
                tracing::error!("Approval expiry error: {e}");
            }
        }
    });

    // Agent auto-update: checks the marketplace every 30 minutes
    let autoupdate_state = state.clone();
    tokio::spawn(async move {
        // Wait 2 minutes after startup before first check
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1800));
        loop {
            interval.tick().await;
            match agents::marketplace::auto_update_agents(
                &autoupdate_state.config.marketplace_url,
                &autoupdate_state.config.installed_agents_dir,
                &autoupdate_state.db,
                &autoupdate_state.agents,
                &autoupdate_state.capabilities,
            )
            .await
            {
                Ok(0) => {}
                Ok(n) => info!("Auto-updated {n} agent(s)"),
                Err(e) => tracing::warn!("Agent auto-update check failed: {e}"),
            }
        }
    });

    // Schedule executor: fires due schedules every 30 seconds
    let schedule_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            schedules::executor::tick(&schedule_state).await;
        }
    });

    let shutdown_state = state.clone();

    let app = axum::Router::new()
        .merge(web::router(state.clone()))
        .merge(api::router(state.clone()))
        .merge(pipes::inbound::router())
        .merge(bluebubbles::adapter::router())
        .merge(telegram::adapter::router())
        .with_state(state.clone());

    // Wrap the entire Axum router with a Tower service that strips the
    // X-Forwarded-Prefix from the URI *before* route matching.  This must
    // be an outer Tower service — Axum's Router::layer() only wraps handlers
    // and cannot rewrite the URI before routes are matched.
    let app = tower::ServiceBuilder::new()
        .layer(web::StripPrefixLayer { state })
        .service(app);

    let addr: std::net::SocketAddr = format!("{}:{}", cfg.server.host, cfg.server.port).parse()?;
    info!("Listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Graceful shutdown: stop accepting requests when a signal arrives, then
    // tear down all running capability containers so they don't get orphaned.
    axum::serve(listener, tower::make::Shared::new(app))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("Shutting down – cleaning up capability containers...");
    shutdown_state.capabilities.close_all().await;
    info!("Shutdown complete");

    Ok(())
}

/// Wait for SIGINT (Ctrl+C) or SIGTERM (docker stop / systemd stop).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received SIGINT"),
        _ = terminate => info!("Received SIGTERM"),
    }
}
