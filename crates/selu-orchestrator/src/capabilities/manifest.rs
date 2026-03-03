use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::fs;

/// Parsed capability manifest (capability.yaml inside agent's capabilities/ dir)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityManifest {
    pub id: String,
    pub class: CapabilityClass,
    pub image: String,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub network: NetworkPolicy,
    #[serde(default)]
    pub filesystem: FilesystemPolicy,
    #[serde(default)]
    pub credentials: Vec<CredentialDeclaration>,
    #[serde(default)]
    pub resources: ResourceLimits,
    /// Contents of the co-located prompt.md, injected into agent context
    #[serde(skip_deserializing)]
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityClass {
    #[default]
    Tool, // stateless, short-lived
    Environment, // stateful, needs a workspace volume
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    /// Legacy field — kept for backward compatibility.
    /// When true and `recommended_policy` is absent, the recommended
    /// policy defaults to `"ask"`.
    #[serde(default)]
    pub requires_confirmation: bool,
    /// The agent author's recommended policy for this tool.
    /// Users see this as the default when configuring permissions during
    /// the install wizard.
    ///
    /// Valid values: `"allow"`, `"ask"`, `"block"`.
    ///
    /// If absent, derived from `requires_confirmation`:
    ///   - `requires_confirmation: true`  → `"ask"`
    ///   - `requires_confirmation: false` → `"block"` (secure default)
    #[serde(default)]
    pub recommended_policy: Option<String>,
}

impl ToolDefinition {
    /// Return the effective recommended policy for this tool.
    ///
    /// Priority:
    ///   1. Explicit `recommended_policy` field
    ///   2. `requires_confirmation: true` → `"ask"`
    ///   3. Default → `"block"` (secure default)
    pub fn effective_recommended_policy(&self) -> &str {
        if let Some(ref p) = self.recommended_policy {
            return p.as_str();
        }
        if self.requires_confirmation {
            "ask"
        } else {
            "block"
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkPolicy {
    #[serde(default)]
    pub mode: NetworkMode,
    #[serde(default)]
    pub hosts: Vec<String>, // "hostname:port" entries for allowlist mode
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    #[default]
    None, // no external network access
    Allowlist, // only listed hosts
    Any,       // unrestricted (not recommended)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemPolicy {
    #[default]
    None, // read-only root, no persistent storage
    Temp,      // tmpfs /tmp only
    Workspace, // named Docker volume mounted at /workspace (Environment class only)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialDeclaration {
    pub name: String,
    pub scope: CredentialScope,
    #[serde(default = "default_cred_type")]
    pub credential_type: String,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub description: String,
}

fn default_cred_type() -> String {
    "secret".to_string()
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialScope {
    User,   // per-user, prompted at first use
    System, // shared across users, set at agent install time
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    #[serde(default = "default_memory_mb")]
    pub max_memory_mb: u32,
    #[serde(default = "default_cpu")]
    pub max_cpu_fraction: f32,
    #[serde(default = "default_cpu_seconds")]
    pub max_cpu_seconds: u32,
    #[serde(default = "default_pids")]
    pub pids_limit: u32,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_memory_mb: default_memory_mb(),
            max_cpu_fraction: default_cpu(),
            max_cpu_seconds: default_cpu_seconds(),
            pids_limit: default_pids(),
        }
    }
}

fn default_memory_mb() -> u32 {
    128
}
fn default_cpu() -> f32 {
    0.5
}
fn default_cpu_seconds() -> u32 {
    30
}
fn default_pids() -> u32 {
    64
}

/// Load a capability manifest from a directory containing manifest.yaml (+ optional prompt.md)
pub async fn load_from_dir(dir: &Path) -> Result<CapabilityManifest> {
    let yaml_path = dir.join("manifest.yaml");
    let prompt_path = dir.join("prompt.md");

    let yaml = fs::read_to_string(&yaml_path)
        .await
        .with_context(|| format!("Missing manifest.yaml in {}", dir.display()))?;

    let mut manifest: CapabilityManifest = serde_yaml::from_str(&yaml)
        .with_context(|| format!("Invalid manifest.yaml in {}", dir.display()))?;

    if prompt_path.exists() {
        manifest.prompt = fs::read_to_string(&prompt_path)
            .await
            .with_context(|| format!("Failed to read prompt.md in {}", dir.display()))?;
    }

    Ok(manifest)
}

/// Load all capabilities declared in an agent's capabilities/ subdirectory
pub async fn load_for_agent(agent_dir: &Path) -> Result<Vec<CapabilityManifest>> {
    let caps_dir = agent_dir.join("capabilities");
    if !caps_dir.exists() {
        return Ok(vec![]);
    }

    let mut caps = Vec::new();
    let mut entries = fs::read_dir(&caps_dir)
        .await
        .with_context(|| format!("Cannot read capabilities dir: {}", caps_dir.display()))?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            match load_from_dir(&path).await {
                Ok(cap) => {
                    tracing::debug!(capability = %cap.id, "Loaded capability manifest");
                    caps.push(cap);
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), "Failed to load capability: {e}");
                }
            }
        }
    }

    Ok(caps)
}
