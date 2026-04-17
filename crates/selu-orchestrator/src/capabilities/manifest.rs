use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tokio::fs;

/// Parsed capability manifest (capability.yaml inside agent's capabilities/ dir)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityManifest {
    pub id: String,
    pub class: CapabilityClass,
    pub image: String,
    #[serde(default)]
    pub tool_source: ToolSource,
    #[serde(default = "default_discovery_tool_name")]
    pub discovery_tool_name: String,
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
    /// User-scoped persistent cache volume (opt-in).
    /// When enabled, a Docker volume is mounted at `/cache` that persists
    /// across sessions for the same user + capability pair.
    #[serde(default)]
    pub cache: CachePolicy,
    /// Cross-session container sharing policy (opt-in).
    /// When `mode: shared`, a single container is reused across all sessions.
    #[serde(default)]
    pub sharing: Option<SharingPolicy>,
    /// Contents of the co-located prompt.md, injected into agent context
    #[serde(skip_deserializing)]
    pub prompt: String,
    #[serde(skip_deserializing, default)]
    pub localized_prompts: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    #[default]
    Manifest,
    Dynamic,
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
    /// When true, a successful invocation of this tool causes the tool loop
    /// to return immediately instead of giving the LLM another iteration.
    /// Useful for "produce-and-done" tools like PDF generation where a
    /// follow-up call would just create a duplicate artifact.
    #[serde(default)]
    pub terminal_on_success: bool,
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

/// User-scoped persistent cache volume policy.
///
/// When `enabled`, the orchestrator mounts a named Docker volume at `/cache`
/// inside the container. The volume is keyed by `(user_id, capability_id)` and
/// persists across sessions so that package-manager caches stay warm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachePolicy {
    #[serde(default)]
    pub enabled: bool,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self { enabled: false }
    }
}

/// Controls whether a capability container can be shared across sessions.
///
/// Only `Tool`-class capabilities with `filesystem: none` or `filesystem: temp`
/// may opt into `shared` mode. The capability author declares this to signal
/// that the container handles concurrent requests safely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharingPolicy {
    #[serde(default)]
    pub mode: SharingMode,
    /// Maximum number of concurrent gRPC invocations to allow.
    /// Default 1 (serialized) — raise only when the container is thread-safe.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
    /// Seconds of inactivity before the shared container is stopped.
    #[serde(default = "default_shared_idle_timeout")]
    pub idle_timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    #[default]
    Exclusive,
    Shared,
}

fn default_max_concurrent() -> u32 {
    1
}
fn default_shared_idle_timeout() -> u64 {
    300
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

fn default_discovery_tool_name() -> String {
    "list_tools".to_string()
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

    if manifest.tool_source == ToolSource::Dynamic && !manifest.tools.is_empty() {
        anyhow::bail!(
            "Invalid manifest.yaml in {}: `tool_source: dynamic` requires `tools: []`",
            dir.display()
        );
    }

    if manifest.filesystem == FilesystemPolicy::Workspace
        && manifest.class != CapabilityClass::Environment
    {
        anyhow::bail!(
            "Invalid manifest.yaml in {}: `filesystem: workspace` is only allowed for `class: environment`",
            dir.display()
        );
    }

    if manifest.cache.enabled && manifest.class != CapabilityClass::Environment {
        anyhow::bail!(
            "Invalid manifest.yaml in {}: `cache.enabled: true` is only allowed for `class: environment`",
            dir.display()
        );
    }

    if manifest.is_shared() {
        if manifest.class == CapabilityClass::Environment {
            anyhow::bail!(
                "Invalid manifest.yaml in {}: `sharing.mode: shared` is not allowed for `class: environment`",
                dir.display()
            );
        }
        if manifest.filesystem == FilesystemPolicy::Workspace {
            anyhow::bail!(
                "Invalid manifest.yaml in {}: `sharing.mode: shared` is not allowed with `filesystem: workspace`",
                dir.display()
            );
        }
    }

    if prompt_path.exists() {
        manifest.prompt = fs::read_to_string(&prompt_path)
            .await
            .with_context(|| format!("Failed to read prompt.md in {}", dir.display()))?;
    }

    manifest.localized_prompts =
        crate::agents::localization::load_localized_markdown_files(dir, "prompt").await?;

    Ok(manifest)
}

impl CapabilityManifest {
    /// Returns `true` when this capability opts into cross-session container sharing.
    pub fn is_shared(&self) -> bool {
        self.sharing
            .as_ref()
            .map_or(false, |s| s.mode == SharingMode::Shared)
    }

    /// Returns the sharing policy if the capability is shared, `None` otherwise.
    pub fn sharing_policy(&self) -> Option<&SharingPolicy> {
        self.sharing
            .as_ref()
            .filter(|s| s.mode == SharingMode::Shared)
    }

    pub fn localized_prompt(&self, requested: &str) -> &str {
        crate::agents::localization::language_candidates(requested, "en")
            .into_iter()
            .find_map(|candidate| self.localized_prompts.get(&candidate))
            .map(|value| value.as_str())
            .unwrap_or(&self.prompt)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::fs;

    async fn make_manifest_dir(yaml: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("selu-manifest-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).await.unwrap();
        fs::write(dir.join("manifest.yaml"), yaml).await.unwrap();
        dir
    }

    #[tokio::test]
    async fn rejects_workspace_filesystem_for_tool_class() {
        let dir = make_manifest_dir(
            r#"
id: bad-cap
class: tool
image: bad:latest
filesystem: workspace
tools: []
"#,
        )
        .await;

        let err = load_from_dir(&dir).await.err().unwrap().to_string();
        assert!(err.contains("filesystem: workspace"));

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn rejects_shared_mode_for_environment_class() {
        let dir = make_manifest_dir(
            r#"
id: bad-cap
class: environment
image: bad:latest
filesystem: temp
sharing:
  mode: shared
tools: []
"#,
        )
        .await;

        let err = load_from_dir(&dir).await.err().unwrap().to_string();
        assert!(err.contains("sharing.mode: shared"));
        assert!(err.contains("class: environment"));

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn rejects_shared_mode_with_workspace_filesystem() {
        let dir = make_manifest_dir(
            r#"
id: bad-cap
class: environment
image: bad:latest
filesystem: workspace
sharing:
  mode: shared
tools: []
"#,
        )
        .await;

        let err = load_from_dir(&dir).await.err().unwrap().to_string();
        // Should hit one of the two rejection rules
        assert!(err.contains("sharing.mode: shared"));

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn allows_shared_mode_for_tool_class() {
        let dir = make_manifest_dir(
            r#"
id: good-shared
class: tool
image: good:latest
filesystem: temp
sharing:
  mode: shared
  max_concurrent: 4
  idle_timeout_seconds: 300
tools: []
"#,
        )
        .await;

        let loaded = load_from_dir(&dir).await.unwrap();
        assert!(loaded.is_shared());
        let policy = loaded.sharing_policy().unwrap();
        assert_eq!(policy.max_concurrent, 4);
        assert_eq!(policy.idle_timeout_seconds, 300);

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn sharing_defaults_to_exclusive() {
        let dir = make_manifest_dir(
            r#"
id: normal-cap
class: tool
image: normal:latest
tools: []
"#,
        )
        .await;

        let loaded = load_from_dir(&dir).await.unwrap();
        assert!(!loaded.is_shared());
        assert!(loaded.sharing_policy().is_none());

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn allows_workspace_filesystem_for_environment_class() {
        let dir = make_manifest_dir(
            r#"
id: good-cap
class: environment
image: good:latest
filesystem: workspace
tools: []
"#,
        )
        .await;

        let loaded = load_from_dir(&dir).await.unwrap();
        assert_eq!(loaded.class, CapabilityClass::Environment);
        assert_eq!(loaded.filesystem, FilesystemPolicy::Workspace);

        let _ = fs::remove_dir_all(&dir).await;
    }
}
