use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpdaterConfig {
    pub server: ServerConfig,
    pub shared_secret: String,
    pub compose_file: String,
    pub compose_project_dir: String,
    pub compose_service: String,
    pub updater_service: String,
    pub compose_env_file: String,
    pub health_url: String,
    pub health_timeout_secs: u64,
    pub health_interval_secs: u64,
    pub docker_bin: String,
    pub image_repo: String,
    pub updater_tag_env_key: String,
    pub updater_pending_tag_env_key: String,
    pub self_update_on_apply: bool,
    pub whatsapp_bridge_enabled: bool,
    pub whatsapp_bridge_image_repo: String,
    pub whatsapp_bridge_container_name: String,
    pub whatsapp_bridge_data_volume: String,
    pub whatsapp_bridge_network: String,
    pub whatsapp_bridge_port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

impl UpdaterConfig {
    pub fn load() -> Result<Self> {
        dotenvy::dotenv().ok();

        let cfg = config::Config::builder()
            .set_default("server.host", "0.0.0.0")?
            .set_default("server.port", 8090)?
            .set_default("shared_secret", "")?
            .set_default("compose_file", "./docker-compose.yml")?
            .set_default("compose_project_dir", "")?
            .set_default("compose_service", "selu")?
            .set_default("updater_service", "selu-updater")?
            .set_default("compose_env_file", "./.env")?
            .set_default("health_url", "http://127.0.0.1:3000/api/health")?
            .set_default("health_timeout_secs", 90)?
            .set_default("health_interval_secs", 3)?
            .set_default("docker_bin", "docker")?
            .set_default("image_repo", "ghcr.io/selu-bot/selu")?
            .set_default("updater_tag_env_key", "SELU_UPDATER_IMAGE_TAG")?
            .set_default("updater_pending_tag_env_key", "SELU_UPDATER_PENDING_TAG")?
            .set_default("self_update_on_apply", true)?
            .set_default("whatsapp_bridge_enabled", true)?
            .set_default(
                "whatsapp_bridge_image_repo",
                "ghcr.io/selu-bot/selu-whatsapp-bridge",
            )?
            .set_default("whatsapp_bridge_container_name", "selu-whatsapp-bridge")?
            .set_default("whatsapp_bridge_data_volume", "selu-whatsapp-bridge-data")?
            .set_default("whatsapp_bridge_network", "")?
            .set_default("whatsapp_bridge_port", 3200)?
            .add_source(
                config::Environment::with_prefix("UPDATER")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()?;

        Ok(cfg.try_deserialize()?)
    }
}
