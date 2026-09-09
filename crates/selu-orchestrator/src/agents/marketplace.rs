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
use sqlx::{Row, SqlitePool};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

use crate::agents::loader::{self, AgentDefinition};
use crate::capabilities::CapabilityEngine;
use crate::capabilities::discovery::{
    load_discovered_tools, sync_dynamic_tools_for_agent, validate_dynamic_tools_for_agent,
};
use crate::capabilities::manifest::{CapabilityManifest, ToolSource};
use crate::permissions::CredentialStore;
use crate::permissions::tool_policy;
use crate::services::docker_storage::DockerStorage;
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

/// Validate an externally supplied agent identifier before it can become a DB
/// key or filesystem component. Marketplace IDs are portable ASCII slugs: one
/// leading alphanumeric followed only by alphanumerics, dots, dashes, or underscores.
pub fn validate_agent_id(agent_id: &str) -> Result<()> {
    let mut chars = agent_id.chars();
    let valid_characters = chars.next().is_some_and(|ch| ch.is_ascii_alphanumeric())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'));
    let stem = agent_id
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let windows_device_name = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && matches!(&stem[..3], "COM" | "LPT")
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    let valid = valid_characters
        && agent_id.len() <= 128
        && !agent_id.ends_with('.')
        && !windows_device_name;
    if !valid {
        anyhow::bail!(
            "Invalid agent id '{}': expected one safe path component",
            agent_id
        );
    }
    Ok(())
}

fn remove_agent_from_map(
    agents: &Arc<ArcSwap<AgentMap>>,
    agent_id: &str,
) -> Option<Arc<AgentDefinition>> {
    let current = agents.load();
    let mut new = (**current).clone();
    let previous = new.remove(agent_id);
    agents.store(Arc::new(new));
    previous
}

fn restore_agent_in_map(
    agents: &Arc<ArcSwap<AgentMap>>,
    agent_id: &str,
    definition: Option<Arc<AgentDefinition>>,
) {
    let Some(definition) = definition else {
        return;
    };
    let current = agents.load();
    let mut new = (**current).clone();
    new.insert(agent_id.to_string(), definition);
    agents.store(Arc::new(new));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivationPhase {
    Prepared,
    OldRetained,
    NewActive,
    RollbackNewStaged,
    RollbackOldActive,
}

impl ActivationPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::OldRetained => "old_retained",
            Self::NewActive => "new_active",
            Self::RollbackNewStaged => "rollback_new_staged",
            Self::RollbackOldActive => "rollback_old_active",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "old_retained" => Ok(Self::OldRetained),
            "new_active" => Ok(Self::NewActive),
            "rollback_new_staged" => Ok(Self::RollbackNewStaged),
            "rollback_old_active" => Ok(Self::RollbackOldActive),
            _ => anyhow::bail!("Unknown agent package activation phase '{value}'"),
        }
    }
}

#[derive(Debug, Clone)]
struct ActivationJournal {
    agent_id: String,
    old_revision_id: String,
    new_revision_id: String,
    active_path: PathBuf,
    retained_path: PathBuf,
    staged_path: PathBuf,
    phase: ActivationPhase,
}

#[derive(Debug, Clone)]
struct ActivationPaths {
    active: PathBuf,
    retained: PathBuf,
    staged: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActivationFsState {
    active: bool,
    retained: bool,
    staged: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconciliationPlan {
    KeepPriorActive,
    RestoreRetained,
    StageNewThenRestoreRetained,
}

fn reconciliation_plan(
    phase: ActivationPhase,
    state: ActivationFsState,
) -> Result<ReconciliationPlan> {
    match (phase, state.active, state.retained, state.staged) {
        // Either no forward rename happened, or filesystem rollback completed
        // but its final database transaction did not.
        (_, true, false, true) => Ok(ReconciliationPlan::KeepPriorActive),
        // The prior package was retained, but no package currently occupies the
        // active path. This also covers a crash after staging the new package
        // during rollback.
        (
            ActivationPhase::Prepared
            | ActivationPhase::OldRetained
            | ActivationPhase::NewActive
            | ActivationPhase::RollbackNewStaged,
            false,
            true,
            true,
        ) => Ok(ReconciliationPlan::RestoreRetained),
        // Both forward renames completed. The active path contains the new
        // package and must first move back to its recorded staging path.
        (ActivationPhase::OldRetained | ActivationPhase::NewActive, true, true, false) => {
            Ok(ReconciliationPlan::StageNewThenRestoreRetained)
        }
        _ => anyhow::bail!(
            "Activation journal phase '{}' has unsafe filesystem state active={}, retained={}, staged={}",
            phase.as_str(),
            state.active,
            state.retained,
            state.staged
        ),
    }
}

fn validate_revision_id(revision_id: &str) -> Result<()> {
    let mut components = Path::new(revision_id).components();
    let valid = revision_id.len() <= 128
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none();
    if !valid {
        anyhow::bail!(
            "Invalid agent package revision id '{}': expected one safe path component",
            revision_id
        );
    }
    Ok(())
}

async fn canonical_managed_root(installed_root: &Path, name: &str) -> Result<PathBuf> {
    let path = installed_root.join(name);
    tokio::fs::create_dir_all(&path)
        .await
        .with_context(|| format!("Failed to create managed package root {}", path.display()))?;
    let metadata = tokio::fs::symlink_metadata(&path)
        .await
        .with_context(|| format!("Failed to inspect managed package root {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!(
            "Managed package root {} must be a real directory, not a symlink",
            path.display()
        );
    }
    let canonical = tokio::fs::canonicalize(&path)
        .await
        .with_context(|| format!("Failed to resolve managed package root {}", path.display()))?;
    if canonical.parent() != Some(installed_root) {
        anyhow::bail!(
            "Managed package root {} escapes configured installed-agent root {}",
            canonical.display(),
            installed_root.display()
        );
    }
    Ok(canonical)
}

async fn activation_paths(
    installed_dir: &str,
    agent_id: &str,
    old_revision_id: &str,
    new_revision_id: &str,
) -> Result<ActivationPaths> {
    validate_agent_id(agent_id)?;
    validate_revision_id(old_revision_id)?;
    validate_revision_id(new_revision_id)?;

    let installed_root = tokio::fs::canonicalize(installed_dir)
        .await
        .with_context(|| {
            format!(
                "Failed to resolve configured installed-agent root '{}'",
                installed_dir
            )
        })?;
    let staging_root = canonical_managed_root(&installed_root, ".selu-staging").await?;
    let revision_root = canonical_managed_root(&installed_root, ".selu-revisions").await?;
    let retained_parent = revision_root.join(agent_id);
    tokio::fs::create_dir_all(&retained_parent)
        .await
        .with_context(|| {
            format!(
                "Failed to create retained package directory {}",
                retained_parent.display()
            )
        })?;
    let retained_metadata = tokio::fs::symlink_metadata(&retained_parent).await?;
    if retained_metadata.file_type().is_symlink() || !retained_metadata.is_dir() {
        anyhow::bail!(
            "Retained package directory {} must be a real directory",
            retained_parent.display()
        );
    }
    let canonical_retained_parent = tokio::fs::canonicalize(&retained_parent).await?;
    if canonical_retained_parent.parent() != Some(revision_root.as_path()) {
        anyhow::bail!(
            "Retained package directory {} escapes managed revision root {}",
            canonical_retained_parent.display(),
            revision_root.display()
        );
    }

    Ok(ActivationPaths {
        active: installed_root.join(agent_id),
        retained: canonical_retained_parent.join(old_revision_id),
        staged: staging_root.join(format!("{agent_id}-{new_revision_id}")),
    })
}

async fn inspect_package_path(path: &Path) -> Result<bool> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to inspect package path {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!(
            "Package path {} must be a real directory, not a symlink",
            path.display()
        );
    }
    let canonical = tokio::fs::canonicalize(path).await?;
    if canonical != path {
        anyhow::bail!(
            "Package path {} resolves outside its recorded managed location ({})",
            path.display(),
            canonical.display()
        );
    }
    Ok(true)
}

impl ActivationPaths {
    async fn inspect(&self) -> Result<ActivationFsState> {
        Ok(ActivationFsState {
            active: inspect_package_path(&self.active).await?,
            retained: inspect_package_path(&self.retained).await?,
            staged: inspect_package_path(&self.staged).await?,
        })
    }

    fn validate_recorded(&self, journal: &ActivationJournal) -> Result<()> {
        for (label, recorded, expected) in [
            ("active", &journal.active_path, &self.active),
            ("retained", &journal.retained_path, &self.retained),
            ("staged", &journal.staged_path, &self.staged),
        ] {
            if recorded != expected {
                anyhow::bail!(
                    "Recorded {label} package path {} is outside its configured managed location {}",
                    recorded.display(),
                    expected.display()
                );
            }
        }
        Ok(())
    }
}

async fn persist_activation_journal(db: &SqlitePool, journal: &ActivationJournal) -> Result<()> {
    let active_path = journal.active_path.to_string_lossy().to_string();
    let retained_path = journal.retained_path.to_string_lossy().to_string();
    let staged_path = journal.staged_path.to_string_lossy().to_string();
    sqlx::query(
        "INSERT INTO agent_package_activation_journal
         (agent_id, old_revision_id, new_revision_id, active_path, retained_path, staged_path, phase)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&journal.agent_id)
    .bind(&journal.old_revision_id)
    .bind(&journal.new_revision_id)
    .bind(active_path)
    .bind(retained_path)
    .bind(staged_path)
    .bind(journal.phase.as_str())
    .execute(db)
    .await
    .with_context(|| {
        format!(
            "Failed to persist activation journal for agent '{}' before filesystem changes",
            journal.agent_id
        )
    })?;
    Ok(())
}

async fn update_activation_phase(
    db: &SqlitePool,
    agent_id: &str,
    phase: ActivationPhase,
) -> Result<()> {
    let changed = sqlx::query(
        "UPDATE agent_package_activation_journal
         SET phase = ?, updated_at = datetime('now') WHERE agent_id = ?",
    )
    .bind(phase.as_str())
    .bind(agent_id)
    .execute(db)
    .await?;
    if changed.rows_affected() != 1 {
        anyhow::bail!(
            "Activation journal for agent '{}' disappeared while recording phase '{}'",
            agent_id,
            phase.as_str()
        );
    }
    Ok(())
}

async fn load_activation_journals(db: &SqlitePool) -> Result<Vec<ActivationJournal>> {
    let rows = sqlx::query(
        "SELECT agent_id, old_revision_id, new_revision_id, active_path,
                retained_path, staged_path, phase
         FROM agent_package_activation_journal ORDER BY agent_id",
    )
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|row| {
            let phase_text: String = row.try_get("phase")?;
            Ok(ActivationJournal {
                agent_id: row.try_get("agent_id")?,
                old_revision_id: row.try_get("old_revision_id")?,
                new_revision_id: row.try_get("new_revision_id")?,
                active_path: PathBuf::from(row.try_get::<String, _>("active_path")?),
                retained_path: PathBuf::from(row.try_get::<String, _>("retained_path")?),
                staged_path: PathBuf::from(row.try_get::<String, _>("staged_path")?),
                phase: ActivationPhase::parse(&phase_text)?,
            })
        })
        .collect()
}

async fn validate_journal_revision_states(
    db: &SqlitePool,
    journal: &ActivationJournal,
) -> Result<()> {
    let old_state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM agent_package_revisions WHERE id = ? AND agent_id = ?",
    )
    .bind(&journal.old_revision_id)
    .bind(&journal.agent_id)
    .fetch_optional(db)
    .await?;
    let new_state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM agent_package_revisions WHERE id = ? AND agent_id = ?",
    )
    .bind(&journal.new_revision_id)
    .bind(&journal.agent_id)
    .fetch_optional(db)
    .await?;
    if old_state.as_deref() != Some("current") || new_state.as_deref() != Some("staged") {
        anyhow::bail!(
            "Activation journal revisions are not safely recoverable: old revision '{}' is {:?}, new revision '{}' is {:?}",
            journal.old_revision_id,
            old_state,
            journal.new_revision_id,
            new_state
        );
    }
    Ok(())
}

async fn reconcile_activation_journal(
    db: &SqlitePool,
    installed_dir: &str,
    journal: &ActivationJournal,
) -> Result<()> {
    let paths = activation_paths(
        installed_dir,
        &journal.agent_id,
        &journal.old_revision_id,
        &journal.new_revision_id,
    )
    .await?;
    paths.validate_recorded(journal)?;
    validate_journal_revision_states(db, journal).await?;

    let state = paths.inspect().await?;
    let plan = reconciliation_plan(journal.phase, state)?;
    match plan {
        ReconciliationPlan::KeepPriorActive => {
            update_activation_phase(db, &journal.agent_id, ActivationPhase::RollbackOldActive)
                .await?;
        }
        ReconciliationPlan::RestoreRetained => {
            tokio::fs::rename(&paths.retained, &paths.active)
                .await
                .with_context(|| {
                    format!(
                        "Failed to restore retained package {} to active path {}",
                        paths.retained.display(),
                        paths.active.display()
                    )
                })?;
            update_activation_phase(db, &journal.agent_id, ActivationPhase::RollbackOldActive)
                .await?;
        }
        ReconciliationPlan::StageNewThenRestoreRetained => {
            tokio::fs::rename(&paths.active, &paths.staged)
                .await
                .with_context(|| {
                    format!(
                        "Failed to return partially activated package {} to staging path {}",
                        paths.active.display(),
                        paths.staged.display()
                    )
                })?;
            update_activation_phase(db, &journal.agent_id, ActivationPhase::RollbackNewStaged)
                .await?;
            tokio::fs::rename(&paths.retained, &paths.active)
                .await
                .with_context(|| {
                    format!(
                        "Failed to restore retained package {} to active path {}",
                        paths.retained.display(),
                        paths.active.display()
                    )
                })?;
            update_activation_phase(db, &journal.agent_id, ActivationPhase::RollbackOldActive)
                .await?;
        }
    }

    let restored = paths.inspect().await?;
    if restored
        != (ActivationFsState {
            active: true,
            retained: false,
            staged: true,
        })
    {
        anyhow::bail!(
            "Filesystem rollback for agent '{}' did not restore the prior active package",
            journal.agent_id
        );
    }

    let mut tx = db.begin().await?;
    let active_path = paths.active.to_string_lossy().to_string();
    let old_changed = sqlx::query(
        "UPDATE agent_package_revisions
         SET state = 'current', retain_until = NULL, package_path = ?, updated_at = datetime('now')
         WHERE id = ? AND agent_id = ? AND state = 'current'",
    )
    .bind(active_path)
    .bind(&journal.old_revision_id)
    .bind(&journal.agent_id)
    .execute(&mut *tx)
    .await?;
    let staged_path = paths.staged.to_string_lossy().to_string();
    let new_changed = sqlx::query(
        "UPDATE agent_package_revisions
         SET state = 'failed', retain_until = datetime('now', '+7 days'),
             package_path = ?, updated_at = datetime('now')
         WHERE id = ? AND agent_id = ? AND state = 'staged'",
    )
    .bind(staged_path)
    .bind(&journal.new_revision_id)
    .bind(&journal.agent_id)
    .execute(&mut *tx)
    .await?;
    let deleted = sqlx::query(
        "DELETE FROM agent_package_activation_journal
         WHERE agent_id = ? AND old_revision_id = ? AND new_revision_id = ?",
    )
    .bind(&journal.agent_id)
    .bind(&journal.old_revision_id)
    .bind(&journal.new_revision_id)
    .execute(&mut *tx)
    .await?;
    if old_changed.rows_affected() != 1
        || new_changed.rows_affected() != 1
        || deleted.rows_affected() != 1
    {
        anyhow::bail!(
            "Activation journal database state changed while reconciling agent '{}'",
            journal.agent_id
        );
    }
    tx.commit().await?;
    info!(agent = %journal.agent_id, "Rolled back interrupted agent package activation");
    Ok(())
}

/// Reconcile every interrupted package activation before installed agents are
/// loaded. A journal is deleted only after the prior package is back at the
/// active path and the revision-state rollback commits atomically.
pub async fn reconcile_agent_package_activations(
    db: &SqlitePool,
    installed_dir: &str,
) -> Result<usize> {
    let journals = load_activation_journals(db).await?;
    let count = journals.len();
    for journal in journals {
        reconcile_activation_journal(db, installed_dir, &journal)
            .await
            .with_context(|| {
                format!(
                    "Cannot safely reconcile interrupted activation for agent '{}' (phase '{}'); inspect {}, {}, and {} before restarting Selu",
                    journal.agent_id,
                    journal.phase.as_str(),
                    journal.active_path.display(),
                    journal.retained_path.display(),
                    journal.staged_path.display()
                )
            })?;
    }
    Ok(count)
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
    for entry in &catalogue.agents {
        validate_agent_id(&entry.id)
            .with_context(|| format!("Marketplace entry '{}' has an unsafe id", entry.name))?;
    }

    info!(
        agents = catalogue.agents.len(),
        "Marketplace catalogue loaded"
    );

    Ok(catalogue)
}

// ── Agent installation ────────────────────────────────────────────────────────

/// Install an agent from a marketplace entry.
///
/// 1. Download and verify the archive
/// 2. Extract and validate it in an isolated staging directory
/// 3. Pull and record exact Docker image identities for the staged revision
/// 4. Atomically rename the package into place and activate its DB revision
/// 5. Sync dynamic tools and expose the agent after setup checks
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
    docker_storage: &DockerStorage,
) -> Result<AgentDefinition> {
    validate_agent_id(&entry.id)?;
    let agent_dir = Path::new(installed_dir).join(&entry.id);
    if agent_dir.exists() {
        return Err(anyhow::anyhow!(
            "Agent '{}' is already installed at {}",
            entry.id,
            agent_dir.display()
        ));
    }

    let _maintenance_lease = docker_storage.exclusive_lease().await;
    info!(agent = %entry.id, url = %entry.archive_url, "Installing agent");
    let archive_bytes = download_archive(&entry.archive_url).await?;
    if !entry.archive_sha256.is_empty() {
        verify_sha256(&archive_bytes, &entry.archive_sha256)?;
    } else {
        warn!(agent = %entry.id, "No SHA-256 checksum provided — skipping verification");
    }

    let (revision_id, staging_dir) = docker_storage
        .create_staged_revision(&entry.id, &entry.version)
        .await?;
    let prepared = async {
        extract_archive_to(&archive_bytes, &staging_dir)?;
        prepare_agent_revision(
            entry,
            &staging_dir,
            docker,
            capabilities,
            cred_store,
            docker_storage,
            &revision_id,
            None,
        )
        .await
    }
    .await;
    let agent_def = match prepared {
        Ok(definition) => definition,
        Err(error) => {
            docker_storage.mark_revision_failed(&revision_id).await?;
            let _ = tokio::fs::remove_dir_all(&staging_dir).await;
            return Err(error);
        }
    };

    let level = crate::agents::runtime_limits::AutonomyLevel::Medium;
    let limits = crate::agents::runtime_limits::limits_for_autonomy(level);
    if let Some(parent) = agent_dir.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::rename(&staging_dir, &agent_dir)
        .await
        .with_context(|| format!("Failed to activate agent package {}", entry.id))?;

    let activated = async {
        let mut tx = db.begin().await?;
        sqlx::query!(
            "INSERT INTO agents (
                id, display_name, version, source_url, is_bundled, setup_complete,
                autonomy_level, max_tool_loop_iterations, max_delegation_hops, auto_update
             ) VALUES (?, ?, ?, ?, 0, 0, ?, ?, ?, 1)",
            entry.id,
            entry.name,
            entry.version,
            entry.archive_url,
            level.as_str(),
            i64::from(limits.max_tool_loop_iterations),
            i64::from(limits.max_delegation_hops)
        )
        .execute(&mut *tx)
        .await?;
        let current_path = agent_dir.to_string_lossy().to_string();
        let changed = sqlx::query!(
            "UPDATE agent_package_revisions
             SET state = 'current', retain_until = NULL, package_path = ?,
                 updated_at = datetime('now')
             WHERE id = ? AND agent_id = ? AND state = 'staged'",
            current_path,
            revision_id,
            entry.id
        )
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(anyhow::anyhow!(
                "The staged agent revision is no longer available"
            ));
        }
        tx.commit().await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Err(error) = activated {
        let _ = tokio::fs::rename(&agent_dir, &staging_dir).await;
        docker_storage.mark_revision_failed(&revision_id).await?;
        return Err(error).context("Failed to activate the installed agent");
    }
    drop(_maintenance_lease);

    sync_dynamic_tools_for_agent(
        db,
        capabilities,
        cred_store,
        &entry.id,
        &agent_def.capability_manifests,
    )
    .await;
    let setup_required = match agent_requires_setup(db, cred_store, &entry.id, &agent_def).await {
        Ok(required) => required,
        Err(error) => {
            warn!(agent = %entry.id, %error, "Agent setup state could not be verified; keeping setup incomplete");
            true
        }
    };
    let setup_complete = if setup_required { 0 } else { 1 };
    sqlx::query!(
        "UPDATE agents SET setup_complete = ? WHERE id = ?",
        setup_complete,
        entry.id
    )
    .execute(db)
    .await
    .context("Failed to save setup state")?;

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
    validate_agent_id(agent_id)?;
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

    let changed = sqlx::query("UPDATE agents SET setup_complete = 1 WHERE id = ?")
        .bind(agent_id)
        .execute(db)
        .await
        .context("Failed to mark setup complete")?;
    if changed.rows_affected() != 1 {
        anyhow::bail!("Agent '{}' is not installed", agent_id);
    }

    let current = agents.load();
    let mut new = (**current).clone();
    new.insert(agent_def.id.clone(), Arc::new(agent_def));
    agents.store(Arc::new(new));

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
/// The replacement package and images are fully staged before activation. The
/// current directory is renamed into the retained revision area, the staged
/// directory is renamed into place, and the DB revision switch is committed as
/// one transaction. Any activation failure restores the old directory. The
/// prior package and exact image IDs remain protected for 30 days.
pub async fn update_agent(
    entry: &MarketplaceEntry,
    installed_dir: &str,
    db: &SqlitePool,
    agents: &Arc<ArcSwap<AgentMap>>,
    docker: &bollard::Docker,
    capabilities: &CapabilityEngine,
    cred_store: &CredentialStore,
    docker_storage: &DockerStorage,
) -> Result<AgentDefinition> {
    update_agent_with_progress(
        entry,
        installed_dir,
        db,
        agents,
        docker,
        capabilities,
        cred_store,
        docker_storage,
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
    docker_storage: &DockerStorage,
    progress_tx: Option<tokio::sync::mpsc::UnboundedSender<PullProgress>>,
) -> Result<AgentDefinition> {
    validate_agent_id(&entry.id)?;
    let agent_dir = Path::new(installed_dir).join(&entry.id);
    if !agent_dir.exists() {
        return Err(anyhow::anyhow!(
            "Agent '{}' is not installed — cannot update",
            entry.id
        ));
    }

    let _maintenance_lease = docker_storage.exclusive_lease().await;
    let installed = sqlx::query!("SELECT is_bundled FROM agents WHERE id = ?", entry.id)
        .fetch_optional(db)
        .await?;
    match installed {
        Some(row) if row.is_bundled == 1 => {
            return Err(anyhow::anyhow!("Cannot update the bundled default agent"));
        }
        Some(_) => {}
        None => return Err(anyhow::anyhow!("Agent '{}' is not installed", entry.id)),
    }

    let old_revision_id = docker_storage
        .current_revision_id(&entry.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("The installed agent image registry is not ready"))?;
    let old_definition = loader::load_one(&agent_dir)
        .await
        .with_context(|| format!("Installed agent '{}' could not be read", entry.id))?;
    let old_cap_ids: Vec<String> = old_definition
        .capability_manifests
        .keys()
        .cloned()
        .collect();

    info!(agent = %entry.id, from_url = %entry.archive_url, version = %entry.version, "Staging agent update");
    let archive_bytes = download_archive(&entry.archive_url).await?;
    if !entry.archive_sha256.is_empty() {
        verify_sha256(&archive_bytes, &entry.archive_sha256)?;
    } else {
        warn!(agent = %entry.id, "No SHA-256 checksum provided — skipping verification");
    }

    let (revision_id, staging_dir) = docker_storage
        .create_staged_revision(&entry.id, &entry.version)
        .await?;
    let prepared = async {
        extract_archive_to(&archive_bytes, &staging_dir)?;
        prepare_agent_revision(
            entry,
            &staging_dir,
            docker,
            capabilities,
            cred_store,
            docker_storage,
            &revision_id,
            progress_tx.as_ref(),
        )
        .await
    }
    .await;
    let agent_def = match prepared {
        Ok(definition) => definition,
        Err(error) => {
            docker_storage.mark_revision_failed(&revision_id).await?;
            let _ = tokio::fs::remove_dir_all(&staging_dir).await;
            return Err(error);
        }
    };

    let paths = activation_paths(installed_dir, &entry.id, &old_revision_id, &revision_id).await?;
    let canonical_staging = tokio::fs::canonicalize(&staging_dir)
        .await
        .with_context(|| format!("Failed to resolve staged package {}", staging_dir.display()))?;
    if canonical_staging != paths.staged {
        anyhow::bail!(
            "Staged package path {} is outside configured managed staging root ({})",
            canonical_staging.display(),
            paths.staged.display()
        );
    }
    let initial_state = paths.inspect().await?;
    if initial_state
        != (ActivationFsState {
            active: true,
            retained: false,
            staged: true,
        })
    {
        anyhow::bail!(
            "Agent '{}' activation paths are not clean before update: active={}, retained={}, staged={}",
            entry.id,
            initial_state.active,
            initial_state.retained,
            initial_state.staged
        );
    }
    let journal = ActivationJournal {
        agent_id: entry.id.clone(),
        old_revision_id: old_revision_id.clone(),
        new_revision_id: revision_id.clone(),
        active_path: paths.active.clone(),
        retained_path: paths.retained.clone(),
        staged_path: paths.staged.clone(),
        phase: ActivationPhase::Prepared,
    };
    persist_activation_journal(db, &journal).await?;

    // Keep the agent absent from the active map for the entire package switch.
    // The exclusive maintenance lease blocks container starts, and staged
    // validation uses the non-recursive lease-held runner path.
    let previous_active = remove_agent_from_map(agents, &entry.id);
    for cap_id in &old_cap_ids {
        capabilities.close_capability(cap_id).await;
    }

    let activated = async {
        tokio::fs::rename(&paths.active, &paths.retained)
            .await
            .with_context(|| format!("Failed to retain the previous package for {}", entry.id))?;
        update_activation_phase(db, &entry.id, ActivationPhase::OldRetained).await?;

        tokio::fs::rename(&paths.staged, &paths.active)
            .await
            .context("Failed to activate the staged agent update")?;
        update_activation_phase(db, &entry.id, ActivationPhase::NewActive).await?;

        let mut tx = db.begin().await?;
        sqlx::query!(
            "UPDATE agent_package_revisions
             SET state = 'failed', retain_until = datetime('now', '+7 days'),
                 updated_at = datetime('now')
             WHERE agent_id = ? AND state = 'previous'",
            entry.id
        )
        .execute(&mut *tx)
        .await?;
        let retained_path = paths.retained.to_string_lossy().to_string();
        let old_changed = sqlx::query!(
            "UPDATE agent_package_revisions
             SET state = 'previous', retain_until = datetime('now', '+30 days'),
                 package_path = ?, updated_at = datetime('now')
             WHERE id = ? AND agent_id = ? AND state = 'current'",
            retained_path,
            old_revision_id,
            entry.id
        )
        .execute(&mut *tx)
        .await?;
        let current_path = paths.active.to_string_lossy().to_string();
        let new_changed = sqlx::query!(
            "UPDATE agent_package_revisions
             SET state = 'current', retain_until = NULL, package_path = ?,
                 updated_at = datetime('now')
             WHERE id = ? AND agent_id = ? AND state = 'staged'",
            current_path,
            revision_id,
            entry.id
        )
        .execute(&mut *tx)
        .await?;
        let agent_changed = sqlx::query!(
            "UPDATE agents SET display_name = ?, version = ?, source_url = ?,
                 setup_complete = 0 WHERE id = ?",
            entry.name,
            entry.version,
            entry.archive_url,
            entry.id
        )
        .execute(&mut *tx)
        .await?;
        let journal_deleted = sqlx::query(
            "DELETE FROM agent_package_activation_journal
             WHERE agent_id = ? AND old_revision_id = ? AND new_revision_id = ?",
        )
        .bind(&entry.id)
        .bind(&old_revision_id)
        .bind(&revision_id)
        .execute(&mut *tx)
        .await?;
        if old_changed.rows_affected() != 1
            || new_changed.rows_affected() != 1
            || agent_changed.rows_affected() != 1
            || journal_deleted.rows_affected() != 1
        {
            anyhow::bail!("Agent package activation database state changed before commit");
        }
        tx.commit().await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Err(activation_error) = activated {
        let rollback_result: Result<()> = async {
            let persisted_journal = load_activation_journals(db)
                .await?
                .into_iter()
                .find(|candidate| candidate.agent_id == entry.id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Activation journal for agent '{}' disappeared before rollback",
                        entry.id
                    )
                })?;
            reconcile_activation_journal(db, installed_dir, &persisted_journal).await
        }
        .await;
        match rollback_result {
            Ok(()) => {
                restore_agent_in_map(agents, &entry.id, previous_active);
                return Err(activation_error)
                    .context("Failed to activate the staged agent update; prior package restored");
            }
            Err(rollback_error) => {
                return Err(anyhow::anyhow!(
                    "Agent '{}' activation failed ({activation_error:#}) and safe filesystem rollback also failed ({rollback_error:#}). The activation journal was retained; inspect {}, {}, and {} before restarting Selu",
                    entry.id,
                    paths.active.display(),
                    paths.retained.display(),
                    paths.staged.display()
                ));
            }
        }
    }
    drop(_maintenance_lease);

    sync_dynamic_tools_for_agent(
        db,
        capabilities,
        cred_store,
        &entry.id,
        &agent_def.capability_manifests,
    )
    .await;
    let setup_required = match agent_requires_setup(db, cred_store, &entry.id, &agent_def).await {
        Ok(required) => required,
        Err(error) => {
            warn!(agent = %entry.id, %error, "Updated agent setup state could not be verified; keeping setup incomplete");
            true
        }
    };
    let setup_complete = if setup_required { 0 } else { 1 };
    sqlx::query!(
        "UPDATE agents SET setup_complete = ? WHERE id = ?",
        setup_complete,
        entry.id
    )
    .execute(db)
    .await
    .context("Failed to save updated setup state")?;

    let current = agents.load();
    let mut new = (**current).clone();
    if setup_required {
        new.remove(&entry.id);
    } else {
        new.insert(agent_def.id.clone(), Arc::new(agent_def.clone()));
    }
    agents.store(Arc::new(new));

    info!(agent = %entry.id, version = %entry.version, setup_complete, "Agent updated");
    Ok(agent_def)
}

async fn prepare_agent_revision(
    entry: &MarketplaceEntry,
    agent_dir: &Path,
    docker: &bollard::Docker,
    capabilities: &CapabilityEngine,
    cred_store: &CredentialStore,
    docker_storage: &DockerStorage,
    revision_id: &str,
    progress_tx: Option<&tokio::sync::mpsc::UnboundedSender<PullProgress>>,
) -> Result<AgentDefinition> {
    let mut agent_def = loader::load_one_strict(agent_dir)
        .await
        .with_context(|| format!("Failed to load staged agent from {}", agent_dir.display()))?;
    if agent_def.id != entry.id {
        warn!(agent = %entry.id, yaml_id = %agent_def.id, "agent.yaml id differs from marketplace id — using marketplace id");
        agent_def.id = entry.id.clone();
    }

    let manifest_images = agent_def
        .capability_manifests
        .values()
        .map(|manifest| manifest.image.clone())
        .collect();
    let image_refs = validate_capability_image_sets(&entry.capability_images, manifest_images)?;
    let image_count = image_refs.len().max(1);
    let mut immutable_images = HashMap::new();
    for (index, image_ref) in image_refs.iter().enumerate() {
        let image_id = pull_image(
            docker,
            docker_storage,
            revision_id,
            image_ref,
            index,
            image_count,
            progress_tx,
        )
        .await?;
        immutable_images.insert(image_ref.clone(), image_id);
    }

    // Discovery must execute the exact identity inspected and recorded after
    // pull. A tag may move between pull and container creation.
    let mut validation_manifests = agent_def.capability_manifests.clone();
    for manifest in validation_manifests.values_mut() {
        manifest.image = immutable_images
            .get(&manifest.image)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Missing immutable staged image identity"))?;
    }
    validate_dynamic_tools_for_agent(capabilities, cred_store, &entry.id, &validation_manifests)
        .await
        .context("Failed to validate staged dynamic capabilities")?;

    Ok(agent_def)
}

fn validate_capability_image_sets(
    marketplace_images: &[String],
    manifest_images: Vec<String>,
) -> Result<Vec<String>> {
    let marketplace: BTreeSet<String> = marketplace_images.iter().cloned().collect();
    let manifest: BTreeSet<String> = manifest_images.into_iter().collect();
    if marketplace != manifest {
        let missing: Vec<&str> = manifest
            .difference(&marketplace)
            .map(String::as_str)
            .collect();
        let unexpected: Vec<&str> = marketplace
            .difference(&manifest)
            .map(String::as_str)
            .collect();
        return Err(anyhow::anyhow!(
            "Marketplace capability images do not match the package manifests (missing from marketplace: {:?}; unexpected in marketplace: {:?})",
            missing,
            unexpected
        ));
    }
    Ok(manifest.into_iter().collect())
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
    validate_agent_id(agent_id)?;
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
    docker_storage: &DockerStorage,
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
            docker_storage,
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

async fn capability_ids_used_by_other_agents(
    installed_dir: &str,
    db: &SqlitePool,
    agents: &Arc<ArcSwap<AgentMap>>,
    excluded_agent_id: &str,
) -> Result<HashSet<String>> {
    let rows =
        sqlx::query_as::<_, (String, i32)>("SELECT id, is_bundled FROM agents WHERE id <> ?")
            .bind(excluded_agent_id)
            .fetch_all(db)
            .await?;
    let active = agents.load_full();
    let mut capability_ids = HashSet::new();
    for (agent_id, is_bundled) in rows {
        validate_agent_id(&agent_id)
            .with_context(|| format!("Installed agent '{}' has an unsafe database id", agent_id))?;
        if is_bundled == 1 {
            let definition = active.get(&agent_id).ok_or_else(|| {
                anyhow::anyhow!(
                    "Cannot prove credential ownership because bundled agent '{}' is not loaded",
                    agent_id
                )
            })?;
            capability_ids.extend(definition.capability_manifests.keys().cloned());
            continue;
        }

        let path = Path::new(installed_dir).join(&agent_id);
        let definition = loader::load_one_strict(&path).await.with_context(|| {
            format!(
                "Cannot prove credential ownership because installed agent '{}' is unreadable",
                agent_id
            )
        })?;
        capability_ids.extend(definition.capability_manifests.keys().cloned());
    }
    Ok(capability_ids)
}

/// Uninstall an agent by moving its package to a retained tombstone first, then
/// committing all agent-scoped DB cleanup. If the process crashes after the
/// rename, a retry detects the tombstone and resumes the transaction.
pub async fn uninstall_agent(
    agent_id: &str,
    installed_dir: &str,
    db: &SqlitePool,
    agents: &Arc<ArcSwap<AgentMap>>,
    capabilities: &CapabilityEngine,
    docker_storage: &DockerStorage,
) -> Result<()> {
    use sqlx::Row;

    validate_agent_id(agent_id)?;
    let _maintenance_lease = docker_storage.exclusive_lease().await;
    let row = sqlx::query_as::<_, (i32, i32)>(
        "SELECT is_bundled, setup_complete FROM agents WHERE id = ?",
    )
    .bind(agent_id)
    .fetch_optional(db)
    .await?;
    let setup_complete = match row {
        Some((1, _)) => anyhow::bail!("Cannot uninstall the bundled default agent"),
        Some((_, setup_complete)) => setup_complete == 1,
        None => anyhow::bail!("Agent '{}' is not installed", agent_id),
    };

    let revision_id = docker_storage
        .current_revision_id(agent_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("The installed agent package registry is not ready"))?;
    let agent_dir = Path::new(installed_dir).join(agent_id);
    let tombstone_dir = docker_storage.retained_revision_path(agent_id, &revision_id);
    if agent_dir.exists() && tombstone_dir.exists() {
        anyhow::bail!("Both active and retained package paths exist; uninstall was stopped safely");
    }
    if !agent_dir.exists() && !tombstone_dir.exists() {
        anyhow::bail!(
            "Installed agent package '{}' is missing; uninstall was stopped safely",
            agent_id
        );
    }
    if let Some(parent) = tombstone_dir.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let package_source = if agent_dir.exists() {
        &agent_dir
    } else {
        &tombstone_dir
    };
    let active_definition = agents.load().get(agent_id).cloned();
    let disk_definition = if active_definition.is_none() {
        loader::load_one(package_source)
            .await
            .ok()
            .map(|mut definition| {
                definition.id = agent_id.to_string();
                Arc::new(definition)
            })
    } else {
        None
    };
    let definition_for_restore = active_definition.or(disk_definition);
    let cap_ids: Vec<String> = definition_for_restore
        .as_ref()
        .map(|definition| definition.capability_manifests.keys().cloned().collect())
        .unwrap_or_default();
    let capabilities_used_elsewhere =
        capability_ids_used_by_other_agents(installed_dir, db, agents, agent_id).await?;

    let network_rows = sqlx::query(
        "SELECT user_id, capability_id FROM user_network_access_policies WHERE agent_id = ?
         UNION
         SELECT user_id, capability_id FROM user_network_host_policies WHERE agent_id = ?",
    )
    .bind(agent_id)
    .bind(agent_id)
    .fetch_all(db)
    .await?;
    let network_cache_keys: Vec<(String, String)> = network_rows
        .into_iter()
        .map(|row| (row.get("user_id"), row.get("capability_id")))
        .collect();

    let previous_active = remove_agent_from_map(agents, agent_id);
    for cap_id in &cap_ids {
        capabilities.close_capability(cap_id).await;
    }

    if agent_dir.exists() {
        if let Err(error) = tokio::fs::rename(&agent_dir, &tombstone_dir).await {
            restore_agent_in_map(agents, agent_id, previous_active);
            return Err(error).with_context(|| {
                format!("Failed to retain agent package {}", agent_dir.display())
            });
        }
    }

    let transaction_result: Result<()> = async {
        let mut tx = db.begin().await?;
        let tombstone_path = tombstone_dir.to_string_lossy().to_string();
        let revision = sqlx::query(
            "UPDATE agent_package_revisions
             SET state = 'uninstalled', retain_until = datetime('now', '+14 days'),
                 package_path = ?, updated_at = datetime('now')
             WHERE id = ? AND agent_id = ? AND state = 'current'",
        )
        .bind(&tombstone_path)
        .bind(&revision_id)
        .bind(agent_id)
        .execute(&mut *tx)
        .await
        .context("Failed to retain the uninstalled agent package and image references")?;
        if revision.rows_affected() != 1 {
            anyhow::bail!("The current agent revision changed while uninstalling");
        }

        for statement in [
            "DELETE FROM tool_policies WHERE agent_id = ?",
            "DELETE FROM global_tool_policies WHERE agent_id = ?",
            "DELETE FROM pending_tool_approvals WHERE agent_id = ?",
            "DELETE FROM user_network_access_policies WHERE agent_id = ?",
            "DELETE FROM user_network_host_policies WHERE agent_id = ?",
            "DELETE FROM discovered_tools WHERE agent_id = ?",
            "DELETE FROM tool_discovery_state WHERE agent_id = ?",
            "DELETE FROM agent_storage WHERE agent_id = ?",
            "DELETE FROM turn_signals WHERE agent_id = ?",
            "DELETE FROM agent_insights WHERE agent_id = ?",
        ] {
            sqlx::query(statement)
                .bind(agent_id)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("Failed to execute uninstall statement: {statement}"))?;
        }

        for cap_id in &cap_ids {
            if capabilities_used_elsewhere.contains(cap_id) {
                continue;
            }
            for statement in [
                "DELETE FROM system_credentials WHERE capability_id = ?",
                "DELETE FROM user_credentials WHERE capability_id = ?",
                "DELETE FROM egress_log WHERE capability_id = ?",
            ] {
                sqlx::query(statement)
                    .bind(cap_id)
                    .execute(&mut *tx)
                    .await
                    .with_context(|| {
                        format!("Failed to execute uninstall statement: {statement}")
                    })?;
            }
        }

        let deleted = sqlx::query("DELETE FROM agents WHERE id = ?")
            .bind(agent_id)
            .execute(&mut *tx)
            .await
            .context("Failed to delete agent from DB")?;
        if deleted.rows_affected() != 1 {
            anyhow::bail!("The agent changed while uninstalling");
        }
        tx.commit()
            .await
            .context("Failed to commit agent uninstall")?;
        Ok(())
    }
    .await;

    if let Err(error) = transaction_result {
        let restore_result = if tombstone_dir.exists() && !agent_dir.exists() {
            tokio::fs::rename(&tombstone_dir, &agent_dir).await
        } else {
            Ok(())
        };
        let restore_definition = if setup_complete {
            previous_active.or(definition_for_restore)
        } else {
            None
        };
        restore_agent_in_map(agents, agent_id, restore_definition);
        if let Err(restore_error) = restore_result {
            return Err(anyhow::anyhow!(
                "Agent uninstall failed ({error:#}); the package remains retained at {} because restoring it also failed: {restore_error}",
                tombstone_dir.display()
            ));
        }
        return Err(error);
    }

    for (user_id, capability_id) in network_cache_keys {
        capabilities
            .invalidate_network_policy_cache(&user_id, agent_id, &capability_id)
            .await;
    }

    info!(agent = agent_id, tombstone = %tombstone_dir.display(), "Agent uninstalled with recovery package retained");
    Ok(())
}

/// Check if an agent is installed.
#[allow(dead_code)]
pub async fn is_installed(db: &SqlitePool, agent_id: &str) -> Result<bool> {
    validate_agent_id(agent_id)?;
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

/// Extract a tar.gz archive into an isolated staging directory.
///
/// The archive may contain files at the root or inside a single top-level
/// directory. We normalize both forms into `target_dir`.
fn extract_archive_to(data: &[u8], target_dir: &Path) -> Result<()> {
    let decoder = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(decoder);

    std::fs::create_dir_all(target_dir)
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

        // Skip empty paths (the top-level directory entry itself).
        if relative.as_os_str().is_empty() {
            continue;
        }
        if relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        }) {
            anyhow::bail!(
                "Archive entry '{}' is not a safe relative path",
                relative.display()
            );
        }

        let full_path = target_dir.join(&relative);

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
    docker_storage: &DockerStorage,
    revision_id: &str,
    image: &str,
    image_index: usize,
    image_count: usize,
    progress_tx: Option<&tokio::sync::mpsc::UnboundedSender<PullProgress>>,
) -> Result<String> {
    use bollard::query_parameters::CreateImageOptions;
    use futures::StreamExt;
    use std::collections::{HashMap, HashSet};

    info!(image, "Pulling capability Docker image");

    let opts = CreateImageOptions {
        from_image: Some(image.to_string()),
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

    let identity = docker_storage.inspect_image(docker, image).await?;
    docker_storage
        .record_image_reference(revision_id, image, &identity)
        .await?;

    info!(image, image_id = %identity.image_id, "Docker image pulled and recorded");
    if let Some(tx) = progress_tx {
        let base = image_index as f32 / image_count as f32;
        let span = 1.0f32 / image_count as f32;
        let _ = tx.send(PullProgress {
            image: image.to_string(),
            overall_fraction: (base + span).clamp(0.0, 1.0),
        });
    }
    Ok(identity.image_id)
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

    #[test]
    fn agent_ids_accept_portable_single_components() {
        for agent_id in ["calendar", "mail-agent", "agent_v2", "vendor.agent-2"] {
            validate_agent_id(agent_id).expect("portable agent id should be accepted");
        }
    }

    #[test]
    fn agent_ids_reject_path_components_and_unsafe_characters() {
        for agent_id in [
            "",
            ".",
            "..",
            "../agent",
            "agent/child",
            r"agent\child",
            "/absolute",
            "C:drive",
            " agent",
            "agent name",
            "agent.",
            "CON",
            "nul.txt",
            "COM1",
        ] {
            assert!(
                validate_agent_id(agent_id).is_err(),
                "unsafe agent id should be rejected: {agent_id:?}"
            );
        }
    }

    #[test]
    fn capability_image_sets_must_match_exactly() {
        let marketplace = vec![
            "ghcr.io/selu/calendar:1".to_string(),
            "ghcr.io/selu/mail:2".to_string(),
        ];
        let manifest = vec![
            "ghcr.io/selu/mail:2".to_string(),
            "ghcr.io/selu/calendar:1".to_string(),
        ];

        let images = validate_capability_image_sets(&marketplace, manifest)
            .expect("the same image set should be accepted");
        assert_eq!(images, marketplace);
    }

    #[test]
    fn capability_image_sets_reject_missing_and_unexpected_images() {
        let marketplace = vec!["ghcr.io/selu/unexpected:1".to_string()];
        let manifest = vec!["ghcr.io/selu/required:1".to_string()];

        let error = validate_capability_image_sets(&marketplace, manifest)
            .expect_err("mismatched image sets must fail");
        let message = error.to_string();
        assert!(message.contains("ghcr.io/selu/required:1"));
        assert!(message.contains("ghcr.io/selu/unexpected:1"));
    }

    #[test]
    fn activation_reconciliation_state_table_covers_every_phase_and_path_shape() {
        let phases = [
            ActivationPhase::Prepared,
            ActivationPhase::OldRetained,
            ActivationPhase::NewActive,
            ActivationPhase::RollbackNewStaged,
            ActivationPhase::RollbackOldActive,
        ];
        for phase in phases {
            for active in [false, true] {
                for retained in [false, true] {
                    for staged in [false, true] {
                        let state = ActivationFsState {
                            active,
                            retained,
                            staged,
                        };
                        let expected = match (phase, active, retained, staged) {
                            (_, true, false, true) => Some(ReconciliationPlan::KeepPriorActive),
                            (
                                ActivationPhase::Prepared
                                | ActivationPhase::OldRetained
                                | ActivationPhase::NewActive
                                | ActivationPhase::RollbackNewStaged,
                                false,
                                true,
                                true,
                            ) => Some(ReconciliationPlan::RestoreRetained),
                            (
                                ActivationPhase::OldRetained | ActivationPhase::NewActive,
                                true,
                                true,
                                false,
                            ) => Some(ReconciliationPlan::StageNewThenRestoreRetained),
                            _ => None,
                        };
                        assert_eq!(
                            reconciliation_plan(phase, state).ok(),
                            expected,
                            "unexpected plan for phase={} active={active} retained={retained} staged={staged}",
                            phase.as_str()
                        );
                    }
                }
            }
        }
    }

    async fn activation_test_db() -> SqlitePool {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect activation test database");
        sqlx::migrate!("./migrations")
            .run(&db)
            .await
            .expect("migrate activation test database");
        db
    }

    fn activation_test_root(label: &str) -> PathBuf {
        let root = std::env::var_os("KIROCREW_SCRATCH")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        root.join(format!("selu-activation-{label}-{}", uuid::Uuid::new_v4()))
    }

    async fn write_package(path: &Path, revision: &str) {
        tokio::fs::create_dir_all(path)
            .await
            .expect("create test package");
        tokio::fs::write(path.join("revision.txt"), revision)
            .await
            .expect("write test package marker");
    }

    async fn seed_activation_journal(
        db: &SqlitePool,
        root: &Path,
        phase: ActivationPhase,
        state: ActivationFsState,
    ) -> (ActivationJournal, ActivationPaths) {
        let installed_dir = root.to_string_lossy().to_string();
        tokio::fs::create_dir_all(root)
            .await
            .expect("create installed-agent root");
        let paths = activation_paths(&installed_dir, "agent-one", "old-rev", "new-rev")
            .await
            .expect("build activation paths");

        if state.active {
            let active_revision = if state.retained && !state.staged {
                "new-rev"
            } else {
                "old-rev"
            };
            write_package(&paths.active, active_revision).await;
        }
        if state.retained {
            write_package(&paths.retained, "old-rev").await;
        }
        if state.staged {
            write_package(&paths.staged, "new-rev").await;
        }

        sqlx::query(
            "INSERT INTO agents (id, display_name, version, is_bundled, setup_complete)
             VALUES ('agent-one', 'Agent One', '1.0.0', 0, 1)",
        )
        .execute(db)
        .await
        .expect("insert test agent");
        sqlx::query(
            "INSERT INTO agent_package_revisions
             (id, agent_id, version, package_path, state)
             VALUES ('old-rev', 'agent-one', '1.0.0', ?, 'current'),
                    ('new-rev', 'agent-one', '2.0.0', ?, 'staged')",
        )
        .bind(paths.active.to_string_lossy().to_string())
        .bind(paths.staged.to_string_lossy().to_string())
        .execute(db)
        .await
        .expect("insert test revisions");
        let journal = ActivationJournal {
            agent_id: "agent-one".to_string(),
            old_revision_id: "old-rev".to_string(),
            new_revision_id: "new-rev".to_string(),
            active_path: paths.active.clone(),
            retained_path: paths.retained.clone(),
            staged_path: paths.staged.clone(),
            phase,
        };
        persist_activation_journal(db, &journal)
            .await
            .expect("persist test activation journal");
        (journal, paths)
    }

    #[tokio::test]
    async fn filesystem_reconciliation_restores_every_durable_activation_phase() {
        let cases = [
            (
                "before-renames",
                ActivationPhase::Prepared,
                ActivationFsState {
                    active: true,
                    retained: false,
                    staged: true,
                },
            ),
            (
                "first-rename-before-phase",
                ActivationPhase::Prepared,
                ActivationFsState {
                    active: false,
                    retained: true,
                    staged: true,
                },
            ),
            (
                "between-renames",
                ActivationPhase::OldRetained,
                ActivationFsState {
                    active: false,
                    retained: true,
                    staged: true,
                },
            ),
            (
                "second-rename-before-phase",
                ActivationPhase::OldRetained,
                ActivationFsState {
                    active: true,
                    retained: true,
                    staged: false,
                },
            ),
            (
                "before-db-commit",
                ActivationPhase::NewActive,
                ActivationFsState {
                    active: true,
                    retained: true,
                    staged: false,
                },
            ),
            (
                "rollback-new-staged",
                ActivationPhase::RollbackNewStaged,
                ActivationFsState {
                    active: false,
                    retained: true,
                    staged: true,
                },
            ),
            (
                "rollback-old-active",
                ActivationPhase::RollbackOldActive,
                ActivationFsState {
                    active: true,
                    retained: false,
                    staged: true,
                },
            ),
        ];

        for (label, phase, state) in cases {
            let db = activation_test_db().await;
            let root = activation_test_root(label);
            let (_journal, paths) = seed_activation_journal(&db, &root, phase, state).await;
            let installed_dir = root.to_string_lossy().to_string();

            assert_eq!(
                reconcile_agent_package_activations(&db, &installed_dir)
                    .await
                    .expect("reconcile interrupted activation"),
                1,
                "case {label}"
            );
            assert_eq!(
                tokio::fs::read_to_string(paths.active.join("revision.txt"))
                    .await
                    .expect("read restored active marker"),
                "old-rev",
                "case {label}"
            );
            assert!(!paths.retained.exists(), "case {label}");
            assert_eq!(
                tokio::fs::read_to_string(paths.staged.join("revision.txt"))
                    .await
                    .expect("read retained failed marker"),
                "new-rev",
                "case {label}"
            );
            let old_state: String = sqlx::query_scalar(
                "SELECT state FROM agent_package_revisions WHERE id = 'old-rev'",
            )
            .fetch_one(&db)
            .await
            .expect("read old revision state");
            let new_state: String = sqlx::query_scalar(
                "SELECT state FROM agent_package_revisions WHERE id = 'new-rev'",
            )
            .fetch_one(&db)
            .await
            .expect("read new revision state");
            let journal_count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM agent_package_activation_journal")
                    .fetch_one(&db)
                    .await
                    .expect("count activation journals");
            assert_eq!(old_state, "current", "case {label}");
            assert_eq!(new_state, "failed", "case {label}");
            assert_eq!(journal_count, 0, "case {label}");
            assert_eq!(
                reconcile_agent_package_activations(&db, &installed_dir)
                    .await
                    .expect("repeat reconciliation is a no-op"),
                0,
                "case {label}"
            );
            let _ = tokio::fs::remove_dir_all(root).await;
        }
    }

    #[tokio::test]
    async fn unsafe_activation_shape_fails_closed_and_keeps_journal() {
        let db = activation_test_db().await;
        let root = activation_test_root("unsafe-shape");
        let (_journal, paths) = seed_activation_journal(
            &db,
            &root,
            ActivationPhase::NewActive,
            ActivationFsState {
                active: true,
                retained: false,
                staged: false,
            },
        )
        .await;
        let installed_dir = root.to_string_lossy().to_string();

        let error = reconcile_agent_package_activations(&db, &installed_dir)
            .await
            .expect_err("ambiguous package identity must stop startup");
        assert!(format!("{error:#}").contains("unsafe filesystem state"));
        assert!(paths.active.exists());
        let journal_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_package_activation_journal")
                .fetch_one(&db)
                .await
                .expect("count retained journal");
        assert_eq!(journal_count, 1);
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn activation_journal_rejects_recorded_paths_outside_managed_roots() {
        let db = activation_test_db().await;
        let root = activation_test_root("escaped-path");
        let (_journal, paths) = seed_activation_journal(
            &db,
            &root,
            ActivationPhase::Prepared,
            ActivationFsState {
                active: true,
                retained: false,
                staged: true,
            },
        )
        .await;
        sqlx::query(
            "UPDATE agent_package_activation_journal SET active_path = ? WHERE agent_id = 'agent-one'",
        )
        .bind(root.join("outside-agent").to_string_lossy().to_string())
        .execute(&db)
        .await
        .expect("tamper recorded path");
        let installed_dir = root.to_string_lossy().to_string();

        let error = reconcile_agent_package_activations(&db, &installed_dir)
            .await
            .expect_err("escaped recorded path must stop startup");
        assert!(format!("{error:#}").contains("outside its configured managed location"));
        assert_eq!(
            tokio::fs::read_to_string(paths.active.join("revision.txt"))
                .await
                .expect("active package remains untouched"),
            "old-rev"
        );
        let journal_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_package_activation_journal")
                .fetch_one(&db)
                .await
                .expect("count retained journal");
        assert_eq!(journal_count, 1);
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
