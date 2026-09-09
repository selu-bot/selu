use anyhow::{Context, Result, anyhow};
use chrono::{Duration as ChronoDuration, Utc};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::warn;

use crate::state::AppState;
use crate::types::{
    ProtectedImageRef, StorageMetadata, WhatsappBridgeChat, WhatsappBridgeChatsResponse,
    WhatsappBridgeStatusResponse,
};

const ROLLBACK_IMAGE_REF_ENV: &str = "SELU_ROLLBACK_IMAGE_REF";
const ROLLBACK_IMAGE_ID_ENV: &str = "SELU_ROLLBACK_IMAGE_ID";
const ROLLBACK_IMAGE_TAG_ENV: &str = "SELU_ROLLBACK_IMAGE_TAG";
const ROLLBACK_IMAGE_DIGEST_ENV: &str = "SELU_ROLLBACK_IMAGE_DIGEST";
const ROLLBACK_IMAGE_RETAIN_UNTIL_ENV: &str = "SELU_ROLLBACK_IMAGE_RETAIN_UNTIL";
const UPDATER_CURRENT_IMAGE_REF_ENV: &str = "SELU_UPDATER_CURRENT_IMAGE_REF";
const UPDATER_PREVIOUS_IMAGE_REF_ENV: &str = "SELU_UPDATER_PREVIOUS_IMAGE_REF";
const UPDATER_PREVIOUS_RETAIN_UNTIL_ENV: &str = "SELU_UPDATER_PREVIOUS_RETAIN_UNTIL";
const UPDATER_HELPER_ACTIVE_ENV: &str = "SELU_UPDATER_RESTART_HELPER_ACTIVE";
const WHATSAPP_CURRENT_IMAGE_REF_ENV: &str = "SELU_WHATSAPP_CURRENT_IMAGE_REF";
const WHATSAPP_PREVIOUS_IMAGE_REF_ENV: &str = "SELU_WHATSAPP_PREVIOUS_IMAGE_REF";
const WHATSAPP_PREVIOUS_RETAIN_UNTIL_ENV: &str = "SELU_WHATSAPP_PREVIOUS_RETAIN_UNTIL";
const SYSTEM_IMAGE_RETENTION_DAYS: i64 = 30;
const WHATSAPP_RETENTION_DAYS: i64 = 14;

static ENV_FILE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static ENV_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Default)]
pub struct InstalledImageState {
    pub tag: String,
    pub digest: String,
    pub image_id: String,
}

/// Build the fail-closed Docker storage contract consumed by the orchestrator.
/// Only immutable image IDs or exact repository digests are exposed.
pub async fn storage_metadata(
    state: &AppState,
    runtime_status: &str,
    active_job: bool,
) -> StorageMetadata {
    let generated_at = Some(Utc::now().to_rfc3339());
    match build_storage_metadata(state, runtime_status, active_job).await {
        Ok(mut metadata) => {
            metadata.generated_at = generated_at;
            metadata
        }
        Err(error) => {
            warn!(%error, "Updater storage metadata is unavailable; cleanup will fail closed");
            StorageMetadata {
                ready: false,
                generated_at,
                cleanup_block_reason: Some("storage_metadata_unavailable".to_string()),
                managed_repositories: Vec::new(),
                protected_image_refs: Vec::new(),
            }
        }
    }
}

async fn build_storage_metadata(
    state: &AppState,
    runtime_status: &str,
    active_job: bool,
) -> Result<StorageMetadata> {
    let selu_repository = managed_repository(&state.config.image_repo)?;
    let installed = get_installed_image_state(state).await?;
    let installed_ref =
        immutable_image_ref(&selu_repository, &installed.digest, &installed.image_id)?;

    let updater_service_image = resolve_updater_service_image(state).await?;
    let updater_repository = managed_repository(&updater_service_image)?;
    let updater_container_id = own_container_id().await?;
    let updater_identity =
        inspect_container_image_identity(state, &updater_container_id, &updater_repository)
            .await?
            .ok_or_else(|| anyhow!("Could not inspect the running updater image"))?;
    validate_exact_protected_ref(&updater_identity.immutable_ref, &updater_repository)?;

    let env_metadata = load_storage_env_metadata(
        &state.config.compose_env_file,
        &updater_identity.immutable_ref,
    )
    .await?;

    let mut managed_repositories = vec![selu_repository.clone(), updater_repository.clone()];
    let whatsapp_repository = if state.config.whatsapp_bridge_enabled {
        let repository = managed_repository(&state.config.whatsapp_bridge_image_repo)?;
        managed_repositories.push(repository.clone());
        Some(repository)
    } else {
        None
    };
    let mut seen_repositories = HashSet::new();
    managed_repositories.retain(|repository| seen_repositories.insert(repository.clone()));
    if managed_repositories.is_empty() {
        return Err(anyhow!("Managed repository allowlist is empty"));
    }

    let mut protected_image_refs = vec![
        ProtectedImageRef {
            image_ref: installed_ref.clone(),
            owner: "selu".to_string(),
            state: "current".to_string(),
            retain_until: None,
        },
        ProtectedImageRef {
            image_ref: updater_identity.immutable_ref.clone(),
            owner: "updater".to_string(),
            state: "current".to_string(),
            retain_until: None,
        },
    ];

    let rollback_ref = env_metadata.rollback_ref;
    if !rollback_ref.is_empty() && rollback_ref != installed_ref {
        validate_exact_protected_ref(&rollback_ref, &selu_repository)?;
        protected_image_refs.push(ProtectedImageRef {
            image_ref: rollback_ref,
            owner: "selu".to_string(),
            state: "previous".to_string(),
            retain_until: Some(env_metadata.rollback_retain_until),
        });
    }

    let updater_previous = env_metadata.updater_previous;
    if !updater_previous.is_empty() && updater_previous != updater_identity.immutable_ref {
        validate_exact_protected_ref(&updater_previous, &updater_repository)?;
        protected_image_refs.push(ProtectedImageRef {
            image_ref: updater_previous,
            owner: "updater".to_string(),
            state: "previous".to_string(),
            retain_until: Some(env_metadata.updater_previous_retain_until),
        });
    }

    if let Some(whatsapp_repository) = whatsapp_repository {
        let whatsapp_current = env_metadata.whatsapp_current;
        if !whatsapp_current.is_empty() {
            validate_exact_protected_ref(&whatsapp_current, &whatsapp_repository)?;
            protected_image_refs.push(ProtectedImageRef {
                image_ref: whatsapp_current.clone(),
                owner: "whatsapp".to_string(),
                state: "current".to_string(),
                retain_until: None,
            });
        }

        let whatsapp_previous = env_metadata.whatsapp_previous;
        if !whatsapp_previous.is_empty() && whatsapp_previous != whatsapp_current {
            validate_exact_protected_ref(&whatsapp_previous, &whatsapp_repository)?;
            protected_image_refs.push(ProtectedImageRef {
                image_ref: whatsapp_previous,
                owner: "whatsapp".to_string(),
                state: "previous".to_string(),
                retain_until: Some(env_metadata.whatsapp_previous_retain_until),
            });
        }
    }

    protected_image_refs.sort_by(|left, right| {
        (&left.owner, &left.state, &left.image_ref).cmp(&(
            &right.owner,
            &right.state,
            &right.image_ref,
        ))
    });

    let cleanup_block_reason = if active_job || runtime_status == "updating" {
        Some("update_active".to_string())
    } else if env_metadata.helper_active {
        Some("updater_restart_helper_active".to_string())
    } else {
        None
    };

    Ok(StorageMetadata {
        ready: true,
        generated_at: None,
        cleanup_block_reason,
        managed_repositories,
        protected_image_refs,
    })
}

#[derive(Debug, Default)]
struct StorageEnvMetadata {
    rollback_ref: String,
    rollback_retain_until: String,
    updater_previous: String,
    updater_previous_retain_until: String,
    whatsapp_current: String,
    whatsapp_previous: String,
    whatsapp_previous_retain_until: String,
    helper_active: bool,
}

async fn load_storage_env_metadata(
    path: &str,
    running_updater_ref: &str,
) -> Result<StorageEnvMetadata> {
    update_env_file(path, |document| {
        let persisted_updater_current = document.value(UPDATER_CURRENT_IMAGE_REF_ENV);
        if persisted_updater_current.is_empty() {
            document.set(UPDATER_CURRENT_IMAGE_REF_ENV, running_updater_ref);
        } else if persisted_updater_current != running_updater_ref {
            return Err(anyhow!(
                "Persisted updater image does not match the running updater"
            ));
        }

        let rollback_ref = {
            let image_ref = document.value(ROLLBACK_IMAGE_REF_ENV);
            if image_ref.is_empty() {
                document.value(ROLLBACK_IMAGE_ID_ENV)
            } else {
                image_ref
            }
        };
        let updater_previous = document.value(UPDATER_PREVIOUS_IMAGE_REF_ENV);
        let whatsapp_previous = document.value(WHATSAPP_PREVIOUS_IMAGE_REF_ENV);

        Ok(StorageEnvMetadata {
            rollback_retain_until: ensure_retention_deadline(
                document,
                ROLLBACK_IMAGE_RETAIN_UNTIL_ENV,
                &rollback_ref,
                SYSTEM_IMAGE_RETENTION_DAYS,
            ),
            updater_previous_retain_until: ensure_retention_deadline(
                document,
                UPDATER_PREVIOUS_RETAIN_UNTIL_ENV,
                &updater_previous,
                SYSTEM_IMAGE_RETENTION_DAYS,
            ),
            whatsapp_previous_retain_until: ensure_retention_deadline(
                document,
                WHATSAPP_PREVIOUS_RETAIN_UNTIL_ENV,
                &whatsapp_previous,
                WHATSAPP_RETENTION_DAYS,
            ),
            rollback_ref,
            updater_previous,
            whatsapp_current: document.value(WHATSAPP_CURRENT_IMAGE_REF_ENV),
            whatsapp_previous,
            helper_active: document
                .value(UPDATER_HELPER_ACTIVE_ENV)
                .eq_ignore_ascii_case("true"),
        })
    })
    .await
}

fn ensure_retention_deadline(
    document: &mut EnvDocument,
    key: &str,
    image_ref: &str,
    days: i64,
) -> String {
    if image_ref.is_empty() {
        return String::new();
    }
    let existing = document.value(key);
    if chrono::DateTime::parse_from_rfc3339(&existing).is_ok() {
        return existing;
    }
    let deadline = (Utc::now() + ChronoDuration::days(days)).to_rfc3339();
    document.set(key, &deadline);
    deadline
}

fn immutable_image_ref(repository: &str, digest: &str, image_id: &str) -> Result<String> {
    if !digest.trim().is_empty() {
        return Ok(format!(
            "{}@{}",
            repository,
            canonical_sha256_digest(digest)?
        ));
    }
    canonical_sha256_digest(image_id)
}

fn managed_repository(image_ref: &str) -> Result<String> {
    let repository = repository_from_image_ref(image_ref)
        .ok_or_else(|| anyhow!("Managed image repository must not be empty"))?;
    if repository == "sha256"
        || image_ref.trim().starts_with("sha256:")
        || repository.chars().any(char::is_whitespace)
        || repository.contains('@')
    {
        return Err(anyhow!("Managed image repository is invalid"));
    }
    Ok(repository)
}

fn validate_exact_protected_ref(image_ref: &str, expected_repository: &str) -> Result<()> {
    if canonical_sha256_digest(image_ref).is_ok() {
        return Ok(());
    }
    let (repository, digest) = image_ref
        .trim()
        .rsplit_once('@')
        .ok_or_else(|| anyhow!("Protected image reference is not immutable"))?;
    if managed_repository(repository)? != expected_repository {
        return Err(anyhow!(
            "Protected image reference does not match its managed repository"
        ));
    }
    canonical_sha256_digest(digest)?;
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ImageIdentity {
    image_id: String,
    immutable_ref: String,
}

#[derive(Debug, Clone, Default)]
struct RollbackRoot {
    display_tag: String,
    compose_tag: String,
    digest: String,
    version: String,
    build: String,
    image_id: String,
    immutable_ref: String,
}

impl RollbackRoot {
    fn is_available(&self) -> bool {
        !self.image_id.is_empty() && !self.compose_tag.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImmutableImagePlan {
    pull_ref: String,
    local_ref: String,
    compose_tag: String,
}

pub async fn run_apply(
    state: AppState,
    job_id: String,
    channel: String,
    target_tag: String,
    target_digest: String,
    target_version: String,
    target_build: String,
) -> Result<()> {
    let target_digest = canonical_sha256_digest(&target_digest)?;
    let previous = capture_rollback_root(&state).await?;

    if !previous.digest.is_empty() && previous.digest == target_digest {
        set_runtime(
            &state,
            None,
            "idle",
            "updates.progress.already_current",
            "You are already on the latest version.",
        )
        .await;
        set_runtime_versions(
            &state,
            &target_tag,
            &target_digest,
            &target_version,
            &target_build,
            &previous.display_tag,
            &previous.digest,
            &previous.version,
            &previous.build,
        )
        .await;
        return Ok(());
    }

    set_runtime(
        &state,
        Some(job_id.clone()),
        "updating",
        "updates.progress.preparing",
        "Preparing update",
    )
    .await;

    activate_image_env(
        &state,
        &target_tag,
        &target_digest,
        &target_version,
        &target_build,
    )
    .await?;

    set_runtime(
        &state,
        Some(job_id.clone()),
        "updating",
        "updates.progress.pulling",
        "Downloading image",
    )
    .await;
    if let Err(update_error) =
        run_compose_cmd(&state, &["pull", &state.config.compose_service]).await
    {
        return Err(
            restore_env_after_pre_activation_failure(&state, &previous, update_error).await,
        );
    }
    if let Err(update_error) = verify_local_digest(
        &state,
        &format!("{}:{}", state.config.image_repo, target_tag),
        &target_digest,
    )
    .await
    {
        return Err(
            restore_env_after_pre_activation_failure(&state, &previous, update_error).await,
        );
    }
    let target_rollback = match capture_target_rollback_root(
        &state,
        &target_tag,
        &target_digest,
        &target_version,
        &target_build,
    )
    .await
    {
        Ok(root) => root,
        Err(update_error) => {
            return Err(
                restore_env_after_pre_activation_failure(&state, &previous, update_error).await,
            );
        }
    };

    set_runtime(
        &state,
        Some(job_id.clone()),
        "updating",
        "updates.progress.restarting",
        "Restarting Selu",
    )
    .await;
    if let Err(update_error) =
        run_compose_cmd(&state, &["up", "-d", &state.config.compose_service]).await
    {
        return finish_failed_apply(&state, &previous, update_error, &target_rollback).await;
    }

    set_runtime(
        &state,
        Some(job_id),
        "updating",
        "updates.progress.health_check",
        "Running health checks",
    )
    .await;
    if let Err(update_error) = wait_for_health(&state).await {
        warn!("Health check failed, attempting rollback: {update_error}");
        return finish_failed_apply(&state, &previous, update_error, &target_rollback).await;
    }

    persist_rollback_root(&state, &previous).await?;
    set_runtime(
        &state,
        None,
        "idle",
        "updates.progress.apply_done",
        "Update completed",
    )
    .await;
    set_runtime_versions(
        &state,
        &target_tag,
        &target_digest,
        &target_version,
        &target_build,
        &previous.display_tag,
        &previous.digest,
        &previous.version,
        &previous.build,
    )
    .await;
    maybe_refresh_updater_sidecar(&state, &channel).await;
    Ok(())
}

async fn finish_failed_apply(
    state: &AppState,
    previous: &RollbackRoot,
    update_error: anyhow::Error,
    failed_target: &RollbackRoot,
) -> Result<()> {
    if !previous.is_available() {
        return Err(anyhow!(
            "Update failed ({update_error:#}) and no immutable rollback image was available"
        ));
    }

    if let Err(rollback_error) = restore_rollback_root(state, previous).await {
        return Err(anyhow!(
            "Update failed ({update_error:#}); automatic rollback also failed: {rollback_error:#}"
        ));
    }

    persist_rollback_root(state, failed_target).await?;
    set_runtime(
        state,
        None,
        "rollback_available",
        "updates.progress.rollback_ready",
        "Update failed. The previous version was recreated and passed its health check.",
    )
    .await;
    set_runtime_versions(
        state,
        &previous.display_tag,
        &previous.digest,
        &previous.version,
        &previous.build,
        &failed_target.display_tag,
        &failed_target.digest,
        &failed_target.version,
        &failed_target.build,
    )
    .await;
    Ok(())
}

pub async fn run_rollback(
    state: AppState,
    job_id: String,
    _channel: String,
    rollback_tag: String,
    rollback_digest: String,
    rollback_version: String,
    rollback_build: String,
) -> Result<()> {
    if rollback_tag.trim().is_empty() {
        return Err(anyhow!("Rollback tag is required"));
    }
    let rollback_digest = canonical_sha256_digest(&rollback_digest)?;
    let rollback_plan = immutable_image_plan(&state.config.image_repo, &rollback_digest)?;
    let previous = capture_rollback_root(&state).await?;

    set_runtime(
        &state,
        Some(job_id.clone()),
        "updating",
        "updates.progress.rollback_start",
        "Starting rollback",
    )
    .await;

    set_runtime(
        &state,
        Some(job_id.clone()),
        "updating",
        "updates.progress.pulling",
        "Downloading immutable rollback image",
    )
    .await;
    run_docker_cmd(&state, &["pull", &rollback_plan.pull_ref]).await?;
    verify_local_digest(&state, &rollback_plan.pull_ref, &rollback_digest).await?;

    let rollback_identity =
        inspect_image_identity(&state, &rollback_plan.pull_ref, &state.config.image_repo).await?;
    if rollback_identity.image_id.is_empty() {
        return Err(anyhow!("Pulled rollback image did not have an image ID"));
    }
    run_docker_cmd(
        &state,
        &[
            "image",
            "tag",
            &rollback_identity.image_id,
            &rollback_plan.local_ref,
        ],
    )
    .await?;

    activate_image_env(
        &state,
        &rollback_plan.compose_tag,
        &rollback_digest,
        &rollback_version,
        &rollback_build,
    )
    .await?;

    set_runtime(
        &state,
        Some(job_id),
        "updating",
        "updates.progress.rollback_restarting",
        "Restarting previous version",
    )
    .await;
    let rollback_result = async {
        recreate_service_without_pull(&state).await?;
        set_runtime(
            &state,
            None,
            "updating",
            "updates.progress.health_check",
            "Running health checks",
        )
        .await;
        wait_for_health(&state).await
    }
    .await;

    if let Err(rollback_error) = rollback_result {
        if previous.is_available() {
            if let Err(restore_error) = restore_rollback_root(&state, &previous).await {
                return Err(anyhow!(
                    "Manual rollback failed ({rollback_error:#}); restoring the current version also failed: {restore_error:#}"
                ));
            }
            return Err(anyhow!(
                "Manual rollback failed and the prior running version was restored: {rollback_error:#}"
            ));
        }

        return match activate_rollback_root_env(&state, &previous).await {
            Ok(()) => Err(anyhow!(
                "Manual rollback failed and no immutable prior image was available: {rollback_error:#}"
            )),
            Err(metadata_error) => Err(anyhow!(
                "Manual rollback failed ({rollback_error:#}); no immutable prior image was available and restoring metadata failed: {metadata_error:#}"
            )),
        };
    }

    persist_rollback_root(&state, &previous).await?;
    set_runtime(
        &state,
        None,
        "idle",
        "updates.progress.rollback_done",
        "Rollback completed",
    )
    .await;
    set_runtime_versions(
        &state,
        &rollback_tag,
        &rollback_digest,
        &rollback_version,
        &rollback_build,
        &previous.display_tag,
        &previous.digest,
        &previous.version,
        &previous.build,
    )
    .await;
    Ok(())
}

#[derive(Debug)]
struct EnvDocument {
    lines: Vec<String>,
    dirty: bool,
}

impl EnvDocument {
    fn parse(content: &str) -> Self {
        Self {
            lines: content.lines().map(str::to_string).collect(),
            dirty: false,
        }
    }

    fn value(&self, key: &str) -> String {
        let prefix = format!("{key}=");
        self.lines
            .iter()
            .find_map(|line| {
                line.trim_start()
                    .strip_prefix(&prefix)
                    .map(|value| value.trim().to_string())
            })
            .unwrap_or_default()
    }

    fn set(&mut self, key: &str, value: &str) {
        let prefix = format!("{key}=");
        let replacement = format!("{key}={value}");
        let mut found = false;
        for line in &mut self.lines {
            if line.trim_start().starts_with(&prefix) {
                found = true;
                if *line != replacement {
                    *line = replacement.clone();
                    self.dirty = true;
                }
            }
        }
        if !found {
            self.lines.push(replacement);
            self.dirty = true;
        }
    }

    fn render(&self) -> String {
        if self.lines.is_empty() {
            String::new()
        } else {
            format!("{}\n", self.lines.join("\n"))
        }
    }
}

async fn read_env_content(path: &Path) -> Result<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to read env file '{}'", path.display()))
        }
    }
}

async fn write_env_atomically(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("Env file path '{}' has no file name", path.display()))?;
    let original_permissions = tokio::fs::metadata(path)
        .await
        .ok()
        .map(|metadata| metadata.permissions());

    let (temp_path, mut temp_file) = loop {
        let sequence = ENV_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".{}.selu-updater-{}-{sequence}.tmp",
            file_name.to_string_lossy(),
            std::process::id()
        ));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .await
        {
            Ok(file) => break (temp_path, file),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to create temporary env file for '{}'",
                        path.display()
                    )
                });
            }
        }
    };

    let write_result: Result<()> = async {
        temp_file
            .write_all(content.as_bytes())
            .await
            .with_context(|| {
                format!(
                    "Failed to write temporary env file '{}'",
                    temp_path.display()
                )
            })?;
        if let Some(permissions) = original_permissions {
            temp_file
                .set_permissions(permissions)
                .await
                .with_context(|| {
                    format!("Failed to preserve permissions for '{}'", path.display())
                })?;
        }
        temp_file.sync_all().await.with_context(|| {
            format!("Failed to sync temporary env file for '{}'", path.display())
        })?;
        Ok(())
    }
    .await;

    if let Err(error) = write_result {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(error);
    }
    drop(temp_file);

    if let Err(error) = tokio::fs::rename(&temp_path, path).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(error)
            .with_context(|| format!("Failed to publish env file '{}'", path.display()));
    }

    #[cfg(unix)]
    tokio::fs::File::open(parent)
        .await
        .with_context(|| format!("Failed to open env directory '{}'", parent.display()))?
        .sync_all()
        .await
        .with_context(|| format!("Failed to sync env directory '{}'", parent.display()))?;

    Ok(())
}

async fn update_env_file<T>(
    path: &str,
    mutation: impl FnOnce(&mut EnvDocument) -> Result<T>,
) -> Result<T> {
    let _guard = ENV_FILE_LOCK.lock().await;
    let path = PathBuf::from(path);
    let existing = read_env_content(&path).await?;
    let mut document = EnvDocument::parse(&existing);
    let result = mutation(&mut document)?;
    if document.dirty {
        write_env_atomically(&path, &document.render()).await?;
    }
    Ok(result)
}

async fn upsert_env_vars(path: &str, values: &[(&str, &str)]) -> Result<()> {
    update_env_file(path, |document| {
        for (key, value) in values {
            document.set(key, value);
        }
        Ok(())
    })
    .await
}

pub async fn upsert_env_var(path: &str, key: &str, value: &str) -> Result<()> {
    upsert_env_vars(path, &[(key, value)]).await
}

async fn get_env_vars(path: &str, keys: &[&str]) -> Result<HashMap<String, String>> {
    let _guard = ENV_FILE_LOCK.lock().await;
    let document = EnvDocument::parse(&read_env_content(Path::new(path)).await?);
    Ok(keys
        .iter()
        .map(|key| ((*key).to_string(), document.value(key)))
        .collect())
}

pub async fn get_env_var(path: &str, key: &str) -> Result<String> {
    Ok(get_env_vars(path, &[key])
        .await?
        .remove(key)
        .unwrap_or_default())
}

async fn run_compose_cmd(state: &AppState, args: &[&str]) -> Result<()> {
    let mut cmd_args: Vec<String> = vec![
        "compose".to_string(),
        "-f".to_string(),
        state.config.compose_file.clone(),
        "--env-file".to_string(),
        state.config.compose_env_file.clone(),
    ];
    if !state.config.compose_project_dir.trim().is_empty() {
        cmd_args.push("--project-directory".to_string());
        cmd_args.push(state.config.compose_project_dir.clone());
    }
    cmd_args.extend(args.iter().map(|s| s.to_string()));

    let output = Command::new(&state.config.docker_bin)
        .args(&cmd_args)
        .output()
        .await
        .context("Failed to launch docker compose command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("docker compose failed: {}", stderr.trim()));
    }

    Ok(())
}

async fn run_compose_cmd_output(state: &AppState, args: &[&str]) -> Result<String> {
    let mut cmd_args: Vec<String> = vec![
        "compose".to_string(),
        "-f".to_string(),
        state.config.compose_file.clone(),
        "--env-file".to_string(),
        state.config.compose_env_file.clone(),
    ];
    if !state.config.compose_project_dir.trim().is_empty() {
        cmd_args.push("--project-directory".to_string());
        cmd_args.push(state.config.compose_project_dir.clone());
    }
    cmd_args.extend(args.iter().map(|s| s.to_string()));

    let output = Command::new(&state.config.docker_bin)
        .args(&cmd_args)
        .output()
        .await
        .context("Failed to launch docker compose command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("docker compose failed: {}", stderr.trim()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn run_docker_cmd(state: &AppState, args: &[&str]) -> Result<String> {
    let output = Command::new(&state.config.docker_bin)
        .args(args)
        .output()
        .await
        .context("Failed to launch docker command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("docker command failed: {}", stderr.trim()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub async fn get_installed_image_state(state: &AppState) -> Result<InstalledImageState> {
    let env = get_env_vars(
        &state.config.compose_env_file,
        &["SELU_IMAGE_TAG", "SELU_IMAGE_DIGEST"],
    )
    .await?;
    get_installed_image_state_from_env(
        state,
        env.get("SELU_IMAGE_TAG").cloned().unwrap_or_default(),
        env.get("SELU_IMAGE_DIGEST").cloned().unwrap_or_default(),
    )
    .await
}

async fn get_installed_image_state_from_env(
    state: &AppState,
    env_tag: String,
    env_digest: String,
) -> Result<InstalledImageState> {
    let mut installed = InstalledImageState {
        tag: env_tag,
        digest: env_digest,
        image_id: String::new(),
    };

    let container_id = run_compose_cmd_output(state, &["ps", "-q", &state.config.compose_service])
        .await?
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();

    if container_id.is_empty() {
        return Ok(installed);
    }

    let configured_image = run_docker_cmd(
        state,
        &["inspect", "--format", "{{.Config.Image}}", &container_id],
    )
    .await?
    .trim()
    .to_string();
    if let Some(tag) = extract_tag_from_image_ref(&configured_image, &state.config.image_repo) {
        installed.tag = tag;
    }

    installed.image_id =
        run_docker_cmd(state, &["inspect", "--format", "{{.Image}}", &container_id])
            .await?
            .trim()
            .to_string();
    if !installed.image_id.is_empty() {
        let repo_digests = image_repo_digests(state, &installed.image_id).await?;
        installed.digest =
            repo_digest_for_repository(&repo_digests, &state.config.image_repo).unwrap_or_default();
    }

    Ok(installed)
}

fn repository_from_image_ref(image_ref: &str) -> Option<String> {
    let without_digest = image_ref.trim().split('@').next()?.trim();
    if without_digest.is_empty() {
        return None;
    }

    let last_slash = without_digest.rfind('/');
    let last_colon = without_digest.rfind(':');
    let repository = match (last_slash, last_colon) {
        (_, Some(colon)) if last_slash.is_none_or(|slash| colon > slash) => {
            &without_digest[..colon]
        }
        _ => without_digest,
    }
    .trim();

    (!repository.is_empty()).then(|| repository.to_string())
}

fn repo_digest_for_repository(output: &str, expected_repo: &str) -> Option<String> {
    let expected_repo = repository_from_image_ref(expected_repo)?;
    output.lines().find_map(|line| {
        let (repo, digest) = line.trim().rsplit_once('@')?;
        if repo.trim() != expected_repo {
            return None;
        }
        canonical_sha256_digest(digest).ok()
    })
}

fn extract_tag_from_image_ref(image_ref: &str, image_repo: &str) -> Option<String> {
    let without_digest = image_ref.trim().split('@').next()?.trim();
    if repository_from_image_ref(without_digest)?.as_str()
        != repository_from_image_ref(image_repo)?.as_str()
    {
        return None;
    }

    let last_slash = without_digest.rfind('/');
    let colon = without_digest.rfind(':')?;
    if last_slash.is_some_and(|slash| colon <= slash) {
        return None;
    }
    let tag = without_digest[colon + 1..].trim();
    (!tag.is_empty()).then(|| tag.to_string())
}

fn canonical_sha256_digest(digest: &str) -> Result<String> {
    let digest = digest.trim().to_ascii_lowercase();
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(anyhow!("Image digest must use sha256:<64 hex characters>"));
    };
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!("Image digest must use sha256:<64 hex characters>"));
    }
    Ok(digest)
}

fn immutable_image_plan(repo: &str, digest: &str) -> Result<ImmutableImagePlan> {
    let repository = repository_from_image_ref(repo)
        .ok_or_else(|| anyhow!("Image repository must not be empty"))?;
    let digest = canonical_sha256_digest(digest)?;
    let digest_hex = digest.trim_start_matches("sha256:");
    let compose_tag = format!("selu-rollback-{digest_hex}");
    Ok(ImmutableImagePlan {
        pull_ref: format!("{repository}@{digest}"),
        local_ref: format!("{repository}:{compose_tag}"),
        compose_tag,
    })
}

async fn image_repo_digests(state: &AppState, image_ref: &str) -> Result<String> {
    run_docker_cmd(
        state,
        &[
            "image",
            "inspect",
            "--format",
            "{{range .RepoDigests}}{{println .}}{{end}}",
            image_ref,
        ],
    )
    .await
}

async fn inspect_image_identity(
    state: &AppState,
    image_ref: &str,
    expected_repo: &str,
) -> Result<ImageIdentity> {
    let image_id = run_docker_cmd(
        state,
        &["image", "inspect", "--format", "{{.Id}}", image_ref],
    )
    .await?
    .trim()
    .to_string();
    if image_id.is_empty() {
        return Err(anyhow!(
            "Docker image '{}' did not have an image ID",
            image_ref
        ));
    }

    let repo_digests = image_repo_digests(state, &image_id).await?;
    let immutable_ref = match repo_digest_for_repository(&repo_digests, expected_repo) {
        Some(digest) => format!(
            "{}@{}",
            repository_from_image_ref(expected_repo)
                .ok_or_else(|| anyhow!("Image repository must not be empty"))?,
            digest
        ),
        None => image_id.clone(),
    };

    Ok(ImageIdentity {
        image_id,
        immutable_ref,
    })
}

async fn inspect_container_image_identity(
    state: &AppState,
    container: &str,
    expected_repo: &str,
) -> Result<Option<ImageIdentity>> {
    let image_id =
        match run_docker_cmd(state, &["inspect", "--format", "{{.Image}}", container]).await {
            Ok(value) => value.trim().to_string(),
            Err(error)
                if error.to_string().contains("No such object")
                    || error.to_string().contains("No such container") =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
    if image_id.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        inspect_image_identity(state, &image_id, expected_repo).await?,
    ))
}

async fn verify_local_digest(
    state: &AppState,
    image_ref: &str,
    expected_digest: &str,
) -> Result<()> {
    let expected_digest = canonical_sha256_digest(expected_digest)?;
    let out = image_repo_digests(state, image_ref).await?;
    let actual_digest = repo_digest_for_repository(&out, &state.config.image_repo);
    if actual_digest.as_deref() == Some(expected_digest.as_str()) {
        return Ok(());
    }

    Err(anyhow!(
        "Pulled image did not expose expected digest {} for exact repository {}",
        expected_digest,
        state.config.image_repo
    ))
}

async fn capture_rollback_root(state: &AppState) -> Result<RollbackRoot> {
    let env = get_env_vars(
        &state.config.compose_env_file,
        &[
            "SELU_IMAGE_TAG",
            "SELU_IMAGE_DIGEST",
            "SELU_IMAGE_VERSION",
            "SELU_IMAGE_BUILD",
        ],
    )
    .await?;
    let env_tag = env.get("SELU_IMAGE_TAG").cloned().unwrap_or_default();
    let env_digest = env.get("SELU_IMAGE_DIGEST").cloned().unwrap_or_default();
    let version = env.get("SELU_IMAGE_VERSION").cloned().unwrap_or_default();
    let build = env.get("SELU_IMAGE_BUILD").cloned().unwrap_or_default();
    let installed = get_installed_image_state_from_env(state, env_tag.clone(), env_digest).await?;

    let mut root = RollbackRoot {
        display_tag: if installed.tag.is_empty() {
            env_tag
        } else {
            installed.tag
        },
        digest: installed.digest,
        version,
        build,
        image_id: installed.image_id,
        ..RollbackRoot::default()
    };

    if !root.image_id.is_empty() {
        let identity_key = if root.digest.is_empty() {
            root.image_id.as_str()
        } else {
            root.digest.as_str()
        };
        let repository = repository_from_image_ref(&state.config.image_repo)
            .ok_or_else(|| anyhow!("Image repository must not be empty"))?;
        let suffix = identity_key
            .trim_start_matches("sha256:")
            .to_ascii_lowercase();
        if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(anyhow!("Running Selu image had an invalid immutable ID"));
        }
        root.compose_tag = format!("selu-rollback-{suffix}");
        let local_ref = format!("{repository}:{}", root.compose_tag);
        run_docker_cmd(state, &["image", "tag", &root.image_id, &local_ref]).await?;
        root.immutable_ref = if root.digest.is_empty() {
            root.image_id.clone()
        } else {
            format!("{repository}@{}", root.digest)
        };
    }

    Ok(root)
}

async fn capture_target_rollback_root(
    state: &AppState,
    tag: &str,
    digest: &str,
    version: &str,
    build: &str,
) -> Result<RollbackRoot> {
    let plan = immutable_image_plan(&state.config.image_repo, digest)?;
    let repository = repository_from_image_ref(&state.config.image_repo)
        .ok_or_else(|| anyhow!("Image repository must not be empty"))?;
    let mutable_ref = format!("{repository}:{tag}");
    let identity = inspect_image_identity(state, &mutable_ref, &repository).await?;
    run_docker_cmd(
        state,
        &["image", "tag", &identity.image_id, &plan.local_ref],
    )
    .await?;

    Ok(RollbackRoot {
        display_tag: tag.to_string(),
        compose_tag: plan.compose_tag,
        digest: canonical_sha256_digest(digest)?,
        version: version.to_string(),
        build: build.to_string(),
        image_id: identity.image_id,
        immutable_ref: identity.immutable_ref,
    })
}

async fn persist_rollback_root(state: &AppState, root: &RollbackRoot) -> Result<()> {
    update_env_file(&state.config.compose_env_file, |document| {
        let prior_ref = document.value(ROLLBACK_IMAGE_REF_ENV);
        let prior_deadline = document.value(ROLLBACK_IMAGE_RETAIN_UNTIL_ENV);
        let retain_until = if !root.immutable_ref.is_empty()
            && root.immutable_ref == prior_ref
            && chrono::DateTime::parse_from_rfc3339(&prior_deadline).is_ok()
        {
            prior_deadline
        } else if root.immutable_ref.is_empty() {
            String::new()
        } else {
            (Utc::now() + ChronoDuration::days(SYSTEM_IMAGE_RETENTION_DAYS)).to_rfc3339()
        };

        document.set(ROLLBACK_IMAGE_REF_ENV, &root.immutable_ref);
        document.set(ROLLBACK_IMAGE_ID_ENV, &root.image_id);
        document.set(ROLLBACK_IMAGE_TAG_ENV, &root.compose_tag);
        document.set(ROLLBACK_IMAGE_DIGEST_ENV, &root.digest);
        document.set(ROLLBACK_IMAGE_RETAIN_UNTIL_ENV, &retain_until);
        Ok(())
    })
    .await
}

async fn activate_image_env(
    state: &AppState,
    tag: &str,
    digest: &str,
    version: &str,
    build: &str,
) -> Result<()> {
    upsert_env_vars(
        &state.config.compose_env_file,
        &[
            ("SELU_IMAGE_TAG", tag),
            ("SELU_IMAGE_DIGEST", digest),
            ("SELU_IMAGE_VERSION", version),
            ("SELU_IMAGE_BUILD", build),
        ],
    )
    .await
}

async fn activate_rollback_root_env(state: &AppState, root: &RollbackRoot) -> Result<()> {
    let tag = if root.compose_tag.is_empty() {
        &root.display_tag
    } else {
        &root.compose_tag
    };
    activate_image_env(state, tag, &root.digest, &root.version, &root.build).await
}

async fn restore_env_after_pre_activation_failure(
    state: &AppState,
    previous: &RollbackRoot,
    update_error: anyhow::Error,
) -> anyhow::Error {
    match activate_rollback_root_env(state, previous).await {
        Ok(()) => update_error,
        Err(restore_error) => anyhow!(
            "Update failed ({update_error:#}); restoring prior image metadata also failed: {restore_error:#}"
        ),
    }
}

async fn recreate_service_without_pull(state: &AppState) -> Result<()> {
    run_compose_cmd(
        state,
        &[
            "up",
            "-d",
            "--no-deps",
            "--force-recreate",
            "--pull",
            "never",
            &state.config.compose_service,
        ],
    )
    .await
}

async fn restore_rollback_root(state: &AppState, root: &RollbackRoot) -> Result<()> {
    if !root.is_available() {
        return Err(anyhow!("No immutable rollback image was captured"));
    }
    activate_rollback_root_env(state, root).await?;
    recreate_service_without_pull(state)
        .await
        .context("Failed to recreate the captured rollback image")?;
    wait_for_health(state)
        .await
        .context("Restored rollback image failed its health check")?;
    Ok(())
}

async fn wait_for_health(state: &AppState) -> Result<()> {
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(state.config.health_timeout_secs);
    let interval = std::time::Duration::from_secs(state.config.health_interval_secs);

    loop {
        if start.elapsed() > timeout {
            return Err(anyhow!("Health check timed out"));
        }

        if let Ok(resp) = reqwest::Client::new()
            .get(&state.config.health_url)
            .send()
            .await
        {
            if resp.status().is_success() {
                return Ok(());
            }
        }

        tokio::time::sleep(interval).await;
    }
}

pub async fn set_runtime(
    state: &AppState,
    active_job_id: Option<String>,
    status: &str,
    progress_key: &str,
    message: &str,
) {
    let mut runtime = state.runtime.lock().await;
    runtime.active_job_id = active_job_id;
    runtime.status = status.to_string();
    runtime.progress_key = progress_key.to_string();
    runtime.message = message.to_string();
}

pub async fn set_runtime_versions(
    state: &AppState,
    installed_tag: &str,
    installed_digest: &str,
    installed_version: &str,
    installed_build: &str,
    previous_tag: &str,
    previous_digest: &str,
    previous_version: &str,
    previous_build: &str,
) {
    let mut runtime = state.runtime.lock().await;
    runtime.installed_tag = installed_tag.to_string();
    runtime.installed_digest = installed_digest.to_string();
    runtime.installed_version = installed_version.to_string();
    runtime.installed_build = installed_build.to_string();
    runtime.previous_tag = previous_tag.to_string();
    runtime.previous_digest = previous_digest.to_string();
    runtime.previous_version = previous_version.to_string();
    runtime.previous_build = previous_build.to_string();
}

async fn maybe_refresh_updater_sidecar(state: &AppState, channel: &str) {
    let desired_channel = channel.trim();
    if desired_channel.is_empty() {
        return;
    }

    if state.config.self_update_on_apply {
        if let Err(e) = refresh_updater_sidecar(state, desired_channel).await {
            warn!("Failed to refresh updater sidecar: {e:#}");
        }
        return;
    }

    let current_tag = get_env_var(
        &state.config.compose_env_file,
        &state.config.updater_tag_env_key,
    )
    .await
    .unwrap_or_default();
    let pending_tag = get_env_var(
        &state.config.compose_env_file,
        &state.config.updater_pending_tag_env_key,
    )
    .await
    .unwrap_or_default();

    let mut effective_tag = current_tag.clone();
    if !pending_tag.trim().is_empty() && pending_tag.trim() != current_tag.trim() {
        if let Err(e) = refresh_updater_sidecar(state, pending_tag.trim()).await {
            warn!("Failed to apply deferred updater refresh: {e:#}");
            return;
        }
        effective_tag = pending_tag.trim().to_string();
    }

    if desired_channel != effective_tag {
        if let Err(e) = upsert_env_var(
            &state.config.compose_env_file,
            &state.config.updater_pending_tag_env_key,
            desired_channel,
        )
        .await
        {
            warn!("Failed to set deferred updater target channel: {e}");
        }
    } else if !pending_tag.trim().is_empty() {
        let _ = upsert_env_var(
            &state.config.compose_env_file,
            &state.config.updater_pending_tag_env_key,
            "",
        )
        .await;
    }
}

async fn refresh_updater_sidecar(state: &AppState, desired_tag: &str) -> Result<()> {
    let prior = get_env_vars(
        &state.config.compose_env_file,
        &[
            &state.config.updater_tag_env_key,
            UPDATER_PREVIOUS_IMAGE_REF_ENV,
            UPDATER_PREVIOUS_RETAIN_UNTIL_ENV,
        ],
    )
    .await?;
    let previous_tag = prior
        .get(&state.config.updater_tag_env_key)
        .cloned()
        .unwrap_or_default();
    let previous_protected = prior
        .get(UPDATER_PREVIOUS_IMAGE_REF_ENV)
        .cloned()
        .unwrap_or_default();
    let previous_retain_until = prior
        .get(UPDATER_PREVIOUS_RETAIN_UNTIL_ENV)
        .cloned()
        .unwrap_or_default();
    let current_service_image = resolve_updater_service_image(state).await?;
    let current_repo = repository_from_image_ref(&current_service_image)
        .ok_or_else(|| anyhow!("Could not resolve updater image repository"))?;
    let own_container_id = own_container_id().await?;
    let running_identity =
        inspect_container_image_identity(state, &own_container_id, &current_repo)
            .await?
            .ok_or_else(|| anyhow!("Could not inspect the running updater image"))?;

    upsert_env_var(
        &state.config.compose_env_file,
        &state.config.updater_tag_env_key,
        desired_tag,
    )
    .await?;
    if let Err(error) = run_compose_cmd(state, &["pull", &state.config.updater_service]).await {
        let _ = upsert_env_var(
            &state.config.compose_env_file,
            &state.config.updater_tag_env_key,
            &previous_tag,
        )
        .await;
        return Err(error);
    }

    let desired_identity = match async {
        let desired_image = resolve_updater_service_image(state).await?;
        let desired_repo = repository_from_image_ref(&desired_image)
            .ok_or_else(|| anyhow!("Could not resolve pulled updater image repository"))?;
        inspect_image_identity(state, &desired_image, &desired_repo).await
    }
    .await
    {
        Ok(identity) => identity,
        Err(error) => {
            let _ = upsert_env_var(
                &state.config.compose_env_file,
                &state.config.updater_tag_env_key,
                &previous_tag,
            )
            .await;
            return Err(error);
        }
    };
    let retained_previous = if desired_identity.immutable_ref != running_identity.immutable_ref {
        running_identity.immutable_ref.clone()
    } else {
        previous_protected.clone()
    };
    persist_updater_restart_state(
        state,
        desired_tag,
        &desired_identity.immutable_ref,
        &retained_previous,
        None,
        true,
    )
    .await?;

    if let Err(error) = restart_self_via_helper(state).await {
        let _ = persist_updater_restart_state(
            state,
            &previous_tag,
            &running_identity.immutable_ref,
            &previous_protected,
            Some(&previous_retain_until),
            false,
        )
        .await;
        return Err(error);
    }
    Ok(())
}

async fn persist_updater_restart_state(
    state: &AppState,
    tag: &str,
    current_ref: &str,
    previous_ref: &str,
    retain_until_override: Option<&str>,
    helper_active: bool,
) -> Result<()> {
    update_env_file(&state.config.compose_env_file, |document| {
        let retain_until = match retain_until_override {
            Some(value) => value.to_string(),
            None => {
                let prior_previous = document.value(UPDATER_PREVIOUS_IMAGE_REF_ENV);
                let prior_deadline = document.value(UPDATER_PREVIOUS_RETAIN_UNTIL_ENV);
                if !previous_ref.is_empty()
                    && previous_ref == prior_previous
                    && chrono::DateTime::parse_from_rfc3339(&prior_deadline).is_ok()
                {
                    prior_deadline
                } else if previous_ref.is_empty() {
                    String::new()
                } else {
                    (Utc::now() + ChronoDuration::days(SYSTEM_IMAGE_RETENTION_DAYS)).to_rfc3339()
                }
            }
        };

        document.set(&state.config.updater_tag_env_key, tag);
        document.set(UPDATER_CURRENT_IMAGE_REF_ENV, current_ref);
        document.set(UPDATER_PREVIOUS_IMAGE_REF_ENV, previous_ref);
        document.set(UPDATER_PREVIOUS_RETAIN_UNTIL_ENV, &retain_until);
        document.set(
            UPDATER_HELPER_ACTIVE_ENV,
            if helper_active { "true" } else { "false" },
        );
        Ok(())
    })
    .await
}

pub async fn ensure_whatsapp_bridge(
    state: &AppState,
    channel: &str,
    inbound_url: &str,
    inbound_token: &str,
    outbound_auth: &str,
) -> Result<()> {
    if !state.config.whatsapp_bridge_enabled {
        return Ok(());
    }

    let desired_channel = channel.trim();
    if desired_channel.is_empty() {
        return Err(anyhow!("Channel is required"));
    }
    if inbound_url.trim().is_empty() || inbound_token.trim().is_empty() {
        return Err(anyhow!("Inbound URL and token are required"));
    }

    let image = resolve_whatsapp_bridge_image_ref(
        &state.config.whatsapp_bridge_image_repo,
        desired_channel,
    )?;
    let container = state.config.whatsapp_bridge_container_name.trim();
    let volume = state.config.whatsapp_bridge_data_volume.trim();
    if container.is_empty() || volume.is_empty() {
        return Err(anyhow!(
            "WhatsApp bridge container name and data volume must be configured"
        ));
    }

    let whatsapp_repo = repository_from_image_ref(&state.config.whatsapp_bridge_image_repo)
        .ok_or_else(|| anyhow!("WhatsApp bridge image repository must not be empty"))?;
    let whatsapp_metadata = get_env_vars(
        &state.config.compose_env_file,
        &[
            WHATSAPP_PREVIOUS_IMAGE_REF_ENV,
            WHATSAPP_PREVIOUS_RETAIN_UNTIL_ENV,
        ],
    )
    .await?;
    let previous_retained_ref = whatsapp_metadata
        .get(WHATSAPP_PREVIOUS_IMAGE_REF_ENV)
        .cloned()
        .unwrap_or_default();
    let previous_retain_until = whatsapp_metadata
        .get(WHATSAPP_PREVIOUS_RETAIN_UNTIL_ENV)
        .cloned()
        .unwrap_or_default();
    let running_identity =
        inspect_container_image_identity(state, container, &whatsapp_repo).await?;
    if let Some(identity) = &running_identity {
        persist_whatsapp_image_refs(
            state,
            &identity.immutable_ref,
            &previous_retained_ref,
            &previous_retain_until,
        )
        .await?;
    }

    let _ = run_docker_cmd(state, &["rm", "-f", container]).await;

    if should_pull_whatsapp_bridge_image(&image) {
        run_docker_cmd(state, &["pull", &image]).await?;
    }

    let mut args: Vec<String> = vec![
        "run".to_string(),
        "-d".to_string(),
        "--restart".to_string(),
        "unless-stopped".to_string(),
        "--name".to_string(),
        container.to_string(),
        "-v".to_string(),
        format!("{}:/data", volume),
        "-e".to_string(),
        format!("SELU_INBOUND_URL={}", inbound_url.trim()),
        "-e".to_string(),
        format!("SELU_INBOUND_TOKEN={}", inbound_token.trim()),
    ];

    if !outbound_auth.trim().is_empty() {
        args.push("-e".to_string());
        args.push(format!("BRIDGE_EXPECT_AUTH={}", outbound_auth.trim()));
    }

    let configured_network = state.config.whatsapp_bridge_network.trim();
    let network = if configured_network.is_empty() {
        detect_compose_network(state).await.unwrap_or_default()
    } else {
        configured_network.to_string()
    };

    if network.is_empty() {
        args.push("-p".to_string());
        args.push(format!(
            "127.0.0.1:{}:3200",
            state.config.whatsapp_bridge_port
        ));
        args.push("--add-host".to_string());
        args.push("host.docker.internal:host-gateway".to_string());
    } else {
        args.push("--network".to_string());
        args.push(network);
        args.push("--network-alias".to_string());
        args.push(container.to_string());
    }

    args.push(image);

    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_docker_cmd(state, &args_ref).await?;

    let current_identity = inspect_container_image_identity(state, container, &whatsapp_repo)
        .await?
        .ok_or_else(|| anyhow!("Could not inspect the running WhatsApp bridge image"))?;
    let (retained_previous, retain_until) = match running_identity {
        Some(previous) if previous.immutable_ref != current_identity.immutable_ref => {
            (previous.immutable_ref, whatsapp_retention_deadline())
        }
        _ => (previous_retained_ref, previous_retain_until),
    };
    persist_whatsapp_image_refs(
        state,
        &current_identity.immutable_ref,
        &retained_previous,
        &retain_until,
    )
    .await?;

    Ok(())
}

fn whatsapp_retention_deadline() -> String {
    (Utc::now() + ChronoDuration::days(WHATSAPP_RETENTION_DAYS)).to_rfc3339()
}

async fn persist_whatsapp_image_refs(
    state: &AppState,
    current_ref: &str,
    previous_ref: &str,
    retain_until: &str,
) -> Result<()> {
    upsert_env_vars(
        &state.config.compose_env_file,
        &[
            (WHATSAPP_CURRENT_IMAGE_REF_ENV, current_ref),
            (WHATSAPP_PREVIOUS_IMAGE_REF_ENV, previous_ref),
            (WHATSAPP_PREVIOUS_RETAIN_UNTIL_ENV, retain_until),
        ],
    )
    .await
}

fn resolve_whatsapp_bridge_image_ref(repo: &str, channel: &str) -> Result<String> {
    let repo = repo.trim();
    if repo.is_empty() {
        return Err(anyhow!(
            "UPDATER__WHATSAPP_BRIDGE_IMAGE_REPO must not be empty"
        ));
    }

    if !channel
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(anyhow!(
            "Invalid WhatsApp bridge channel '{}'. Use only letters, numbers, '.', '_' or '-'.",
            channel
        ));
    }

    // If repo is already pinned by digest or has an explicit tag, use it as-is.
    if repo.contains('@') || image_repo_has_tag(repo) {
        return Ok(repo.to_string());
    }

    Ok(format!("{repo}:{channel}"))
}

fn image_repo_has_tag(repo: &str) -> bool {
    let last_slash = repo.rfind('/').unwrap_or(0);
    match repo.rfind(':') {
        Some(colon) => colon > last_slash,
        None => false,
    }
}

fn should_pull_whatsapp_bridge_image(image: &str) -> bool {
    let image = image.trim();
    if image.is_empty() {
        return false;
    }

    if image.contains('@') {
        return true;
    }

    let first_segment = image.split('/').next().unwrap_or_default();
    let has_path = image.contains('/');
    let looks_like_registry = first_segment.contains('.')
        || first_segment.eq_ignore_ascii_case("localhost")
        || (has_path && first_segment.contains(':'));

    looks_like_registry
}

pub async fn stop_whatsapp_bridge(state: &AppState, channel: &str) -> Result<()> {
    if !state.config.whatsapp_bridge_enabled {
        return Ok(());
    }

    let container = state.config.whatsapp_bridge_container_name.trim();
    let volume = state.config.whatsapp_bridge_data_volume.trim();
    if container.is_empty() {
        return Err(anyhow!("WhatsApp bridge container name must be configured"));
    }
    if volume.is_empty() {
        return Err(anyhow!("WhatsApp bridge data volume must be configured"));
    }

    match run_docker_cmd(state, &["rm", "-f", container]).await {
        Ok(_) => clear_whatsapp_bridge_auth(state, volume, channel).await,
        Err(e) if e.to_string().contains("No such container") => {
            clear_whatsapp_bridge_auth(state, volume, channel).await
        }
        Err(e) => Err(e),
    }
}

async fn clear_whatsapp_bridge_auth(state: &AppState, volume: &str, channel: &str) -> Result<()> {
    if is_bind_mount_source(volume) {
        clear_whatsapp_bridge_bind_mount(state, volume, channel).await?;
        return Ok(());
    }

    match run_docker_cmd(state, &["volume", "rm", "-f", volume]).await {
        Ok(_) => {}
        Err(e) if e.to_string().contains("No such volume") => {}
        Err(e) => return Err(e).context("remove WhatsApp bridge data volume"),
    }

    run_docker_cmd(state, &["volume", "create", volume])
        .await
        .context("recreate WhatsApp bridge data volume")?;

    Ok(())
}

async fn clear_whatsapp_bridge_bind_mount(
    state: &AppState,
    mount_path: &str,
    channel: &str,
) -> Result<()> {
    let channel = if channel.trim().is_empty() {
        "stable"
    } else {
        channel.trim()
    };
    let image =
        resolve_whatsapp_bridge_image_ref(&state.config.whatsapp_bridge_image_repo, channel)?;

    let helper_name = format!(
        "selu-whatsapp-auth-cleanup-{}",
        uuid::Uuid::new_v4().simple()
    );

    let _ = run_docker_cmd(state, &["rm", "-f", &helper_name]).await;

    let result = run_docker_cmd(
        state,
        &[
            "run",
            "--rm",
            "--name",
            &helper_name,
            "--entrypoint",
            "sh",
            "-v",
            &format!("{mount_path}:/data"),
            &image,
            "-c",
            "rm -rf /data/auth && mkdir -p /data/auth",
        ],
    )
    .await;

    let _ = run_docker_cmd(state, &["rm", "-f", &helper_name]).await;

    result
        .map(|_| ())
        .context("clear WhatsApp bridge auth bind mount")
}

fn is_bind_mount_source(source: &str) -> bool {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return false;
    }

    Path::new(trimmed).is_absolute()
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
        || trimmed.contains(std::path::MAIN_SEPARATOR)
}

pub async fn whatsapp_bridge_status(state: &AppState) -> Result<WhatsappBridgeStatusResponse> {
    if !state.config.whatsapp_bridge_enabled {
        return Ok(WhatsappBridgeStatusResponse {
            running: false,
            connection_state: None,
            requires_qr: false,
            qr_data_url: None,
            jid: None,
            last_error: None,
            message: Some("WhatsApp bridge is disabled".to_string()),
        });
    }

    let container = state.config.whatsapp_bridge_container_name.trim();
    if container.is_empty() {
        return Err(anyhow!("WhatsApp bridge container name must be configured"));
    }

    let running_out = run_docker_cmd(
        state,
        &["inspect", "--format", "{{.State.Running}}", container],
    )
    .await;
    let is_running = match running_out {
        Ok(v) => v.trim() == "true",
        Err(e) if e.to_string().contains("No such object") => false,
        Err(e) if e.to_string().contains("No such container") => false,
        Err(e) => return Err(e),
    };

    if !is_running {
        return Ok(WhatsappBridgeStatusResponse {
            running: false,
            connection_state: None,
            requires_qr: false,
            qr_data_url: None,
            jid: None,
            last_error: None,
            message: Some("WhatsApp bridge is not running".to_string()),
        });
    }

    for base in bridge_base_urls(state) {
        if let Ok(session) = fetch_bridge_session(&base).await {
            let mut qr_data_url = None;
            if session.requires_qr && session.qr_available {
                qr_data_url = fetch_bridge_qr(&base).await.ok();
            }

            return Ok(WhatsappBridgeStatusResponse {
                running: true,
                connection_state: Some(session.connection_state),
                requires_qr: session.requires_qr,
                qr_data_url,
                jid: session.jid,
                last_error: session.last_error,
                message: Some("WhatsApp bridge is running".to_string()),
            });
        }
    }

    Ok(WhatsappBridgeStatusResponse {
        running: true,
        connection_state: None,
        requires_qr: false,
        qr_data_url: None,
        jid: None,
        last_error: None,
        message: Some("WhatsApp bridge is running, but session status is unavailable".to_string()),
    })
}

pub async fn whatsapp_bridge_chats(
    state: &AppState,
    query: &str,
) -> Result<WhatsappBridgeChatsResponse> {
    if !state.config.whatsapp_bridge_enabled {
        return Ok(WhatsappBridgeChatsResponse {
            running: false,
            connection_state: None,
            chats: Vec::new(),
            message: Some("WhatsApp bridge is disabled".to_string()),
        });
    }

    let container = state.config.whatsapp_bridge_container_name.trim();
    if container.is_empty() {
        return Err(anyhow!("WhatsApp bridge container name must be configured"));
    }

    let running_out = run_docker_cmd(
        state,
        &["inspect", "--format", "{{.State.Running}}", container],
    )
    .await;
    let is_running = match running_out {
        Ok(v) => v.trim() == "true",
        Err(e) if e.to_string().contains("No such object") => false,
        Err(e) if e.to_string().contains("No such container") => false,
        Err(e) => return Err(e),
    };
    if !is_running {
        return Ok(WhatsappBridgeChatsResponse {
            running: false,
            connection_state: None,
            chats: Vec::new(),
            message: Some("WhatsApp bridge is not running".to_string()),
        });
    }

    for base in bridge_base_urls(state) {
        if let Ok(resp) = fetch_bridge_chats(&base, query).await {
            return Ok(WhatsappBridgeChatsResponse {
                running: true,
                connection_state: Some(resp.connection_state),
                chats: resp.chats,
                message: Some("WhatsApp chats loaded".to_string()),
            });
        }
    }

    Ok(WhatsappBridgeChatsResponse {
        running: true,
        connection_state: None,
        chats: Vec::new(),
        message: Some("WhatsApp bridge is running, but chats are unavailable".to_string()),
    })
}

#[derive(Debug, Deserialize)]
struct BridgeSessionStatus {
    connection_state: String,
    requires_qr: bool,
    qr_available: bool,
    jid: Option<String>,
    last_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BridgeQrResponse {
    qr_data_url: String,
}

#[derive(Debug, Deserialize)]
struct BridgeChatsResponse {
    chats: Vec<WhatsappBridgeChat>,
    connection_state: String,
}

async fn fetch_bridge_session(base_url: &str) -> Result<BridgeSessionStatus> {
    reqwest::Client::new()
        .get(format!("{}/session/status", base_url))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .with_context(|| format!("Failed to query bridge session status at {}", base_url))?
        .error_for_status()
        .context("Bridge session status returned non-success")?
        .json::<BridgeSessionStatus>()
        .await
        .context("Invalid bridge session status payload")
}

async fn fetch_bridge_qr(base_url: &str) -> Result<String> {
    Ok(reqwest::Client::new()
        .get(format!("{}/session/qr", base_url))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .with_context(|| format!("Failed to query bridge QR at {}", base_url))?
        .error_for_status()
        .context("Bridge QR endpoint returned non-success")?
        .json::<BridgeQrResponse>()
        .await
        .context("Invalid bridge QR payload")?
        .qr_data_url)
}

async fn fetch_bridge_chats(base_url: &str, query: &str) -> Result<BridgeChatsResponse> {
    reqwest::Client::new()
        .get(format!("{}/chats", base_url))
        .query(&[("q", query)])
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .with_context(|| format!("Failed to query bridge chats at {}", base_url))?
        .error_for_status()
        .context("Bridge chats endpoint returned non-success")?
        .json::<BridgeChatsResponse>()
        .await
        .context("Invalid bridge chats payload")
}

fn bridge_base_urls(state: &AppState) -> [String; 2] {
    let container = state.config.whatsapp_bridge_container_name.trim();
    [
        format!("http://{}:3200", container),
        format!("http://127.0.0.1:{}", state.config.whatsapp_bridge_port),
    ]
}

async fn detect_compose_network(state: &AppState) -> Option<String> {
    let self_container_id = tokio::fs::read_to_string("/etc/hostname")
        .await
        .ok()?
        .trim()
        .to_string();
    if self_container_id.is_empty() {
        return None;
    }

    let out = run_docker_cmd(
        state,
        &[
            "inspect",
            "--format",
            "{{range $k,$v := .NetworkSettings.Networks}}{{println $k}}{{end}}",
            &self_container_id,
        ],
    )
    .await
    .ok()?;

    out.lines()
        .map(str::trim)
        .find(|n| !n.is_empty() && *n != "bridge" && *n != "host" && *n != "none")
        .map(|s| s.to_string())
}

/// Restart the updater by spawning a detached helper container.
///
/// Running `docker compose up -d selu-updater` directly would kill the CLI
/// process mid-execution (exit 137) because it recreates the container we are
/// running in.  Instead we launch a short-lived helper container that inherits
/// our volume mounts (compose file, env file, docker socket) via
/// `--volumes-from` and performs the restart from *outside* this container.
async fn own_container_id() -> Result<String> {
    let container_id = tokio::fs::read_to_string("/etc/hostname")
        .await
        .context("Failed to read own container ID from /etc/hostname")?
        .trim()
        .to_string();
    if container_id.is_empty() {
        return Err(anyhow!("Own container ID is empty"));
    }
    Ok(container_id)
}

/// Restart the updater by spawning a detached helper container.
///
/// The helper-active marker is durable so image GC can skip cleanup while the
/// old updater is stopped and the replacement has not started yet.
async fn restart_self_via_helper(state: &AppState) -> Result<()> {
    let container_id = own_container_id().await?;
    let image = resolve_updater_service_image(state).await?;
    let helper_name = format!(
        "selu-updater-restart-{}",
        &container_id[..12.min(container_id.len())]
    );

    let _ = run_docker_cmd(state, &["rm", "-f", &helper_name]).await;
    run_docker_cmd(
        state,
        &[
            "run",
            "--rm",
            "-d",
            "--name",
            &helper_name,
            "--volumes-from",
            &container_id,
            "--entrypoint",
            "sh",
            &image,
            "-c",
            &build_self_restart_helper_script(state, &container_id),
        ],
    )
    .await
    .map(|_| ())
}

fn build_self_restart_helper_script(state: &AppState, current_container_id: &str) -> String {
    let compose_args = build_compose_shell_args(state);
    let current_container_id = shell_quote(current_container_id);
    let updater_service = shell_quote(&state.config.updater_service);
    let env_file = shell_quote(&state.config.compose_env_file);

    format!(
        concat!(
            "set -e; ",
            "clear_helper_flag() {{ ",
            "if grep -q '^SELU_UPDATER_RESTART_HELPER_ACTIVE=' {env_file}; then ",
            "sed -i 's/^SELU_UPDATER_RESTART_HELPER_ACTIVE=.*/SELU_UPDATER_RESTART_HELPER_ACTIVE=false/' {env_file}; ",
            "else printf '\\nSELU_UPDATER_RESTART_HELPER_ACTIVE=false\\n' >> {env_file}; fi; ",
            "}}; trap clear_helper_flag EXIT; ",
            "sleep 2; ",
            "docker stop {current_container_id} >/dev/null 2>&1 || true; ",
            "docker rm -f {current_container_id} >/dev/null 2>&1 || true; ",
            "docker compose {compose_args} rm -f -s {updater_service} >/dev/null 2>&1 || true; ",
            "clear_helper_flag; trap - EXIT; ",
            "docker compose {compose_args} up -d --no-deps --force-recreate --pull never {updater_service}"
        ),
        current_container_id = current_container_id,
        compose_args = compose_args,
        updater_service = updater_service,
        env_file = env_file,
    )
}

fn build_compose_shell_args(state: &AppState) -> String {
    let mut args = vec![
        "-f".to_string(),
        shell_quote(&state.config.compose_file),
        "--env-file".to_string(),
        shell_quote(&state.config.compose_env_file),
    ];
    if !state.config.compose_project_dir.trim().is_empty() {
        args.push("--project-directory".to_string());
        args.push(shell_quote(&state.config.compose_project_dir));
    }
    args.join(" ")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

async fn resolve_updater_service_image(state: &AppState) -> Result<String> {
    let mut args = vec![
        "compose",
        "-f",
        &state.config.compose_file,
        "--env-file",
        &state.config.compose_env_file,
    ];
    if !state.config.compose_project_dir.trim().is_empty() {
        args.push("--project-directory");
        args.push(&state.config.compose_project_dir);
    }
    args.extend(["config", "--images", &state.config.updater_service]);

    let image = run_docker_cmd(state, &args)
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if image.is_empty() {
        return Err(anyhow!(
            "Could not resolve updater service image from compose config"
        ));
    }
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::{
        EnvDocument, ImmutableImagePlan, RollbackRoot, build_compose_shell_args,
        build_self_restart_helper_script, extract_tag_from_image_ref, immutable_image_plan,
        persist_rollback_root, repo_digest_for_repository, resolve_whatsapp_bridge_image_ref,
        restore_rollback_root, should_pull_whatsapp_bridge_image, upsert_env_vars,
    };
    use crate::config::{ServerConfig, UpdaterConfig};
    use crate::state::AppState;
    use std::path::PathBuf;

    const DIGEST_A: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST_B: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn resolves_whatsapp_bridge_image_with_channel_tag() {
        let image =
            resolve_whatsapp_bridge_image_ref("ghcr.io/selu-bot/selu-whatsapp-bridge", "stable")
                .expect("image should resolve");
        assert_eq!(image, "ghcr.io/selu-bot/selu-whatsapp-bridge:stable");
    }

    #[test]
    fn keeps_existing_whatsapp_bridge_image_tag() {
        let image =
            resolve_whatsapp_bridge_image_ref("ghcr.io/selu-bot/selu-whatsapp-bridge:1.2.3", "x")
                .expect("image should resolve");
        assert_eq!(image, "ghcr.io/selu-bot/selu-whatsapp-bridge:1.2.3");
    }

    #[test]
    fn pulls_registry_backed_whatsapp_bridge_images() {
        assert!(should_pull_whatsapp_bridge_image(
            "ghcr.io/selu-bot/selu-whatsapp-bridge:stable"
        ));
        assert!(should_pull_whatsapp_bridge_image(
            "ghcr.io/selu-bot/selu-whatsapp-bridge@sha256:abc"
        ));
        assert!(!should_pull_whatsapp_bridge_image(
            "selu-whatsapp-bridge:stable"
        ));
    }

    #[test]
    fn exact_repository_digest_matching_rejects_same_digest_from_other_repo() {
        let output =
            format!("ghcr.io/attacker/selu@{DIGEST_A}\nghcr.io/selu-bot/selu@{DIGEST_B}\n");
        assert_eq!(
            repo_digest_for_repository(&output, "ghcr.io/selu-bot/selu:stable"),
            Some(DIGEST_B.to_string())
        );
        assert_ne!(
            repo_digest_for_repository(&output, "ghcr.io/selu-bot/selu"),
            Some(DIGEST_A.to_string())
        );
        assert_eq!(
            repo_digest_for_repository(
                &format!("ghcr.io/attacker/selu@{DIGEST_A}"),
                "ghcr.io/selu-bot/selu"
            ),
            None
        );
    }

    #[test]
    fn manual_rollback_plan_never_uses_mutable_channel_tag() {
        let plan = immutable_image_plan("ghcr.io/selu-bot/selu:stable", DIGEST_A)
            .expect("plan should be valid");
        assert_eq!(
            plan,
            ImmutableImagePlan {
                pull_ref: format!("ghcr.io/selu-bot/selu@{DIGEST_A}"),
                local_ref: format!(
                    "ghcr.io/selu-bot/selu:selu-rollback-{}",
                    DIGEST_A.trim_start_matches("sha256:")
                ),
                compose_tag: format!("selu-rollback-{}", DIGEST_A.trim_start_matches("sha256:")),
            }
        );
        assert!(!plan.pull_ref.ends_with(":stable"));
    }

    #[test]
    fn helper_script_marks_restart_window_and_disables_pull() {
        let state = AppState::new(test_config());
        let script = build_self_restart_helper_script(&state, "abc123");

        assert!(script.contains("trap clear_helper_flag EXIT"));
        assert!(script.contains("SELU_UPDATER_RESTART_HELPER_ACTIVE=false"));
        assert!(script.contains("docker stop 'abc123'"));
        assert!(script.contains("docker rm -f 'abc123'"));
        assert!(script.contains(
            "docker compose -f './docker-compose.yml' --env-file './.env' rm -f -s 'selu-updater'"
        ));
        assert!(script.contains(
            "docker compose -f './docker-compose.yml' --env-file './.env' up -d --no-deps --force-recreate --pull never 'selu-updater'"
        ));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn automatic_rollback_propagates_recreate_failure() {
        let fixture =
            DockerFixture::new("printf '%s' \"$*\" | grep -q 'compose .* up ' && exit 42");
        let state = AppState::new(fixture.config(1));
        let error = restore_rollback_root(&state, &rollback_root())
            .await
            .expect_err("recreate failure must propagate");

        assert!(error.to_string().contains("Failed to recreate"));
        assert!(fixture.log().contains("--pull never"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn automatic_rollback_propagates_restored_health_failure() {
        let fixture = DockerFixture::new("");
        let state = AppState::new(fixture.config(0));
        let error = restore_rollback_root(&state, &rollback_root())
            .await
            .expect_err("restored image health failure must propagate");

        assert!(error.to_string().contains("failed its health check"));
        assert!(fixture.log().contains("--pull never"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn rollback_metadata_is_one_exact_group_and_preserves_the_env_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let fixture = DockerFixture::new("");
        std::fs::write(
            &fixture.env_file,
            format!(
                "# keep this comment\nUNRELATED=value\nSELU_ROLLBACK_IMAGE_REF=ghcr.io/selu-bot/selu@{DIGEST_A}\nSELU_ROLLBACK_IMAGE_ID={DIGEST_A}\nSELU_ROLLBACK_IMAGE_TAG=old\nSELU_ROLLBACK_IMAGE_DIGEST={DIGEST_A}\nSELU_ROLLBACK_IMAGE_RETAIN_UNTIL=2027-01-01T00:00:00Z\n"
            ),
        )
        .expect("env fixture");
        std::fs::set_permissions(&fixture.env_file, std::fs::Permissions::from_mode(0o640))
            .expect("env permissions");
        let state = AppState::new(fixture.config(1));
        let failed_target = RollbackRoot {
            display_tag: "failed".to_string(),
            compose_tag: format!("selu-rollback-{}", DIGEST_B.trim_start_matches("sha256:")),
            digest: DIGEST_B.to_string(),
            version: "2.0.0".to_string(),
            build: "failed-build".to_string(),
            image_id: DIGEST_B.to_string(),
            immutable_ref: format!("ghcr.io/selu-bot/selu@{DIGEST_B}"),
        };

        persist_rollback_root(&state, &failed_target)
            .await
            .expect("persist rollback group");

        let content = std::fs::read_to_string(&fixture.env_file).expect("published env file");
        let document = EnvDocument::parse(&content);
        assert!(content.contains("# keep this comment\nUNRELATED=value\n"));
        assert_eq!(
            document.value("SELU_ROLLBACK_IMAGE_REF"),
            format!("ghcr.io/selu-bot/selu@{DIGEST_B}")
        );
        assert_eq!(document.value("SELU_ROLLBACK_IMAGE_ID"), DIGEST_B);
        assert_eq!(
            document.value("SELU_ROLLBACK_IMAGE_TAG"),
            failed_target.compose_tag
        );
        assert_eq!(document.value("SELU_ROLLBACK_IMAGE_DIGEST"), DIGEST_B);
        assert!(
            chrono::DateTime::parse_from_rfc3339(
                &document.value("SELU_ROLLBACK_IMAGE_RETAIN_UNTIL")
            )
            .is_ok()
        );
        assert_eq!(
            std::fs::metadata(&fixture.env_file)
                .expect("env metadata")
                .mode()
                & 0o777,
            0o640
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn concurrent_group_writers_never_publish_half_a_reference_set() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let fixture = DockerFixture::new("");
        std::fs::write(
            &fixture.env_file,
            "UNRELATED=preserved\nCURRENT=seed\nPREVIOUS=seed\n",
        )
        .expect("env fixture");
        let path = Arc::new(fixture.env_file.display().to_string());
        let done = Arc::new(AtomicBool::new(false));
        let reader_path = Arc::clone(&path);
        let reader_done = Arc::clone(&done);
        let reader = tokio::spawn(async move {
            while !reader_done.load(Ordering::Acquire) {
                let content = tokio::fs::read_to_string(reader_path.as_str())
                    .await
                    .expect("read atomic generation");
                let document = EnvDocument::parse(&content);
                assert_eq!(document.value("CURRENT"), document.value("PREVIOUS"));
                assert_eq!(document.value("UNRELATED"), "preserved");
                tokio::task::yield_now().await;
            }
        });

        let mut writers = Vec::new();
        for index in 0..64 {
            let writer_path = Arc::clone(&path);
            writers.push(tokio::spawn(async move {
                let generation = format!("generation-{index}");
                upsert_env_vars(
                    writer_path.as_str(),
                    &[("CURRENT", &generation), ("PREVIOUS", &generation)],
                )
                .await
                .expect("publish grouped generation");
            }));
        }
        for writer in writers {
            writer.await.expect("writer task");
        }
        done.store(true, Ordering::Release);
        reader.await.expect("reader task");

        let content = std::fs::read_to_string(path.as_str()).expect("final env file");
        let document = EnvDocument::parse(&content);
        assert_eq!(document.value("CURRENT"), document.value("PREVIOUS"));
        assert_eq!(document.value("UNRELATED"), "preserved");
    }

    #[test]
    fn compose_shell_args_quote_project_directory() {
        let mut cfg = test_config();
        cfg.compose_file = "/tmp/dir with spaces/docker-compose.yml".to_string();
        cfg.compose_env_file = "/tmp/dir with spaces/.env".to_string();
        cfg.compose_project_dir = "/tmp/dir with spaces".to_string();
        let state = AppState::new(cfg);

        let args = build_compose_shell_args(&state);
        assert_eq!(
            args,
            "-f '/tmp/dir with spaces/docker-compose.yml' --env-file '/tmp/dir with spaces/.env' --project-directory '/tmp/dir with spaces'"
        );
    }

    #[test]
    fn extracts_tag_only_for_exact_repository() {
        assert_eq!(
            extract_tag_from_image_ref("ghcr.io/selu-bot/selu:unstable", "ghcr.io/selu-bot/selu"),
            Some("unstable".to_string())
        );
        assert_eq!(
            extract_tag_from_image_ref("localhost:5000/selu-bot/selu:dev", "ghcr.io/selu-bot/selu"),
            None
        );
    }

    fn rollback_root() -> RollbackRoot {
        RollbackRoot {
            display_tag: "stable".to_string(),
            compose_tag: format!("selu-rollback-{}", DIGEST_A.trim_start_matches("sha256:")),
            digest: DIGEST_A.to_string(),
            version: "1.0.0".to_string(),
            build: "test".to_string(),
            image_id: DIGEST_A.to_string(),
            immutable_ref: format!("ghcr.io/selu-bot/selu@{DIGEST_A}"),
        }
    }

    #[cfg(unix)]
    struct DockerFixture {
        root: PathBuf,
        docker: PathBuf,
        env_file: PathBuf,
        log_file: PathBuf,
    }

    #[cfg(unix)]
    impl DockerFixture {
        fn new(extra_script: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;

            let root = std::env::temp_dir().join(format!(
                "selu-updater-test-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&root).expect("fixture directory");
            let docker = root.join("docker");
            let env_file = root.join(".env");
            let log_file = root.join("docker.log");
            std::fs::write(&env_file, "SELU_IMAGE_TAG=stable\n").expect("env fixture");
            std::fs::write(
                &docker,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{}\nexit 0\n",
                    log_file.display(),
                    extra_script
                ),
            )
            .expect("docker fixture");
            let mut permissions = std::fs::metadata(&docker)
                .expect("docker metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&docker, permissions).expect("docker permissions");
            Self {
                root,
                docker,
                env_file,
                log_file,
            }
        }

        fn config(&self, health_timeout_secs: u64) -> UpdaterConfig {
            let mut config = test_config();
            config.docker_bin = self.docker.display().to_string();
            config.compose_env_file = self.env_file.display().to_string();
            config.health_timeout_secs = health_timeout_secs;
            config.health_interval_secs = 0;
            config.health_url = "http://127.0.0.1:9/health".to_string();
            config
        }

        fn log(&self) -> String {
            std::fs::read_to_string(&self.log_file).unwrap_or_default()
        }
    }

    #[cfg(unix)]
    impl Drop for DockerFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn test_config() -> UpdaterConfig {
        UpdaterConfig {
            server: ServerConfig {
                host: "0.0.0.0".to_string(),
                port: 8090,
            },
            shared_secret: String::new(),
            compose_file: "./docker-compose.yml".to_string(),
            compose_project_dir: String::new(),
            compose_service: "selu".to_string(),
            updater_service: "selu-updater".to_string(),
            compose_env_file: "./.env".to_string(),
            health_url: "http://127.0.0.1:3000/api/health".to_string(),
            health_timeout_secs: 90,
            health_interval_secs: 3,
            docker_bin: "docker".to_string(),
            image_repo: "ghcr.io/selu-bot/selu".to_string(),
            updater_tag_env_key: "SELU_UPDATER_IMAGE_TAG".to_string(),
            updater_pending_tag_env_key: "SELU_UPDATER_PENDING_TAG".to_string(),
            self_update_on_apply: false,
            whatsapp_bridge_enabled: true,
            whatsapp_bridge_image_repo: "ghcr.io/selu-bot/selu-whatsapp-bridge".to_string(),
            whatsapp_bridge_container_name: "selu-whatsapp-bridge".to_string(),
            whatsapp_bridge_data_volume: "selu-whatsapp-bridge-data".to_string(),
            whatsapp_bridge_network: String::new(),
            whatsapp_bridge_port: 3200,
        }
    }
}
