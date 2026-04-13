use anyhow::Result;
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod admin;
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
mod telemetry;
mod updater;
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

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = config::AppConfig::load()?;

    if let Some(cmd) = admin::parse(&args)? {
        if matches!(cmd, admin::AdminCommand::Help) {
            admin::print_usage();
            return Ok(());
        }
        let db = persistence::db::connect(&cfg.database.url).await?;
        return admin::run(cmd, &db).await;
    }

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

    // ── Backfill built-in tool policies for all agents (handles upgrades) ─────
    agents::bundled::backfill_builtin_policies(&db).await?;

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
    if let Err(e) = runner.ensure_networks().await {
        warn!(
            "Capability Docker setup unavailable at startup; continuing without capability network preflight: {e:#}"
        );
    }
    if let Err(e) = runner.cleanup_stale_containers().await {
        warn!("Failed to cleanup stale capability containers at startup: {e:#}");
    }
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

    web::system_updates::hydrate_public_origin_override(&state).await;

    // Best-effort dynamic capability tool sync at startup.
    {
        let sync_state = state.clone();
        tokio::spawn(async move {
            let agents_snapshot = sync_state.agents.load();
            capabilities::discovery::sync_dynamic_tools_for_all_agents(
                &sync_state.db,
                &sync_state.capabilities,
                &sync_state.credentials,
                &agents_snapshot,
            )
            .await;
        });
    }

    // Start fanout after state is built (needs AppState)
    fanout::start(state.clone(), fanout_rx, cfg.max_chain_depth);

    // ── BlueBubbles adapters ──────────────────────────────────────────────────
    // Register channel senders + auto-upgrade legacy polling configs to webhooks
    bluebubbles::adapter::init_all(state.clone()).await;

    // ── Telegram adapters ─────────────────────────────────────────────────────
    // Register channel senders + set webhooks with Telegram
    telegram::adapter::init_all(state.clone()).await;

    // ── WhatsApp bridge sidecar ───────────────────────────────────────────────
    // If an active WhatsApp pipe exists, ensure its bridge container is running.
    web::whatsapp::ensure_bridge_for_active_pipe(&state).await;

    // ── Background tasks ──────────────────────────────────────────────────────

    // Session idle cleanup: runs every 5 minutes
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            // Resolve per-agent idle timeout values.
            let agents = cleanup_state.agents.load();
            let mut idle_by_agent = std::collections::HashMap::new();
            for (agent_id, def) in agents.iter() {
                idle_by_agent.insert(agent_id.clone(), def.session.idle_timeout_minutes);
            }
            drop(agents);

            match agents::session::close_idle_sessions(&cleanup_state.db, &idle_by_agent, 30).await
            {
                Ok(ref idled) if !idled.is_empty() => {
                    info!("Closed {} idle session(s)", idled.len());
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

    // Shared capability idle cleanup: runs every 60 seconds
    let shared_reaper_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            // Collect all capability manifests across all agents.
            let agents = shared_reaper_state.agents.load();
            let mut all_manifests = std::collections::HashMap::new();
            for agent in agents.values() {
                for (cap_id, manifest) in &agent.capability_manifests {
                    all_manifests
                        .entry(cap_id.clone())
                        .or_insert(manifest.clone());
                }
            }
            shared_reaper_state
                .capabilities
                .close_idle_shared(&all_manifests)
                .await;
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
                &autoupdate_state.credentials,
            )
            .await
            {
                Ok(0) => {}
                Ok(n) => info!("Auto-updated {n} agent(s)"),
                Err(e) => tracing::warn!("Agent auto-update check failed: {e}"),
            }
        }
    });

    // System update metadata checks: runs daily (if enabled in settings)
    let system_update_state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(90)).await;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60 * 60 * 24));
        loop {
            if let Err(e) =
                web::system_updates::run_auto_check_if_enabled(&system_update_state).await
            {
                tracing::warn!("System update auto-check failed: {e}");
            }
            interval.tick().await;
        }
    });

    // Anonymous installation telemetry: send at startup and every 24 hours
    let telemetry_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60 * 60 * 24));
        loop {
            interval.tick().await;
            telemetry::send_heartbeat_with_logging(&telemetry_state).await;
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

    // Self-improvement periodic aggregation: every 6 hours
    // Promotes candidate insights, prunes old signals, aggregates daily metrics.
    let improvement_db = state.db.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(6 * 60 * 60));
        loop {
            interval.tick().await;
            if let Err(e) =
                crate::agents::improvement::run_periodic_aggregation(&improvement_db).await
            {
                tracing::debug!("Improvement aggregation failed (non-fatal): {e}");
            }
        }
    });

    // Profile fact optimization: runs at startup and then every 24 hours.
    // Deduplicates, recategorizes, and consolidates user profile facts via LLM.
    let profile_state = state.clone();
    tokio::spawn(async move {
        // Short delay to let the system settle before first run.
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60 * 60 * 24));
        loop {
            if let Err(e) = crate::agents::profile::run_optimization(
                &profile_state.db,
                &profile_state.credentials,
            )
            .await
            {
                tracing::debug!("Profile optimization failed (non-fatal): {e}");
            }
            interval.tick().await;
        }
    });

    let shutdown_state = state.clone();

    let app = axum::Router::new()
        .merge(web::router(state.clone()))
        .merge(api::router(state.clone()))
        .merge(api::mobile::router())
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
