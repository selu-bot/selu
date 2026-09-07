use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use serde::{Deserialize, Serialize};

use crate::{
    api::auth::ApiAdmin,
    services::system_updates::{self, UpdateSettingsError, UpdateSettingsPatch},
    state::AppState,
    web::{BasePath, ExternalOrigin},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/system-updates",
            get(overview).route_layer(axum::middleware::from_fn(no_store)),
        )
        .route("/api/v1/system-updates/settings", patch(update_settings))
        .route("/api/v1/system-updates/check", post(check_now))
        .route("/api/v1/system-updates/apply", post(apply))
        .route("/api/v1/system-updates/rollback", post(rollback))
        .route(
            "/api/v1/system-updates/status",
            get(status).route_layer(axum::middleware::from_fn(no_store)),
        )
}

async fn no_store(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

#[derive(Debug, Deserialize)]
struct UpdateSettingsRequest {
    release_channel: Option<String>,
    auto_update: Option<bool>,
    installation_telemetry_opt_out: Option<bool>,
    push_notifications_enabled: Option<bool>,
    public_origin: Option<String>,
}

#[derive(Debug, Serialize)]
struct ActionResponse {
    ok: bool,
    message_key: &'static str,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
}

fn error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (
        status,
        Json(ErrorEnvelope {
            error: ErrorBody { code, message },
        }),
    )
        .into_response()
}

async fn overview(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    ExternalOrigin(external_origin): ExternalOrigin,
) -> Response {
    match system_updates::get_settings(&state, &external_origin, &base_path).await {
        Ok(settings) => Json(settings).into_response(),
        Err(service_error) => {
            tracing::error!(error = %service_error, "Failed to load system update settings");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "system_updates_unavailable",
                "System update settings could not be loaded.",
            )
        }
    }
}

async fn update_settings(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Json(request): Json<UpdateSettingsRequest>,
) -> Response {
    let patch = UpdateSettingsPatch {
        release_channel: request.release_channel,
        auto_update: request.auto_update,
        installation_telemetry_opt_out: request.installation_telemetry_opt_out,
        push_notifications_enabled: request.push_notifications_enabled,
        public_origin: request.public_origin,
    };

    match system_updates::update_settings(&state, patch).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(UpdateSettingsError::InvalidReleaseChannel) => error(
            StatusCode::BAD_REQUEST,
            "invalid_release_channel",
            "Choose a supported release channel.",
        ),
        Err(UpdateSettingsError::InvalidPublicOrigin) => error(
            StatusCode::BAD_REQUEST,
            "invalid_public_origin",
            "Enter a full web address such as https://selu.example.com.",
        ),
        Err(service_error) => {
            tracing::error!(error = %service_error, "Failed to save system update settings");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "settings_not_saved",
                "The settings could not be saved.",
            )
        }
    }
}

async fn check_now(_admin: ApiAdmin, State(state): State<AppState>) -> Response {
    match system_updates::check_for_updates(&state).await {
        Ok(true) => Json(ActionResponse {
            ok: true,
            message_key: "updates.flash.check_available",
        })
        .into_response(),
        Ok(false) => Json(ActionResponse {
            ok: true,
            message_key: "updates.flash.check_none",
        })
        .into_response(),
        Err(action_error) => {
            tracing::error!(error = %action_error, "System update check failed");
            error(
                StatusCode::BAD_GATEWAY,
                "update_check_failed",
                "Selu could not check for updates right now.",
            )
        }
    }
}

async fn apply(_admin: ApiAdmin, State(state): State<AppState>) -> Response {
    match system_updates::apply_update(&state).await {
        Ok(()) => Json(ActionResponse {
            ok: true,
            message_key: "updates.flash.apply_started",
        })
        .into_response(),
        Err(action_error) => {
            tracing::error!(error = %action_error, "System update apply failed");
            error(
                StatusCode::BAD_REQUEST,
                "update_apply_failed",
                "The update could not be started.",
            )
        }
    }
}

async fn rollback(_admin: ApiAdmin, State(state): State<AppState>) -> Response {
    match system_updates::rollback_update(&state).await {
        Ok(()) => Json(ActionResponse {
            ok: true,
            message_key: "updates.flash.rollback_started",
        })
        .into_response(),
        Err(action_error) => {
            tracing::error!(error = %action_error, "System update rollback failed");
            error(
                StatusCode::BAD_REQUEST,
                "update_rollback_failed",
                "The previous version could not be restored.",
            )
        }
    }
}

async fn status(_admin: ApiAdmin, State(state): State<AppState>) -> Response {
    match system_updates::status(&state).await {
        Ok(status) => Json(status).into_response(),
        Err(service_error) => {
            tracing::error!(error = %service_error, "Failed to load system update status");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "system_update_status_unavailable",
                "System update status could not be loaded.",
            )
        }
    }
}
