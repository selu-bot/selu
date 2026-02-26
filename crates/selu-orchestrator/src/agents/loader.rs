use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::Path;
use tokio::fs;

use crate::capabilities::manifest::{load_for_agent, CapabilityManifest};

// ── Agent definition ──────────────────────────────────────────────────────────

/// Parsed agent definition loaded from the installed agents directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub id: String,
    pub name: String,
    /// Model configuration is now optional — marketplace agents ship without a
    /// model. The actual model is resolved at runtime via `agents::model::resolve_model()`.
    #[serde(default)]
    pub model: Option<ModelConfig>,
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
    /// Installation steps for the setup wizard (optional, defined in agent.yaml)
    #[serde(default)]
    pub install_steps: Vec<InstallStep>,
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

// ── Install steps (generic wizard framework) ──────────────────────────────────

/// A single step in the post-install setup wizard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallStep {
    /// Unique step ID (used in template variable substitution, e.g. `{{step_id}}`)
    pub id: String,
    /// Step type: `input` (user fills in a value) or `test` (run a verification check)
    #[serde(rename = "type")]
    pub step_type: StepType,
    /// Human-readable label shown in the wizard
    pub label: String,
    /// Longer description / help text
    #[serde(default)]
    pub description: String,
    /// Default value for input steps
    #[serde(default)]
    pub default: Option<String>,
    /// Validation hint: "url", "not_empty", etc.
    #[serde(default)]
    pub validation: Option<String>,
    /// Where to store the value (for input steps)
    #[serde(default)]
    pub store_as: Option<CredentialTarget>,
    /// HTTP request to execute (for test steps)
    #[serde(default)]
    pub request: Option<TestRequest>,
    /// Step ID that must be completed before this step can run
    #[serde(default)]
    pub depends_on: Option<String>,
}

/// Where to persist a step's value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialTarget {
    /// "system_credential" or "user_credential"
    pub scope: String,
    pub capability_id: String,
    pub credential_name: String,
}

/// HTTP request for a test step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRequest {
    pub method: String,
    /// URL template — supports `{{step_id}}` for variable substitution
    pub url: String,
    pub expect_status: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StepType {
    Input,
    Test,
}

// ── Loading ───────────────────────────────────────────────────────────────────

/// Load all installed agent definitions.
///
/// Reads the `agents` DB table to find installed agents with `setup_complete = 1`,
/// then loads each agent's files from the `installed_dir`.
///
/// The bundled default agent is NOT loaded here — it is injected separately by
/// `agents::bundled::default_agent()`.
pub async fn load_installed(
    db: &SqlitePool,
    installed_dir: &str,
) -> Result<HashMap<String, AgentDefinition>> {
    let mut agents = HashMap::new();

    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM agents WHERE setup_complete = 1 AND is_bundled = 0",
    )
    .fetch_all(db)
    .await
    .context("Failed to query installed agents")?;

    let dir = Path::new(installed_dir);

    for (agent_id,) in rows {
        let agent_dir = dir.join(&agent_id);
        match load_one(&agent_dir).await {
            Ok(agent) => {
                tracing::info!(agent = %agent.id, "Loaded installed agent");
                agents.insert(agent.id.clone(), agent);
            }
            Err(e) => {
                tracing::warn!(
                    agent = %agent_id,
                    dir = %agent_dir.display(),
                    "Failed to load installed agent: {e}"
                );
            }
        }
    }

    Ok(agents)
}

/// Load a single agent definition from a directory.
///
/// Public so that the marketplace installer can use it after extracting an
/// agent archive.
pub async fn load_one(dir: &Path) -> Result<AgentDefinition> {
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
