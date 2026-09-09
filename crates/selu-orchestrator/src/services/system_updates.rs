use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use thiserror::Error;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::state::AppState;
use crate::updater::client::SidecarUpdaterClient;
use crate::updater::types::{
    SidecarApplyRequest, SidecarCheckRequest, SidecarRollbackRequest, SidecarStatusResponse,
};

const STATUS_IDLE: &str = "idle";
const STATUS_CHECKING: &str = "checking";
const STATUS_UPDATE_AVAILABLE: &str = "update_available";
const STATUS_UPDATING: &str = "updating";
const STATUS_ROLLBACK_AVAILABLE: &str = "rollback_available";
const STATUS_FAILED: &str = "failed";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChannelOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Deserialize)]
struct ChannelSummary {
    channel: String,
}

#[derive(Debug, Deserialize)]
struct ChannelsApiResponse {
    channels: Vec<ChannelSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemUpdateSettings {
    pub release_channel: String,
    pub auto_check: bool,
    pub auto_update: bool,
    pub installation_telemetry_opt_out: bool,
    pub public_origin: String,
    pub current_origin: String,
    pub push_notifications_enabled: bool,
    pub available_channels: Vec<ChannelOption>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateSettingsPatch {
    pub release_channel: Option<String>,
    pub auto_update: Option<bool>,
    pub installation_telemetry_opt_out: Option<bool>,
    pub push_notifications_enabled: Option<bool>,
    pub public_origin: Option<String>,
}

#[derive(Debug, Error)]
pub enum UpdateSettingsError {
    #[error("unsupported release channel")]
    InvalidReleaseChannel,
    #[error("invalid public origin")]
    InvalidPublicOrigin,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ReleaseMetadata {
    #[serde(default)]
    channel: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    image: String,
    #[serde(default)]
    tag: String,
    #[serde(default)]
    digest: String,
    #[serde(default)]
    published_at: String,
    #[serde(default)]
    changelog_url: String,
    #[serde(default)]
    changelog_body: String,
    #[serde(default)]
    build_number: String,
    #[serde(default)]
    previous_digest: String,
    #[serde(default)]
    min_supported_db_schema: i32,
}

#[derive(Debug, Clone)]
struct UpdateSettings {
    release_channel: String,
    auto_check: bool,
    auto_update: bool,
    installation_telemetry_opt_out: bool,
    external_url: String,
    push_notifications_enabled: bool,
}

#[derive(Debug, Clone, Default)]
struct UpdateState {
    installed_version: String,
    installed_digest: String,
    installed_release_version: String,
    installed_build_number: String,
    available_version: String,
    available_digest: String,
    available_release_version: String,
    available_build_number: String,
    available_changelog_url: String,
    available_changelog_body: String,
    previous_version: String,
    previous_digest: String,
    previous_release_version: String,
    previous_build_number: String,
    last_checked_at: String,
    last_attempt_at: String,
    active_job_id: String,
    last_good_tag: String,
    last_good_digest: String,
    last_error: String,
    status: String,
    progress_key: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub active_job_id: String,
    pub status: String,
    pub progress_key: String,
    pub last_error: String,
    pub last_checked_at: String,
    pub last_attempt_at: String,
    pub installed_version: String,
    pub installed_display: String,
    pub installed_release_version: String,
    pub installed_build_number: String,
    pub available_version: String,
    pub available_display: String,
    pub available_release_version: String,
    pub available_build_number: String,
    pub available_changelog_url: String,
    pub available_changelog_body: String,
    pub previous_version: String,
    pub previous_display: String,
    pub previous_release_version: String,
    pub previous_build_number: String,
    pub update_available: bool,
    pub rollback_available: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct JobCompletion {
    id: String,
    outcome: &'static str,
}

pub async fn settings(state: &AppState) -> Result<SystemUpdateSettings> {
    ensure_defaults(state).await?;
    let settings = load_settings(state).await?;
    Ok(SystemUpdateSettings {
        release_channel: settings.release_channel,
        auto_check: settings.auto_check,
        auto_update: settings.auto_update,
        installation_telemetry_opt_out: settings.installation_telemetry_opt_out,
        public_origin: settings.external_url,
        current_origin: String::new(),
        push_notifications_enabled: settings.push_notifications_enabled,
        available_channels: available_channels(state).await,
    })
}

pub async fn get_settings(
    state: &AppState,
    external_origin: &str,
    base_path: &str,
) -> Result<SystemUpdateSettings> {
    let mut settings = settings(state).await?;
    settings.current_origin = request_origin_without_base_path(external_origin, base_path);
    Ok(settings)
}

pub async fn update_settings(
    state: &AppState,
    patch: UpdateSettingsPatch,
) -> std::result::Result<(), UpdateSettingsError> {
    ensure_defaults(state).await?;

    let release_channel = patch
        .release_channel
        .as_deref()
        .map(normalize_release_channel)
        .transpose()?;
    let public_origin = patch
        .public_origin
        .as_deref()
        .map(normalize_public_origin)
        .transpose()?;

    sqlx::query(
        "UPDATE system_update_settings SET
             release_channel = COALESCE(?, release_channel),
             auto_update = COALESCE(?, auto_update),
             installation_telemetry_opt_out = COALESCE(?, installation_telemetry_opt_out),
             push_notifications_enabled = COALESCE(?, push_notifications_enabled),
             external_url = COALESCE(?, external_url),
             updated_at = datetime('now')
         WHERE id = 'global'",
    )
    .bind(release_channel)
    .bind(patch.auto_update.map(i64::from))
    .bind(patch.installation_telemetry_opt_out.map(i64::from))
    .bind(patch.push_notifications_enabled.map(i64::from))
    .bind(public_origin.as_deref())
    .execute(&state.db)
    .await
    .context("Failed to update system update settings")?;

    if let Some(origin) = public_origin {
        state.public_origin_override.store(if origin.is_empty() {
            None
        } else {
            Some(std::sync::Arc::new(origin))
        });
    }

    Ok(())
}

pub async fn update_status(state: &AppState) -> Result<UpdateStatus> {
    ensure_defaults(state).await?;
    let settings = load_settings(state).await?;
    let mut current = load_state(state).await?;
    hydrate_installed_from_sidecar(state, &settings, &mut current).await;

    if !current.active_job_id.trim().is_empty() {
        match sidecar_client(state) {
            Ok(client) => match client.status().await {
                Ok(sidecar_status) => {
                    if let Some(completion) = merge_sidecar_status(&mut current, sidecar_status) {
                        let _ = finish_update_job(
                            state,
                            &completion.id,
                            completion.outcome,
                            &current.progress_key,
                            &current.last_error,
                        )
                        .await;
                    }
                    let _ = persist_state(state, &current).await;
                }
                Err(e) => mark_status_poll_failed(state, &mut current, e).await,
            },
            Err(e) => mark_status_poll_failed(state, &mut current, e).await,
        }
    }

    Ok(status_response(&current))
}

pub async fn status(state: &AppState) -> Result<UpdateStatus> {
    update_status(state).await
}

async fn mark_status_poll_failed(
    state: &AppState,
    current: &mut UpdateState,
    error: anyhow::Error,
) {
    warn!(
        "Failed to poll updater sidecar for job {}: {error:#}",
        current.active_job_id
    );
    current.status = STATUS_FAILED.to_string();
    current.progress_key = "updates.progress.apply_failed".to_string();
    current.last_error = format!("Lost contact with updater sidecar: {error}");
    let active_job_id = std::mem::take(&mut current.active_job_id);
    if !active_job_id.trim().is_empty() {
        let _ = finish_update_job(
            state,
            &active_job_id,
            "failed",
            &current.progress_key,
            &current.last_error,
        )
        .await;
    }
    let _ = persist_state(state, current).await;
}

fn merge_sidecar_status(
    current: &mut UpdateState,
    status: SidecarStatusResponse,
) -> Option<JobCompletion> {
    let previous_active_job_id = current.active_job_id.clone();
    current.status = status.status;
    if let Some(progress_key) = status.progress_key.filter(|value| !value.trim().is_empty()) {
        current.progress_key = progress_key;
    }
    if let Some(job_id) = status.job_id.filter(|value| !value.trim().is_empty()) {
        current.active_job_id = job_id;
    }
    if let Some(value) = status
        .installed_tag
        .filter(|value| !value.trim().is_empty())
    {
        current.installed_version = value;
    }
    if let Some(value) = status
        .installed_digest
        .filter(|value| !value.trim().is_empty())
    {
        current.installed_digest = value;
    }
    if let Some(value) = status
        .installed_version
        .filter(|value| !value.trim().is_empty())
    {
        current.installed_release_version = value;
    }
    if let Some(value) = status
        .installed_build
        .filter(|value| !value.trim().is_empty())
    {
        current.installed_build_number = value;
    }
    if let Some(value) = status.previous_tag.filter(|value| !value.trim().is_empty()) {
        current.previous_version = value;
    }
    if let Some(value) = status
        .previous_digest
        .filter(|value| !value.trim().is_empty())
    {
        current.previous_digest = value;
    }
    if let Some(value) = status
        .previous_version
        .filter(|value| !value.trim().is_empty())
    {
        current.previous_release_version = value;
    }
    if let Some(value) = status
        .previous_build
        .filter(|value| !value.trim().is_empty())
    {
        current.previous_build_number = value;
    }
    if let Some(message) = status.message {
        if !message.trim().is_empty() && current.status == STATUS_FAILED {
            current.last_error = message;
        } else if current.status != STATUS_FAILED {
            current.last_error.clear();
        }
    }

    let terminal = !matches!(current.status.as_str(), STATUS_UPDATING | STATUS_CHECKING);
    if !terminal {
        return None;
    }
    if current.status != STATUS_FAILED {
        current.last_error.clear();
    }
    current.active_job_id.clear();
    (!previous_active_job_id.trim().is_empty()).then(|| JobCompletion {
        id: previous_active_job_id,
        outcome: if current.status == STATUS_FAILED {
            "failed"
        } else {
            "completed"
        },
    })
}

fn status_response(current: &UpdateState) -> UpdateStatus {
    UpdateStatus {
        active_job_id: current.active_job_id.clone(),
        status: current.status.clone(),
        progress_key: current.progress_key.clone(),
        last_error: current.last_error.clone(),
        last_checked_at: current.last_checked_at.clone(),
        last_attempt_at: current.last_attempt_at.clone(),
        installed_version: current.installed_version.clone(),
        installed_display: format_release_display(
            &current.installed_release_version,
            &current.installed_build_number,
            &current.installed_version,
            &current.installed_digest,
        ),
        installed_release_version: current.installed_release_version.clone(),
        installed_build_number: current.installed_build_number.clone(),
        available_version: current.available_version.clone(),
        available_display: format_release_display(
            &current.available_release_version,
            &current.available_build_number,
            &current.available_version,
            &current.available_digest,
        ),
        available_release_version: current.available_release_version.clone(),
        available_build_number: current.available_build_number.clone(),
        available_changelog_url: current.available_changelog_url.clone(),
        available_changelog_body: current.available_changelog_body.clone(),
        previous_version: current.previous_version.clone(),
        previous_display: format_release_display(
            &current.previous_release_version,
            &current.previous_build_number,
            &current.previous_version,
            &current.previous_digest,
        ),
        previous_release_version: current.previous_release_version.clone(),
        previous_build_number: current.previous_build_number.clone(),
        update_available: is_update_available(current),
        rollback_available: !current.previous_version.trim().is_empty()
            || current.status == STATUS_ROLLBACK_AVAILABLE,
    }
}

pub async fn available_channels(state: &AppState) -> Vec<ChannelOption> {
    let fallback = available_channel_options();
    let url = format!(
        "{}/channels",
        state.config.release_metadata_url.trim_end_matches('/')
    );
    let response = match reqwest::Client::new().get(url).send().await {
        Ok(response) if response.status().is_success() => response,
        _ => return fallback,
    };
    let body = match response.json::<ChannelsApiResponse>().await {
        Ok(body) if !body.channels.is_empty() => body,
        _ => return fallback,
    };

    body.channels
        .into_iter()
        .map(|channel| ChannelOption {
            label: match channel.channel.as_str() {
                "stable" => "Stable".to_string(),
                "unstable" => "Unstable".to_string(),
                other => other.to_string(),
            },
            value: channel.channel,
        })
        .collect()
}

fn available_channel_options() -> Vec<ChannelOption> {
    vec![
        ChannelOption {
            value: "stable".to_string(),
            label: "Stable".to_string(),
        },
        ChannelOption {
            value: "unstable".to_string(),
            label: "Unstable".to_string(),
        },
    ]
}

pub async fn check_for_updates(state: &AppState) -> Result<bool> {
    ensure_defaults(state).await?;
    let settings = load_settings(state).await?;
    check_for_updates_via_sidecar(state, &settings).await
}

pub async fn run_auto_check_if_enabled(state: &AppState) -> Result<()> {
    ensure_defaults(state).await?;
    let settings = load_settings(state).await?;
    if !settings.auto_check {
        return Ok(());
    }

    let has_update = check_for_updates(state).await?;
    if has_update {
        info!(
            "System update available on channel {}",
            settings.release_channel
        );
        if settings.auto_update {
            info!(
                "Auto-update enabled; applying update from channel {}",
                settings.release_channel
            );
            if let Err(e) = apply_update(state).await {
                warn!("Auto-update apply failed: {e}");
            }
        }
    }
    Ok(())
}

async fn check_for_updates_via_sidecar(
    state: &AppState,
    settings: &UpdateSettings,
) -> Result<bool> {
    let job_id = Uuid::new_v4().to_string();
    if !try_claim_active_job(
        state,
        &job_id,
        STATUS_CHECKING,
        "updates.progress.checking",
        false,
    )
    .await?
    {
        return Err(anyhow!("Another update action is already running"));
    }
    if let Err(e) = create_update_job(state, &job_id, "check", settings, "", "", "ui").await {
        let _ = set_failure_state(
            state,
            "updates.progress.check_failed",
            "Could not start update check",
        )
        .await;
        return Err(e);
    }

    let client = sidecar_client(state)?;
    info!(
        "Checking for updates on channel '{}'",
        settings.release_channel
    );
    let metadata = fetch_release_metadata(state, &settings.release_channel).await?;
    if metadata.tag.trim().is_empty() {
        return Err(anyhow!("Release metadata did not include a tag"));
    }
    let ack = client
        .check(&SidecarCheckRequest {
            request_id: job_id.clone(),
            channel: settings.release_channel.clone(),
            target_tag: metadata.tag.clone(),
            target_digest: metadata.digest.clone(),
        })
        .await;

    let mut update_state = load_state(state).await?;
    hydrate_installed_from_sidecar(state, settings, &mut update_state).await;
    update_state.available_version = metadata.tag.clone();
    update_state.available_digest = metadata.digest.clone();
    update_state.available_release_version =
        normalize_release_version(&metadata.version, &metadata.tag, &settings.release_channel);
    update_state.available_build_number = metadata.build_number.clone();
    update_state.available_changelog_url = metadata.changelog_url.clone();
    update_state.available_changelog_body = metadata.changelog_body.clone();
    update_state.last_checked_at = chrono::Utc::now().to_rfc3339();

    // Reset status so is_update_available doesn't short-circuit on STATUS_CHECKING.
    update_state.status = STATUS_IDLE.to_string();

    match ack {
        Ok(ack) => {
            let has_update = is_update_available(&update_state);
            update_state.status = if has_update {
                STATUS_UPDATE_AVAILABLE.to_string()
            } else {
                STATUS_IDLE.to_string()
            };
            update_state.progress_key = ack.progress_key.unwrap_or_else(|| {
                if has_update {
                    "updates.progress.update_available".to_string()
                } else {
                    "updates.progress.already_current".to_string()
                }
            });
            update_state.last_error.clear();
            update_state.active_job_id.clear();
            persist_state(state, &update_state).await?;
            finish_update_job(
                state,
                &job_id,
                if ack.accepted { "completed" } else { "failed" },
                &update_state.progress_key,
                ack.message.as_deref().unwrap_or(""),
            )
            .await?;
            Ok(has_update)
        }
        Err(e) => {
            set_failure_state(
                state,
                "updates.progress.check_failed",
                "Could not check for updates via updater sidecar.",
            )
            .await?;
            set_active_job(state, None).await?;
            finish_update_job(
                state,
                &job_id,
                "failed",
                "updates.progress.check_failed",
                &e.to_string(),
            )
            .await?;
            Err(e)
        }
    }
}

async fn apply_update_via_sidecar(state: &AppState, settings: &UpdateSettings) -> Result<()> {
    let _maintenance_lease = state.docker_storage.shared_lease().await;
    let metadata = fetch_release_metadata(state, &settings.release_channel).await?;
    if metadata.tag.trim().is_empty() {
        return Err(anyhow!("Release metadata did not include a tag"));
    }
    if metadata.digest.trim().is_empty() {
        return Err(anyhow!("Release metadata did not include a digest"));
    }
    let job_id = Uuid::new_v4().to_string();
    if !try_claim_active_job(
        state,
        &job_id,
        STATUS_UPDATING,
        "updates.progress.preparing",
        true,
    )
    .await?
    {
        return Err(anyhow!("Another update action is already running"));
    }
    if let Err(e) = create_update_job(
        state,
        &job_id,
        "apply",
        settings,
        &metadata.tag,
        &metadata.digest,
        "ui",
    )
    .await
    {
        let _ = set_failure_state(
            state,
            "updates.progress.apply_failed",
            "Could not start update job",
        )
        .await;
        return Err(e);
    }

    let client = sidecar_client(state)?;
    let ack = client
        .apply(&SidecarApplyRequest {
            request_id: job_id.clone(),
            channel: settings.release_channel.clone(),
            target_tag: metadata.tag.clone(),
            target_digest: metadata.digest.clone(),
            target_version: normalize_release_version(
                &metadata.version,
                &metadata.tag,
                &settings.release_channel,
            ),
            target_build: metadata.build_number.clone(),
        })
        .await;

    match ack {
        Ok(ack) => {
            let mut current = load_state(state).await?;
            current.available_version = metadata.tag.clone();
            current.available_digest = metadata.digest.clone();
            current.available_release_version = normalize_release_version(
                &metadata.version,
                &metadata.tag,
                &settings.release_channel,
            );
            current.available_build_number = metadata.build_number.clone();
            current.available_changelog_url = metadata.changelog_url.clone();
            current.available_changelog_body = metadata.changelog_body.clone();
            current.last_attempt_at = chrono::Utc::now().to_rfc3339();
            current.status = ack
                .status
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| STATUS_UPDATING.to_string());
            current.progress_key = ack
                .progress_key
                .unwrap_or_else(|| "updates.progress.restarting".to_string());
            if ack.accepted {
                current.previous_version = current.installed_version.clone();
                current.previous_digest = current.installed_digest.clone();
                current.previous_release_version = current.installed_release_version.clone();
                current.previous_build_number = current.installed_build_number.clone();
                current.last_error.clear();
                current.active_job_id = ack.job_id.unwrap_or(job_id.clone());
                finish_update_job(
                    state,
                    &job_id,
                    "started",
                    &current.progress_key,
                    ack.message.as_deref().unwrap_or(""),
                )
                .await?;
            } else {
                current.status = STATUS_FAILED.to_string();
                current.progress_key = "updates.progress.apply_failed".to_string();
                current.last_error = "Updater sidecar rejected the update request.".to_string();
                current.active_job_id.clear();
                finish_update_job(
                    state,
                    &job_id,
                    "failed",
                    &current.progress_key,
                    "Updater sidecar rejected the update request.",
                )
                .await?;
                persist_state(state, &current).await?;
                return Err(anyhow!("Updater sidecar rejected update request"));
            }
            persist_state(state, &current).await?;
            Ok(())
        }
        Err(e) => {
            set_failure_state(
                state,
                "updates.progress.apply_failed",
                "Could not reach updater sidecar for update.",
            )
            .await?;
            set_active_job(state, None).await?;
            finish_update_job(
                state,
                &job_id,
                "failed",
                "updates.progress.apply_failed",
                &e.to_string(),
            )
            .await?;
            Err(e)
        }
    }
}

async fn rollback_update_via_sidecar(state: &AppState, current: &UpdateState) -> Result<()> {
    let _maintenance_lease = state.docker_storage.shared_lease().await;
    if current.previous_version.trim().is_empty() {
        return Err(anyhow!("No previous version is available for rollback"));
    }

    let settings = load_settings(state).await?;
    let job_id = Uuid::new_v4().to_string();
    if !try_claim_active_job(
        state,
        &job_id,
        STATUS_UPDATING,
        "updates.progress.rollback_start",
        true,
    )
    .await?
    {
        return Err(anyhow!("Another update action is already running"));
    }
    if let Err(e) = create_update_job(
        state,
        &job_id,
        "rollback",
        &settings,
        &current.previous_version,
        &current.previous_digest,
        "ui",
    )
    .await
    {
        let _ = set_failure_state(
            state,
            "updates.progress.apply_failed",
            "Could not start rollback job",
        )
        .await;
        return Err(e);
    }

    let client = sidecar_client(state)?;
    let ack = client
        .rollback(&SidecarRollbackRequest {
            request_id: job_id.clone(),
            channel: settings.release_channel,
            rollback_tag: current.previous_version.clone(),
            rollback_digest: current.previous_digest.clone(),
            rollback_version: current.previous_release_version.clone(),
            rollback_build: current.previous_build_number.clone(),
        })
        .await;

    match ack {
        Ok(ack) => {
            let mut next = load_state(state).await?;
            next.last_attempt_at = chrono::Utc::now().to_rfc3339();
            next.status = ack
                .status
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| STATUS_UPDATING.to_string());
            next.progress_key = ack
                .progress_key
                .unwrap_or_else(|| "updates.progress.rollback_restarting".to_string());
            if ack.accepted {
                next.previous_version = next.installed_version.clone();
                next.previous_digest = next.installed_digest.clone();
                next.previous_release_version = next.installed_release_version.clone();
                next.previous_build_number = next.installed_build_number.clone();
                next.last_error.clear();
                next.active_job_id = ack.job_id.unwrap_or(job_id.clone());
                finish_update_job(
                    state,
                    &job_id,
                    "started",
                    &next.progress_key,
                    ack.message.as_deref().unwrap_or(""),
                )
                .await?;
            } else {
                next.status = STATUS_FAILED.to_string();
                next.progress_key = "updates.progress.apply_failed".to_string();
                next.last_error = "Updater sidecar rejected the rollback request.".to_string();
                next.active_job_id.clear();
                finish_update_job(
                    state,
                    &job_id,
                    "failed",
                    &next.progress_key,
                    "Updater sidecar rejected the rollback request.",
                )
                .await?;
                persist_state(state, &next).await?;
                return Err(anyhow!("Updater sidecar rejected rollback request"));
            }
            persist_state(state, &next).await?;
            Ok(())
        }
        Err(e) => {
            set_failure_state(
                state,
                "updates.progress.apply_failed",
                "Could not reach updater sidecar for rollback.",
            )
            .await?;
            set_active_job(state, None).await?;
            finish_update_job(
                state,
                &job_id,
                "failed",
                "updates.progress.apply_failed",
                &e.to_string(),
            )
            .await?;
            Err(e)
        }
    }
}

pub async fn apply_update(state: &AppState) -> Result<()> {
    ensure_defaults(state).await?;
    let settings = load_settings(state).await?;
    apply_update_via_sidecar(state, &settings).await
}

pub async fn rollback_update(state: &AppState) -> Result<()> {
    ensure_defaults(state).await?;
    let current = load_state(state).await?;
    rollback_update_via_sidecar(state, &current).await
}

fn is_update_available(s: &UpdateState) -> bool {
    if matches!(s.status.as_str(), STATUS_CHECKING | STATUS_UPDATING) {
        return false;
    }
    if s.available_version.trim().is_empty() {
        return false;
    }
    if s.installed_version.trim().is_empty() && s.installed_digest.trim().is_empty() {
        return false;
    }
    if !s.available_digest.trim().is_empty() && !s.installed_digest.trim().is_empty() {
        return s.available_digest != s.installed_digest;
    }
    if s.installed_version.trim().is_empty() {
        return false;
    }
    s.available_version != s.installed_version
}

fn format_release_display(
    release_version: &str,
    build_number: &str,
    tag: &str,
    digest: &str,
) -> String {
    let rv = release_version.trim();
    let build = build_number.trim();
    if !rv.is_empty() && !build.is_empty() {
        return format!("{} (build {})", rv, build);
    }
    if !rv.is_empty() {
        return rv.to_string();
    }

    let v = tag.trim();
    let d = digest.trim();
    if v.is_empty() && d.is_empty() {
        return String::new();
    }
    if d.is_empty() {
        return v.to_string();
    }
    let short_digest = d
        .strip_prefix("sha256:")
        .unwrap_or(d)
        .chars()
        .take(12)
        .collect::<String>();
    if v.is_empty() {
        return format!("sha256:{}", short_digest);
    }
    format!("{} ({})", v, short_digest)
}

fn normalize_release_version(version: &str, tag: &str, _channel: &str) -> String {
    let raw = version.trim();
    if raw.is_empty() {
        let fallback = tag.trim();
        return if fallback.is_empty() {
            String::new()
        } else {
            fallback.to_string()
        };
    }
    raw.to_string()
}

/// Populate installed version fields from the sidecar status endpoint.
/// Falls back to the configured release channel for the tag when
/// nothing else is available (e.g. very first boot before any check).
async fn hydrate_installed_from_sidecar(
    state: &AppState,
    settings: &UpdateSettings,
    s: &mut UpdateState,
) {
    if let Ok(client) = sidecar_client(state) {
        if let Ok(status) = client.status().await {
            if let Some(tag) = status.installed_tag {
                if !tag.trim().is_empty() && s.installed_version.trim().is_empty() {
                    s.installed_version = tag;
                }
            }
            if let Some(digest) = status.installed_digest {
                if !digest.trim().is_empty() && s.installed_digest.trim().is_empty() {
                    s.installed_digest = digest;
                }
            }
            if let Some(version) = status.installed_version {
                if !version.trim().is_empty() && s.installed_release_version.trim().is_empty() {
                    s.installed_release_version = version;
                }
            }
            if let Some(build) = status.installed_build {
                if !build.trim().is_empty() && s.installed_build_number.trim().is_empty() {
                    s.installed_build_number = build;
                }
            }
        }
    }

    // Last-resort fallback: use the release channel as the tag.
    if s.installed_version.trim().is_empty() {
        s.installed_version = settings.release_channel.clone();
    }
    // Normalize: if release_version is still empty, fall back to the tag so the
    // display format matches the "available" column.
    if s.installed_release_version.trim().is_empty() {
        s.installed_release_version =
            normalize_release_version("", &s.installed_version, &settings.release_channel);
    }
}

async fn fetch_release_metadata(state: &AppState, channel: &str) -> Result<ReleaseMetadata> {
    let url = format!(
        "{}?channel={}",
        state.config.release_metadata_url.trim_end_matches('/'),
        channel
    );
    let response = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .context("Failed to reach release metadata endpoint")?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "Release metadata endpoint returned {}",
            response.status()
        ));
    }

    let body = response
        .text()
        .await
        .context("Failed to read release metadata response body")?;

    serde_json::from_str::<ReleaseMetadata>(&body).map_err(|e| {
        error!("Failed to parse release metadata: {e}\nResponse body: {body}");
        anyhow!("Invalid release metadata response: {e}")
    })
}

async fn ensure_defaults(state: &AppState) -> Result<()> {
    let seeded_channel = std::env::var("SELU_RELEASE_CHANNEL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "stable".to_string());

    sqlx::query(
        "INSERT OR IGNORE INTO system_update_settings
         (id, release_channel, auto_check, auto_update, installation_telemetry_opt_out, external_url, push_notifications_enabled)
         VALUES ('global', ?, 1, 1, 0, '', 1)",
    )
    .bind(&seeded_channel)
    .execute(&state.db)
    .await
    .context("Failed to seed system_update_settings")?;

    sqlx::query(
        "INSERT OR IGNORE INTO system_update_state
         (id, installed_version, installed_digest, available_version, available_digest, previous_version, previous_digest, last_error, status, progress_key)
         VALUES ('global', '', '', '', '', '', '', '', 'idle', 'updates.progress.idle')",
    )
    .execute(&state.db)
    .await
    .context("Failed to seed system_update_state")?;

    Ok(())
}

/// Returns the release channel stored in the database, falling back to "stable".
pub async fn release_channel(state: &AppState) -> String {
    sqlx::query_scalar::<_, String>(
        "SELECT release_channel FROM system_update_settings WHERE id = 'global'",
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .unwrap_or_else(|| "stable".to_string())
}

async fn load_settings(state: &AppState) -> Result<UpdateSettings> {
    let row = sqlx::query(
        "SELECT release_channel,
                auto_check,
                auto_update,
                COALESCE(installation_telemetry_opt_out, 0) AS installation_telemetry_opt_out,
                COALESCE(external_url, '') AS external_url,
                COALESCE(push_notifications_enabled, 0) AS push_notifications_enabled
         FROM system_update_settings
         WHERE id = 'global'",
    )
    .fetch_one(&state.db)
    .await
    .context("Failed to query update settings")?;

    Ok(UpdateSettings {
        release_channel: row
            .try_get::<String, _>("release_channel")
            .unwrap_or_else(|_| "stable".to_string()),
        auto_check: row.try_get::<i64, _>("auto_check").unwrap_or(1) != 0,
        auto_update: row.try_get::<i64, _>("auto_update").unwrap_or(0) != 0,
        installation_telemetry_opt_out: row
            .try_get::<i64, _>("installation_telemetry_opt_out")
            .unwrap_or(0)
            != 0,
        external_url: row.try_get::<String, _>("external_url").unwrap_or_default(),
        push_notifications_enabled: row
            .try_get::<i64, _>("push_notifications_enabled")
            .unwrap_or(0)
            != 0,
    })
}

/// Returns whether push notifications are enabled in system settings.
pub async fn push_notifications_enabled(state: &AppState) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(push_notifications_enabled, 0) FROM system_update_settings WHERE id = 'global'",
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .unwrap_or(0)
        != 0
}

pub async fn hydrate_public_origin_override(state: &AppState) {
    match load_public_origin(state).await {
        Ok(origin) => state
            .public_origin_override
            .store(origin.map(std::sync::Arc::new)),
        Err(e) => warn!("Failed to hydrate public web address setting: {e}"),
    }
}

async fn load_public_origin(state: &AppState) -> Result<Option<String>> {
    let row = sqlx::query(
        "SELECT COALESCE(external_url, '') AS external_url
         FROM system_update_settings
         WHERE id = 'global'",
    )
    .fetch_optional(&state.db)
    .await
    .context("Failed to query public web address setting")?;

    let Some(row) = row else {
        return Ok(None);
    };

    let value = row.try_get::<String, _>("external_url").unwrap_or_default();
    let value = value.trim().trim_end_matches('/').to_string();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn normalize_release_channel(value: &str) -> std::result::Result<&str, UpdateSettingsError> {
    let value = value.trim();
    if value.is_empty() {
        Err(UpdateSettingsError::InvalidReleaseChannel)
    } else {
        Ok(value)
    }
}

fn normalize_public_origin(input: &str) -> Result<String, UpdateSettingsError> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }

    let parsed =
        reqwest::Url::parse(trimmed).map_err(|_| UpdateSettingsError::InvalidPublicOrigin)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().unwrap_or_default().trim().is_empty()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(UpdateSettingsError::InvalidPublicOrigin);
    }

    Ok(trimmed.to_string())
}

pub fn request_origin_without_base_path(external_origin: &str, base_path: &str) -> String {
    if base_path.is_empty() {
        return external_origin.trim_end_matches('/').to_string();
    }

    external_origin
        .trim_end_matches('/')
        .strip_suffix(base_path)
        .unwrap_or(external_origin.trim_end_matches('/'))
        .trim_end_matches('/')
        .to_string()
}

async fn load_state(state: &AppState) -> Result<UpdateState> {
    let row = sqlx::query(
        "SELECT installed_version, installed_digest, available_version, available_digest,
                COALESCE(installed_release_version, '') AS installed_release_version,
                COALESCE(installed_build_number, '') AS installed_build_number,
                COALESCE(available_release_version, '') AS available_release_version,
                COALESCE(available_build_number, '') AS available_build_number,
                COALESCE(available_changelog_url, '') AS available_changelog_url,
                COALESCE(available_changelog_body, '') AS available_changelog_body,
                previous_version, previous_digest, COALESCE(last_checked_at, '') AS last_checked_at,
                COALESCE(previous_release_version, '') AS previous_release_version,
                COALESCE(previous_build_number, '') AS previous_build_number,
                COALESCE(last_attempt_at, '') AS last_attempt_at, last_error,
                COALESCE(active_job_id, '') AS active_job_id,
                COALESCE(last_good_tag, '') AS last_good_tag,
                COALESCE(last_good_digest, '') AS last_good_digest,
                COALESCE(status, 'idle') AS status,
                COALESCE(progress_key, 'updates.progress.idle') AS progress_key
         FROM system_update_state
         WHERE id = 'global'",
    )
    .fetch_one(&state.db)
    .await
    .context("Failed to query update state")?;

    Ok(UpdateState {
        installed_version: row
            .try_get::<String, _>("installed_version")
            .unwrap_or_default(),
        installed_digest: row
            .try_get::<String, _>("installed_digest")
            .unwrap_or_default(),
        installed_release_version: row
            .try_get::<String, _>("installed_release_version")
            .unwrap_or_default(),
        installed_build_number: row
            .try_get::<String, _>("installed_build_number")
            .unwrap_or_default(),
        available_version: row
            .try_get::<String, _>("available_version")
            .unwrap_or_default(),
        available_digest: row
            .try_get::<String, _>("available_digest")
            .unwrap_or_default(),
        available_release_version: row
            .try_get::<String, _>("available_release_version")
            .unwrap_or_default(),
        available_build_number: row
            .try_get::<String, _>("available_build_number")
            .unwrap_or_default(),
        available_changelog_url: row
            .try_get::<String, _>("available_changelog_url")
            .unwrap_or_default(),
        available_changelog_body: row
            .try_get::<String, _>("available_changelog_body")
            .unwrap_or_default(),
        previous_version: row
            .try_get::<String, _>("previous_version")
            .unwrap_or_default(),
        previous_digest: row
            .try_get::<String, _>("previous_digest")
            .unwrap_or_default(),
        previous_release_version: row
            .try_get::<String, _>("previous_release_version")
            .unwrap_or_default(),
        previous_build_number: row
            .try_get::<String, _>("previous_build_number")
            .unwrap_or_default(),
        last_checked_at: row
            .try_get::<String, _>("last_checked_at")
            .unwrap_or_default(),
        last_attempt_at: row
            .try_get::<String, _>("last_attempt_at")
            .unwrap_or_default(),
        active_job_id: row
            .try_get::<String, _>("active_job_id")
            .unwrap_or_default(),
        last_good_tag: row
            .try_get::<String, _>("last_good_tag")
            .unwrap_or_default(),
        last_good_digest: row
            .try_get::<String, _>("last_good_digest")
            .unwrap_or_default(),
        last_error: row.try_get::<String, _>("last_error").unwrap_or_default(),
        status: row
            .try_get::<String, _>("status")
            .unwrap_or_else(|_| STATUS_IDLE.to_string()),
        progress_key: row
            .try_get::<String, _>("progress_key")
            .unwrap_or_else(|_| "updates.progress.idle".to_string()),
    })
}

async fn persist_state(state: &AppState, s: &UpdateState) -> Result<()> {
    sqlx::query(
        "UPDATE system_update_state
         SET installed_version = ?,
             installed_digest = ?,
             installed_release_version = ?,
             installed_build_number = ?,
             available_version = ?,
             available_digest = ?,
             available_release_version = ?,
             available_build_number = ?,
             available_changelog_url = ?,
             available_changelog_body = ?,
             previous_version = ?,
             previous_digest = ?,
             previous_release_version = ?,
             previous_build_number = ?,
             last_checked_at = ?,
             last_attempt_at = ?,
             active_job_id = ?,
             last_good_tag = ?,
             last_good_digest = ?,
             last_error = ?,
             status = ?,
             progress_key = ?,
             updated_at = datetime('now')
         WHERE id = 'global'",
    )
    .bind(&s.installed_version)
    .bind(&s.installed_digest)
    .bind(&s.installed_release_version)
    .bind(&s.installed_build_number)
    .bind(&s.available_version)
    .bind(&s.available_digest)
    .bind(&s.available_release_version)
    .bind(&s.available_build_number)
    .bind(&s.available_changelog_url)
    .bind(&s.available_changelog_body)
    .bind(&s.previous_version)
    .bind(&s.previous_digest)
    .bind(&s.previous_release_version)
    .bind(&s.previous_build_number)
    .bind(&s.last_checked_at)
    .bind(&s.last_attempt_at)
    .bind(&s.active_job_id)
    .bind(&s.last_good_tag)
    .bind(&s.last_good_digest)
    .bind(&s.last_error)
    .bind(&s.status)
    .bind(&s.progress_key)
    .execute(&state.db)
    .await
    .context("Failed to persist update state")?;
    Ok(())
}

async fn try_claim_active_job(
    state: &AppState,
    job_id: &str,
    status: &str,
    progress_key: &str,
    set_last_attempt: bool,
) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let result = sqlx::query(
        "UPDATE system_update_state
         SET status = ?,
             progress_key = ?,
             active_job_id = ?,
             last_attempt_at = CASE WHEN ? = 1 THEN ? ELSE last_attempt_at END,
             updated_at = datetime('now')
         WHERE id = 'global' AND COALESCE(active_job_id, '') = ''",
    )
    .bind(status)
    .bind(progress_key)
    .bind(job_id)
    .bind(if set_last_attempt { 1 } else { 0 })
    .bind(now)
    .execute(&state.db)
    .await
    .context("Failed to claim active update job slot")?;

    Ok(result.rows_affected() > 0)
}

async fn set_active_job(state: &AppState, active_job_id: Option<&str>) -> Result<()> {
    sqlx::query(
        "UPDATE system_update_state
         SET active_job_id = ?, updated_at = datetime('now')
         WHERE id = 'global'",
    )
    .bind(active_job_id.unwrap_or(""))
    .execute(&state.db)
    .await
    .context("Failed to set active update job")?;
    Ok(())
}

async fn set_failure_state(state: &AppState, progress_key: &str, message: &str) -> Result<()> {
    sqlx::query(
        "UPDATE system_update_state
         SET status = ?, progress_key = ?, last_error = ?, active_job_id = '', updated_at = datetime('now')
         WHERE id = 'global'",
    )
    .bind(STATUS_FAILED)
    .bind(progress_key)
    .bind(message)
    .execute(&state.db)
    .await
    .context("Failed to persist failure state")?;
    Ok(())
}

async fn create_update_job(
    state: &AppState,
    id: &str,
    action: &str,
    settings: &UpdateSettings,
    requested_tag: &str,
    requested_digest: &str,
    initiator: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO system_update_jobs
         (id, action, status, channel, requested_tag, requested_digest, progress_key, initiator)
         VALUES (?, ?, 'started', ?, ?, ?, 'updates.progress.preparing', ?)",
    )
    .bind(id)
    .bind(action)
    .bind(&settings.release_channel)
    .bind(requested_tag)
    .bind(requested_digest)
    .bind(initiator)
    .execute(&state.db)
    .await
    .context("Failed to create update job")?;
    Ok(())
}

async fn finish_update_job(
    state: &AppState,
    id: &str,
    status: &str,
    progress_key: &str,
    error_message: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE system_update_jobs
         SET status = ?, progress_key = ?, error_message = ?, finished_at = datetime('now')
         WHERE id = ?",
    )
    .bind(status)
    .bind(progress_key)
    .bind(error_message)
    .bind(id)
    .execute(&state.db)
    .await
    .context("Failed to finish update job")?;
    Ok(())
}

fn sidecar_client(state: &AppState) -> Result<SidecarUpdaterClient> {
    SidecarUpdaterClient::from_config(&state.config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sidecar_status(status: &str, progress_key: &str) -> SidecarStatusResponse {
        SidecarStatusResponse {
            status: status.to_string(),
            progress_key: Some(progress_key.to_string()),
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
            storage_metadata_ready: false,
            storage_metadata_generated_at: None,
            storage_cleanup_block_reason: None,
            managed_repositories: Vec::new(),
            protected_image_refs: Vec::new(),
        }
    }

    #[test]
    fn release_channels_accept_custom_values_but_not_blanks() {
        assert_eq!(normalize_release_channel(" unstable ").unwrap(), "unstable");
        assert_eq!(normalize_release_channel("preview").unwrap(), "preview");
        assert!(matches!(
            normalize_release_channel("   "),
            Err(UpdateSettingsError::InvalidReleaseChannel)
        ));
    }

    #[test]
    fn public_origin_can_be_cleared_and_rejects_non_origins() {
        assert_eq!(
            normalize_public_origin("https://selu.example.com/").unwrap(),
            "https://selu.example.com"
        );
        assert_eq!(normalize_public_origin("  ").unwrap(), "");
        assert!(matches!(
            normalize_public_origin("javascript:alert(1)"),
            Err(UpdateSettingsError::InvalidPublicOrigin)
        ));
        assert!(normalize_public_origin("https://user:secret@selu.example.com").is_err());
        assert!(normalize_public_origin("https://selu.example.com/path").is_err());
    }

    #[test]
    fn current_origin_removes_only_the_resolved_base_path() {
        assert_eq!(
            request_origin_without_base_path("https://selu.example.com/tenant", "/tenant"),
            "https://selu.example.com"
        );
        assert_eq!(
            request_origin_without_base_path("https://selu.example.com", ""),
            "https://selu.example.com"
        );
        assert_eq!(
            request_origin_without_base_path("https://selu.example.com/tenant-app", "/tenant"),
            "https://selu.example.com/tenant-app"
        );
    }

    #[test]
    fn running_status_keeps_the_active_job() {
        let mut current = UpdateState {
            active_job_id: "job-1".to_string(),
            status: STATUS_UPDATING.to_string(),
            last_error: "old error".to_string(),
            ..UpdateState::default()
        };
        let completion = merge_sidecar_status(
            &mut current,
            sidecar_status(STATUS_UPDATING, "updates.progress.pulling"),
        );
        assert_eq!(completion, None);
        assert_eq!(current.active_job_id, "job-1");
        assert_eq!(current.progress_key, "updates.progress.pulling");
    }

    #[test]
    fn terminal_apply_status_clears_the_job_and_refreshes_versions() {
        let mut current = UpdateState {
            active_job_id: "job-1".to_string(),
            status: STATUS_UPDATING.to_string(),
            last_error: "old error".to_string(),
            available_version: "selu:2".to_string(),
            available_digest: "sha256:new".to_string(),
            ..UpdateState::default()
        };
        let mut status = sidecar_status(STATUS_IDLE, "updates.progress.apply_done");
        status.installed_tag = Some("selu:2".to_string());
        status.installed_digest = Some("sha256:new".to_string());
        status.previous_tag = Some("selu:1".to_string());
        status.previous_digest = Some("sha256:old".to_string());

        let completion = merge_sidecar_status(&mut current, status);
        assert_eq!(
            completion,
            Some(JobCompletion {
                id: "job-1".to_string(),
                outcome: "completed",
            })
        );
        assert!(current.active_job_id.is_empty());
        assert!(current.last_error.is_empty());
        let response = status_response(&current);
        assert!(!response.update_available);
        assert!(response.rollback_available);
    }

    #[test]
    fn rollback_available_status_is_terminal_even_without_previous_metadata() {
        let mut current = UpdateState {
            active_job_id: "job-2".to_string(),
            status: STATUS_UPDATING.to_string(),
            ..UpdateState::default()
        };
        let completion = merge_sidecar_status(
            &mut current,
            sidecar_status(STATUS_ROLLBACK_AVAILABLE, "updates.progress.rollback_ready"),
        );
        assert_eq!(completion.unwrap().outcome, "completed");
        assert!(current.active_job_id.is_empty());
        assert!(status_response(&current).rollback_available);
    }

    #[test]
    fn failed_status_preserves_the_sidecar_error_and_finishes_failed() {
        let mut current = UpdateState {
            active_job_id: "job-3".to_string(),
            status: STATUS_UPDATING.to_string(),
            ..UpdateState::default()
        };
        let mut status = sidecar_status(STATUS_FAILED, "updates.progress.apply_failed");
        status.message = Some("health check failed".to_string());
        let completion = merge_sidecar_status(&mut current, status).unwrap();
        assert_eq!(completion.outcome, "failed");
        assert_eq!(current.last_error, "health check failed");
        assert!(current.active_job_id.is_empty());
    }
}
