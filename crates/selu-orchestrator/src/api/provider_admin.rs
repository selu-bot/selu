//! Versioned REST surface for provider administration and write-only secrets.
//!
//! Provider routes and system-secret routes require [`ApiAdmin`]. User-secret
//! routes without a user ID always use the authenticated principal. The only
//! cross-user routes live under the explicit `/api/v1/admin/users/{user_id}`
//! namespace and also require [`ApiAdmin`].

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::Serialize;

use crate::{
    api::auth::{ApiAdmin, ApiPrincipal},
    services::provider_admin::{
        self, ProviderConfigurationInput, SecretMetadata, SecretValueInput, ServiceError,
    },
    state::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        // Provider catalogue and configuration are admin-only.
        .route("/api/v1/providers", get(list_providers))
        .route("/api/v1/providers/{id}", get(read_provider))
        .route(
            "/api/v1/providers/{id}/configuration",
            put(put_provider_configuration).delete(delete_provider_configuration),
        )
        .route(
            "/api/v1/providers/{id}/connection-test",
            post(test_provider_connection),
        )
        .route("/api/v1/providers/{id}/models", get(list_provider_models))
        // System secrets are shared and therefore always admin-only.
        .route("/api/v1/secrets/system", get(list_all_system_secrets))
        .route(
            "/api/v1/secrets/system/{capability_id}",
            get(list_system_secrets),
        )
        .route(
            "/api/v1/secrets/system/{capability_id}/{name}",
            put(put_system_secret).delete(delete_system_secret),
        )
        // Caller-owned user secrets never accept a user ID from the request.
        .route("/api/v1/secrets/user", get(list_all_own_user_secrets))
        .route(
            "/api/v1/secrets/user/{capability_id}",
            get(list_own_user_secrets),
        )
        .route(
            "/api/v1/secrets/user/{capability_id}/{name}",
            put(put_own_user_secret).delete(delete_own_user_secret),
        )
        // Cross-user operations are intentionally explicit and admin-only.
        .route(
            "/api/v1/admin/users/{user_id}/secrets/{capability_id}",
            get(list_target_user_secrets),
        )
        .route(
            "/api/v1/admin/users/{user_id}/secrets/{capability_id}/{name}",
            put(put_target_user_secret).delete(delete_target_user_secret),
        )
}

async fn list_providers(
    _admin: ApiAdmin,
    State(state): State<AppState>,
) -> Result<Json<Vec<provider_admin::ProviderView>>, ApiError> {
    Ok(Json(provider_admin::provider_catalogue(&state.db).await?))
}

async fn read_provider(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
) -> Result<Json<provider_admin::ProviderView>, ApiError> {
    Ok(Json(
        provider_admin::get_provider(&state.db, &provider_id).await?,
    ))
}

async fn put_provider_configuration(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
    Json(input): Json<ProviderConfigurationInput>,
) -> Result<Json<provider_admin::ProviderView>, ApiError> {
    Ok(Json(
        provider_admin::put_provider_configuration(
            &state.db,
            &state.credentials,
            &state.provider_cache,
            &provider_id,
            input,
        )
        .await?,
    ))
}

async fn delete_provider_configuration(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    provider_admin::delete_provider_configuration(&state.db, &state.provider_cache, &provider_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct ConnectionTestResponse {
    ok: bool,
}

async fn test_provider_connection(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
) -> Result<Json<ConnectionTestResponse>, ApiError> {
    provider_admin::test_provider_connection(&state.db, &state.credentials, &provider_id).await?;
    Ok(Json(ConnectionTestResponse { ok: true }))
}

async fn list_provider_models(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
) -> Result<Json<Vec<crate::llm::models::ModelInfo>>, ApiError> {
    Ok(Json(
        provider_admin::provider_models(&state.db, &state.credentials, &provider_id).await?,
    ))
}

async fn list_all_system_secrets(
    _admin: ApiAdmin,
    State(state): State<AppState>,
) -> Result<Json<Vec<SecretMetadata>>, ApiError> {
    Ok(Json(
        provider_admin::list_all_system_secrets(&state.db).await?,
    ))
}

async fn list_system_secrets(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Path(capability_id): Path<String>,
) -> Result<Json<Vec<SecretMetadata>>, ApiError> {
    Ok(Json(
        provider_admin::list_system_secrets(&state.db, &capability_id).await?,
    ))
}

async fn put_system_secret(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Path((capability_id, name)): Path<(String, String)>,
    Json(input): Json<SecretValueInput>,
) -> Result<Json<SecretMetadata>, ApiError> {
    Ok(Json(
        provider_admin::put_system_secret(
            &state.db,
            &state.credentials,
            &capability_id,
            &name,
            &input.value,
        )
        .await?,
    ))
}

async fn delete_system_secret(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Path((capability_id, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    provider_admin::delete_system_secret(&state.db, &state.credentials, &capability_id, &name)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_all_own_user_secrets(
    principal: ApiPrincipal,
    State(state): State<AppState>,
) -> Result<Json<Vec<SecretMetadata>>, ApiError> {
    Ok(Json(
        provider_admin::list_all_user_secrets(&state.db, &principal.user_id).await?,
    ))
}

async fn list_own_user_secrets(
    principal: ApiPrincipal,
    State(state): State<AppState>,
    Path(capability_id): Path<String>,
) -> Result<Json<Vec<SecretMetadata>>, ApiError> {
    Ok(Json(
        provider_admin::list_user_secrets(&state.db, &principal.user_id, &capability_id).await?,
    ))
}

async fn put_own_user_secret(
    principal: ApiPrincipal,
    State(state): State<AppState>,
    Path((capability_id, name)): Path<(String, String)>,
    Json(input): Json<SecretValueInput>,
) -> Result<Json<SecretMetadata>, ApiError> {
    Ok(Json(
        provider_admin::put_user_secret(
            &state.db,
            &state.credentials,
            &principal.user_id,
            &capability_id,
            &name,
            &input.value,
        )
        .await?,
    ))
}

async fn delete_own_user_secret(
    principal: ApiPrincipal,
    State(state): State<AppState>,
    Path((capability_id, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    provider_admin::delete_user_secret(
        &state.db,
        &state.credentials,
        &principal.user_id,
        &capability_id,
        &name,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_target_user_secrets(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Path((user_id, capability_id)): Path<(String, String)>,
) -> Result<Json<Vec<SecretMetadata>>, ApiError> {
    Ok(Json(
        provider_admin::list_user_secrets(&state.db, &user_id, &capability_id).await?,
    ))
}

async fn put_target_user_secret(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Path((user_id, capability_id, name)): Path<(String, String, String)>,
    Json(input): Json<SecretValueInput>,
) -> Result<Json<SecretMetadata>, ApiError> {
    Ok(Json(
        provider_admin::put_user_secret(
            &state.db,
            &state.credentials,
            &user_id,
            &capability_id,
            &name,
            &input.value,
        )
        .await?,
    ))
}

async fn delete_target_user_secret(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Path((user_id, capability_id, name)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    provider_admin::delete_user_secret(
        &state.db,
        &state.credentials,
        &user_id,
        &capability_id,
        &name,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

struct ApiError(ServiceError);

impl From<ServiceError> for ApiError {
    fn from(error: ServiceError) -> Self {
        Self(error)
    }
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self.0 {
            ServiceError::UnknownProvider => (
                StatusCode::NOT_FOUND,
                "provider_not_found",
                "The provider does not exist.",
            ),
            ServiceError::NotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "The requested resource does not exist.",
            ),
            ServiceError::NotConfigured => (
                StatusCode::CONFLICT,
                "provider_not_configured",
                "The provider is not configured.",
            ),
            ServiceError::Validation(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The request is invalid.",
            ),
            ServiceError::Connection(_) => (
                StatusCode::BAD_GATEWAY,
                "connection_failed",
                "The provider connection test failed.",
            ),
            ServiceError::Database(_) | ServiceError::Credential(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "The request could not be completed.",
            ),
        };

        if status.is_server_error() {
            tracing::error!(error = %self.0, "Provider administration API request failed");
        }

        (
            status,
            Json(ErrorEnvelope {
                error: ErrorBody { code, message },
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;

    use super::*;

    async fn response_json(response: Response) -> serde_json::Value {
        let body = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn service_errors_have_stable_status_and_envelope() {
        let response = ApiError(ServiceError::UnknownProvider).into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "provider_not_found"
        );

        let response = ApiError(ServiceError::NotConfigured).into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "provider_not_configured"
        );

        let response =
            ApiError(ServiceError::Connection("upstream detail".to_string())).into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], "connection_failed");
        assert!(!body.to_string().contains("upstream detail"));
    }

    #[test]
    fn secret_input_cannot_be_serialized_as_a_response() {
        // The request type deliberately implements Deserialize but not
        // Serialize. This compile-time assertion protects the write-only API.
        fn accepts_deserialize_only<T: for<'de> serde::Deserialize<'de>>() {}
        accepts_deserialize_only::<SecretValueInput>();
    }
}
