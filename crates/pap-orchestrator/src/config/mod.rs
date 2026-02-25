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
    /// Embedding API configuration for semantic memory
    #[serde(default)]
    pub embedding: EmbeddingApiConfig,
}

fn default_max_chain_depth() -> i32 {
    3
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

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct EmbeddingApiConfig {
    /// API key for the embedding provider (e.g. OpenAI)
    #[serde(default)]
    pub api_key: String,
    /// Base URL for the embedding API (default: https://api.openai.com)
    #[serde(default = "default_embedding_url")]
    pub base_url: String,
    /// Embedding model name (default: text-embedding-3-small)
    #[serde(default = "default_embedding_model")]
    pub model: String,
}

fn default_embedding_url() -> String {
    "https://api.openai.com".into()
}
fn default_embedding_model() -> String {
    "text-embedding-3-small".into()
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        dotenvy::dotenv().ok();

        let cfg = config::Config::builder()
            .set_default("server.host", "0.0.0.0")?
            .set_default("server.port", 3000)?
            .set_default("database.url", "sqlite://pap.db?mode=rwc")?
            .set_default("marketplace_url", "https://test.doko-ost.de/agents.json")?
            .set_default("installed_agents_dir", "./installed_agents")?
            .set_default("egress_proxy_addr", "0.0.0.0:8888")?
            .set_default("max_chain_depth", 3)?
            .set_default("embedding.api_key", "")?
            .set_default("embedding.base_url", "https://api.openai.com")?
            .set_default("embedding.model", "text-embedding-3-small")?
            .add_source(
                config::Environment::with_prefix("PAP")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()?;

        Ok(cfg.try_deserialize()?)
    }
}
