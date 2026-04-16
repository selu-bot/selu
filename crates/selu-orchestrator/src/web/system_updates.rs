use anyhow::{Context, Result, anyhow};
use askama::Template;
use axum::{
    Form, Json,
    extract::{Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::state::AppState;
use crate::updater::client::SidecarUpdaterClient;
use crate::updater::types::{SidecarApplyRequest, SidecarCheckRequest, SidecarRollbackRequest};
use crate::web::auth::AuthUser;
use crate::web::{BasePath, ExternalOrigin, prefixed, prefixed_redirect};

const STATUS_IDLE: &str = "idle";
const STATUS_CHECKING: &str = "checking";
const STATUS_UPDATE_AVAILABLE: &str = "update_available";
const STATUS_UPDATING: &str = "updating";
const STATUS_ROLLBACK_AVAILABLE: &str = "rollback_available";
const STATUS_FAILED: &str = "failed";

#[derive(Template)]
#[template(path = "system_updates.html")]
struct SystemUpdatesTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    release_channel: String,
    available_channels: Vec<ChannelOption>,
    auto_update: bool,
    #[allow(dead_code)]
    installation_telemetry_opt_out: bool,
    push_notifications_enabled: bool,
    public_origin: String,
    current_origin_display: String,
    installed_display: String,
    available_display: String,
    available_changelog_url: String,
    available_changelog_body: String,
    last_checked_at: String,
    last_attempt_at: String,
    active_job_id: String,
    last_error: String,
    update_available: bool,
    rollback_available: bool,
    status_key: String,
    status_fallback: String,
    progress_key: String,
    progress_fallback: String,
    error_key: Option<String>,
    error_fallback: Option<String>,
    success_key: Option<String>,
    success_fallback: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct ChannelSummary {
    channel: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    build_number: Option<String>,
    #[serde(default)]
    published_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ChannelsApiResponse {
    channels: Vec<ChannelSummary>,
}

#[derive(Debug, Deserialize)]
pub struct UpdatesQuery {
    pub error_key: Option<String>,
    pub success_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChannelForm {
    pub release_channel: String,
}

#[derive(Debug, Deserialize)]
pub struct ToggleForm {
    pub auto_update: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InstallationTelemetryForm {
    pub installation_telemetry_opt_out: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PushNotificationsForm {
    pub push_notifications_enabled: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PublicOriginForm {
    pub external_url: String,
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

#[derive(Debug, Clone)]
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
pub struct UpdateJobStatusResponse {
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

#[derive(Debug, Serialize)]
pub struct UpdateActionResponse {
    pub ok: bool,
    pub error_key: Option<String>,
    pub message_key: Option<String>,
}

pub async fn updates_index(
    user: AuthUser,
    Query(q): Query<UpdatesQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    ExternalOrigin(external_origin): ExternalOrigin,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }

    if let Err(e) = ensure_defaults(&state).await {
        error!("Failed to load update settings: {e}");
        return prefixed_redirect(&base_path, "/chat?error=Couldn%27t+load+Settings.")
            .into_response();
    }

    let settings = match load_settings(&state).await {
        Ok(v) => v,
        Err(e) => {
            error!("Failed to fetch update settings: {e}");
            return prefixed_redirect(&base_path, "/chat?error=Couldn%27t+load+Settings.")
                .into_response();
        }
    };
    let mut state_row = match load_state(&state).await {
        Ok(v) => v,
        Err(e) => {
            error!("Failed to fetch update state: {e}");
            return prefixed_redirect(&base_path, "/chat?error=Couldn%27t+load+Settings+status.")
                .into_response();
        }
    };

    hydrate_installed_from_sidecar(&state, &settings, &mut state_row).await;

    let available_channels = fetch_channel_options(&state).await;
    let current_origin_display = request_origin_without_base_path(&external_origin, &base_path);

    let update_available = is_update_available(&state_row);
    let rollback_available = !state_row.previous_version.trim().is_empty()
        || state_row.status == STATUS_ROLLBACK_AVAILABLE;
    let installed_display = format_release_display(
        &state_row.installed_release_version,
        &state_row.installed_build_number,
        &state_row.installed_version,
        &state_row.installed_digest,
    );
    let available_display = format_release_display(
        &state_row.available_release_version,
        &state_row.available_build_number,
        &state_row.available_version,
        &state_row.available_digest,
    );
    let (error_fallback, success_fallback) = (
        q.error_key
            .as_deref()
            .map(fallback_text_for_key)
            .map(str::to_string),
        q.success_key
            .as_deref()
            .map(fallback_text_for_key)
            .map(str::to_string),
    );

    match (SystemUpdatesTemplate {
        active_nav: "system_updates",
        is_admin: user.is_admin,
        base_path,
        release_channel: settings.release_channel,
        available_channels,
        auto_update: settings.auto_update,
        installation_telemetry_opt_out: settings.installation_telemetry_opt_out,
        push_notifications_enabled: settings.push_notifications_enabled,
        public_origin: settings.external_url.clone(),
        current_origin_display,
        installed_display,
        available_display,
        available_changelog_url: state_row.available_changelog_url.clone(),
        available_changelog_body: state_row.available_changelog_body.clone(),
        last_checked_at: state_row.last_checked_at,
        last_attempt_at: state_row.last_attempt_at,
        active_job_id: state_row.active_job_id,
        last_error: state_row.last_error,
        update_available,
        rollback_available,
        status_key: status_key_for(&state_row.status).to_string(),
        status_fallback: fallback_text_for_key(status_key_for(&state_row.status)).to_string(),
        progress_key: state_row.progress_key.clone(),
        progress_fallback: fallback_text_for_key(&state_row.progress_key).to_string(),
        error_key: q.error_key,
        error_fallback,
        success_key: q.success_key,
        success_fallback,
    })
    .render()
    {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn updates_set_channel(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<ChannelForm>,
) -> Redirect {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat");
    }

    let channel = form.release_channel.trim().to_string();
    if channel.is_empty() {
        return redirect_with_error(&base_path, "updates.error.invalid_channel");
    }

    if let Err(e) = sqlx::query(
        "UPDATE system_update_settings
         SET release_channel = ?, updated_at = datetime('now')
         WHERE id = 'global'",
    )
    .bind(&channel)
    .execute(&state.db)
    .await
    {
        error!("Failed to save update channel: {e}");
        return redirect_with_error(&base_path, "updates.error.channel_save_failed");
    }

    redirect_with_success(&base_path, "updates.flash.channel_saved")
}

pub async fn updates_set_public_origin(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<PublicOriginForm>,
) -> Redirect {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat");
    }

    let normalized = match normalize_public_origin(&form.external_url) {
        Ok(value) => value,
        Err(_) => return redirect_with_error(&base_path, "updates.error.public_origin_invalid"),
    };

    if let Err(e) = save_public_origin(&state, &normalized).await {
        error!("Failed to save public web address: {e}");
        return redirect_with_error(&base_path, "updates.error.public_origin_save_failed");
    }

    redirect_with_success(&base_path, "updates.flash.public_origin_saved")
}

pub async fn updates_apply_current_host(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    ExternalOrigin(external_origin): ExternalOrigin,
) -> Redirect {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat");
    }

    let current_origin = request_origin_without_base_path(&external_origin, &base_path);
    let normalized = match normalize_public_origin(&current_origin) {
        Ok(value) => value,
        Err(_) => return redirect_with_error(&base_path, "updates.error.public_origin_invalid"),
    };

    if let Err(e) = save_public_origin(&state, &normalized).await {
        error!("Failed to apply current host as public web address: {e}");
        return redirect_with_error(&base_path, "updates.error.public_origin_save_failed");
    }

    redirect_with_success(&base_path, "updates.flash.public_origin_applied")
}

pub async fn updates_toggle_auto_update(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<ToggleForm>,
) -> Redirect {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat");
    }

    let auto_update = if form.auto_update.as_deref() == Some("1") {
        1
    } else {
        0
    };

    if let Err(e) = sqlx::query(
        "UPDATE system_update_settings
         SET auto_update = ?, updated_at = datetime('now')
         WHERE id = 'global'",
    )
    .bind(auto_update)
    .execute(&state.db)
    .await
    {
        error!("Failed to save auto-update setting: {e}");
        return redirect_with_error(&base_path, "updates.error.auto_update_save_failed");
    }

    redirect_with_success(&base_path, "updates.flash.auto_update_saved")
}

pub async fn updates_toggle_installation_telemetry(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<InstallationTelemetryForm>,
) -> Redirect {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat");
    }

    let installation_telemetry_opt_out =
        if form.installation_telemetry_opt_out.as_deref() == Some("1") {
            1
        } else {
            0
        };

    if let Err(e) = sqlx::query(
        "UPDATE system_update_settings
         SET installation_telemetry_opt_out = ?, updated_at = datetime('now')
         WHERE id = 'global'",
    )
    .bind(installation_telemetry_opt_out)
    .execute(&state.db)
    .await
    {
        error!("Failed to save installation telemetry setting: {e}");
        return redirect_with_error(
            &base_path,
            "updates.error.installation_telemetry_save_failed",
        );
    }

    redirect_with_success(&base_path, "updates.flash.installation_telemetry_saved")
}

pub async fn updates_toggle_push_notifications(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<PushNotificationsForm>,
) -> Redirect {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat");
    }

    let enabled = if form.push_notifications_enabled.as_deref() == Some("1") {
        1
    } else {
        0
    };

    if let Err(e) = sqlx::query(
        "UPDATE system_update_settings
         SET push_notifications_enabled = ?, updated_at = datetime('now')
         WHERE id = 'global'",
    )
    .bind(enabled)
    .execute(&state.db)
    .await
    {
        error!("Failed to save push notifications setting: {e}");
        return redirect_with_error(
            &base_path,
            "updates.error.push_notifications_save_failed",
        );
    }

    redirect_with_success(&base_path, "updates.flash.push_notifications_saved")
}

pub async fn updates_check_now(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Redirect {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat");
    }

    match check_for_updates(&state).await {
        Ok(true) => redirect_with_success(&base_path, "updates.flash.check_available"),
        Ok(false) => redirect_with_success(&base_path, "updates.flash.check_none"),
        Err(e) => {
            error!("Update check failed: {e}");
            let _ = set_failure_state(
                &state,
                "updates.progress.check_failed",
                "Could not check for updates.",
            )
            .await;
            redirect_with_error(&base_path, "updates.error.check_failed")
        }
    }
}

pub async fn updates_job_status(user: AuthUser, State(state): State<AppState>) -> Response {
    if !user.is_admin {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    match load_state(&state).await {
        Ok(mut s) => {
            let settings = load_settings(&state).await.unwrap_or(UpdateSettings {
                release_channel: "stable".to_string(),
                auto_check: true,
                auto_update: true,
                installation_telemetry_opt_out: false,
                external_url: String::new(),
                push_notifications_enabled: true,
            });
            hydrate_installed_from_sidecar(&state, &settings, &mut s).await;

            if !s.active_job_id.trim().is_empty() {
                let status_result = match sidecar_client(&state) {
                    Ok(client) => client.status().await,
                    Err(e) => Err(e),
                };

                match status_result {
                    Ok(status) => {
                        let previous_active_job_id = s.active_job_id.clone();
                        s.status = status.status;
                        if let Some(progress_key) = status.progress_key {
                            if !progress_key.trim().is_empty() {
                                s.progress_key = progress_key;
                            }
                        }
                        if let Some(job_id) = status.job_id {
                            if !job_id.trim().is_empty() {
                                s.active_job_id = job_id;
                            }
                        }
                        if let Some(installed_tag) = status.installed_tag {
                            if !installed_tag.trim().is_empty() {
                                s.installed_version = installed_tag;
                            }
                        }
                        if let Some(installed_digest) = status.installed_digest {
                            if !installed_digest.trim().is_empty() {
                                s.installed_digest = installed_digest;
                            }
                        }
                        if let Some(installed_version) = status.installed_version {
                            if !installed_version.trim().is_empty() {
                                s.installed_release_version = installed_version;
                            }
                        }
                        if let Some(installed_build) = status.installed_build {
                            if !installed_build.trim().is_empty() {
                                s.installed_build_number = installed_build;
                            }
                        }
                        if let Some(previous_tag) = status.previous_tag {
                            if !previous_tag.trim().is_empty() {
                                s.previous_version = previous_tag;
                            }
                        }
                        if let Some(previous_digest) = status.previous_digest {
                            if !previous_digest.trim().is_empty() {
                                s.previous_digest = previous_digest;
                            }
                        }
                        if let Some(previous_version) = status.previous_version {
                            if !previous_version.trim().is_empty() {
                                s.previous_release_version = previous_version;
                            }
                        }
                        if let Some(previous_build) = status.previous_build {
                            if !previous_build.trim().is_empty() {
                                s.previous_build_number = previous_build;
                            }
                        }
                        if let Some(message) = status.message {
                            if !message.trim().is_empty() && s.status == STATUS_FAILED {
                                s.last_error = message;
                            } else if s.status != STATUS_FAILED {
                                s.last_error.clear();
                            }
                        }

                        let terminal =
                            !matches!(s.status.as_str(), STATUS_UPDATING | STATUS_CHECKING);
                        if terminal {
                            if s.status != STATUS_FAILED {
                                s.last_error.clear();
                            }
                            s.active_job_id.clear();
                            if !previous_active_job_id.trim().is_empty() {
                                let outcome = if s.status == STATUS_FAILED {
                                    "failed"
                                } else {
                                    "completed"
                                };
                                let _ = finish_update_job(
                                    &state,
                                    &previous_active_job_id,
                                    outcome,
                                    &s.progress_key,
                                    &s.last_error,
                                )
                                .await;
                            }
                        }
                        let _ = persist_state(&state, &s).await;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to poll updater sidecar for job {}: {e:#}",
                            s.active_job_id
                        );
                        s.status = STATUS_FAILED.to_string();
                        s.progress_key = "updates.progress.apply_failed".to_string();
                        s.last_error = format!("Lost contact with updater sidecar: {e}");
                        let previous_active_job_id = s.active_job_id.clone();
                        s.active_job_id.clear();
                        if !previous_active_job_id.trim().is_empty() {
                            let _ = finish_update_job(
                                &state,
                                &previous_active_job_id,
                                "failed",
                                &s.progress_key,
                                &s.last_error,
                            )
                            .await;
                        }
                        let _ = persist_state(&state, &s).await;
                    }
                }
            }

            Json(UpdateJobStatusResponse {
                active_job_id: s.active_job_id.clone(),
                status: s.status.clone(),
                progress_key: s.progress_key.clone(),
                last_error: s.last_error.clone(),
                last_checked_at: s.last_checked_at.clone(),
                last_attempt_at: s.last_attempt_at.clone(),
                installed_version: s.installed_version.clone(),
                installed_display: format_release_display(
                    &s.installed_release_version,
                    &s.installed_build_number,
                    &s.installed_version,
                    &s.installed_digest,
                ),
                installed_release_version: s.installed_release_version.clone(),
                installed_build_number: s.installed_build_number.clone(),
                available_version: s.available_version.clone(),
                available_display: format_release_display(
                    &s.available_release_version,
                    &s.available_build_number,
                    &s.available_version,
                    &s.available_digest,
                ),
                available_release_version: s.available_release_version.clone(),
                available_build_number: s.available_build_number.clone(),
                available_changelog_url: s.available_changelog_url.clone(),
                available_changelog_body: s.available_changelog_body.clone(),
                previous_version: s.previous_version.clone(),
                previous_display: format_release_display(
                    &s.previous_release_version,
                    &s.previous_build_number,
                    &s.previous_version,
                    &s.previous_digest,
                ),
                previous_release_version: s.previous_release_version.clone(),
                previous_build_number: s.previous_build_number.clone(),
                update_available: is_update_available(&s),
                rollback_available: !s.previous_version.trim().is_empty()
                    || s.status == STATUS_ROLLBACK_AVAILABLE,
            })
            .into_response()
        }
        Err(e) => {
            error!("Failed to load update job status: {e}");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn updates_apply_now(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Redirect {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat");
    }

    match apply_update(&state).await {
        Ok(()) => redirect_with_success(&base_path, "updates.flash.apply_started"),
        Err(e) => {
            error!("Update apply failed: {e}");
            redirect_with_error(&base_path, "updates.error.apply_failed")
        }
    }
}

pub async fn updates_apply_start(user: AuthUser, State(state): State<AppState>) -> Response {
    if !user.is_admin {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    match apply_update(&state).await {
        Ok(()) => Json(UpdateActionResponse {
            ok: true,
            error_key: None,
            message_key: Some("updates.flash.apply_started".to_string()),
        })
        .into_response(),
        Err(e) => {
            error!("Update apply failed: {e}");
            (
                axum::http::StatusCode::BAD_REQUEST,
                Json(UpdateActionResponse {
                    ok: false,
                    error_key: Some("updates.error.apply_failed".to_string()),
                    message_key: None,
                }),
            )
                .into_response()
        }
    }
}

pub async fn updates_rollback_now(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Redirect {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat");
    }

    match rollback_update(&state).await {
        Ok(()) => redirect_with_success(&base_path, "updates.flash.rollback_started"),
        Err(e) => {
            error!("Rollback failed: {e}");
            redirect_with_error(&base_path, "updates.error.rollback_failed")
        }
    }
}

pub async fn updates_rollback_start(user: AuthUser, State(state): State<AppState>) -> Response {
    if !user.is_admin {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    match rollback_update(&state).await {
        Ok(()) => Json(UpdateActionResponse {
            ok: true,
            error_key: None,
            message_key: Some("updates.flash.rollback_started".to_string()),
        })
        .into_response(),
        Err(e) => {
            error!("Rollback failed: {e}");
            (
                axum::http::StatusCode::BAD_REQUEST,
                Json(UpdateActionResponse {
                    ok: false,
                    error_key: Some("updates.error.rollback_failed".to_string()),
                    message_key: None,
                }),
            )
                .into_response()
        }
    }
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

async fn apply_update(state: &AppState) -> Result<()> {
    ensure_defaults(state).await?;
    let settings = load_settings(state).await?;
    apply_update_via_sidecar(state, &settings).await
}

async fn rollback_update(state: &AppState) -> Result<()> {
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
    let seeded_channel =
        std::env::var("SELU_RELEASE_CHANNEL").unwrap_or_else(|_| "stable".to_string());
    let seeded_channel = if seeded_channel.trim().is_empty() {
        "stable".to_string()
    } else {
        seeded_channel.trim().to_string()
    };

    info!("ensure_defaults: SELU_RELEASE_CHANNEL={seeded_channel}");

    sqlx::query(
        "INSERT OR IGNORE INTO system_update_settings
         (id, release_channel, auto_check, auto_update, external_url, installation_telemetry_opt_out)
         VALUES ('global', ?, 1, 1, '', 0)",
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
        Ok(Some(origin)) => state
            .public_origin_override
            .store(Some(std::sync::Arc::new(origin))),
        Ok(None) => {}
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

async fn save_public_origin(state: &AppState, value: &str) -> Result<()> {
    sqlx::query(
        "UPDATE system_update_settings
         SET external_url = ?, updated_at = datetime('now')
         WHERE id = 'global'",
    )
    .bind(value)
    .execute(&state.db)
    .await
    .context("Failed to update public web address setting")?;

    state
        .public_origin_override
        .store(Some(std::sync::Arc::new(value.to_string())));
    Ok(())
}

fn normalize_public_origin(input: &str) -> Result<String> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(anyhow!("public web address is empty"));
    }

    let parsed = reqwest::Url::parse(trimmed).context("invalid public web address")?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(anyhow!("public web address must use http or https"));
    }
    if parsed.host_str().unwrap_or_default().trim().is_empty() {
        return Err(anyhow!("public web address is missing a host"));
    }
    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(anyhow!(
            "public web address must not include a path, query, or fragment"
        ));
    }

    Ok(trimmed.to_string())
}

fn request_origin_without_base_path(external_origin: &str, base_path: &str) -> String {
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

fn status_key_for(status: &str) -> &'static str {
    match status {
        STATUS_CHECKING => "updates.state.checking",
        STATUS_UPDATE_AVAILABLE => "updates.state.update_available",
        STATUS_UPDATING => "updates.state.updating",
        STATUS_ROLLBACK_AVAILABLE => "updates.state.rollback_available",
        STATUS_FAILED => "updates.state.failed",
        _ => "updates.state.idle",
    }
}

fn fallback_text_for_key(key: &str) -> &'static str {
    match key {
        "updates.error.invalid_channel" => "Please choose a valid release channel.",
        "updates.error.channel_save_failed" => "Could not save your release channel.",
        "updates.error.auto_update_save_failed" => "Could not save automatic updates.",
        "updates.error.installation_telemetry_save_failed" => {
            "Could not save anonymous installation statistics setting."
        }
        "updates.error.push_notifications_save_failed" => {
            "Could not save push notification setting."
        }
        "updates.error.public_origin_invalid" => {
            "Enter a full web address like https://selu.example.com."
        }
        "updates.error.public_origin_save_failed" => "Could not save the public web address.",
        "updates.error.check_failed" => "Could not check for updates. Please try again.",
        "updates.error.apply_failed" => {
            "Update failed. Your previous version was kept when possible."
        }
        "updates.error.rollback_failed" => "Rollback failed. Please check your Docker setup.",
        "updates.flash.channel_saved" => "Release channel saved.",
        "updates.flash.public_origin_saved" => "Public web address saved.",
        "updates.flash.public_origin_applied" => "Current host applied as the public web address.",
        "updates.flash.auto_update_saved" => "Automatic update setting saved.",
        "updates.flash.installation_telemetry_saved" => {
            "Anonymous installation statistics setting saved."
        }
        "updates.flash.push_notifications_saved" => "Push notification setting saved.",
        "updates.flash.check_available" => "Update check complete. A new version is available.",
        "updates.flash.check_none" => "Update check complete. You are already up to date.",
        "updates.flash.apply_started" => "Update started.",
        "updates.flash.apply_done" => "Update complete.",
        "updates.flash.rollback_started" => "Rollback started.",
        "updates.flash.rollback_done" => "Rollback complete.",
        "updates.state.idle" => "Idle",
        "updates.state.checking" => "Checking for updates",
        "updates.state.update_available" => "Update available",
        "updates.state.updating" => "Updating",
        "updates.state.rollback_available" => "Rollback available",
        "updates.state.failed" => "Needs attention",
        "updates.progress.idle" => "Waiting for the next check.",
        "updates.progress.checking" => "Checking release metadata.",
        "updates.progress.check_failed" => "Could not check for updates.",
        "updates.progress.update_available" => "A newer version is ready.",
        "updates.progress.preparing" => "Preparing update settings.",
        "updates.progress.pulling" => "Downloading the new image.",
        "updates.progress.restarting" => "Restarting Selu.",
        "updates.progress.health_check" => "Running health checks.",
        "updates.progress.apply_done" => "Update finished successfully.",
        "updates.progress.apply_failed" => "Update failed.",
        "updates.progress.rollback_ready" => "A rollback is available.",
        "updates.progress.rollback_start" => "Starting rollback.",
        "updates.progress.rollback_restarting" => "Restarting previous version.",
        "updates.progress.rollback_done" => "Rollback finished successfully.",
        "updates.progress.already_current" => "You are already on the latest version.",
        _ => "Done.",
    }
}

async fn fetch_channel_options(state: &AppState) -> Vec<ChannelOption> {
    let url = format!(
        "{}/channels",
        state.config.release_metadata_url.trim_end_matches('/')
    );
    let fallback = vec![
        ChannelOption {
            value: "stable".to_string(),
            label: "Stable".to_string(),
        },
        ChannelOption {
            value: "unstable".to_string(),
            label: "Unstable".to_string(),
        },
    ];

    let resp = match reqwest::Client::new().get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return fallback,
    };

    let body: ChannelsApiResponse = match resp.json().await {
        Ok(b) => b,
        Err(_) => return fallback,
    };

    if body.channels.is_empty() {
        return fallback;
    }

    body.channels
        .into_iter()
        .map(|ch| {
            let label = match ch.channel.as_str() {
                "stable" => "Stable".to_string(),
                "unstable" => "Unstable".to_string(),
                other => other.to_string(),
            };
            ChannelOption {
                value: ch.channel,
                label,
            }
        })
        .collect()
}

fn redirect_with_success(base_path: &str, key: &str) -> Redirect {
    Redirect::to(&prefixed(
        base_path,
        &format!("/system-updates?success_key={}", key),
    ))
}

fn redirect_with_error(base_path: &str, key: &str) -> Redirect {
    Redirect::to(&prefixed(
        base_path,
        &format!("/system-updates?error_key={}", key),
    ))
}
