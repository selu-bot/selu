//! Versioned JSON API for caller-owned automations.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, put},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    api::auth::ApiPrincipal,
    services::automations::{
        self, Automation, AutomationError, AutomationWrite, DeliveryDestination, TimingInput,
    },
    state::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/automations",
            get(list_automations).post(create_automation),
        )
        .route(
            "/api/v1/automation-destinations",
            get(list_automation_destinations),
        )
        .route(
            "/api/v1/automations/{automation_id}",
            get(get_automation)
                .put(update_automation)
                .delete(delete_automation),
        )
        .route(
            "/api/v1/automations/{automation_id}/state",
            axum::routing::patch(set_automation_state),
        )
        .route("/api/v1/users/me/timezone", put(set_user_timezone))
}

#[derive(Debug, Deserialize)]
struct WriteAutomationRequest {
    name: String,
    prompt: String,
    #[serde(default)]
    agent_id: Option<String>,
    pipe_ids: Vec<String>,
    timing: TimingInput,
}

#[derive(Debug, Deserialize)]
struct StateRequest {
    active: bool,
}

#[derive(Debug, Deserialize)]
struct TimezoneRequest {
    timezone: String,
}

#[derive(Debug, Serialize)]
struct AutomationListResponse {
    automations: Vec<Automation>,
}

#[derive(Debug, Serialize)]
struct AutomationDestinationsResponse {
    destinations: Vec<DeliveryDestination>,
}

#[derive(Debug, Serialize)]
struct TimezoneResponse {
    timezone: String,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

async fn list_automations(principal: ApiPrincipal, State(state): State<AppState>) -> Response {
    match automations::list_automations(&state.db, &principal.user_id).await {
        Ok(automations) => Json(AutomationListResponse { automations }).into_response(),
        Err(error) => service_error(error),
    }
}

async fn list_automation_destinations(
    principal: ApiPrincipal,
    State(state): State<AppState>,
) -> Response {
    match automations::list_delivery_destinations(&state.db, &principal.user_id).await {
        Ok(destinations) => Json(AutomationDestinationsResponse { destinations }).into_response(),
        Err(error) => service_error(error),
    }
}

async fn get_automation(
    principal: ApiPrincipal,
    State(state): State<AppState>,
    Path(automation_id): Path<String>,
) -> Response {
    match automations::get_automation(&state.db, &principal.user_id, &automation_id).await {
        Ok(Some(automation)) => Json(automation).into_response(),
        Ok(None) => not_found(),
        Err(error) => service_error(error),
    }
}

async fn create_automation(
    principal: ApiPrincipal,
    State(state): State<AppState>,
    body: Bytes,
) -> Response {
    let request: WriteAutomationRequest = match decode_json(body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(error) =
        automations::validate_agent_access(&state, &principal.user_id, request.agent_id.as_deref())
            .await
    {
        return service_error(error);
    }
    let timing = match automations::normalize_timing(
        &state,
        &principal.user_id,
        request.agent_id.as_deref(),
        request.timing,
    )
    .await
    {
        Ok(timing) => timing,
        Err(error) => return service_error(error),
    };
    let write = AutomationWrite {
        name: request.name,
        prompt: request.prompt,
        agent_id: request.agent_id,
        pipe_ids: request.pipe_ids,
        timing,
    };
    match automations::create_automation(&state.db, &principal.user_id, write).await {
        Ok(automation) => (StatusCode::CREATED, Json(automation)).into_response(),
        Err(error) => service_error(error),
    }
}

async fn update_automation(
    principal: ApiPrincipal,
    State(state): State<AppState>,
    Path(automation_id): Path<String>,
    body: Bytes,
) -> Response {
    let request: WriteAutomationRequest = match decode_json(body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(error) =
        automations::validate_agent_access(&state, &principal.user_id, request.agent_id.as_deref())
            .await
    {
        return service_error(error);
    }
    let timing = match automations::normalize_timing(
        &state,
        &principal.user_id,
        request.agent_id.as_deref(),
        request.timing,
    )
    .await
    {
        Ok(timing) => timing,
        Err(error) => return service_error(error),
    };
    let write = AutomationWrite {
        name: request.name,
        prompt: request.prompt,
        agent_id: request.agent_id,
        pipe_ids: request.pipe_ids,
        timing,
    };
    match automations::update_automation(&state.db, &principal.user_id, &automation_id, write).await
    {
        Ok(automation) => Json(automation).into_response(),
        Err(error) => service_error(error),
    }
}

async fn delete_automation(
    principal: ApiPrincipal,
    State(state): State<AppState>,
    Path(automation_id): Path<String>,
) -> Response {
    match automations::delete_automation(&state.db, &principal.user_id, &automation_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => service_error(error),
    }
}

async fn set_automation_state(
    principal: ApiPrincipal,
    State(state): State<AppState>,
    Path(automation_id): Path<String>,
    body: Bytes,
) -> Response {
    let request: StateRequest = match decode_json(body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    match automations::set_automation_state(
        &state.db,
        &principal.user_id,
        &automation_id,
        request.active,
    )
    .await
    {
        Ok(automation) => Json(automation).into_response(),
        Err(error) => service_error(error),
    }
}

async fn set_user_timezone(
    principal: ApiPrincipal,
    State(state): State<AppState>,
    body: Bytes,
) -> Response {
    let request: TimezoneRequest = match decode_json(body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    match automations::set_user_timezone(&state.db, &principal.user_id, &request.timezone).await {
        Ok(timezone) => Json(TimezoneResponse { timezone }).into_response(),
        Err(error) => service_error(error),
    }
}

fn decode_json<T: DeserializeOwned>(body: Bytes) -> Result<T, Response> {
    serde_json::from_slice(&body).map_err(|error| {
        json_error(
            StatusCode::BAD_REQUEST,
            "automation.invalid_json",
            format!("The request body is not valid JSON: {error}"),
        )
    })
}

fn service_error(error: AutomationError) -> Response {
    match error {
        AutomationError::NotFound => not_found(),
        AutomationError::BadRequest { code, message } => {
            json_error(StatusCode::BAD_REQUEST, code, message)
        }
        AutomationError::Validation { code, message } => {
            json_error(StatusCode::UNPROCESSABLE_ENTITY, code, message)
        }
        AutomationError::Conflict { code, message } => {
            json_error(StatusCode::CONFLICT, code, message)
        }
        AutomationError::Unavailable { code, message } => {
            json_error(StatusCode::SERVICE_UNAVAILABLE, code, message)
        }
        AutomationError::Database(error) => {
            tracing::error!("Automation API request failed: {error:#}");
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "automation.internal_error",
                "The automation request could not be completed.",
            )
        }
    }
}

fn not_found() -> Response {
    json_error(
        StatusCode::NOT_FOUND,
        "automation.not_found",
        "The automation was not found.",
    )
}

fn json_error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorEnvelope {
            error: ErrorBody {
                code,
                message: message.into(),
            },
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[test]
    fn router_registers_as_app_state_router() {
        let _: Router<AppState> = router();
    }

    #[tokio::test]
    async fn malformed_json_has_stable_json_error() {
        let response = decode_json::<StateRequest>(Bytes::from_static(b"not-json")).unwrap_err();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "application/json"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["code"], "automation.invalid_json");
    }

    #[test]
    fn service_errors_map_to_stable_statuses() {
        let cases = [
            (
                AutomationError::BadRequest {
                    code: "bad",
                    message: "bad".to_string(),
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                AutomationError::Validation {
                    code: "invalid",
                    message: "invalid".to_string(),
                },
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                AutomationError::Conflict {
                    code: "conflict",
                    message: "conflict".to_string(),
                },
                StatusCode::CONFLICT,
            ),
            (
                AutomationError::Unavailable {
                    code: "unavailable",
                    message: "unavailable".to_string(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(service_error(error).status(), expected);
        }
    }
}
