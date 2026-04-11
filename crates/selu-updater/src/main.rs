mod api;
mod config;
mod engine;
mod state;
mod types;

use anyhow::Result;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::config::UpdaterConfig;
use crate::state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "selu_updater=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cfg = UpdaterConfig::load()?;
    let addr: std::net::SocketAddr = format!("{}:{}", cfg.server.host, cfg.server.port).parse()?;

    check_docker(&cfg).await;

    let state = AppState::new(cfg);
    let app = api::router(state);

    info!("Starting selu-updater on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn check_docker(cfg: &UpdaterConfig) {
    match tokio::process::Command::new(&cfg.docker_bin)
        .args(["compose", "version"])
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            info!(
                docker_bin = %cfg.docker_bin,
                "Docker compose available: {}",
                version.trim()
            );
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!(
                docker_bin = %cfg.docker_bin,
                "Docker compose command failed: {}",
                stderr.trim()
            );
        }
        Err(e) => {
            error!(
                docker_bin = %cfg.docker_bin,
                "Cannot execute docker binary: {e}"
            );
        }
    }

    // Check compose file exists
    if !tokio::fs::try_exists(&cfg.compose_file)
        .await
        .unwrap_or(false)
    {
        warn!(
            compose_file = %cfg.compose_file,
            "Compose file not found — updates will fail"
        );
    }

    // Check env file exists
    if !tokio::fs::try_exists(&cfg.compose_env_file)
        .await
        .unwrap_or(false)
    {
        warn!(
            compose_env_file = %cfg.compose_env_file,
            "Compose env file not found — it will be created on first update"
        );
    }
}
