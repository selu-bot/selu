use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tokio::fs;

use crate::capabilities::manifest::{load_for_agent, CapabilityManifest};

/// Parsed agent definition loaded from the agents/ directory
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub id: String,
    pub name: String,
    pub model: ModelConfig,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    /// System prompt loaded from agent.md
    #[serde(skip_deserializing)]
    pub system_prompt: String,
    #[serde(default)]
    pub capabilities: Vec<CapabilityRef>,
    /// Capability manifests loaded from the agent's capabilities/ subdirectory
    #[serde(skip)]
    pub capability_manifests: HashMap<String, CapabilityManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub model_id: String,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
}

fn default_temperature() -> f32 { 0.7 }

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionConfig {
    #[serde(default = "default_trigger")]
    pub trigger: String,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_minutes: u32,
}

fn default_trigger() -> String { "mention".to_string() }
fn default_idle_timeout() -> u32 { 30 }

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryConfig {
    #[serde(default = "default_policy")]
    pub policy: String,
    #[serde(default = "default_top_k")]
    pub top_k: u32,
}

fn default_policy() -> String { "retrieval".to_string() }
fn default_top_k() -> u32 { 8 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityRef {
    pub id: String,
}

/// Loads all agent definitions from the agents_dir directory.
/// Each sub-directory with an agent.yaml is treated as an agent.
pub async fn load_all(agents_dir: &str) -> Result<HashMap<String, AgentDefinition>> {
    let mut agents = HashMap::new();
    let dir = Path::new(agents_dir);

    if !dir.exists() {
        tracing::warn!(dir = agents_dir, "Agents directory does not exist");
        return Ok(agents);
    }

    let mut entries = fs::read_dir(dir).await
        .with_context(|| format!("Failed to read agents dir: {}", agents_dir))?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        match load_one(&path).await {
            Ok(agent) => {
                tracing::info!(agent = %agent.id, "Loaded agent definition");
                agents.insert(agent.id.clone(), agent);
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), "Failed to load agent: {}", e);
            }
        }
    }

    Ok(agents)
}

async fn load_one(dir: &Path) -> Result<AgentDefinition> {
    let yaml_path = dir.join("agent.yaml");
    let md_path = dir.join("agent.md");

    let yaml_content = fs::read_to_string(&yaml_path).await
        .with_context(|| format!("Missing agent.yaml in {}", dir.display()))?;

    let mut agent: AgentDefinition = serde_yaml::from_str(&yaml_content)
        .with_context(|| format!("Invalid agent.yaml in {}", dir.display()))?;

    if md_path.exists() {
        agent.system_prompt = fs::read_to_string(&md_path).await
            .with_context(|| format!("Failed to read agent.md in {}", dir.display()))?;
    }

    // Load capability manifests from capabilities/ subdirectory
    let cap_list = load_for_agent(dir).await.unwrap_or_default();
    agent.capability_manifests = cap_list
        .into_iter()
        .map(|m| (m.id.clone(), m))
        .collect();

    Ok(agent)
}
