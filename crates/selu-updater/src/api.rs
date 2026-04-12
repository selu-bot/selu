use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use tracing::{error, info};

use crate::engine;
use crate::state::AppState;
use crate::types::{
    AckResponse, ApplyRequest, CheckRequest, EnsureWhatsappBridgeRequest, RollbackRequest,
    StatusResponse, StopWhatsappBridgeRequest, WhatsappBridgeChatsResponse,
    WhatsappBridgeStatusResponse,
};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/update/check", post(update_check))
        .route("/update/apply", post(update_apply))
        .route("/update/rollback", post(update_rollback))
        .route("/update/status", get(update_status))
        .route("/sidecars/whatsapp/ensure", post(ensure_whatsapp_bridge))
        .route("/sidecars/whatsapp/status", get(whatsapp_bridge_status))
        .route("/sidecars/whatsapp/chats", get(whatsapp_bridge_chats))
        .route("/sidecars/whatsapp/stop", post(stop_whatsapp_bridge))
        .with_state(state)
}

async fn health() -> StatusCode {
    StatusCode::OK
}

async fn update_check(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CheckRequest>,
) -> (StatusCode, Json<AckResponse>) {
    if !is_authorized(&state, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(AckResponse {
                accepted: false,
                job_id: None,
                status: Some("failed".to_string()),
                progress_key: Some("updates.progress.apply_failed".to_string()),
                message: Some("Unauthorized updater request".to_string()),
            }),
        );
    }
    if req.target_tag.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(rejected("Check request must include target tag")),
        );
    }

    let installed = match engine::get_installed_image_state(&state).await {
        Ok(v) => v,
        Err(e) => {
            error!("Failed to resolve installed image state for update check: {e:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(rejected("Could not determine installed version")),
            );
        }
    };
    let has_update = if !req.target_digest.trim().is_empty() && !installed.digest.trim().is_empty()
    {
        req.target_digest.trim() != installed.digest.trim()
    } else {
        installed.tag.trim() != req.target_tag.trim()
    };

    let status = if has_update {
        "update_available"
    } else {
        "idle"
    };
    let progress_key = if has_update {
        "updates.progress.update_available"
    } else {
        "updates.progress.already_current"
    };

    (
        StatusCode::OK,
        Json(AckResponse {
            accepted: true,
            job_id: Some(req.request_id),
            status: Some(status.to_string()),
            progress_key: Some(progress_key.to_string()),
            message: Some(format!(
                "Channel {} checked{}{}",
                req.channel,
                if has_update { ": update available" } else { "" },
                " (digest-aware check)"
            )),
        }),
    )
}

async fn update_apply(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ApplyRequest>,
) -> (StatusCode, Json<AckResponse>) {
    if !is_authorized(&state, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(rejected("Unauthorized updater request")),
        );
    }
    if req.target_tag.trim().is_empty() || req.target_digest.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(rejected("Update request must include tag and digest")),
        );
    }

    let mut runtime = state.runtime.lock().await;
    if runtime.active_job_id.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(rejected("Another update job is already running")),
        );
    }

    runtime.active_job_id = Some(req.request_id.clone());
    runtime.status = "updating".to_string();
    runtime.progress_key = "updates.progress.preparing".to_string();
    runtime.message = "Update started".to_string();
    drop(runtime);

    let target_tag = req.target_tag.clone();
    let target_digest = req.target_digest.clone();
    let response_channel = req.channel.clone();
    let response_channel_for_worker = response_channel.clone();
    let spawn_state = state.clone();
    let job_id = req.request_id.clone();
    tokio::spawn(async move {
        if let Err(e) = engine::run_apply(
            spawn_state.clone(),
            job_id.clone(),
            response_channel_for_worker,
            target_tag,
            target_digest,
            req.target_version.clone(),
            req.target_build.clone(),
        )
        .await
        {
            error!(job_id = %job_id, "Sidecar apply failed: {e:#}");
            engine::set_runtime(
                &spawn_state,
                None,
                "failed",
                "updates.progress.apply_failed",
                "Update failed in updater sidecar",
            )
            .await;
        } else {
            info!(job_id = %job_id, "Sidecar apply finished");
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(AckResponse {
            accepted: true,
            job_id: Some(req.request_id),
            status: Some("updating".to_string()),
            progress_key: Some("updates.progress.preparing".to_string()),
            message: Some(format!(
                "Applying {} from channel {}",
                req.target_tag, response_channel
            )),
        }),
    )
}

async fn update_rollback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RollbackRequest>,
) -> (StatusCode, Json<AckResponse>) {
    if !is_authorized(&state, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(rejected("Unauthorized updater request")),
        );
    }
    if req.rollback_tag.trim().is_empty() || req.rollback_digest.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(rejected("Rollback request must include tag and digest")),
        );
    }

    let mut runtime = state.runtime.lock().await;
    if runtime.active_job_id.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(rejected("Another update job is already running")),
        );
    }

    runtime.active_job_id = Some(req.request_id.clone());
    runtime.status = "updating".to_string();
    runtime.progress_key = "updates.progress.rollback_start".to_string();
    runtime.message = "Rollback started".to_string();
    drop(runtime);

    let rollback_tag = req.rollback_tag.clone();
    let rollback_digest = req.rollback_digest.clone();
    let response_channel = req.channel.clone();
    let response_channel_for_worker = response_channel.clone();
    let spawn_state = state.clone();
    let job_id = req.request_id.clone();
    tokio::spawn(async move {
        if let Err(e) = engine::run_rollback(
            spawn_state.clone(),
            job_id.clone(),
            response_channel_for_worker,
            rollback_tag,
            rollback_digest,
            req.rollback_version.clone(),
            req.rollback_build.clone(),
        )
        .await
        {
            error!(job_id = %job_id, "Sidecar rollback failed: {e:#}");
            engine::set_runtime(
                &spawn_state,
                None,
                "failed",
                "updates.progress.apply_failed",
                "Rollback failed in updater sidecar",
            )
            .await;
        } else {
            info!(job_id = %job_id, "Sidecar rollback finished");
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(AckResponse {
            accepted: true,
            job_id: Some(req.request_id),
            status: Some("updating".to_string()),
            progress_key: Some("updates.progress.rollback_start".to_string()),
            message: Some(format!(
                "Rolling back to {} from channel {}",
                req.rollback_tag, response_channel
            )),
        }),
    )
}

async fn ensure_whatsapp_bridge(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<EnsureWhatsappBridgeRequest>,
) -> (StatusCode, Json<AckResponse>) {
    if !is_authorized(&state, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(rejected("Unauthorized updater request")),
        );
    }

    let channel = req.channel.trim().to_string();
    if channel.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(rejected("Channel is required to ensure WhatsApp bridge")),
        );
    }
    if req.inbound_url.trim().is_empty() || req.inbound_token.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(rejected(
                "Inbound URL and token are required to ensure WhatsApp bridge",
            )),
        );
    }

    match engine::ensure_whatsapp_bridge(
        &state,
        &channel,
        req.inbound_url.trim(),
        req.inbound_token.trim(),
        req.outbound_auth.trim(),
    )
    .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(AckResponse {
                accepted: true,
                job_id: Some(req.request_id),
                status: Some("idle".to_string()),
                progress_key: None,
                message: Some("WhatsApp bridge is running".to_string()),
            }),
        ),
        Err(e) => {
            error!("Failed to ensure WhatsApp bridge: {e:#}");
            let message = user_friendly_whatsapp_bridge_error(&e.to_string());
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AckResponse {
                    accepted: false,
                    job_id: Some(req.request_id),
                    status: Some("failed".to_string()),
                    progress_key: None,
                    message: Some(message),
                }),
            )
        }
    }
}

async fn whatsapp_bridge_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> (StatusCode, Json<WhatsappBridgeStatusResponse>) {
    if !is_authorized(&state, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(WhatsappBridgeStatusResponse {
                running: false,
                connection_state: None,
                requires_qr: false,
                qr_data_url: None,
                jid: None,
                last_error: None,
                message: Some("Unauthorized updater request".to_string()),
            }),
        );
    }

    match engine::whatsapp_bridge_status(&state).await {
        Ok(status) => (StatusCode::OK, Json(status)),
        Err(e) => {
            error!("Failed to fetch WhatsApp bridge status: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(WhatsappBridgeStatusResponse {
                    running: false,
                    connection_state: None,
                    requires_qr: false,
                    qr_data_url: None,
                    jid: None,
                    last_error: None,
                    message: Some("Could not read WhatsApp bridge status".to_string()),
                }),
            )
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct WhatsappChatsQuery {
    q: Option<String>,
}

async fn whatsapp_bridge_chats(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<WhatsappChatsQuery>,
) -> (StatusCode, Json<WhatsappBridgeChatsResponse>) {
    if !is_authorized(&state, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(WhatsappBridgeChatsResponse {
                running: false,
                connection_state: None,
                chats: Vec::new(),
                message: Some("Unauthorized updater request".to_string()),
            }),
        );
    }

    match engine::whatsapp_bridge_chats(&state, q.q.as_deref().unwrap_or("").trim()).await {
        Ok(resp) => (StatusCode::OK, Json(resp)),
        Err(e) => {
            error!("Failed to fetch WhatsApp chats: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(WhatsappBridgeChatsResponse {
                    running: false,
                    connection_state: None,
                    chats: Vec::new(),
                    message: Some("Could not load WhatsApp chats".to_string()),
                }),
            )
        }
    }
}

async fn stop_whatsapp_bridge(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<StopWhatsappBridgeRequest>,
) -> (StatusCode, Json<AckResponse>) {
    if !is_authorized(&state, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(rejected("Unauthorized updater request")),
        );
    }

    match engine::stop_whatsapp_bridge(&state, &req.channel).await {
        Ok(()) => (
            StatusCode::OK,
            Json(AckResponse {
                accepted: true,
                job_id: Some(req.request_id),
                status: Some("idle".to_string()),
                progress_key: None,
                message: Some("WhatsApp bridge stopped".to_string()),
            }),
        ),
        Err(e) => {
            error!("Failed to stop WhatsApp bridge: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AckResponse {
                    accepted: false,
                    job_id: Some(req.request_id),
                    status: Some("failed".to_string()),
                    progress_key: None,
                    message: Some("Could not stop WhatsApp bridge".to_string()),
                }),
            )
        }
    }
}

async fn update_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> (StatusCode, Json<StatusResponse>) {
    if !is_authorized(&state, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(StatusResponse {
                status: "failed".to_string(),
                progress_key: Some("updates.progress.apply_failed".to_string()),
                message: Some("Unauthorized updater request".to_string()),
                job_id: None,
                installed_tag: None,
                installed_digest: None,
                installed_version: None,
                installed_build: None,
                previous_tag: None,
                previous_digest: None,
                previous_version: None,
                previous_build: None,
            }),
        );
    }

    let mut runtime = state.runtime.lock().await.clone();
    match engine::get_installed_image_state(&state).await {
        Ok(installed) => {
            runtime.installed_tag = installed.tag;
            runtime.installed_digest = installed.digest;
        }
        Err(e) => {
            error!("Failed to resolve installed image state for status endpoint: {e:#}");
        }
    }
    if runtime.installed_version.trim().is_empty() {
        runtime.installed_version =
            engine::get_env_var(&state.config.compose_env_file, "SELU_IMAGE_VERSION")
                .await
                .unwrap_or_default();
    }
    if runtime.installed_build.trim().is_empty() {
        runtime.installed_build =
            engine::get_env_var(&state.config.compose_env_file, "SELU_IMAGE_BUILD")
                .await
                .unwrap_or_default();
    }
    (
        StatusCode::OK,
        Json(StatusResponse {
            status: runtime.status,
            progress_key: Some(runtime.progress_key),
            message: if runtime.message.is_empty() {
                None
            } else {
                Some(runtime.message)
            },
            job_id: runtime.active_job_id,
            installed_tag: if runtime.installed_tag.is_empty() {
                None
            } else {
                Some(runtime.installed_tag)
            },
            installed_digest: if runtime.installed_digest.is_empty() {
                None
            } else {
                Some(runtime.installed_digest)
            },
            installed_version: if runtime.installed_version.is_empty() {
                None
            } else {
                Some(runtime.installed_version)
            },
            installed_build: if runtime.installed_build.is_empty() {
                None
            } else {
                Some(runtime.installed_build)
            },
            previous_tag: if runtime.previous_tag.is_empty() {
                None
            } else {
                Some(runtime.previous_tag)
            },
            previous_digest: if runtime.previous_digest.is_empty() {
                None
            } else {
                Some(runtime.previous_digest)
            },
            previous_version: if runtime.previous_version.is_empty() {
                None
            } else {
                Some(runtime.previous_version)
            },
            previous_build: if runtime.previous_build.is_empty() {
                None
            } else {
                Some(runtime.previous_build)
            },
        }),
    )
}

fn is_authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let expected = state.config.shared_secret.trim();
    if expected.is_empty() {
        return true;
    }

    let Some(value) = headers.get("X-Selu-Updater-Secret") else {
        return false;
    };
    let Ok(received) = value.to_str() else {
        return false;
    };
    received == expected
}

fn rejected(message: &str) -> AckResponse {
    AckResponse {
        accepted: false,
        job_id: None,
        status: Some("failed".to_string()),
        progress_key: Some("updates.progress.apply_failed".to_string()),
        message: Some(message.to_string()),
    }
}

fn user_friendly_whatsapp_bridge_error(err: &str) -> String {
    let e = err.to_ascii_lowercase();
    if e.contains("connection refused")
        || e.contains("cannot connect to the docker daemon")
        || e.contains("failed to connect to docker daemon")
    {
        return "Could not start WhatsApp bridge: Docker is not running. Start Docker and try again."
            .to_string();
    }
    if e.contains("permission denied") {
        return "Could not start WhatsApp bridge: Docker permission denied for this user."
            .to_string();
    }
    if e.contains("pull access denied") || e.contains("manifest unknown") || e.contains("not found")
    {
        return "Could not start WhatsApp bridge: bridge image could not be pulled.".to_string();
    }
    if e.contains("invalid reference format") {
        return "Could not start WhatsApp bridge: invalid bridge image reference. Check UPDATER__WHATSAPP_BRIDGE_IMAGE_REPO."
            .to_string();
    }
    "Could not start WhatsApp bridge. Please try again.".to_string()
}
