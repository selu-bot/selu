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
    /// Optional external URL override for webhook callbacks.
    /// When not set, defaults to `http://localhost:{server.port}`.
    /// Set via `SELU__EXTERNAL_URL` for non-local setups.
    #[serde(default)]
    pub external_url: Option<String>,
}

fn default_max_chain_depth() -> i32 {
    3
}

impl AppConfig {
    /// The base URL external services should use to reach this Selu instance.
    /// Defaults to `http://localhost:{port}` when `external_url` is not set.
    pub fn external_base_url(&self) -> String {
        self.external_url
            .clone()
            .unwrap_or_else(|| format!("http://localhost:{}", self.server.port))
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
            .add_source(
                config::Environment::with_prefix("SELU")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()?;

        Ok(cfg.try_deserialize()?)
    }
}
