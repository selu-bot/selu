use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use bollard::Docker;
use bollard::errors::Error as DockerError;
use bollard::query_parameters::{
    ListContainersOptions, ListImagesOptionsBuilder, RemoveImageOptionsBuilder,
};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use uuid::Uuid;

use crate::agents::loader;
use crate::capabilities::manifest::{self, CapabilityManifest};
use crate::updater::client::SidecarUpdaterClient;
use crate::updater::types::{SidecarProtectedImageRef, SidecarStatusResponse};

const SIDECAR_METADATA_MAX_AGE_SECONDS: i64 = 300;
const SIDECAR_METADATA_MAX_FUTURE_SKEW_SECONDS: i64 = 60;
const SUPERSEDED_SYSTEM_IMAGE_DAYS: i64 = 7;
const INITIAL_MANAGED_IMAGE_GRACE_DAYS: i64 = 7;
#[cfg(test)]
const PREVIOUS_REVISION_DAYS: i64 = 30;
#[cfg(test)]
const UNINSTALL_TOMBSTONE_DAYS: i64 = 14;
#[cfg(test)]
const STAGED_IMAGE_DAYS: i64 = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedImageIdentity {
    pub image_id: String,
    pub repo_digest: Option<String>,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordedImageReference {
    pub image_id: String,
    pub repo_digest: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StorageEntry {
    pub image_id: String,
    pub display_name: String,
    pub size_bytes: u64,
    pub state: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StorageReport {
    pub managed_bytes: u64,
    pub protected_bytes: u64,
    pub reclaimable_bytes: u64,
    pub managed_image_count: u64,
    pub protected_image_count: u64,
    pub reclaimable_image_count: u64,
    pub last_cleanup_at: String,
    pub blocked_code: String,
    pub blocked_reason: String,
    pub entries: Vec<StorageEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reclaimed_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reclaimed_image_count: Option<u64>,
}

#[derive(Debug, Clone)]
struct DiscoveredAgent {
    agent_id: String,
    version: String,
    package_path: PathBuf,
    image_refs: Vec<String>,
}

#[derive(Debug, Clone)]
struct ImageRow {
    image_id: String,
    display_name: String,
    size_bytes: u64,
    first_seen_at: String,
    refs: Vec<RetentionRef>,
}

#[derive(Debug, Clone)]
struct RetentionRef {
    state: String,
    retain_until: Option<String>,
    owner: Option<String>,
}

#[derive(Clone)]
pub struct DockerStorage {
    db: SqlitePool,
    installed_agents_dir: Arc<PathBuf>,
    maintenance_lock: Arc<RwLock<()>>,
    updater: Option<SidecarUpdaterClient>,
}

#[derive(Debug, Clone)]
struct ValidatedStorageMetadata {
    managed_repositories: HashSet<String>,
    protected_image_refs: Vec<SidecarProtectedImageRef>,
}

#[derive(Debug, Clone)]
struct LocalManagedImage {
    image_id: String,
    display_name: String,
    size_bytes: u64,
    repo_digests: Vec<String>,
    repositories: Vec<String>,
}

#[derive(Debug, Clone)]
struct ResolvedSystemRef {
    owner: String,
    state: String,
    image_id: String,
    image_ref: String,
    retain_until: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StorageBlock {
    code: &'static str,
    reason: String,
}

impl StorageBlock {
    fn new(code: &'static str, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
}

impl DockerStorage {
    pub fn new(db: SqlitePool, installed_agents_dir: impl Into<PathBuf>) -> Self {
        Self {
            db,
            installed_agents_dir: Arc::new(installed_agents_dir.into()),
            maintenance_lock: Arc::new(RwLock::new(())),
            updater: None,
        }
    }

    pub fn with_updater(
        db: SqlitePool,
        installed_agents_dir: impl Into<PathBuf>,
        updater: SidecarUpdaterClient,
    ) -> Self {
        Self {
            db,
            installed_agents_dir: Arc::new(installed_agents_dir.into()),
            maintenance_lock: Arc::new(RwLock::new(())),
            updater: Some(updater),
        }
    }

    /// Pulls, container starts, installs, updates, and uninstalls take a shared
    /// lease. Preview and cleanup take the exclusive lease, so cleanup cannot
    /// race any operation that can create or change image reachability.
    pub async fn shared_lease(&self) -> OwnedRwLockReadGuard<()> {
        self.maintenance_lock.clone().read_owned().await
    }

    pub async fn exclusive_lease(&self) -> OwnedRwLockWriteGuard<()> {
        self.maintenance_lock.clone().write_owned().await
    }

    pub async fn create_staged_revision(
        &self,
        agent_id: &str,
        version: &str,
    ) -> Result<(String, PathBuf)> {
        let revision_id = Uuid::new_v4().to_string();
        let package_path = self.staging_path(agent_id, &revision_id);
        let package_path_text = package_path.to_string_lossy().to_string();
        sqlx::query!(
            "INSERT INTO agent_package_revisions
             (id, agent_id, version, package_path, state, retain_until)
             VALUES (?, ?, ?, ?, 'staged', datetime('now', '+7 days'))",
            revision_id,
            agent_id,
            version,
            package_path_text
        )
        .execute(&self.db)
        .await
        .context("Failed to stage the agent revision")?;
        Ok((revision_id, package_path))
    }

    pub async fn mark_revision_failed(&self, revision_id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE agent_package_revisions
             SET state = 'failed', retain_until = datetime('now', '+7 days'),
                 updated_at = datetime('now')
             WHERE id = ?",
            revision_id
        )
        .execute(&self.db)
        .await
        .context("Failed to retain the failed agent revision")?;
        Ok(())
    }

    pub async fn mark_agent_uninstalled(&self, agent_id: &str, package_path: &Path) -> Result<()> {
        let package_path = package_path.to_string_lossy().to_string();
        sqlx::query!(
            "UPDATE agent_package_revisions
             SET state = 'uninstalled', retain_until = datetime('now', '+14 days'),
                 package_path = ?, updated_at = datetime('now')
             WHERE agent_id = ? AND state = 'current'",
            package_path,
            agent_id
        )
        .execute(&self.db)
        .await
        .context("Failed to retain the uninstalled agent package and image references")?;
        Ok(())
    }

    pub async fn current_revision_id(&self, agent_id: &str) -> Result<Option<String>> {
        let row = sqlx::query!(
            "SELECT id AS \"id!\" FROM agent_package_revisions
             WHERE agent_id = ? AND state = 'current' LIMIT 1",
            agent_id
        )
        .fetch_optional(&self.db)
        .await?;
        Ok(row.map(|row| row.id))
    }

    /// Re-read the capability manifest from the package revision that is current
    /// after the caller acquires the maintenance lease. Runtime hydration may
    /// populate dynamic tools, so those generated entries are excluded from the
    /// package identity comparison; all startup and policy fields must match.
    pub(crate) async fn validate_current_manifest(
        &self,
        agent_id: &str,
        passed_manifest: &CapabilityManifest,
    ) -> Result<()> {
        let row = sqlx::query(
            "SELECT id, package_path FROM agent_package_revisions
             WHERE agent_id = ? AND state = 'current' LIMIT 1",
        )
        .bind(agent_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| anyhow!("The current agent package revision is not registered"))?;
        let revision_id: String = row.try_get("id")?;
        let package_path: Option<String> = row.try_get("package_path")?;
        let package_path = package_path
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("The current agent package revision has no package path"))?;
        let expected_path = self.installed_agents_dir.join(agent_id);
        if package_path != expected_path {
            return Err(anyhow!(
                "The current agent package revision does not point to the active package"
            ));
        }

        let active_manifests = manifest::load_for_agent_strict(&package_path)
            .await
            .with_context(|| {
                format!(
                    "Failed to revalidate capability manifests for current revision '{revision_id}'"
                )
            })?;
        let active_manifest = active_manifests
            .iter()
            .find(|manifest| manifest.id == passed_manifest.id)
            .ok_or_else(|| {
                anyhow!(
                    "Capability '{}' is not part of the current agent revision",
                    passed_manifest.id
                )
            })?;
        if !manifest_matches_current_revision(passed_manifest, active_manifest)? {
            return Err(anyhow!(
                "Capability '{}' changed while the invocation was waiting; retry with the current agent revision",
                passed_manifest.id
            ));
        }
        Ok(())
    }

    /// Resolve the one immutable image identity recorded for the current package
    /// revision and declared manifest reference. Mutable tags are never inspected
    /// or written from this path.
    pub(crate) async fn current_image_reference(
        &self,
        agent_id: &str,
        image_ref: &str,
    ) -> Result<RecordedImageReference> {
        let rows = sqlx::query(
            "SELECT ref.image_id, ref.repo_digest
             FROM managed_docker_image_refs ref
             JOIN agent_package_revisions revision
               ON revision.id = ref.agent_revision_id
             WHERE revision.agent_id = ? AND revision.state = 'current'
               AND ref.image_ref = ?",
        )
        .bind(agent_id)
        .bind(image_ref)
        .fetch_all(&self.db)
        .await?;
        if rows.len() != 1 {
            return Err(anyhow!(
                "The current agent revision has {} immutable image records for '{}' (expected exactly one)",
                rows.len(),
                image_ref
            ));
        }
        let record = RecordedImageReference {
            image_id: rows[0].try_get("image_id")?,
            repo_digest: rows[0].try_get("repo_digest")?,
        };
        if !is_exact_image_id(&record.image_id) {
            return Err(anyhow!(
                "The current agent revision recorded a non-immutable Docker image ID"
            ));
        }
        Ok(record)
    }

    async fn revision_has_image_reference(
        &self,
        revision_id: &str,
        image_ref: &str,
    ) -> Result<bool> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM managed_docker_image_refs
             WHERE agent_revision_id = ? AND image_ref = ?",
        )
        .bind(revision_id)
        .bind(image_ref)
        .fetch_one(&self.db)
        .await?;
        Ok(count > 0)
    }

    pub async fn inspect_image(
        &self,
        docker: &Docker,
        image_ref: &str,
    ) -> Result<ManagedImageIdentity> {
        let image = docker
            .inspect_image(image_ref)
            .await
            .with_context(|| format!("Failed to inspect managed image '{image_ref}'"))?;
        let image_id = image.id.filter(|id| !id.trim().is_empty()).ok_or_else(|| {
            anyhow!("Docker did not return an immutable image ID for '{image_ref}'")
        })?;
        let repo_digest = select_repo_digest(image_ref, image.repo_digests.as_deref());
        let size_bytes = image.size.unwrap_or_default().max(0) as u64;
        Ok(ManagedImageIdentity {
            image_id,
            repo_digest,
            size_bytes,
        })
    }

    pub async fn record_image_reference(
        &self,
        revision_id: &str,
        image_ref: &str,
        identity: &ManagedImageIdentity,
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let revision = sqlx::query!(
            "SELECT agent_id AS \"agent_id!\", version AS \"version!\"
             FROM agent_package_revisions WHERE id = ?",
            revision_id
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow!("The managed agent revision no longer exists"))?;
        let revision_state: String =
            sqlx::query_scalar("SELECT state FROM agent_package_revisions WHERE id = ?")
                .bind(revision_id)
                .fetch_one(&mut *tx)
                .await?;

        let displaced = sqlx::query!(
            "SELECT COUNT(*) AS \"count!: i64\" FROM managed_docker_image_refs
             WHERE agent_revision_id = ? AND image_ref = ? AND image_id <> ?",
            revision_id,
            image_ref,
            identity.image_id
        )
        .fetch_one(&mut *tx)
        .await?
        .count;
        if displaced > 0 && revision_state == "current" {
            return Err(anyhow!(
                "Refusing to rewrite the immutable image recorded for a current agent revision"
            ));
        }
        if displaced > 0 {
            let displaced_revision_id = Uuid::new_v4().to_string();
            sqlx::query!(
                "INSERT INTO agent_package_revisions
                 (id, agent_id, version, package_path, state, retain_until)
                 VALUES (?, ?, ?, NULL, 'failed', datetime('now', '+7 days'))",
                displaced_revision_id,
                revision.agent_id,
                revision.version
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query!(
                "UPDATE managed_docker_image_refs SET agent_revision_id = ?,
                     updated_at = datetime('now')
                 WHERE agent_revision_id = ? AND image_ref = ? AND image_id <> ?",
                displaced_revision_id,
                revision_id,
                image_ref,
                identity.image_id
            )
            .execute(&mut *tx)
            .await?;
        }

        let size_bytes = i64::try_from(identity.size_bytes).unwrap_or(i64::MAX);
        sqlx::query!(
            "INSERT INTO managed_docker_images
             (image_id, size_bytes, display_name)
             VALUES (?, ?, ?)
             ON CONFLICT(image_id) DO UPDATE SET
                 size_bytes = excluded.size_bytes,
                 display_name = excluded.display_name,
                 last_seen_at = datetime('now')",
            identity.image_id,
            size_bytes,
            image_ref
        )
        .execute(&mut *tx)
        .await
        .context("Failed to record the managed Docker image")?;

        let mut repositories = HashSet::from([image_repository(image_ref)]);
        if let Some(repo_digest) = identity.repo_digest.as_deref() {
            repositories.insert(image_repository(repo_digest));
        }
        for repository in repositories {
            sqlx::query!(
                "INSERT INTO managed_docker_image_repositories (image_id, repository)
                 VALUES (?, ?)
                 ON CONFLICT(image_id, repository) DO UPDATE SET
                     updated_at = datetime('now')",
                identity.image_id,
                repository
            )
            .execute(&mut *tx)
            .await?;
        }

        let reference_id = Uuid::new_v4().to_string();
        sqlx::query!(
            "INSERT INTO managed_docker_image_refs
             (id, agent_revision_id, image_id, image_ref, repo_digest)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(agent_revision_id, image_ref, image_id) DO UPDATE SET
                 repo_digest = excluded.repo_digest,
                 updated_at = datetime('now')",
            reference_id,
            revision_id,
            identity.image_id,
            image_ref,
            identity.repo_digest
        )
        .execute(&mut *tx)
        .await
        .context("Failed to record the managed Docker image reference")?;
        tx.commit().await?;
        Ok(())
    }

    /// Adopt every current installed-agent manifest, including setup-incomplete
    /// DB rows and valid filesystem-only directories. A DB-backed package that
    /// cannot be read blocks cleanup rather than allowing an incomplete graph.
    pub async fn bootstrap(&self) -> Result<()> {
        let discovered = match self.discover_installed_agents().await {
            Ok(discovered) => discovered,
            Err(error) => {
                self.set_bootstrap_blocked(
                    "Storage cleanup is unavailable because an installed agent package could not be read.",
                )
                .await;
                return Err(error);
            }
        };

        let docker = match Docker::connect_with_local_defaults() {
            Ok(docker) => docker,
            Err(error) => {
                self.set_bootstrap_blocked(
                    "Storage cleanup is unavailable because Docker could not be checked.",
                )
                .await;
                return Err(error).context("Failed to connect to Docker during image bootstrap");
            }
        };
        let _lease = self.exclusive_lease().await;

        let mut current_revisions = HashMap::new();
        for agent in &discovered {
            if let Some(revision_id) = self.current_revision_id(&agent.agent_id).await? {
                current_revisions.insert(agent.agent_id.clone(), revision_id);
            }
        }

        let mut resolved = Vec::new();
        for agent in &discovered {
            for image_ref in &agent.image_refs {
                if let Some(revision_id) = current_revisions.get(&agent.agent_id)
                    && self
                        .revision_has_image_reference(revision_id, image_ref)
                        .await?
                {
                    continue;
                }
                match self.inspect_image(&docker, image_ref).await {
                    Ok(identity) => {
                        resolved.push((agent.agent_id.clone(), image_ref.clone(), identity))
                    }
                    Err(error) if is_missing_image_error_chain(&error) => {
                        tracing::warn!(agent = %agent.agent_id, image = %image_ref, "Installed agent image is not currently present in Docker");
                    }
                    Err(error) => {
                        self.set_bootstrap_blocked(
                            "Storage cleanup is unavailable because Docker image details could not be read.",
                        )
                        .await;
                        return Err(error);
                    }
                }
            }
        }

        let mut revisions = HashMap::new();
        for agent in &discovered {
            let revision_id = self
                .ensure_bootstrap_revision(&agent.agent_id, &agent.version, &agent.package_path)
                .await?;
            revisions.insert(agent.agent_id.clone(), revision_id);
        }
        for (agent_id, image_ref, identity) in resolved {
            let revision_id = revisions
                .get(&agent_id)
                .ok_or_else(|| anyhow!("Missing bootstrapped agent revision"))?;
            self.record_image_reference(revision_id, &image_ref, &identity)
                .await?;
        }

        sqlx::query!(
            "UPDATE docker_storage_state
             SET bootstrap_status = 'ready', blocked_reason = '',
                 bootstrapped_at = datetime('now'), updated_at = datetime('now')
             WHERE id = 'global'"
        )
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn refresh_system_metadata_locked(
        &self,
        docker: &Docker,
    ) -> std::result::Result<(), StorageBlock> {
        let updater = self.updater.as_ref().ok_or_else(|| {
            StorageBlock::new(
                "safety_metadata_unavailable",
                "Storage cleanup is unavailable until the updater provides image safety metadata.",
            )
        })?;
        let status = updater.status().await.map_err(|error| {
            tracing::warn!(%error, "Updater storage metadata could not be fetched");
            StorageBlock::new(
                "safety_metadata_unavailable",
                "Storage cleanup is unavailable because updater safety metadata could not be checked.",
            )
        })?;
        let metadata = validate_sidecar_metadata(&status, Utc::now())?;
        self.sync_system_metadata(docker, &metadata)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "Updater storage metadata could not be accepted");
                StorageBlock::new(
                    "safety_metadata_invalid",
                    "Storage cleanup is unavailable because updater image metadata was inconsistent.",
                )
            })
    }

    async fn sync_system_metadata(
        &self,
        docker: &Docker,
        metadata: &ValidatedStorageMetadata,
    ) -> Result<()> {
        let images = docker
            .list_images(Some(
                ListImagesOptionsBuilder::default()
                    .all(true)
                    .digests(true)
                    .build(),
            ))
            .await
            .context("Failed to inventory local Docker images")?;

        let mut managed_images = Vec::new();
        for image in images {
            if !image_is_exclusively_allowlisted(
                &image.repo_tags,
                &image.repo_digests,
                &metadata.managed_repositories,
            ) {
                continue;
            }
            let display_name = image
                .repo_tags
                .iter()
                .chain(image.repo_digests.iter())
                .find(|image_ref| {
                    metadata
                        .managed_repositories
                        .contains(&image_repository(image_ref))
                })
                .cloned()
                .unwrap_or_else(|| image.id.clone());
            let repositories = image_repositories(&image.repo_tags, &image.repo_digests)
                .into_iter()
                .collect();
            managed_images.push(LocalManagedImage {
                image_id: image.id,
                display_name,
                size_bytes: image.size.max(0) as u64,
                repo_digests: image.repo_digests,
                repositories,
            });
        }

        let mut resolved_refs = Vec::new();
        for protected in &metadata.protected_image_refs {
            let matching: Vec<_> = if is_exact_image_id(&protected.image_ref) {
                managed_images
                    .iter()
                    .filter(|image| image.image_id == protected.image_ref)
                    .collect()
            } else {
                managed_images
                    .iter()
                    .filter(|image| image.repo_digests.contains(&protected.image_ref))
                    .collect()
            };
            if matching.len() != 1 {
                return Err(anyhow!(
                    "Protected {} {} image reference resolved to {} allowlisted images",
                    protected.owner,
                    protected.state,
                    matching.len()
                ));
            }
            resolved_refs.push(ResolvedSystemRef {
                owner: protected.owner.clone(),
                state: protected.state.clone(),
                image_id: matching[0].image_id.clone(),
                image_ref: protected.image_ref.clone(),
                retain_until: protected.retain_until.clone(),
            });
        }

        let mut tx = self.db.begin().await?;
        for image in managed_images {
            let size_bytes = i64::try_from(image.size_bytes).unwrap_or(i64::MAX);
            sqlx::query!(
                "INSERT INTO managed_docker_images
                 (image_id, size_bytes, display_name)
                 VALUES (?, ?, ?)
                 ON CONFLICT(image_id) DO UPDATE SET
                     size_bytes = excluded.size_bytes,
                     display_name = excluded.display_name,
                     last_seen_at = datetime('now')",
                image.image_id,
                size_bytes,
                image.display_name
            )
            .execute(&mut *tx)
            .await?;
            for repository in image.repositories {
                sqlx::query!(
                    "INSERT INTO managed_docker_image_repositories (image_id, repository)
                     VALUES (?, ?)
                     ON CONFLICT(image_id, repository) DO UPDATE SET
                         updated_at = datetime('now')",
                    image.image_id,
                    repository
                )
                .execute(&mut *tx)
                .await?;
            }
        }

        let superseded_retain_until =
            (Utc::now() + Duration::days(SUPERSEDED_SYSTEM_IMAGE_DAYS)).to_rfc3339();
        sqlx::query!(
            "UPDATE managed_docker_system_refs
             SET state = 'superseded', retain_until = ?,
                 updated_at = datetime('now')
             WHERE state IN ('current', 'previous')",
            superseded_retain_until
        )
        .execute(&mut *tx)
        .await?;

        for protected in resolved_refs {
            let id = Uuid::new_v4().to_string();
            sqlx::query!(
                "INSERT INTO managed_docker_system_refs
                 (id, owner, state, image_id, image_ref, retain_until)
                 VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT(owner, image_ref, image_id) DO UPDATE SET
                     state = excluded.state,
                     retain_until = excluded.retain_until,
                     updated_at = datetime('now')",
                id,
                protected.owner,
                protected.state,
                protected.image_id,
                protected.image_ref,
                protected.retain_until
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn preview(&self) -> Result<StorageReport> {
        let _lease = self.exclusive_lease().await;
        self.report_locked().await
    }

    pub async fn cleanup(
        &self,
        automatic: bool,
        confirmed_image_ids: Option<&HashSet<String>>,
    ) -> Result<StorageReport> {
        let _lease = self.exclusive_lease().await;
        let status = self.bootstrap_status().await?;
        if status.0 != "ready" {
            let mut report = self
                .report_from_db(HashSet::new(), "bootstrap_incomplete", status.1)
                .await?;
            report.reclaimed_bytes = Some(0);
            report.reclaimed_image_count = Some(0);
            return Ok(report);
        }

        let docker = match Docker::connect_with_local_defaults() {
            Ok(docker) => docker,
            Err(error) => {
                if automatic {
                    tracing::warn!(%error, "Automatic image cleanup skipped because Docker is unavailable");
                }
                let mut report = self
                    .report_from_db(
                        HashSet::new(),
                        "docker_unavailable",
                        "Storage cleanup is unavailable because Docker could not be checked."
                            .to_string(),
                    )
                    .await?;
                report.reclaimed_bytes = Some(0);
                report.reclaimed_image_count = Some(0);
                return Ok(report);
            }
        };

        if let Err(blocked) = self.refresh_system_metadata_locked(&docker).await {
            if automatic {
                tracing::warn!(reason = %blocked.reason, code = blocked.code, "Automatic image cleanup skipped by updater safety gate");
            }
            let mut report = self
                .report_from_db(HashSet::new(), blocked.code, blocked.reason)
                .await?;
            report.reclaimed_bytes = Some(0);
            report.reclaimed_image_count = Some(0);
            return Ok(report);
        }
        self.cleanup_expired_packages().await?;

        let protected_ids = match container_image_ids(&docker).await {
            Ok(ids) => ids,
            Err(error) => {
                if automatic {
                    tracing::warn!(%error, "Automatic image cleanup skipped because containers could not be checked");
                }
                let mut report = self
                    .report_from_db(
                        HashSet::new(),
                        "container_inventory_unavailable",
                        "Storage cleanup is unavailable because running and stopped containers could not be checked."
                            .to_string(),
                    )
                    .await?;
                report.reclaimed_bytes = Some(0);
                report.reclaimed_image_count = Some(0);
                return Ok(report);
            }
        };

        let before = self
            .report_from_db(protected_ids.clone(), "", String::new())
            .await?;
        let candidates: Vec<_> = before
            .entries
            .iter()
            .filter(|entry| entry.state == "reclaimable")
            .cloned()
            .collect();
        if let Some(confirmed) = confirmed_image_ids {
            if !cleanup_preview_matches(&candidates, confirmed) {
                let mut report = before;
                report.blocked_code = "preview_changed".to_string();
                report.blocked_reason =
                    "The cleanup preview changed. Review the current images before confirming again."
                        .to_string();
                report.reclaimed_bytes = Some(0);
                report.reclaimed_image_count = Some(0);
                return Ok(report);
            }
        }
        let mut reclaimed_bytes = 0u64;
        let mut reclaimed_image_count = 0u64;
        let mut removal_blocked = false;

        for entry in candidates {
            match image_has_only_managed_repositories(&docker, &self.db, &entry.image_id).await {
                Ok(true) => {}
                Ok(false) => {
                    removal_blocked = true;
                    tracing::warn!(image_id = %entry.image_id, "Managed image has an unrelated repository alias; refusing deletion");
                    continue;
                }
                Err(error) => {
                    removal_blocked = true;
                    tracing::warn!(image_id = %entry.image_id, %error, "Managed image repositories could not be rechecked; refusing deletion");
                    continue;
                }
            }
            let options = RemoveImageOptionsBuilder::default()
                .force(false)
                .noprune(true)
                .build();
            match docker
                .remove_image(&entry.image_id, Some(options), None)
                .await
            {
                Ok(_) => {
                    sqlx::query!(
                        "DELETE FROM managed_docker_images WHERE image_id = ?",
                        entry.image_id
                    )
                    .execute(&self.db)
                    .await?;
                    reclaimed_bytes = reclaimed_bytes.saturating_add(entry.size_bytes);
                    reclaimed_image_count = reclaimed_image_count.saturating_add(1);
                }
                Err(error) if is_missing_image_error(&error) => {
                    sqlx::query!(
                        "DELETE FROM managed_docker_images WHERE image_id = ?",
                        entry.image_id
                    )
                    .execute(&self.db)
                    .await?;
                }
                Err(error) => {
                    removal_blocked = true;
                    tracing::warn!(image_id = %entry.image_id, %error, "Managed image could not be removed without force");
                }
            }
        }

        sqlx::query!(
            "UPDATE docker_storage_state
             SET last_cleanup_at = datetime('now'), updated_at = datetime('now')
             WHERE id = 'global'"
        )
        .execute(&self.db)
        .await?;

        let (blocked_code, blocked_reason) = if removal_blocked {
            (
                "cleanup_incomplete",
                "Some eligible images are still in use and could not be removed.".to_string(),
            )
        } else {
            ("", String::new())
        };
        let mut report = self
            .report_from_db(protected_ids, blocked_code, blocked_reason)
            .await?;
        report.reclaimed_bytes = Some(reclaimed_bytes);
        report.reclaimed_image_count = Some(reclaimed_image_count);
        Ok(report)
    }

    async fn report_locked(&self) -> Result<StorageReport> {
        let status = self.bootstrap_status().await?;
        if status.0 != "ready" {
            return self
                .report_from_db(HashSet::new(), "bootstrap_incomplete", status.1)
                .await;
        }
        let docker = match Docker::connect_with_local_defaults() {
            Ok(docker) => docker,
            Err(_) => {
                return self
                    .report_from_db(
                        HashSet::new(),
                        "docker_unavailable",
                        "Storage cleanup is unavailable because Docker could not be checked."
                            .to_string(),
                    )
                    .await;
            }
        };
        if let Err(blocked) = self.refresh_system_metadata_locked(&docker).await {
            return self
                .report_from_db(HashSet::new(), blocked.code, blocked.reason)
                .await;
        }
        match container_image_ids(&docker).await {
            Ok(ids) => self.report_from_db(ids, "", String::new()).await,
            Err(_) => {
                self.report_from_db(
                    HashSet::new(),
                    "container_inventory_unavailable",
                    "Storage cleanup is unavailable because running and stopped containers could not be checked."
                        .to_string(),
                )
                .await
            }
        }
    }

    async fn report_from_db(
        &self,
        container_images: HashSet<String>,
        blocked_code: &str,
        blocked_reason: String,
    ) -> Result<StorageReport> {
        let rows = sqlx::query!(
            "WITH retention_refs AS (
                 SELECT ref.image_id, r.state, r.retain_until, NULL AS owner, r.created_at
                 FROM managed_docker_image_refs ref
                 JOIN agent_package_revisions r ON r.id = ref.agent_revision_id
                 UNION ALL
                 SELECT image_id, state, retain_until, owner, created_at
                 FROM managed_docker_system_refs
             )
             SELECT i.image_id AS \"image_id!\", i.display_name AS \"display_name!\",
                    i.size_bytes AS \"size_bytes!\", i.first_seen_at AS \"first_seen_at!\",
                    ref.state, ref.retain_until, ref.owner
             FROM managed_docker_images i
             LEFT JOIN retention_refs ref ON ref.image_id = i.image_id
             ORDER BY i.image_id, ref.created_at DESC"
        )
        .fetch_all(&self.db)
        .await?;
        let state =
            sqlx::query!("SELECT last_cleanup_at FROM docker_storage_state WHERE id = 'global'")
                .fetch_one(&self.db)
                .await?;

        let mut images: HashMap<String, ImageRow> = HashMap::new();
        for row in rows {
            let entry = images
                .entry(row.image_id.clone())
                .or_insert_with(|| ImageRow {
                    image_id: row.image_id,
                    display_name: row.display_name,
                    size_bytes: row.size_bytes.max(0) as u64,
                    first_seen_at: row.first_seen_at,
                    refs: Vec::new(),
                });
            if let Some(reference_state) = row.state {
                entry.refs.push(RetentionRef {
                    state: reference_state,
                    retain_until: row.retain_until,
                    owner: row.owner,
                });
            }
        }

        Ok(classify_images(
            images.into_values().collect(),
            &container_images,
            blocked_code,
            &blocked_reason,
            state.last_cleanup_at,
            Utc::now(),
        ))
    }

    async fn bootstrap_status(&self) -> Result<(String, String)> {
        let state = sqlx::query!(
            "SELECT bootstrap_status, blocked_reason
             FROM docker_storage_state WHERE id = 'global'"
        )
        .fetch_one(&self.db)
        .await?;
        Ok((state.bootstrap_status, state.blocked_reason))
    }

    async fn set_bootstrap_blocked(&self, reason: &str) {
        if let Err(error) = sqlx::query!(
            "UPDATE docker_storage_state
             SET bootstrap_status = 'blocked', blocked_reason = ?,
                 updated_at = datetime('now') WHERE id = 'global'",
            reason
        )
        .execute(&self.db)
        .await
        {
            tracing::error!(%error, "Failed to persist blocked Docker storage bootstrap state");
        }
    }

    async fn ensure_bootstrap_revision(
        &self,
        agent_id: &str,
        version: &str,
        package_path: &Path,
    ) -> Result<String> {
        if let Some(id) = self.current_revision_id(agent_id).await? {
            return Ok(id);
        }
        let revision_id = Uuid::new_v4().to_string();
        let package_path = package_path.to_string_lossy().to_string();
        sqlx::query!(
            "INSERT INTO agent_package_revisions
             (id, agent_id, version, package_path, state)
             VALUES (?, ?, ?, ?, 'current')",
            revision_id,
            agent_id,
            version,
            package_path
        )
        .execute(&self.db)
        .await?;
        Ok(revision_id)
    }

    async fn discover_installed_agents(&self) -> Result<Vec<DiscoveredAgent>> {
        let rows = sqlx::query!(
            "SELECT id AS \"id!\", version AS \"version!\"
             FROM agents WHERE is_bundled = 0 ORDER BY id"
        )
        .fetch_all(&self.db)
        .await
        .context("Failed to list installed agents for image bootstrap")?;
        let mut discovered = Vec::new();
        let mut db_ids = HashSet::new();

        for row in rows {
            db_ids.insert(row.id.clone());
            let package_path = self.installed_agents_dir.join(&row.id);
            let _definition = loader::load_one(&package_path)
                .await
                .with_context(|| format!("Installed agent '{}' could not be read", row.id))?;
            let image_refs = strict_manifest_image_refs(&package_path)
                .await
                .with_context(|| {
                    format!(
                        "Installed agent '{}' has an unreadable capability manifest",
                        row.id
                    )
                })?;
            discovered.push(DiscoveredAgent {
                agent_id: row.id,
                version: row.version,
                package_path,
                image_refs,
            });
        }

        let mut entries = match tokio::fs::read_dir(self.installed_agents_dir.as_ref()).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(discovered),
            Err(error) => return Err(error).context("Failed to scan installed agent packages"),
        };
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || db_ids.contains(&name) {
                continue;
            }
            let package_path = entry.path();
            match loader::load_one(&package_path).await {
                Ok(_definition) => match strict_manifest_image_refs(&package_path).await {
                    Ok(image_refs) => discovered.push(DiscoveredAgent {
                        agent_id: name,
                        version: String::new(),
                        package_path,
                        image_refs,
                    }),
                    Err(error) => {
                        tracing::warn!(dir = %package_path.display(), %error, "Ignoring filesystem-only agent package with an unreadable capability manifest")
                    }
                },
                Err(error) => {
                    tracing::warn!(dir = %package_path.display(), %error, "Ignoring unreadable filesystem-only agent package during image bootstrap")
                }
            }
        }
        Ok(discovered)
    }

    async fn cleanup_expired_packages(&self) -> Result<()> {
        let rows = sqlx::query!(
            "SELECT id, package_path FROM agent_package_revisions
             WHERE state <> 'current' AND retain_until IS NOT NULL
               AND retain_until <= datetime('now')"
        )
        .fetch_all(&self.db)
        .await?;
        let revision_root = self.installed_agents_dir.join(".selu-revisions");
        let staging_root = self.installed_agents_dir.join(".selu-staging");

        for row in rows {
            if let Some(package_path) = row.package_path {
                let path = PathBuf::from(package_path);
                let safe = path.starts_with(&revision_root) || path.starts_with(&staging_root);
                if safe && path.exists() {
                    if let Err(error) = tokio::fs::remove_dir_all(&path).await {
                        tracing::warn!(path = %path.display(), %error, "Expired agent package revision could not be removed");
                        continue;
                    }
                }
            }
            sqlx::query!("DELETE FROM agent_package_revisions WHERE id = ?", row.id)
                .execute(&self.db)
                .await?;
        }
        Ok(())
    }

    pub fn staging_path(&self, agent_id: &str, revision_id: &str) -> PathBuf {
        self.installed_agents_dir
            .join(".selu-staging")
            .join(format!("{agent_id}-{revision_id}"))
    }

    pub fn retained_revision_path(&self, agent_id: &str, revision_id: &str) -> PathBuf {
        self.installed_agents_dir
            .join(".selu-revisions")
            .join(agent_id)
            .join(revision_id)
    }
}

fn validate_sidecar_metadata(
    status: &SidecarStatusResponse,
    now: DateTime<Utc>,
) -> std::result::Result<ValidatedStorageMetadata, StorageBlock> {
    if !status.storage_metadata_ready {
        return Err(StorageBlock::new(
            "safety_metadata_unavailable",
            "Storage cleanup is unavailable until the updater provides image safety metadata.",
        ));
    }

    let generated_at = status
        .storage_metadata_generated_at
        .as_deref()
        .and_then(parse_timestamp)
        .ok_or_else(|| {
            StorageBlock::new(
                "safety_metadata_stale",
                "Storage cleanup is unavailable because updater safety metadata is too old.",
            )
        })?;
    let age = now.signed_duration_since(generated_at);
    if age > Duration::seconds(SIDECAR_METADATA_MAX_AGE_SECONDS)
        || age < -Duration::seconds(SIDECAR_METADATA_MAX_FUTURE_SKEW_SECONDS)
    {
        return Err(StorageBlock::new(
            "safety_metadata_stale",
            "Storage cleanup is unavailable because updater safety metadata is too old.",
        ));
    }

    if status.status == "updating" || status.job_id.is_some() {
        return Err(StorageBlock::new(
            "update_active",
            "Storage cleanup is paused while a Selu update or rollback is active.",
        ));
    }
    if let Some(reason) = status
        .storage_cleanup_block_reason
        .as_deref()
        .filter(|reason| !reason.trim().is_empty())
    {
        return Err(match reason {
            "update_active" => StorageBlock::new(
                "update_active",
                "Storage cleanup is paused while a Selu update or rollback is active.",
            ),
            "updater_restart_helper_active" => StorageBlock::new(
                "restart_helper_active",
                "Storage cleanup is paused while the updater is restarting.",
            ),
            _ => StorageBlock::new(
                "safety_metadata_invalid",
                "Storage cleanup is unavailable because updater safety metadata is incomplete.",
            ),
        });
    }

    let mut managed_repositories = HashSet::new();
    for repository in &status.managed_repositories {
        let repository = repository.trim();
        if repository.is_empty()
            || repository != image_repository(repository)
            || repository == "sha256"
            || repository.starts_with("sha256:")
            || repository.chars().any(char::is_whitespace)
        {
            return Err(StorageBlock::new(
                "safety_metadata_invalid",
                "Storage cleanup is unavailable because the managed image allowlist is invalid.",
            ));
        }
        managed_repositories.insert(repository.to_string());
    }
    if managed_repositories.is_empty() {
        return Err(StorageBlock::new(
            "safety_metadata_invalid",
            "Storage cleanup is unavailable because the managed image allowlist is empty.",
        ));
    }

    let mut owner_states = HashSet::new();
    for protected in &status.protected_image_refs {
        if !matches!(protected.owner.as_str(), "selu" | "updater" | "whatsapp")
            || !matches!(protected.state.as_str(), "current" | "previous")
            || !owner_states.insert((protected.owner.clone(), protected.state.clone()))
        {
            return Err(StorageBlock::new(
                "safety_metadata_invalid",
                "Storage cleanup is unavailable because updater image metadata is inconsistent.",
            ));
        }

        if !is_exact_image_id(&protected.image_ref) {
            let Some((repository, digest)) = protected.image_ref.rsplit_once('@') else {
                return Err(StorageBlock::new(
                    "safety_metadata_invalid",
                    "Storage cleanup is unavailable because a protected image reference is mutable.",
                ));
            };
            if !managed_repositories.contains(repository) || !is_exact_image_id(digest) {
                return Err(StorageBlock::new(
                    "safety_metadata_invalid",
                    "Storage cleanup is unavailable because a protected image reference is outside the managed allowlist.",
                ));
            }
        }

        if protected.state == "previous"
            && protected
                .retain_until
                .as_deref()
                .and_then(parse_timestamp)
                .is_none()
        {
            return Err(StorageBlock::new(
                "safety_metadata_invalid",
                "Storage cleanup is unavailable because previous-image retention metadata is invalid.",
            ));
        }
    }
    if !owner_states.contains(&("selu".to_string(), "current".to_string()))
        || !owner_states.contains(&("updater".to_string(), "current".to_string()))
    {
        return Err(StorageBlock::new(
            "safety_metadata_invalid",
            "Storage cleanup is unavailable because current system image metadata is incomplete.",
        ));
    }

    Ok(ValidatedStorageMetadata {
        managed_repositories,
        protected_image_refs: status.protected_image_refs.clone(),
    })
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                .map(|timestamp| timestamp.and_utc())
        })
        .ok()
}

fn is_exact_image_id(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64 && hex.chars().all(|character| character.is_ascii_hexdigit())
}

fn image_repositories(repo_tags: &[String], repo_digests: &[String]) -> HashSet<String> {
    repo_tags
        .iter()
        .chain(repo_digests.iter())
        .filter(|image_ref| image_ref.as_str() != "<none>:<none>")
        .map(|image_ref| image_repository(image_ref))
        .collect()
}

fn image_is_exclusively_allowlisted(
    repo_tags: &[String],
    repo_digests: &[String],
    allowlist: &HashSet<String>,
) -> bool {
    let repositories = image_repositories(repo_tags, repo_digests);
    !repositories.is_empty()
        && repositories
            .iter()
            .all(|repository| allowlist.contains(repository))
}

async fn strict_manifest_image_refs(agent_dir: &Path) -> Result<Vec<String>> {
    let capabilities_dir = agent_dir.join("capabilities");
    if !capabilities_dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = tokio::fs::read_dir(&capabilities_dir)
        .await
        .with_context(|| {
            format!(
                "Cannot read capabilities directory {}",
                capabilities_dir.display()
            )
        })?;
    let mut refs = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let capability = manifest::load_from_dir(&entry.path()).await?;
        refs.push(capability.image);
    }
    refs.sort();
    refs.dedup();
    Ok(refs)
}

fn select_repo_digest(image_ref: &str, repo_digests: Option<&[String]>) -> Option<String> {
    let digests = repo_digests?;
    let repository = image_repository(image_ref);
    digests
        .iter()
        .find(|digest| digest.split('@').next() == Some(repository.as_str()))
        .cloned()
}

pub(crate) fn restore_ref_for_recorded_image(
    declared_image_ref: &str,
    recorded: &RecordedImageReference,
) -> Result<String> {
    let repo_digest = recorded.repo_digest.as_deref().ok_or_else(|| {
        anyhow!(
            "The pinned image '{}' is missing and its revision has no immutable repository digest",
            recorded.image_id
        )
    })?;
    let Some((repository, digest)) = repo_digest.rsplit_once('@') else {
        return Err(anyhow!(
            "The recorded repository digest for '{}' is not immutable",
            recorded.image_id
        ));
    };
    if repository != image_repository(declared_image_ref) || !is_exact_image_id(digest) {
        return Err(anyhow!(
            "The recorded repository digest for '{}' does not match the capability image repository",
            recorded.image_id
        ));
    }
    Ok(repo_digest.to_string())
}

fn manifest_matches_current_revision(
    passed: &CapabilityManifest,
    active: &CapabilityManifest,
) -> Result<bool> {
    let mut normalized_passed = passed.clone();
    if active.tool_source == manifest::ToolSource::Dynamic {
        normalized_passed.tools.clear();
    }
    Ok(serde_json::to_value(normalized_passed)? == serde_json::to_value(active)?)
}

fn image_repository(image_ref: &str) -> String {
    if let Some((repository, _)) = image_ref.split_once('@') {
        return repository.to_string();
    }
    let slash = image_ref.rfind('/');
    let colon = image_ref.rfind(':');
    match (slash, colon) {
        (_, Some(colon)) if slash.is_none_or(|slash| colon > slash) => {
            image_ref[..colon].to_string()
        }
        _ => image_ref.to_string(),
    }
}

async fn image_has_only_managed_repositories(
    docker: &Docker,
    db: &SqlitePool,
    image_id: &str,
) -> Result<bool> {
    let image = match docker.inspect_image(image_id).await {
        Ok(image) => image,
        Err(error) if is_missing_image_error(&error) => return Ok(true),
        Err(error) => return Err(error).context("Failed to inspect image repositories"),
    };
    let local_repositories = image_repositories(
        image.repo_tags.as_deref().unwrap_or_default(),
        image.repo_digests.as_deref().unwrap_or_default(),
    );
    if local_repositories.is_empty() {
        return Ok(true);
    }
    let rows = sqlx::query!(
        "SELECT repository AS \"repository!\"
         FROM managed_docker_image_repositories WHERE image_id = ?",
        image_id
    )
    .fetch_all(db)
    .await?;
    let managed_repositories: HashSet<_> = rows.into_iter().map(|row| row.repository).collect();
    Ok(local_repositories.is_subset(&managed_repositories))
}

async fn container_image_ids(docker: &Docker) -> Result<HashSet<String>> {
    let containers = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            ..Default::default()
        }))
        .await
        .context("Failed to list Docker containers for image protection")?;
    Ok(containers
        .into_iter()
        .filter_map(|container| container.image_id)
        .collect())
}

fn cleanup_preview_matches(
    candidates: &[StorageEntry],
    confirmed_image_ids: &HashSet<String>,
) -> bool {
    let candidate_ids: HashSet<_> = candidates
        .iter()
        .map(|entry| entry.image_id.clone())
        .collect();
    candidate_ids == *confirmed_image_ids
}

fn classify_images(
    mut images: Vec<ImageRow>,
    container_images: &HashSet<String>,
    blocked_code: &str,
    blocked_reason: &str,
    last_cleanup_at: String,
    now: chrono::DateTime<Utc>,
) -> StorageReport {
    images.sort_by(|left, right| left.display_name.cmp(&right.display_name));
    let mut entries = Vec::with_capacity(images.len());

    for image in images {
        let (protected, reason) = if !blocked_reason.is_empty() {
            (true, "cleanup_blocked".to_string())
        } else if container_images.contains(&image.image_id) {
            (true, "container_in_use".to_string())
        } else if image.refs.is_empty()
            && parse_timestamp(&image.first_seen_at).is_none_or(|first_seen| {
                first_seen + Duration::days(INITIAL_MANAGED_IMAGE_GRACE_DAYS) > now
            })
        {
            (true, "initial_grace_period".to_string())
        } else if let Some(reference) = image
            .refs
            .iter()
            .find(|reference| reference.state == "current")
        {
            let reason = if reference.owner.is_some() {
                "current_system_image"
            } else {
                "current_revision"
            };
            (true, reason.to_string())
        } else if let Some(reference) = image.refs.iter().find(|reference| {
            reference
                .retain_until
                .as_deref()
                .is_some_and(|until| timestamp_is_future(until, now))
        }) {
            let reason = match reference.state.as_str() {
                "previous" => "previous_revision",
                "uninstalled" => "uninstall_retention",
                "staged" | "failed" => "staged_or_failed_revision",
                "superseded" => "superseded_retention",
                _ => "retention_period",
            };
            (true, reason.to_string())
        } else {
            (false, "unreferenced".to_string())
        };
        entries.push(StorageEntry {
            image_id: image.image_id,
            display_name: image.display_name,
            size_bytes: image.size_bytes,
            state: if protected {
                "protected"
            } else {
                "reclaimable"
            }
            .to_string(),
            reason,
        });
    }

    let managed_bytes = entries.iter().map(|entry| entry.size_bytes).sum();
    let protected_bytes = entries
        .iter()
        .filter(|entry| entry.state == "protected")
        .map(|entry| entry.size_bytes)
        .sum();
    let reclaimable_bytes = managed_bytes - protected_bytes;
    let protected_image_count = entries
        .iter()
        .filter(|entry| entry.state == "protected")
        .count() as u64;
    let managed_image_count = entries.len() as u64;

    StorageReport {
        managed_bytes,
        protected_bytes,
        reclaimable_bytes,
        managed_image_count,
        protected_image_count,
        reclaimable_image_count: managed_image_count - protected_image_count,
        last_cleanup_at,
        blocked_code: blocked_code.to_string(),
        blocked_reason: blocked_reason.to_string(),
        entries,
        reclaimed_bytes: None,
        reclaimed_image_count: None,
    }
}

fn timestamp_is_future(value: &str, now: chrono::DateTime<Utc>) -> bool {
    parse_timestamp(value)
        .map(|timestamp| timestamp > now)
        .unwrap_or(false)
}

fn is_missing_image_error(error: &DockerError) -> bool {
    matches!(
        error,
        DockerError::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

fn is_missing_image_error_chain(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<DockerError>()
            .is_some_and(is_missing_image_error)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMAGE_ID_A: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const IMAGE_ID_B: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn sidecar_status(now: DateTime<Utc>) -> SidecarStatusResponse {
        SidecarStatusResponse {
            status: "idle".to_string(),
            progress_key: None,
            message: None,
            job_id: None,
            installed_tag: None,
            installed_digest: None,
            installed_version: None,
            installed_build: None,
            previous_tag: None,
            previous_digest: None,
            previous_version: None,
            previous_build: None,
            storage_metadata_ready: true,
            storage_metadata_generated_at: Some(now.to_rfc3339()),
            storage_cleanup_block_reason: None,
            managed_repositories: vec![
                "ghcr.io/selu-bot/selu".to_string(),
                "ghcr.io/selu-bot/selu-updater".to_string(),
            ],
            protected_image_refs: vec![
                SidecarProtectedImageRef {
                    image_ref: format!("ghcr.io/selu-bot/selu@{IMAGE_ID_A}"),
                    owner: "selu".to_string(),
                    state: "current".to_string(),
                    retain_until: None,
                },
                SidecarProtectedImageRef {
                    image_ref: format!("ghcr.io/selu-bot/selu-updater@{IMAGE_ID_B}"),
                    owner: "updater".to_string(),
                    state: "current".to_string(),
                    retain_until: None,
                },
            ],
        }
    }

    fn image(image_id: &str, state: &str, retain_until: Option<String>) -> ImageRow {
        ImageRow {
            image_id: image_id.to_string(),
            display_name: format!("example/{image_id}:latest"),
            size_bytes: 100,
            first_seen_at: (Utc::now() - Duration::days(INITIAL_MANAGED_IMAGE_GRACE_DAYS + 1))
                .to_rfc3339(),
            refs: vec![RetentionRef {
                state: state.to_string(),
                retain_until,
                owner: None,
            }],
        }
    }

    #[test]
    fn shared_image_is_protected_when_any_revision_is_reachable() {
        let now = Utc::now();
        let mut shared = image(
            "sha256:shared",
            "failed",
            Some(
                (now - Duration::days(1))
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string(),
            ),
        );
        shared.refs.push(RetentionRef {
            state: "current".to_string(),
            retain_until: None,
            owner: None,
        });
        let report = classify_images(vec![shared], &HashSet::new(), "", "", String::new(), now);
        assert_eq!(report.protected_image_count, 1);
        assert_eq!(report.reclaimable_image_count, 0);
    }

    #[test]
    fn current_system_reference_has_priority_and_distinct_reason() {
        let now = Utc::now();
        let mut system = image(
            "sha256:system",
            "previous",
            Some((now + Duration::days(1)).to_rfc3339()),
        );
        system.refs.push(RetentionRef {
            state: "current".to_string(),
            retain_until: None,
            owner: Some("selu".to_string()),
        });

        let report = classify_images(vec![system], &HashSet::new(), "", "", String::new(), now);
        assert_eq!(report.entries[0].state, "protected");
        assert_eq!(report.entries[0].reason, "current_system_image");
    }

    #[test]
    fn grace_periods_expire_conservatively() {
        let now = Utc::now();
        let future = (now + Duration::days(1))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let past = (now - Duration::seconds(1))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let report = classify_images(
            vec![
                image("sha256:previous", "previous", Some(future)),
                image("sha256:failed", "failed", Some(past)),
            ],
            &HashSet::new(),
            "",
            "",
            String::new(),
            now,
        );
        assert_eq!(report.protected_image_count, 1);
        assert_eq!(report.reclaimable_image_count, 1);
    }

    #[test]
    fn stopped_container_image_id_protects_an_expired_image() {
        let now = Utc::now();
        let past = (now - Duration::days(1))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let containers = HashSet::from(["sha256:stopped".to_string()]);
        let report = classify_images(
            vec![image("sha256:stopped", "failed", Some(past))],
            &containers,
            "",
            "",
            String::new(),
            now,
        );
        assert_eq!(report.entries[0].state, "protected");
        assert_eq!(report.entries[0].reason, "container_in_use");
    }

    #[test]
    fn newly_adopted_unreferenced_image_waits_seven_days_before_cleanup() {
        let now = Utc::now();
        let mut recent = image("sha256:recent", "failed", None);
        recent.refs.clear();
        recent.first_seen_at = now.to_rfc3339();
        let mut old = image("sha256:old", "failed", None);
        old.refs.clear();
        old.first_seen_at =
            (now - Duration::days(INITIAL_MANAGED_IMAGE_GRACE_DAYS + 1)).to_rfc3339();

        let report = classify_images(
            vec![recent, old],
            &HashSet::new(),
            "",
            "",
            String::new(),
            now,
        );

        assert_eq!(report.protected_image_count, 1);
        assert_eq!(report.reclaimable_image_count, 1);
        assert!(report.entries.iter().any(|entry| {
            entry.image_id == "sha256:recent" && entry.reason == "initial_grace_period"
        }));
    }

    #[test]
    fn mutable_tags_and_multiple_refs_keep_exact_old_and_new_ids_distinct() {
        let now = Utc::now();
        let report = classify_images(
            vec![
                image(
                    "sha256:old",
                    "previous",
                    Some(
                        (now + Duration::days(PREVIOUS_REVISION_DAYS))
                            .format("%Y-%m-%d %H:%M:%S")
                            .to_string(),
                    ),
                ),
                image("sha256:new", "current", None),
            ],
            &HashSet::new(),
            "",
            "",
            String::new(),
            now,
        );
        assert_eq!(report.managed_image_count, 2);
        assert_eq!(report.protected_image_count, 2);
    }

    #[test]
    fn blocked_registry_fails_closed() {
        let report = classify_images(
            vec![image("sha256:managed", "failed", None)],
            &HashSet::new(),
            "bootstrap_incomplete",
            "Registry bootstrap failed.",
            String::new(),
            Utc::now(),
        );
        assert_eq!(report.reclaimable_image_count, 0);
        assert_eq!(report.entries[0].state, "protected");
        assert_eq!(report.blocked_code, "bootstrap_incomplete");
        assert_eq!(report.entries[0].reason, "cleanup_blocked");
    }

    #[test]
    fn manual_cleanup_requires_the_exact_reviewed_candidate_ids() {
        let candidates = vec![
            StorageEntry {
                image_id: "sha256:first".to_string(),
                display_name: "First".to_string(),
                size_bytes: 10,
                state: "reclaimable".to_string(),
                reason: "unreferenced".to_string(),
            },
            StorageEntry {
                image_id: "sha256:second".to_string(),
                display_name: "Second".to_string(),
                size_bytes: 20,
                state: "reclaimable".to_string(),
                reason: "unreferenced".to_string(),
            },
        ];

        assert!(cleanup_preview_matches(
            &candidates,
            &HashSet::from(["sha256:first".to_string(), "sha256:second".to_string()]),
        ));
        assert!(!cleanup_preview_matches(
            &candidates,
            &HashSet::from(["sha256:first".to_string()]),
        ));
        assert!(!cleanup_preview_matches(
            &candidates,
            &HashSet::from([
                "sha256:first".to_string(),
                "sha256:second".to_string(),
                "sha256:unexpected".to_string(),
            ]),
        ));
    }

    #[tokio::test]
    async fn exclusive_cleanup_lease_waits_for_pull_or_start_lease() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let storage = DockerStorage::new(db, ".");
        let shared = storage.shared_lease().await;
        let waiting = {
            let storage = storage.clone();
            tokio::spawn(async move { storage.exclusive_lease().await })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(shared);
        let exclusive = waiting.await.unwrap();
        drop(exclusive);
    }

    #[test]
    fn repo_digest_selection_handles_registry_ports_and_multiple_refs() {
        let digests = vec![
            "other.example/agent@sha256:one".to_string(),
            "registry.example:5000/agent@sha256:two".to_string(),
        ];
        assert_eq!(
            select_repo_digest("registry.example:5000/agent:latest", Some(&digests)),
            Some("registry.example:5000/agent@sha256:two".to_string())
        );
    }

    #[test]
    fn repo_digest_selection_rejects_other_repository_fallback() {
        let digests = vec![format!("ghcr.io/attacker/selu@{IMAGE_ID_A}")];
        assert_eq!(
            select_repo_digest("ghcr.io/selu-bot/selu:stable", Some(&digests)),
            None
        );
    }

    #[test]
    fn local_image_inventory_requires_only_exact_allowlisted_repositories() {
        let allowlist = HashSet::from(["ghcr.io/selu-bot/selu".to_string()]);
        assert!(image_is_exclusively_allowlisted(
            &["ghcr.io/selu-bot/selu:stable".to_string()],
            &[format!("ghcr.io/selu-bot/selu@{IMAGE_ID_A}")],
            &allowlist,
        ));
        assert!(!image_is_exclusively_allowlisted(
            &["ghcr.io/selu-bot/selu-tools:stable".to_string()],
            &[],
            &allowlist,
        ));
        assert!(!image_is_exclusively_allowlisted(
            &[
                "ghcr.io/selu-bot/selu:stable".to_string(),
                "ghcr.io/unrelated/selu:stable".to_string(),
            ],
            &[],
            &allowlist,
        ));
    }

    #[test]
    fn old_or_stale_updater_metadata_fails_closed() {
        let now = Utc::now();
        let mut old_updater = sidecar_status(now);
        old_updater.storage_metadata_ready = false;
        old_updater.storage_metadata_generated_at = None;
        assert!(validate_sidecar_metadata(&old_updater, now).is_err());

        let mut stale = sidecar_status(now);
        stale.storage_metadata_generated_at =
            Some((now - Duration::seconds(SIDECAR_METADATA_MAX_AGE_SECONDS + 1)).to_rfc3339());
        assert!(validate_sidecar_metadata(&stale, now).is_err());
    }

    #[test]
    fn update_and_restart_helper_gate_cleanup() {
        let now = Utc::now();
        let mut update = sidecar_status(now);
        update.storage_cleanup_block_reason = Some("update_active".to_string());
        let update_block = validate_sidecar_metadata(&update, now).unwrap_err();
        assert_eq!(update_block.code, "update_active");
        assert!(update_block.reason.contains("update or rollback"));

        let mut helper = sidecar_status(now);
        helper.storage_cleanup_block_reason = Some("updater_restart_helper_active".to_string());
        let helper_block = validate_sidecar_metadata(&helper, now).unwrap_err();
        assert_eq!(helper_block.code, "restart_helper_active");
        assert!(helper_block.reason.contains("updater is restarting"));
    }

    #[test]
    fn system_current_previous_and_superseded_refs_follow_retention() {
        let now = Utc::now();
        let future = Some((now + Duration::days(7)).to_rfc3339());
        let past = Some((now - Duration::seconds(1)).to_rfc3339());
        let report = classify_images(
            vec![
                image("sha256:system-current", "current", None),
                image("sha256:system-previous", "previous", future.clone()),
                image("sha256:system-superseded", "superseded", future),
                image("sha256:system-expired", "superseded", past),
            ],
            &HashSet::new(),
            "",
            "",
            String::new(),
            now,
        );
        assert_eq!(report.protected_image_count, 3);
        assert_eq!(report.reclaimable_image_count, 1);
    }

    #[test]
    fn rfc3339_retention_deadline_is_accepted() {
        let now = Utc::now();
        assert!(timestamp_is_future(
            &(now + Duration::minutes(5)).to_rfc3339(),
            now
        ));
        assert!(!timestamp_is_future(
            &(now - Duration::minutes(5)).to_rfc3339(),
            now
        ));
    }

    async fn migrated_db() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        db
    }

    #[tokio::test]
    async fn db_backed_agent_with_missing_manifest_blocks_discovery() {
        let db = migrated_db().await;
        sqlx::query(
            "INSERT INTO agents (id, display_name, version, is_bundled, setup_complete)
             VALUES ('broken-agent', 'Broken', '1.0.0', 0, 0)",
        )
        .execute(&db)
        .await
        .unwrap();
        let root = std::env::temp_dir().join(format!(
            "selu-docker-storage-missing-manifest-{}",
            Uuid::new_v4()
        ));
        let agent_dir = root.join("broken-agent");
        tokio::fs::create_dir_all(agent_dir.join("capabilities/broken"))
            .await
            .unwrap();
        tokio::fs::write(
            agent_dir.join("agent.yaml"),
            "id: broken-agent\nname: Broken\n",
        )
        .await
        .unwrap();

        let storage = DockerStorage::new(db, &root);
        let error = storage.discover_installed_agents().await.unwrap_err();
        assert!(error.to_string().contains("unreadable capability manifest"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn mutable_tag_movement_cannot_rewrite_current_revision() {
        let db = migrated_db().await;
        sqlx::query(
            "INSERT INTO agent_package_revisions
             (id, agent_id, version, state) VALUES ('revision-current', 'agent-1', '1.0.0', 'current')",
        )
        .execute(&db)
        .await
        .unwrap();
        let storage = DockerStorage::new(db.clone(), ".");
        storage
            .record_image_reference(
                "revision-current",
                "example/agent:latest",
                &ManagedImageIdentity {
                    image_id: IMAGE_ID_A.to_string(),
                    repo_digest: Some(format!("example/agent@{IMAGE_ID_A}")),
                    size_bytes: 10,
                },
            )
            .await
            .unwrap();
        let error = storage
            .record_image_reference(
                "revision-current",
                "example/agent:latest",
                &ManagedImageIdentity {
                    image_id: IMAGE_ID_B.to_string(),
                    repo_digest: Some(format!("example/agent@{IMAGE_ID_B}")),
                    size_bytes: 11,
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Refusing to rewrite"));

        let pinned = storage
            .current_image_reference("agent-1", "example/agent:latest")
            .await
            .unwrap();
        assert_eq!(pinned.image_id, IMAGE_ID_A);
        assert_eq!(
            pinned.repo_digest.as_deref(),
            Some(format!("example/agent@{IMAGE_ID_A}").as_str())
        );

        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT ref.image_id, revision.state
             FROM managed_docker_image_refs ref
             JOIN agent_package_revisions revision ON revision.id = ref.agent_revision_id",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(rows, vec![(IMAGE_ID_A.to_string(), "current".to_string())]);
    }

    #[tokio::test]
    async fn queued_old_manifest_aborts_after_revision_update() {
        let db = migrated_db().await;
        let root =
            std::env::temp_dir().join(format!("selu-stale-capability-manifest-{}", Uuid::new_v4()));
        let agent_dir = root.join("agent-1");
        let capability_dir = agent_dir.join("capabilities/capability-1");
        tokio::fs::create_dir_all(&capability_dir).await.unwrap();
        tokio::fs::write(
            capability_dir.join("manifest.yaml"),
            "id: capability-1\nclass: tool\nimage: example/capability:latest\nnetwork:\n  mode: allowlist\n  hosts:\n    - old.example:443\n",
        )
        .await
        .unwrap();
        let passed_manifest = manifest::load_from_dir(&capability_dir).await.unwrap();
        let package_path = agent_dir.to_string_lossy().to_string();
        sqlx::query(
            "INSERT INTO agent_package_revisions
             (id, agent_id, version, package_path, state)
             VALUES ('revision-old', 'agent-1', '1.0.0', ?, 'current')",
        )
        .bind(&package_path)
        .execute(&db)
        .await
        .unwrap();

        let storage = DockerStorage::new(db.clone(), &root);
        let update_lease = storage.exclusive_lease().await;
        let queued = {
            let storage = storage.clone();
            tokio::spawn(async move {
                let _invocation_lease = storage.shared_lease().await;
                storage
                    .validate_current_manifest("agent-1", &passed_manifest)
                    .await
            })
        };
        tokio::task::yield_now().await;
        assert!(
            !queued.is_finished(),
            "queued invocation must wait for the exclusive update lease"
        );

        tokio::fs::write(
            capability_dir.join("manifest.yaml"),
            "id: capability-1\nclass: tool\nimage: example/capability:latest\nnetwork:\n  mode: allowlist\n  hosts:\n    - new.example:443\n",
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE agent_package_revisions SET state = 'previous'
             WHERE id = 'revision-old'",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_package_revisions
             (id, agent_id, version, package_path, state)
             VALUES ('revision-new', 'agent-1', '2.0.0', ?, 'current')",
        )
        .bind(&package_path)
        .execute(&db)
        .await
        .unwrap();
        drop(update_lease);

        let error = queued.await.unwrap().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("changed while the invocation was waiting")
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn uninstall_retains_tombstone_package_path() {
        let db = migrated_db().await;
        sqlx::query(
            "INSERT INTO agent_package_revisions
             (id, agent_id, version, package_path, state)
             VALUES ('revision-current', 'agent-1', '1.0.0', '/agents/agent-1', 'current')",
        )
        .execute(&db)
        .await
        .unwrap();
        let storage = DockerStorage::new(db.clone(), "/agents");
        let tombstone = Path::new("/agents/.selu-revisions/agent-1/revision-current");
        storage
            .mark_agent_uninstalled("agent-1", tombstone)
            .await
            .unwrap();

        let row: (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT state, package_path, retain_until
             FROM agent_package_revisions WHERE id = 'revision-current'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(row.0, "uninstalled");
        assert_eq!(row.1.as_deref(), Some(tombstone.to_str().unwrap()));
        assert!(row.2.is_some());
    }

    #[test]
    fn retention_constants_match_the_cleanup_contract() {
        assert_eq!(PREVIOUS_REVISION_DAYS, 30);
        assert_eq!(UNINSTALL_TOMBSTONE_DAYS, 14);
        assert_eq!(STAGED_IMAGE_DAYS, 7);
        assert_eq!(SUPERSEDED_SYSTEM_IMAGE_DAYS, 7);
    }
}
