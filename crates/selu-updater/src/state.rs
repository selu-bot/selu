use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::UpdaterConfig;

#[derive(Debug, Clone)]
pub struct RuntimeState {
    pub active_job_id: Option<String>,
    pub status: String,
    pub progress_key: String,
    pub message: String,
    pub installed_tag: String,
    pub installed_digest: String,
    pub installed_version: String,
    pub installed_build: String,
    pub previous_tag: String,
    pub previous_digest: String,
    pub previous_version: String,
    pub previous_build: String,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<UpdaterConfig>,
    pub runtime: Arc<Mutex<RuntimeState>>,
}

impl AppState {
    pub fn new(config: UpdaterConfig) -> Self {
        Self {
            config: Arc::new(config),
            runtime: Arc::new(Mutex::new(RuntimeState {
                active_job_id: None,
                status: "idle".to_string(),
                progress_key: "updates.progress.idle".to_string(),
                message: String::new(),
                installed_tag: String::new(),
                installed_digest: String::new(),
                installed_version: String::new(),
                installed_build: String::new(),
                previous_tag: String::new(),
                previous_digest: String::new(),
                previous_version: String::new(),
                previous_build: String::new(),
            })),
        }
    }
}
