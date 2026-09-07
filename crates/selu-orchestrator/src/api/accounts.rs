use axum::{
    Json, Router,
    extract::{FromRef, FromRequest, Path, Request, State, rejection::JsonRejection},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use axum_extra::extract::cookie::CookieJar;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sqlx::SqlitePool;

use crate::{
    api::auth::{ApiAdmin, ApiPrincipal},
    services::{
        accounts::{self, AccountError, CreateUser, GeneralFeedback, UpdateOwnProfile, UpdateUser},
        auth::SESSION_COOKIE,
    },
    state::AppState,
    web::ExternalOrigin,
};

#[derive(Clone)]
struct AccountsApiState {
    db: SqlitePool,
    marketplace_url: String,
    agent_ids: Vec<String>,
    client: reqwest::Client,
}

impl FromRef<AppState> for AccountsApiState {
    fn from_ref(state: &AppState) -> Self {
        let mut agent_ids = state
            .agents
            .load()
            .keys()
            .filter(|id| id.as_str() != "default")
            .cloned()
            .collect::<Vec<_>>();
        agent_ids.sort();
        Self {
            db: state.db.clone(),
            marketplace_url: state.config.marketplace_url.clone(),
            agent_ids,
            client: reqwest::Client::new(),
        }
    }
}

impl FromRef<AccountsApiState> for SqlitePool {
    fn from_ref(state: &AccountsApiState) -> Self {
        state.db.clone()
    }
}

/// Routes are relative to `/api/v1`; integrate with
/// `Router::nest("/api/v1", accounts::router())`.
pub fn router() -> Router<AppState> {
    routes::<AppState>()
}

fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    AccountsApiState: FromRef<S>,
    SqlitePool: FromRef<S>,
{
    Router::new()
        .route("/users/me", get(get_me).patch(patch_me))
        .route("/users/me/password", put(change_my_password))
        .route(
            "/users/me/profile-facts",
            get(list_my_profile_facts).post(create_my_profile_fact),
        )
        .route(
            "/users/me/profile-facts/{fact_id}",
            put(update_my_profile_fact).delete(delete_my_profile_fact),
        )
        .route(
            "/users/me/mobile-pairing-tokens",
            post(create_mobile_pairing_token),
        )
        .route("/users", get(list_users).post(create_user))
        .route(
            "/users/{user_id}",
            get(get_user).patch(patch_user).delete(delete_user),
        )
        .route("/users/{user_id}/agent-access", put(set_agent_access))
        .route("/feedback", post(submit_feedback))
}

struct ApiJson<T>(T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(json_rejection)
    }
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

fn json_rejection(_rejection: JsonRejection) -> Response {
    json_error(
        StatusCode::BAD_REQUEST,
        "invalid_json",
        "The request body must be valid JSON.",
    )
}

fn account_error(error: AccountError) -> Response {
    let status = match &error {
        AccountError::Invalid { .. } => StatusCode::BAD_REQUEST,
        AccountError::UserNotFound | AccountError::ProfileFactNotFound => StatusCode::NOT_FOUND,
        AccountError::UsernameTaken
        | AccountError::SelfDeletionForbidden
        | AccountError::LastAdminRequired => StatusCode::CONFLICT,
        AccountError::InvalidCurrentPassword => StatusCode::FORBIDDEN,
        AccountError::FeedbackUnavailable => StatusCode::BAD_GATEWAY,
        AccountError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if status.is_server_error() {
        tracing::error!(code = error.code(), "Account API operation failed");
    }
    json_error(status, error.code(), error.to_string())
}

async fn get_me(principal: ApiPrincipal, State(state): State<AccountsApiState>) -> Response {
    match accounts::get_user(&state.db, &principal.user_id).await {
        Ok(user) => Json(user).into_response(),
        Err(error) => account_error(error),
    }
}

#[derive(Debug, Deserialize)]
struct UpdateMeRequest {
    display_name: Option<String>,
    language: Option<String>,
    timezone: Option<String>,
}

async fn patch_me(
    principal: ApiPrincipal,
    State(state): State<AccountsApiState>,
    ApiJson(request): ApiJson<UpdateMeRequest>,
) -> Response {
    match accounts::update_own_profile(
        &state.db,
        &principal.user_id,
        UpdateOwnProfile {
            display_name: request.display_name,
            language: request.language,
            timezone: request.timezone,
        },
    )
    .await
    {
        Ok(user) => Json(user).into_response(),
        Err(error) => account_error(error),
    }
}

#[derive(Debug, Deserialize)]
struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

async fn change_my_password(
    principal: ApiPrincipal,
    State(state): State<AccountsApiState>,
    jar: CookieJar,
    ApiJson(request): ApiJson<ChangePasswordRequest>,
) -> Response {
    let current_session_id = jar.get(SESSION_COOKIE).map(|cookie| cookie.value());
    match accounts::change_password(
        &state.db,
        &principal.user_id,
        current_session_id,
        &request.current_password,
        &request.new_password,
    )
    .await
    {
        Ok(receipt) => Json(receipt).into_response(),
        Err(error) => account_error(error),
    }
}

async fn list_users(_admin: ApiAdmin, State(state): State<AccountsApiState>) -> Response {
    match accounts::list_users(&state.db).await {
        Ok(users) => Json(users).into_response(),
        Err(error) => account_error(error),
    }
}

#[derive(Debug, Deserialize)]
struct CreateUserRequest {
    username: String,
    #[serde(default)]
    display_name: String,
    password: String,
    #[serde(default)]
    is_admin: bool,
    #[serde(default = "default_language")]
    language: String,
    #[serde(default = "default_timezone")]
    timezone: String,
}

fn default_language() -> String {
    "en".to_string()
}

fn default_timezone() -> String {
    "UTC".to_string()
}

async fn create_user(
    _admin: ApiAdmin,
    State(state): State<AccountsApiState>,
    ApiJson(request): ApiJson<CreateUserRequest>,
) -> Response {
    match accounts::create_user(
        &state.db,
        CreateUser {
            username: request.username,
            display_name: request.display_name,
            password: request.password,
            is_admin: request.is_admin,
            language: request.language,
            timezone: request.timezone,
        },
    )
    .await
    {
        Ok(user) => (StatusCode::CREATED, Json(user)).into_response(),
        Err(error) => account_error(error),
    }
}

async fn get_user(
    _admin: ApiAdmin,
    State(state): State<AccountsApiState>,
    Path(user_id): Path<String>,
) -> Response {
    match accounts::get_user(&state.db, &user_id).await {
        Ok(user) => Json(user).into_response(),
        Err(error) => account_error(error),
    }
}

#[derive(Debug, Deserialize)]
struct UpdateUserRequest {
    username: Option<String>,
    display_name: Option<String>,
    is_admin: Option<bool>,
    language: Option<String>,
    timezone: Option<String>,
}

async fn patch_user(
    _admin: ApiAdmin,
    State(state): State<AccountsApiState>,
    Path(user_id): Path<String>,
    ApiJson(request): ApiJson<UpdateUserRequest>,
) -> Response {
    match accounts::update_user(
        &state.db,
        &user_id,
        UpdateUser {
            username: request.username,
            display_name: request.display_name,
            is_admin: request.is_admin,
            language: request.language,
            timezone: request.timezone,
        },
    )
    .await
    {
        Ok(user) => Json(user).into_response(),
        Err(error) => account_error(error),
    }
}

async fn delete_user(
    admin: ApiAdmin,
    State(state): State<AccountsApiState>,
    Path(user_id): Path<String>,
) -> Response {
    match accounts::delete_user(&state.db, &admin.user_id, &user_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => account_error(error),
    }
}

#[derive(Debug, Deserialize)]
struct AgentAccessRequest {
    agent_ids: Vec<String>,
}

async fn set_agent_access(
    _admin: ApiAdmin,
    State(state): State<AccountsApiState>,
    Path(user_id): Path<String>,
    ApiJson(request): ApiJson<AgentAccessRequest>,
) -> Response {
    match accounts::set_agent_access(&state.db, &user_id, &request.agent_ids, &state.agent_ids)
        .await
    {
        Ok(access) => Json(access).into_response(),
        Err(error) => account_error(error),
    }
}

async fn list_my_profile_facts(
    principal: ApiPrincipal,
    State(state): State<AccountsApiState>,
) -> Response {
    match accounts::list_profile_facts(&state.db, &principal.user_id).await {
        Ok(facts) => Json(facts).into_response(),
        Err(error) => account_error(error),
    }
}

#[derive(Debug, Deserialize)]
struct ProfileFactRequest {
    fact: String,
    category: Option<String>,
}

async fn create_my_profile_fact(
    principal: ApiPrincipal,
    State(state): State<AccountsApiState>,
    ApiJson(request): ApiJson<ProfileFactRequest>,
) -> Response {
    match accounts::create_profile_fact(
        &state.db,
        &principal.user_id,
        &request.fact,
        request.category.as_deref(),
    )
    .await
    {
        Ok(id) => (StatusCode::CREATED, Json(serde_json::json!({ "id": id }))).into_response(),
        Err(error) => account_error(error),
    }
}

async fn update_my_profile_fact(
    principal: ApiPrincipal,
    State(state): State<AccountsApiState>,
    Path(fact_id): Path<String>,
    ApiJson(request): ApiJson<ProfileFactRequest>,
) -> Response {
    match accounts::update_profile_fact(
        &state.db,
        &principal.user_id,
        &fact_id,
        &request.fact,
        request.category.as_deref(),
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => account_error(error),
    }
}

async fn delete_my_profile_fact(
    principal: ApiPrincipal,
    State(state): State<AccountsApiState>,
    Path(fact_id): Path<String>,
) -> Response {
    match accounts::delete_profile_fact(&state.db, &principal.user_id, &fact_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => account_error(error),
    }
}

#[derive(Debug, Serialize)]
struct MobilePairingResponse {
    token: String,
    server_url: String,
    expires_at: String,
}

async fn create_mobile_pairing_token(
    principal: ApiPrincipal,
    State(state): State<AccountsApiState>,
    ExternalOrigin(server_url): ExternalOrigin,
) -> Response {
    match accounts::create_pairing_token(&state.db, &principal.user_id).await {
        Ok(pairing) => (
            StatusCode::CREATED,
            [(header::CACHE_CONTROL, "no-store")],
            Json(MobilePairingResponse {
                token: pairing.token,
                server_url,
                expires_at: pairing.expires_at,
            }),
        )
            .into_response(),
        Err(error) => account_error(error),
    }
}

async fn submit_feedback(
    _principal: ApiPrincipal,
    State(state): State<AccountsApiState>,
    ApiJson(request): ApiJson<GeneralFeedback>,
) -> Response {
    let instance_id = match crate::persistence::db::get_instance_id(&state.db).await {
        Ok(instance_id) => instance_id,
        Err(_) => return account_error(AccountError::FeedbackUnavailable),
    };
    match accounts::submit_general_feedback(
        &state.client,
        &state.marketplace_url,
        &instance_id,
        &request,
    )
    .await
    {
        Ok(receipt) => Json(receipt).into_response(),
        Err(error) => account_error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    async fn test_state() -> AccountsApiState {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        AccountsApiState {
            db,
            marketplace_url: "https://selu.bot/api/marketplace/agents".to_string(),
            agent_ids: vec!["alpha".to_string(), "beta".to_string()],
            client: reqwest::Client::new(),
        }
    }

    async fn response_json(response: Response) -> Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn request(
        method: &str,
        uri: &str,
        cookie: Option<&str>,
        body: Option<Value>,
    ) -> axum::http::Request<Body> {
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        let body = match body {
            Some(value) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&value).unwrap())
            }
            None => Body::empty(),
        };
        builder.body(body).unwrap()
    }

    async fn setup_admin(state: &AccountsApiState) -> (String, String) {
        let grant = match crate::services::auth::setup_first_admin(
            &state.db,
            "admin",
            "Admin",
            "password-123",
            "en",
        )
        .await
        .unwrap()
        {
            crate::services::auth::SetupOutcome::Created(grant) => grant,
            crate::services::auth::SetupOutcome::AlreadyConfigured => panic!("unexpected setup"),
        };
        (
            grant.user.user_id,
            format!("{SESSION_COOKIE}={}", grant.session_id),
        )
    }

    #[tokio::test]
    async fn router_returns_stable_json_for_anonymous_and_invalid_json_requests() {
        let state = test_state().await;
        let (_, admin_cookie) = setup_admin(&state).await;
        let app = Router::new()
            .nest("/api/v1", routes::<AccountsApiState>())
            .with_state(state);

        let anonymous = app
            .clone()
            .oneshot(request("GET", "/api/v1/users/me", None, None))
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response_json(anonymous).await["error"]["code"],
            "unauthorized"
        );

        let invalid = app
            .oneshot(
                axum::http::Request::builder()
                    .method("PATCH")
                    .uri("/api/v1/users/me")
                    .header(header::COOKIE, admin_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response_json(invalid).await["error"]["code"],
            "invalid_json"
        );
    }

    #[tokio::test]
    async fn own_profile_route_updates_only_the_authenticated_user() {
        let state = test_state().await;
        let (admin_id, admin_cookie) = setup_admin(&state).await;
        let app = Router::new()
            .nest("/api/v1", routes::<AccountsApiState>())
            .with_state(state.clone());
        let response = app
            .oneshot(request(
                "PATCH",
                "/api/v1/users/me",
                Some(&admin_cookie),
                Some(json!({
                    "display_name": "Updated Admin",
                    "language": "de",
                    "timezone": "Europe/Berlin"
                })),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["id"], admin_id);
        assert_eq!(body["username"], "admin");
        assert_eq!(body["is_admin"], true);
        assert_eq!(body["timezone"], "Europe/Berlin");
    }

    #[tokio::test]
    async fn admin_routes_enforce_role_and_deletion_safeguards() {
        let state = test_state().await;
        let (admin_id, admin_cookie) = setup_admin(&state).await;
        let member = accounts::create_user(
            &state.db,
            CreateUser {
                username: "member".to_string(),
                display_name: "Member".to_string(),
                password: "password-123".to_string(),
                is_admin: false,
                language: "en".to_string(),
                timezone: "UTC".to_string(),
            },
        )
        .await
        .unwrap();
        let member_session = crate::services::auth::create_session(&state.db, &member.id)
            .await
            .unwrap();
        let member_cookie = format!("{SESSION_COOKIE}={member_session}");
        let app = Router::new()
            .nest("/api/v1", routes::<AccountsApiState>())
            .with_state(state);

        let forbidden = app
            .clone()
            .oneshot(request("GET", "/api/v1/users", Some(&member_cookie), None))
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

        let self_delete = app
            .clone()
            .oneshot(request(
                "DELETE",
                &format!("/api/v1/users/{admin_id}"),
                Some(&admin_cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(self_delete.status(), StatusCode::CONFLICT);
        assert_eq!(
            response_json(self_delete).await["error"]["code"],
            "self_deletion_forbidden"
        );

        let deleted = app
            .oneshot(request(
                "DELETE",
                &format!("/api/v1/users/{}", member.id),
                Some(&admin_cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn profile_and_pairing_routes_are_owner_derived_and_never_put_token_in_url() {
        let state = test_state().await;
        let (_, admin_cookie) = setup_admin(&state).await;
        let app = Router::new()
            .nest("/api/v1", routes::<AccountsApiState>())
            .with_state(state);

        let created = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/v1/users/me/profile-facts",
                Some(&admin_cookie),
                Some(json!({"fact": "Likes Rust", "category": "preferences"})),
            ))
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);

        let mut pairing_request = request(
            "POST",
            "/api/v1/users/me/mobile-pairing-tokens",
            Some(&admin_cookie),
            None,
        );
        pairing_request
            .extensions_mut()
            .insert(ExternalOrigin("https://selu.example.test".to_string()));
        let pairing = app.oneshot(pairing_request).await.unwrap();
        assert_eq!(pairing.status(), StatusCode::CREATED);
        assert_eq!(
            pairing.headers()[header::CACHE_CONTROL],
            header::HeaderValue::from_static("no-store")
        );
        assert!(pairing.headers().get(header::LOCATION).is_none());
        let body = response_json(pairing).await;
        assert!(body["token"].as_str().unwrap().len() >= 40);
        assert_eq!(body["server_url"], "https://selu.example.test");
        assert!(
            !body["server_url"]
                .as_str()
                .unwrap()
                .contains(body["token"].as_str().unwrap())
        );
    }
}
