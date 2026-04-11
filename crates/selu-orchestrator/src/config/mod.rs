use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    /// URL of the agent marketplace catalogue JSON
    pub marketplace_url: String,
    /// Directory where installed agents are stored (Docker volume mount)
    pub installed_agents_dir: String,
    pub encryption_key: String, // base64-encoded 32-byte key
    /// Address the egress proxy listens on.
    /// Listens on 0.0.0.0:<port> so it is reachable from all Docker bridge networks.
    /// The port is injected into containers as HTTP_PROXY with the per-network gateway IP.
    /// Default: "0.0.0.0:8888"
    pub egress_proxy_addr: String,
    /// Maximum event chain depth for loop prevention (default: 3)
    #[serde(default = "default_max_chain_depth")]
    pub max_chain_depth: i32,
    /// Maximum LLM tool-loop iterations per turn (default: 16).
    /// This bounds recursive tool use while allowing complex multi-step flows.
    #[serde(default = "default_max_tool_loop_iterations")]
    pub max_tool_loop_iterations: u32,
    /// Optional startup fallback for the public web address Selu shares with
    /// external services. Admin users can override this in the UI.
    /// When not set, defaults to `http://localhost:{server.port}`.
    #[serde(default)]
    pub external_url: Option<String>,
    /// Optional URL path prefix for running behind a reverse proxy with a
    /// sub-path (e.g. `/selu`).  When set, all generated links, redirects,
    /// and cookie paths include this prefix.  The value must start with `/`
    /// and must **not** end with `/`.
    /// Set via `SELU__BASE_PATH`.
    #[serde(default)]
    pub base_path: Option<String>,
    /// Release metadata endpoint used for server update checks.
    /// Example: https://selu.bot/api/releases/selu
    pub release_metadata_url: String,
    /// Sidecar updater base URL.
    pub updater_url: String,
    /// Shared secret used to authenticate orchestrator -> updater calls.
    pub updater_shared_secret: String,
    /// Request timeout for updater sidecar calls.
    pub updater_request_timeout_secs: u64,
}

fn default_max_chain_depth() -> i32 {
    3
}

fn default_max_tool_loop_iterations() -> u32 {
    32
}

impl AppConfig {
    /// The URL path prefix for reverse-proxy sub-path deployments.
    /// Returns an empty string when no prefix is configured.
    /// Always starts with `/` (when set) and never ends with `/`.
    pub fn base_path(&self) -> &str {
        match &self.base_path {
            Some(p) => p.trim_end_matches('/'),
            None => "",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseConfig {
    pub url: String,
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        dotenvy::dotenv().ok();

        let cfg = config::Config::builder()
            .set_default("server.host", "0.0.0.0")?
            .set_default("server.port", 3000)?
            .set_default("database.url", "sqlite://selu.db?mode=rwc")?
            .set_default("marketplace_url", "https://selu.bot/api/marketplace/agents")?
            .set_default("installed_agents_dir", "./installed_agents")?
            .set_default("egress_proxy_addr", "0.0.0.0:8888")?
            .set_default("max_chain_depth", 3)?
            .set_default("max_tool_loop_iterations", 32)?
            .set_default("release_metadata_url", "https://selu.bot/api/releases/selu")?
            .set_default("updater_url", "http://selu-updater:8090")?
            .set_default("updater_shared_secret", "")?
            .set_default("updater_request_timeout_secs", 30)?
            .add_source(
                config::Environment::with_prefix("SELU")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()?;

        Ok(cfg.try_deserialize()?)
    }
}
