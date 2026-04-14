/// Agent marketplace: fetch catalogue, download, verify, extract, and install agents.
///
/// The marketplace is a JSON catalogue served over HTTPS. Each entry points to a
/// GitHub release archive (tar.gz) containing the agent's files. Capability
/// Docker images are pulled from a container registry (e.g. GHCR).
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use bollard::models::CreateImageInfo;
use ring::digest;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

use crate::agents::loader::{self, AgentDefinition};
use crate::capabilities::CapabilityEngine;
use crate::capabilities::discovery::{load_discovered_tools, sync_dynamic_tools_for_agent};
use crate::capabilities::manifest::{CapabilityManifest, ToolSource};
use crate::permissions::CredentialStore;
use crate::permissions::tool_policy;
use crate::state::AgentMap;

// ── Marketplace types ─────────────────────────────────────────────────────────

/// The top-level marketplace catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceCatalogue {
    pub version: u32,
    pub agents: Vec<MarketplaceEntry>,
}

/// A single agent listing in the marketplace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub name_localizations: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub description_localizations: std::collections::HashMap<String, String>,
    pub version: String,
    #[serde(default)]
    pub author: String,
    /// URL to download the agent archive (tar.gz)
    pub archive_url: String,
    /// SHA-256 hex digest of the archive for verification
    #[serde(default)]
    pub archive_sha256: String,
    /// Docker images to pull for the agent's capabilities
    #[serde(default)]
    pub capability_images: Vec<String>,
    /// Optional marketplace rating average (1.0-5.0).
    #[serde(default)]
    pub average_rating: Option<f64>,
    /// Number of ratings the average is based on.
    #[serde(default)]
    pub rating_count: Option<u32>,
}

impl MarketplaceEntry {
    pub fn localized_name(&self, requested: &str) -> String {
        crate::agents::localization::language_candidates(requested, "en")
            .into_iter()
            .find_map(|candidate| self.name_localizations.get(&candidate).cloned())
            .unwrap_or_else(|| self.name.clone())
    }

    pub fn localized_description(&self, requested: &str) -> String {
        crate::agents::localization::language_candidates(requested, "en")
            .into_iter()
            .find_map(|candidate| self.description_localizations.get(&candidate).cloned())
            .unwrap_or_else(|| self.description.clone())
    }
}

#[derive(Debug, Clone)]
pub struct PullProgress {
    pub image: String,
    /// Overall pull progress across all images as a 0.0..=1.0 fraction.
    pub overall_fraction: f32,
}

// ── Catalogue fetching ────────────────────────────────────────────────────────

/// Fetch the marketplace catalogue from the configured URL.
pub async fn fetch_catalogue(marketplace_url: &str) -> Result<MarketplaceCatalogue> {
    info!(url = marketplace_url, "Fetching marketplace catalogue");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("Failed to build HTTP client")?;

    let resp = client
        .get(marketplace_url)
        .send()
        .await
        .with_context(|| format!("Failed to fetch marketplace at {}", marketplace_url))?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Marketplace returned HTTP {} for {}",
            resp.status(),
            marketplace_url
        ));
    }

    let catalogue: MarketplaceCatalogue = resp
        .json()
        .await
        .context("Failed to parse marketplace JSON")?;

    info!(
        agents = catalogue.agents.len(),
        "Marketplace catalogue loaded"
    );

    Ok(catalogue)
}

// ── Agent installation ────────────────────────────────────────────────────────

/// Install an agent from a marketplace entry.
///
/// 1. Download the archive
/// 2. Verify SHA-256 checksum (if provided)
/// 3. Extract to `installed_dir/{agent_id}/`
/// 4. Pull Docker images for capabilities
/// 5. Insert DB row
/// 6. Load the agent definition and add to the in-memory map
///
/// Returns the loaded `AgentDefinition` for the setup wizard.
pub async fn install_agent(
    entry: &MarketplaceEntry,
    installed_dir: &str,
    db: &SqlitePool,
    agents: &Arc<ArcSwap<AgentMap>>,
    docker: &bollard::Docker,
    capabilities: &CapabilityEngine,
    cred_store: &CredentialStore,
) -> Result<AgentDefinition> {
    let agent_dir = Path::new(installed_dir).join(&entry.id);

    // Check if already installed
    if agent_dir.exists() {
        return Err(anyhow::anyhow!(
            "Agent '{}' is already installed at {}",
            entry.id,
            agent_dir.display()
        ));
    }

    info!(agent = %entry.id, url = %entry.archive_url, "Installing agent");

    // 1. Download the archive
    let archive_bytes = download_archive(&entry.archive_url).await?;

    // 2. Verify checksum
    if !entry.archive_sha256.is_empty() {
        verify_sha256(&archive_bytes, &entry.archive_sha256)?;
    } else {
        warn!(agent = %entry.id, "No SHA-256 checksum provided — skipping verification");
    }

    // 3. Extract archive
    extract_archive(&archive_bytes, installed_dir, &entry.id)?;

    // Steps 4-6 can fail after files have been extracted to disk.
    // If anything goes wrong, clean up the extracted directory and any
    // partial DB row so the user can retry cleanly.
    match install_agent_inner(
        entry,
        &agent_dir,
        db,
        agents,
        docker,
        capabilities,
        cred_store,
    )
    .await
    {
        Ok(agent_def) => Ok(agent_def),
        Err(e) => {
            warn!(agent = %entry.id, "Installation failed, cleaning up: {e}");

            // Remove from in-memory map (best-effort, may not have been added)
            {
                let current = agents.load();
                let mut new = (**current).clone();
                new.remove(&entry.id);
                agents.store(Arc::new(new));
            }

            // Remove partially-inserted DB row (best-effort)
            let _ = sqlx::query("DELETE FROM agents WHERE id = ? AND is_bundled = 0")
                .bind(&entry.id)
                .execute(db)
                .await;

            // Remove extracted directory (best-effort)
            if agent_dir.exists() {
                let _ = tokio::fs::remove_dir_all(&agent_dir).await;
            }

            Err(e)
        }
    }
}

/// Inner install logic (steps 4-6) that runs after the archive has been
/// extracted. Factored out so that `install_agent` can clean up on failure.
async fn install_agent_inner(
    entry: &MarketplaceEntry,
    agent_dir: &Path,
    db: &SqlitePool,
    agents: &Arc<ArcSwap<AgentMap>>,
    docker: &bollard::Docker,
    capabilities: &CapabilityEngine,
    cred_store: &CredentialStore,
) -> Result<AgentDefinition> {
    // 4. Pull Docker images
    let image_count = entry.capability_images.len().max(1);
    for (idx, image) in entry.capability_images.iter().enumerate() {
        pull_image(docker, image, idx, image_count, None).await?;
    }

    // 5. Insert DB row (setup_complete = 0 if the agent has install steps)
    let mut agent_def = loader::load_one(agent_dir).await.with_context(|| {
        format!(
            "Failed to load extracted agent from {}",
            agent_dir.display()
        )
    })?;

    // The marketplace entry ID is authoritative (used for DB + filesystem).
    // Override the YAML id to keep the in-memory map consistent.
    if agent_def.id != entry.id {
        warn!(
            agent = %entry.id,
            yaml_id = %agent_def.id,
            "agent.yaml id differs from marketplace id — using marketplace id"
        );
        agent_def.id = entry.id.clone();
    }

    let level = crate::agents::runtime_limits::AutonomyLevel::Medium;
    let limits = crate::agents::runtime_limits::limits_for_autonomy(level);

    sqlx::query(
        "INSERT INTO agents (
            id, display_name, version, source_url, is_bundled, setup_complete,
            autonomy_level, max_tool_loop_iterations, max_delegation_hops, auto_update
         ) \
         VALUES (?, ?, ?, ?, 0, 0, ?, ?, ?, 1) \
         ON CONFLICT(id) DO UPDATE SET display_name = excluded.display_name, \
         version = excluded.version, source_url = excluded.source_url, \
         setup_complete = excluded.setup_complete",
    )
    .bind(&entry.id)
    .bind(&entry.name)
    .bind(&entry.version)
    .bind(&entry.archive_url)
    .bind(level.as_str())
    .bind(i64::from(limits.max_tool_loop_iterations))
    .bind(i64::from(limits.max_delegation_hops))
    .execute(db)
    .await
    .context("Failed to insert agent into DB")?;

    sync_dynamic_tools_for_agent(
        db,
        capabilities,
        cred_store,
        &entry.id,
        &agent_def.capability_manifests,
    )
    .await;

    let setup_required = agent_requires_setup(db, cred_store, &entry.id, &agent_def).await?;
    let setup_complete = if setup_required { 0 } else { 1 };

    sqlx::query("UPDATE agents SET setup_complete = ? WHERE id = ?")
        .bind(setup_complete)
        .bind(&entry.id)
        .execute(db)
        .await
        .context("Failed to save setup state")?;

    // 6. Add to in-memory map (only if setup is complete / no steps needed)
    if !setup_required {
        let current = agents.load();
        let mut new = (**current).clone();
        new.insert(agent_def.id.clone(), Arc::new(agent_def.clone()));
        agents.store(Arc::new(new));
    }

    info!(agent = %entry.id, setup_complete, "Agent installed");
    Ok(agent_def)
}

/// Mark an agent's setup as complete and add it to the in-memory map.
pub async fn complete_setup(
    agent_id: &str,
    installed_dir: &str,
    db: &SqlitePool,
    agents: &Arc<ArcSwap<AgentMap>>,
) -> Result<()> {
    sqlx::query("UPDATE agents SET setup_complete = 1 WHERE id = ?")
        .bind(agent_id)
        .execute(db)
        .await
        .context("Failed to mark setup complete")?;

    let agent_dir = Path::new(installed_dir).join(agent_id);
    let mut agent_def = loader::load_one(&agent_dir).await?;

    // The DB/marketplace ID is authoritative — override YAML id if it differs.
    if agent_def.id != agent_id {
        warn!(
            agent = %agent_id,
            yaml_id = %agent_def.id,
            "agent.yaml id differs from DB id — using DB id"
        );
        agent_def.id = agent_id.to_owned();
    }

    {
        let current = agents.load();
        let mut new = (**current).clone();
        new.insert(agent_def.id.clone(), Arc::new(agent_def));
        agents.store(Arc::new(new));
    }

    info!(agent = agent_id, "Agent setup completed");
    Ok(())
}

/// Compare two semver-like version strings and return true if `marketplace`
/// is newer than `installed`. Falls back to lexicographic comparison when
/// versions don't parse as semver triples.
pub fn is_newer_version(installed: &str, marketplace: &str) -> bool {
    fn parse_triple(v: &str) -> Option<(u64, u64, u64)> {
        let parts: Vec<&str> = v.trim_start_matches('v').split('.').collect();
        if parts.len() == 3 {
            let major = parts[0].parse().ok()?;
            let minor = parts[1].parse().ok()?;
            // Strip any pre-release suffix (e.g. "1-beta" -> "1")
            let patch_str = parts[2].split('-').next()?;
            let patch = patch_str.parse().ok()?;
            Some((major, minor, patch))
        } else {
            None
        }
    }

    if installed.is_empty() || marketplace.is_empty() {
        return false;
    }

    match (parse_triple(installed), parse_triple(marketplace)) {
        (Some(i), Some(m)) => m > i,
        _ => marketplace > installed, // lexicographic fallback
    }
}

/// Update an already-installed agent to a newer marketplace version.
///
/// This is similar to `install_agent` but handles the existing installation:
/// 1. Stop running capability containers for this agent
/// 2. Remove the old agent directory
/// 3. Download + verify + extract the new archive
/// 4. Pull new Docker images
/// 5. Update the DB row (version, source_url)
/// 6. Reload agent definition into memory
///
/// Credentials and tool policies are preserved (they live in separate DB tables).
pub async fn update_agent(
    entry: &MarketplaceEntry,
    installed_dir: &str,
    db: &SqlitePool,
    agents: &Arc<ArcSwap<AgentMap>>,
    docker: &bollard::Docker,
    capabilities: &CapabilityEngine,
    cred_store: &CredentialStore,
) -> Result<AgentDefinition> {
    update_agent_with_progress(
        entry,
        installed_dir,
        db,
        agents,
        docker,
        capabilities,
        cred_store,
        None,
    )
    .await
}

pub async fn update_agent_with_progress(
    entry: &MarketplaceEntry,
    installed_dir: &str,
    db: &SqlitePool,
    agents: &Arc<ArcSwap<AgentMap>>,
    docker: &bollard::Docker,
    capabilities: &CapabilityEngine,
    cred_store: &CredentialStore,
    progress_tx: Option<tokio::sync::mpsc::UnboundedSender<PullProgress>>,
) -> Result<AgentDefinition> {
    let agent_dir = Path::new(installed_dir).join(&entry.id);

    // Verify the agent is actually installed
    if !agent_dir.exists() {
        return Err(anyhow::anyhow!(
            "Agent '{}' is not installed — cannot update",
            entry.id
        ));
    }

    // Don't allow updating the bundled default
    let row = sqlx::query_as::<_, (i32,)>("SELECT is_bundled FROM agents WHERE id = ?")
        .bind(&entry.id)
        .fetch_optional(db)
        .await?;

    if let Some((1,)) = row {
        return Err(anyhow::anyhow!("Cannot update the bundled default agent"));
    }

    info!(agent = %entry.id, from_url = %entry.archive_url, version = %entry.version, "Updating agent");

    // 1. Remove from in-memory map to prevent new sessions
    {
        let current = agents.load();
        let mut new = (**current).clone();
        new.remove(&entry.id);
        agents.store(Arc::new(new));
    }

    // 2. Stop any running capability containers for this agent
    // Collect capability IDs before removing files
    let old_cap_ids: Vec<String> = loader::load_one(&agent_dir)
        .await
        .map(|d| d.capability_manifests.keys().cloned().collect())
        .unwrap_or_default();

    for cap_id in &old_cap_ids {
        capabilities.close_capability(cap_id).await;
    }

    // 3. Download the new archive
    let archive_bytes = download_archive(&entry.archive_url).await?;

    // 4. Verify checksum
    if !entry.archive_sha256.is_empty() {
        verify_sha256(&archive_bytes, &entry.archive_sha256)?;
    } else {
        warn!(agent = %entry.id, "No SHA-256 checksum provided — skipping verification");
    }

    // 5. Remove old agent directory
    tokio::fs::remove_dir_all(&agent_dir)
        .await
        .with_context(|| format!("Failed to remove old agent dir {}", agent_dir.display()))?;

    // 6. Extract new archive
    extract_archive(&archive_bytes, installed_dir, &entry.id)?;

    // 7. Pull Docker images + update DB + reload into memory
    match update_agent_inner(
        entry,
        &agent_dir,
        db,
        agents,
        docker,
        capabilities,
        cred_store,
        progress_tx,
    )
    .await
    {
        Ok(agent_def) => Ok(agent_def),
        Err(e) => {
            warn!(agent = %entry.id, "Update failed after extraction: {e}");
            // Best-effort: remove broken extraction
            if agent_dir.exists() {
                let _ = tokio::fs::remove_dir_all(&agent_dir).await;
            }
            Err(e)
        }
    }
}

/// Inner update logic (pull images, update DB, reload).
async fn update_agent_inner(
    entry: &MarketplaceEntry,
    agent_dir: &Path,
    db: &SqlitePool,
    agents: &Arc<ArcSwap<AgentMap>>,
    docker: &bollard::Docker,
    capabilities: &CapabilityEngine,
    cred_store: &CredentialStore,
    progress_tx: Option<tokio::sync::mpsc::UnboundedSender<PullProgress>>,
) -> Result<AgentDefinition> {
    // Pull Docker images
    let image_count = entry.capability_images.len().max(1);
    for (idx, image) in entry.capability_images.iter().enumerate() {
        pull_image(docker, image, idx, image_count, progress_tx.as_ref()).await?;
    }

    // Load the new agent definition
    let mut agent_def = loader::load_one(agent_dir)
        .await
        .with_context(|| format!("Failed to load updated agent from {}", agent_dir.display()))?;

    // The marketplace entry ID is authoritative — override YAML id if it differs.
    if agent_def.id != entry.id {
        warn!(
            agent = %entry.id,
            yaml_id = %agent_def.id,
            "agent.yaml id differs from marketplace id — using marketplace id"
        );
        agent_def.id = entry.id.clone();
    }

    sqlx::query(
        "UPDATE agents SET display_name = ?, version = ?, source_url = ?, \
         setup_complete = 0 WHERE id = ?",
    )
    .bind(&entry.name)
    .bind(&entry.version)
    .bind(&entry.archive_url)
    .bind(&entry.id)
    .execute(db)
    .await
    .context("Failed to update agent in DB")?;

    sync_dynamic_tools_for_agent(
        db,
        capabilities,
        cred_store,
        &entry.id,
        &agent_def.capability_manifests,
    )
    .await;

    let setup_required = agent_requires_setup(db, cred_store, &entry.id, &agent_def).await?;
    let setup_complete = if setup_required { 0 } else { 1 };

    sqlx::query("UPDATE agents SET setup_complete = ? WHERE id = ?")
        .bind(setup_complete)
        .bind(&entry.id)
        .execute(db)
        .await
        .context("Failed to update setup state")?;

    // Add to in-memory map (only if setup is complete)
    if !setup_required {
        let current = agents.load();
        let mut new = (**current).clone();
        new.insert(agent_def.id.clone(), Arc::new(agent_def.clone()));
        agents.store(Arc::new(new));
    }

    info!(agent = %entry.id, version = %entry.version, setup_complete, "Agent updated");
    Ok(agent_def)
}

fn builtin_tool_defaults() -> [(&'static str, &'static str); 5] {
    [
        ("__builtin__", "delegate_to_agent"),
        ("__builtin__", "memory_remember"),
        ("__builtin__", "memory_forget"),
        ("__builtin__", "memory_search"),
        ("__builtin__", "memory_list"),
    ]
}

async fn manifest_tools_for_setup_check(
    db: &SqlitePool,
    agent_id: &str,
    capability_id: &str,
    manifest: &CapabilityManifest,
) -> Vec<String> {
    if manifest.tool_source == ToolSource::Dynamic {
        return load_discovered_tools(db, agent_id, capability_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
    }

    manifest
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect()
}

pub async fn agent_requires_setup(
    db: &SqlitePool,
    cred_store: &CredentialStore,
    agent_id: &str,
    agent_def: &AgentDefinition,
) -> Result<bool> {
    let mut system_credentials_by_capability: HashMap<String, Vec<String>> = HashMap::new();

    for step in &agent_def.install_steps {
        let Some(target) = &step.store_as else {
            continue;
        };

        if target.scope != "system_credential" {
            return Ok(true);
        }

        if !system_credentials_by_capability.contains_key(&target.capability_id) {
            let names = cred_store
                .list_system(&target.capability_id)
                .await
                .with_context(|| {
                    format!(
                        "Failed to read system credentials for capability {}",
                        target.capability_id
                    )
                })?;
            system_credentials_by_capability.insert(target.capability_id.clone(), names);
        }

        let has_value = system_credentials_by_capability
            .get(&target.capability_id)
            .is_some_and(|names| names.iter().any(|name| name == &target.credential_name));
        if !has_value {
            return Ok(true);
        }
    }

    for (capability_id, manifest) in &agent_def.capability_manifests {
        for tool_name in manifest_tools_for_setup_check(db, agent_id, capability_id, manifest).await
        {
            let policy =
                tool_policy::get_global_policy(db, agent_id, capability_id, &tool_name).await?;
            if policy.is_none() {
                return Ok(true);
            }
        }
    }

    for (capability_id, tool_name) in builtin_tool_defaults() {
        let policy = tool_policy::get_global_policy(db, agent_id, capability_id, tool_name).await?;
        if policy.is_none() {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Perform auto-update for all agents that have `auto_update = 1`.
///
/// Called periodically from the background task. Returns the number of agents updated.
pub async fn auto_update_agents(
    marketplace_url: &str,
    installed_dir: &str,
    db: &SqlitePool,
    agents: &Arc<ArcSwap<AgentMap>>,
    capabilities: &CapabilityEngine,
    cred_store: &CredentialStore,
) -> Result<usize> {
    // Fetch the current catalogue
    let catalogue = fetch_catalogue(marketplace_url).await?;

    // Get agents with auto_update enabled
    let auto_rows = sqlx::query_as::<_, (String, String)>(
        "SELECT id, version FROM agents WHERE auto_update = 1 AND is_bundled = 0 AND setup_complete = 1",
    )
    .fetch_all(db)
    .await
    .context("Failed to query auto-update agents")?;

    if auto_rows.is_empty() {
        return Ok(0);
    }

    let docker = bollard::Docker::connect_with_local_defaults()
        .context("Failed to connect to Docker for auto-update")?;

    let mut updated = 0;

    for (agent_id, installed_version) in &auto_rows {
        // Find the matching marketplace entry
        let entry = match catalogue.agents.iter().find(|e| &e.id == agent_id) {
            Some(e) => e,
            None => continue,
        };

        if !is_newer_version(installed_version, &entry.version) {
            continue;
        }

        info!(agent = %agent_id, from = %installed_version, to = %entry.version, "Auto-updating agent");

        match update_agent(
            entry,
            installed_dir,
            db,
            agents,
            &docker,
            capabilities,
            cred_store,
        )
        .await
        {
            Ok(_) => {
                updated += 1;
                info!(agent = %agent_id, version = %entry.version, "Auto-update completed");
            }
            Err(e) => {
                // Log but don't fail the whole batch
                tracing::error!(agent = %agent_id, "Auto-update failed: {e}");
            }
        }
    }

    Ok(updated)
}

/// Uninstall an agent: remove from DB, memory, and filesystem.
pub async fn uninstall_agent(
    agent_id: &str,
    installed_dir: &str,
    db: &SqlitePool,
    agents: &Arc<ArcSwap<AgentMap>>,
    capabilities: &CapabilityEngine,
) -> Result<()> {
    // Don't allow uninstalling the bundled default
    let row = sqlx::query_as::<_, (i32,)>("SELECT is_bundled FROM agents WHERE id = ?")
        .bind(agent_id)
        .fetch_optional(db)
        .await?;

    if let Some((1,)) = row {
        return Err(anyhow::anyhow!(
            "Cannot uninstall the bundled default agent"
        ));
    }

    // Collect capability IDs before we tear anything down. Try the
    // in-memory map first; fall back to loading from disk.
    let cap_ids: Vec<String> = {
        let map = agents.load();
        if let Some(def) = map.get(agent_id) {
            def.capability_manifests.keys().cloned().collect()
        } else {
            let agent_dir = Path::new(installed_dir).join(agent_id);
            loader::load_one(&agent_dir)
                .await
                .map(|d| d.capability_manifests.keys().cloned().collect())
                .unwrap_or_default()
        }
    };

    // Remove from in-memory map
    {
        let current = agents.load();
        let mut new = (**current).clone();
        new.remove(agent_id);
        agents.store(Arc::new(new));
    }

    // Remove tool policies for this agent (all users — per-user overrides)
    sqlx::query("DELETE FROM tool_policies WHERE agent_id = ?")
        .bind(agent_id)
        .execute(db)
        .await
        .context("Failed to delete tool policies")?;

    // Remove global tool policies for this agent
    sqlx::query("DELETE FROM global_tool_policies WHERE agent_id = ?")
        .bind(agent_id)
        .execute(db)
        .await
        .context("Failed to delete global tool policies")?;

    // Remove pending approvals for this agent
    sqlx::query("DELETE FROM pending_tool_approvals WHERE agent_id = ?")
        .bind(agent_id)
        .execute(db)
        .await
        .context("Failed to delete pending approvals")?;

    // Remove credentials and egress log entries for each capability
    for cap_id in &cap_ids {
        sqlx::query("DELETE FROM system_credentials WHERE capability_id = ?")
            .bind(cap_id)
            .execute(db)
            .await
            .context("Failed to delete system credentials")?;

        sqlx::query("DELETE FROM user_credentials WHERE capability_id = ?")
            .bind(cap_id)
            .execute(db)
            .await
            .context("Failed to delete user credentials")?;

        sqlx::query("DELETE FROM egress_log WHERE capability_id = ?")
            .bind(cap_id)
            .execute(db)
            .await
            .context("Failed to delete egress log entries")?;
    }

    // Remove persistent agent storage for all users
    crate::agents::storage::delete_all_for_agent(db, agent_id)
        .await
        .context("Failed to delete agent storage")?;

    // Remove improvement data (turn signals, insights, metrics)
    crate::agents::improvement::delete_all_for_agent(db, agent_id)
        .await
        .context("Failed to delete improvement data")?;

    // Remove agent DB row
    sqlx::query("DELETE FROM agents WHERE id = ?")
        .bind(agent_id)
        .execute(db)
        .await
        .context("Failed to delete agent from DB")?;

    // Stop running capability containers before removing files on disk
    for cap_id in &cap_ids {
        capabilities.close_capability(cap_id).await;
    }

    // Remove filesystem
    let agent_dir = Path::new(installed_dir).join(agent_id);
    if agent_dir.exists() {
        tokio::fs::remove_dir_all(&agent_dir)
            .await
            .with_context(|| format!("Failed to remove agent dir {}", agent_dir.display()))?;
    }

    info!(agent = agent_id, "Agent uninstalled");
    Ok(())
}

/// Check if an agent is installed.
#[allow(dead_code)]
pub async fn is_installed(db: &SqlitePool, agent_id: &str) -> Result<bool> {
    let row = sqlx::query_as::<_, (i32,)>("SELECT COUNT(*) FROM agents WHERE id = ?")
        .bind(agent_id)
        .fetch_one(db)
        .await?;

    Ok(row.0 > 0)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Download an archive from a URL.
async fn download_archive(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Failed to download archive from {}", url))?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Archive download returned HTTP {} for {}",
            resp.status(),
            url
        ));
    }

    let bytes = resp.bytes().await.context("Failed to read archive bytes")?;

    info!(bytes = bytes.len(), "Downloaded archive");
    Ok(bytes.to_vec())
}

/// Verify SHA-256 checksum of downloaded bytes.
fn verify_sha256(data: &[u8], expected_hex: &str) -> Result<()> {
    let actual = digest::digest(&digest::SHA256, data);
    let actual_hex = hex_encode(actual.as_ref());

    if actual_hex != expected_hex.to_lowercase() {
        return Err(anyhow::anyhow!(
            "SHA-256 mismatch: expected {}, got {}",
            expected_hex,
            actual_hex
        ));
    }

    info!("Archive checksum verified");
    Ok(())
}

/// Simple hex encoding (avoids pulling in the `hex` crate).
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Extract a tar.gz archive into `installed_dir/{agent_id}/`.
///
/// The archive may contain files at the root or inside a single top-level
/// directory. We normalize to `installed_dir/{agent_id}/...`.
fn extract_archive(data: &[u8], installed_dir: &str, agent_id: &str) -> Result<()> {
    let decoder = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(decoder);

    let target_dir = Path::new(installed_dir).join(agent_id);
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("Failed to create directory {}", target_dir.display()))?;

    // First pass: detect if there's a common top-level directory
    let decoder2 = flate2::read::GzDecoder::new(data);
    let mut archive2 = tar::Archive::new(decoder2);
    let mut top_dirs = std::collections::HashSet::new();

    for entry in archive2
        .entries()
        .context("Failed to read archive entries")?
    {
        let entry = entry.context("Failed to read archive entry")?;
        if let Ok(path) = entry.path() {
            if let Some(first) = path.components().next() {
                top_dirs.insert(first.as_os_str().to_owned());
            }
        }
    }

    // If all files share a single top-level directory, strip it
    let strip_prefix = if top_dirs.len() == 1 {
        top_dirs.into_iter().next()
    } else {
        None
    };

    for entry in archive
        .entries()
        .context("Failed to read archive entries")?
    {
        let mut entry = entry.context("Failed to read archive entry")?;
        let path = entry
            .path()
            .context("Invalid path in archive")?
            .into_owned();

        let relative = if let Some(ref prefix) = strip_prefix {
            match path.strip_prefix(prefix) {
                Ok(p) => p.to_path_buf(),
                Err(_) => path,
            }
        } else {
            path
        };

        // Skip empty paths (the top-level directory entry itself)
        if relative.as_os_str().is_empty() {
            continue;
        }

        let full_path = target_dir.join(&relative);

        // Security: ensure we don't escape the target directory
        if !full_path.starts_with(&target_dir) {
            warn!(
                path = %relative.display(),
                "Archive path escapes target directory — skipping"
            );
            continue;
        }

        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&full_path).ok();
        } else {
            if let Some(parent) = full_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let mut file = std::fs::File::create(&full_path)
                .with_context(|| format!("Failed to create {}", full_path.display()))?;
            std::io::copy(&mut entry, &mut file)
                .with_context(|| format!("Failed to write {}", full_path.display()))?;
        }
    }

    info!(
        dir = %target_dir.display(),
        "Extracted archive"
    );
    Ok(())
}

/// Pull a Docker image.
async fn pull_image(
    docker: &bollard::Docker,
    image: &str,
    image_index: usize,
    image_count: usize,
    progress_tx: Option<&tokio::sync::mpsc::UnboundedSender<PullProgress>>,
) -> Result<()> {
    use bollard::image::CreateImageOptions;
    use futures::StreamExt;
    use std::collections::{HashMap, HashSet};

    info!(image, "Pulling capability Docker image");

    let opts = CreateImageOptions {
        from_image: image,
        ..Default::default()
    };

    let mut stream = docker.create_image(Some(opts), None, None);
    let mut layers: HashMap<String, (u64, u64)> = HashMap::new();
    let mut complete_layers: HashSet<String> = HashSet::new();

    while let Some(result) = stream.next().await {
        match result {
            Ok(info) => {
                if let Some(status) = &info.status {
                    tracing::debug!(image, status, "Docker pull progress");
                }
                if let Some(tx) = progress_tx {
                    let image_fraction =
                        update_image_progress_state(&mut layers, &mut complete_layers, &info);
                    let base = image_index as f32 / image_count as f32;
                    let span = 1.0f32 / image_count as f32;
                    let overall_fraction = (base + span * image_fraction).clamp(0.0, 1.0);
                    let _ = tx.send(PullProgress {
                        image: image.to_string(),
                        overall_fraction,
                    });
                }
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Failed to pull image '{}': {}", image, e));
            }
        }
    }

    info!(image, "Docker image pulled");
    if let Some(tx) = progress_tx {
        let base = image_index as f32 / image_count as f32;
        let span = 1.0f32 / image_count as f32;
        let _ = tx.send(PullProgress {
            image: image.to_string(),
            overall_fraction: (base + span).clamp(0.0, 1.0),
        });
    }
    Ok(())
}

fn update_image_progress_state(
    layers: &mut std::collections::HashMap<String, (u64, u64)>,
    complete_layers: &mut std::collections::HashSet<String>,
    info: &CreateImageInfo,
) -> f32 {
    if let Some(id) = info.id.clone() {
        if let Some(detail) = &info.progress_detail {
            let total = detail.total.unwrap_or(0).max(0) as u64;
            let current = detail.current.unwrap_or(0).max(0) as u64;
            if total > 0 {
                let entry = layers.entry(id.clone()).or_insert((0, total));
                entry.1 = entry.1.max(total);
                entry.0 = entry.0.max(current.min(entry.1));
            }
        }

        if let Some(status) = info.status.as_deref() {
            let lower = status.to_ascii_lowercase();
            if lower.contains("already exists")
                || lower.contains("pull complete")
                || lower.contains("download complete")
            {
                complete_layers.insert(id.clone());
                let entry = layers.entry(id).or_insert((1, 1));
                if entry.1 == 0 {
                    entry.0 = 1;
                    entry.1 = 1;
                } else {
                    entry.0 = entry.1;
                }
            }
        }
    }

    let mut total_sum: u64 = 0;
    let mut current_sum: u64 = 0;
    for (layer_id, (current, total)) in layers.iter() {
        if *total > 0 {
            total_sum = total_sum.saturating_add(*total);
            current_sum = current_sum.saturating_add((*current).min(*total));
        } else if complete_layers.contains(layer_id) {
            total_sum = total_sum.saturating_add(1);
            current_sum = current_sum.saturating_add(1);
        }
    }

    if total_sum > 0 {
        (current_sum as f32 / total_sum as f32).clamp(0.0, 1.0)
    } else if !complete_layers.is_empty() {
        0.98
    } else {
        0.02
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer_version_basic() {
        assert!(is_newer_version("1.0.0", "1.0.1"));
        assert!(is_newer_version("1.0.0", "1.1.0"));
        assert!(is_newer_version("1.0.0", "2.0.0"));
        assert!(!is_newer_version("1.0.1", "1.0.0"));
        assert!(!is_newer_version("1.0.0", "1.0.0"));
    }

    #[test]
    fn test_is_newer_version_with_prefix() {
        assert!(is_newer_version("v1.0.0", "v1.0.1"));
        assert!(is_newer_version("1.0.0", "v2.0.0"));
        assert!(!is_newer_version("v2.0.0", "1.0.0"));
    }

    #[test]
    fn test_is_newer_version_empty() {
        assert!(!is_newer_version("", "1.0.0"));
        assert!(!is_newer_version("1.0.0", ""));
        assert!(!is_newer_version("", ""));
    }

    #[test]
    fn test_is_newer_version_pre_release() {
        // Pre-release suffix is stripped for comparison
        assert!(is_newer_version("1.0.0-beta", "1.0.1"));
        assert!(is_newer_version("1.0.0", "1.0.1-beta"));
    }

    #[test]
    fn test_is_newer_version_multi_digit() {
        assert!(is_newer_version("1.9.0", "1.10.0"));
        assert!(is_newer_version("0.1.0", "0.1.12"));
        assert!(!is_newer_version("1.10.0", "1.9.0"));
    }
}
