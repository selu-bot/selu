use axum::{
    Json,
    extract::{FromRef, FromRequestParts, State},
    http::{StatusCode, request::Parts},
    response::{IntoResponse, Response},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::{
    services::auth::{self, SESSION_COOKIE, SESSION_TTL_DAYS, SessionUser, SetupOutcome},
    state::AppState,
    web::ExternalOrigin,
};

impl FromRef<AppState> for SqlitePool {
    fn from_ref(state: &AppState) -> Self {
        state.db.clone()
    }
}

#[derive(Debug, Clone)]
pub struct ApiPrincipal(pub SessionUser);

impl std::ops::Deref for ApiPrincipal {
    type Target = SessionUser;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<S> FromRequestParts<S> for ApiPrincipal
where
    S: Send + Sync,
    SqlitePool: FromRef<S>,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_headers(&parts.headers);
        let Some(session_id) = jar.get(SESSION_COOKIE).map(Cookie::value) else {
            return Err(unauthorized());
        };
        let db = SqlitePool::from_ref(state);
        match auth::resolve_session(&db, session_id).await {
            Ok(Some(user)) => Ok(Self(user)),
            Ok(None) => Err(unauthorized()),
            Err(error) => {
                tracing::error!("Failed to resolve API session: {error:#}");
                Err(internal_error())
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApiAdmin(pub ApiPrincipal);

impl std::ops::Deref for ApiAdmin {
    type Target = SessionUser;

    fn deref(&self) -> &Self::Target {
        &self.0.0
    }
}

impl<S> FromRequestParts<S> for ApiAdmin
where
    S: Send + Sync,
    SqlitePool: FromRef<S>,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let principal = ApiPrincipal::from_request_parts(parts, state).await?;
        if principal.is_admin {
            Ok(Self(principal))
        } else {
            Err(forbidden())
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ScopeForbidden;

impl ApiPrincipal {
    /// Resolve a caller-selected user scope without allowing a regular user to
    /// cross the authenticated principal boundary. Administrators may select a
    /// user explicitly; regular callers always resolve to their own identity.
    pub fn user_scope(&self, requested_user_id: &str) -> Result<String, ScopeForbidden> {
        if self.is_admin || requested_user_id == self.user_id {
            Ok(if self.is_admin {
                requested_user_id.to_string()
            } else {
                self.user_id.clone()
            })
        } else {
            Err(ScopeForbidden)
        }
    }
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

pub fn unauthorized() -> Response {
    json_error(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "Authentication is required.",
    )
}

pub fn forbidden() -> Response {
    json_error(
        StatusCode::FORBIDDEN,
        "forbidden",
        "You do not have permission to do that.",
    )
}

pub fn internal_error() -> Response {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        "The request could not be completed.",
    )
}

fn json_error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (
        status,
        Json(ErrorEnvelope {
            error: ErrorBody { code, message },
        }),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct SetupRequest {
    pub username: String,
    #[serde(default)]
    pub display_name: String,
    pub password: String,
    #[serde(default = "default_language")]
    pub language: String,
}

fn default_language() -> String {
    "en".to_string()
}

#[derive(Debug, Serialize)]
pub struct AuthStateResponse {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<SessionUser>,
}

#[derive(Debug, Serialize)]
struct AuthenticatedResponse {
    status: &'static str,
    user: SessionUser,
}

pub async fn state(State(state): State<AppState>, jar: CookieJar) -> Response {
    match auth::users_exist(&state.db).await {
        Ok(false) => Json(AuthStateResponse {
            status: "setup_required",
            user: None,
        })
        .into_response(),
        Ok(true) => {
            let user = match jar.get(SESSION_COOKIE) {
                Some(cookie) => match auth::resolve_session(&state.db, cookie.value()).await {
                    Ok(user) => user,
                    Err(error) => {
                        tracing::error!("Failed to read auth state: {error:#}");
                        return internal_error();
                    }
                },
                None => None,
            };
            Json(AuthStateResponse {
                status: if user.is_some() {
                    "authenticated"
                } else {
                    "anonymous"
                },
                user,
            })
            .into_response()
        }
        Err(error) => {
            tracing::error!("Failed to read auth setup state: {error:#}");
            internal_error()
        }
    }
}

pub async fn login(
    State(state): State<AppState>,
    ExternalOrigin(origin): ExternalOrigin,
    jar: CookieJar,
    Json(request): Json<LoginRequest>,
) -> Response {
    match auth::login(&state.db, &request.username, &request.password).await {
        Ok(Some(grant)) => (
            jar.add(session_cookie(
                grant.session_id,
                &state.base_path,
                origin.starts_with("https://"),
            )),
            Json(AuthenticatedResponse {
                status: "authenticated",
                user: grant.user,
            }),
        )
            .into_response(),
        Ok(None) => json_error(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "The username or password is incorrect.",
        ),
        Err(error) => {
            tracing::error!("API login failed: {error:#}");
            internal_error()
        }
    }
}

pub async fn setup(
    State(state): State<AppState>,
    ExternalOrigin(origin): ExternalOrigin,
    jar: CookieJar,
    Json(request): Json<SetupRequest>,
) -> Response {
    if request.username.trim().is_empty() || request.password.is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Username and password are required.",
        );
    }

    match auth::setup_first_admin(
        &state.db,
        &request.username,
        &request.display_name,
        &request.password,
        &request.language,
    )
    .await
    {
        Ok(SetupOutcome::Created(grant)) => (
            jar.add(session_cookie(
                grant.session_id,
                &state.base_path,
                origin.starts_with("https://"),
            )),
            (
                StatusCode::CREATED,
                Json(AuthenticatedResponse {
                    status: "authenticated",
                    user: grant.user,
                }),
            ),
        )
            .into_response(),
        Ok(SetupOutcome::AlreadyConfigured) => json_error(
            StatusCode::CONFLICT,
            "setup_complete",
            "Setup has already been completed.",
        ),
        Err(error) => {
            tracing::error!("API setup failed: {error:#}");
            internal_error()
        }
    }
}

pub async fn logout(
    State(state): State<AppState>,
    ExternalOrigin(origin): ExternalOrigin,
    jar: CookieJar,
) -> Response {
    if let Some(cookie) = jar.get(SESSION_COOKIE)
        && let Err(error) = auth::delete_session(&state.db, cookie.value()).await
    {
        tracing::error!("API logout failed: {error:#}");
        return internal_error();
    }

    (
        jar.remove(expired_session_cookie(
            &state.base_path,
            origin.starts_with("https://"),
        )),
        Json(serde_json::json!({ "status": "anonymous" })),
    )
        .into_response()
}

fn cookie_path(base_path: &str) -> String {
    if base_path.is_empty() {
        "/".to_string()
    } else {
        format!("{}/", base_path.trim_end_matches('/'))
    }
}

fn session_cookie(session_id: String, base_path: &str, secure: bool) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, session_id))
        .path(cookie_path(base_path))
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(SESSION_TTL_DAYS))
        .build()
}

fn expired_session_cookie(base_path: &str, secure: bool) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, ""))
        .path(cookie_path(base_path))
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::ZERO)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, extract::Path, routing::get};
    use http_body_util::BodyExt;
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    async fn test_db() -> SqlitePool {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        db
    }

    async fn principal_handler(principal: ApiPrincipal) -> Json<SessionUser> {
        Json(principal.0)
    }

    async fn admin_handler(_admin: ApiAdmin) -> StatusCode {
        StatusCode::NO_CONTENT
    }

    async fn scoped_handler(principal: ApiPrincipal, Path(user_id): Path<String>) -> Response {
        match principal.user_scope(&user_id) {
            Ok(user_id) => Json(serde_json::json!({ "user_id": user_id })).into_response(),
            Err(_) => forbidden(),
        }
    }

    async fn response_json(response: Response) -> serde_json::Value {
        let body = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn anonymous_api_auth_is_json_unauthorized() {
        let db = test_db().await;
        let app = Router::new()
            .route("/principal", get(principal_handler))
            .with_state(db);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/principal")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "unauthorized"
        );
    }

    #[tokio::test]
    async fn user_admin_and_cross_user_rules_are_enforced_over_http() {
        let db = test_db().await;
        let SetupOutcome::Created(admin) =
            auth::setup_first_admin(&db, "admin", "Admin", "secret", "en")
                .await
                .unwrap()
        else {
            panic!("admin setup failed")
        };
        let password_hash: String =
            sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
                .bind(&admin.user.user_id)
                .fetch_one(&db)
                .await
                .unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, display_name, password_hash, is_admin, language) VALUES ('user-2', 'user', 'User', ?, 0, 'en')",
        )
        .bind(password_hash)
        .execute(&db)
        .await
        .unwrap();
        let user_session = auth::create_session(&db, "user-2").await.unwrap();

        let app = Router::new()
            .route("/admin", get(admin_handler))
            .route("/scope/{user_id}", get(scoped_handler))
            .with_state(db);
        let user_cookie = format!("{SESSION_COOKIE}={user_session}");
        let admin_cookie = format!("{SESSION_COOKIE}={}", admin.session_id);

        let user_admin_response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/admin")
                    .header("cookie", &user_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(user_admin_response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response_json(user_admin_response).await["error"]["code"],
            "forbidden"
        );

        let cross_user_response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/scope/{}", admin.user.user_id))
                    .header("cookie", &user_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cross_user_response.status(), StatusCode::FORBIDDEN);

        let own_response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/scope/user-2")
                    .header("cookie", &user_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(own_response.status(), StatusCode::OK);
        assert_eq!(response_json(own_response).await["user_id"], "user-2");

        let admin_cross_user_response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/scope/user-2")
                    .header("cookie", &admin_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(admin_cross_user_response.status(), StatusCode::OK);
        assert_eq!(
            response_json(admin_cross_user_response).await["user_id"],
            "user-2"
        );
    }

    #[test]
    fn cookie_preserves_security_and_base_path_semantics() {
        let cookie = session_cookie("session".to_string(), "/selu", true);
        assert_eq!(cookie.path(), Some("/selu/"));
        assert_eq!(cookie.same_site(), Some(SameSite::Lax));
        assert_eq!(cookie.max_age(), Some(time::Duration::days(7)));
        assert_eq!(cookie.http_only(), Some(true));
        assert_eq!(cookie.secure(), Some(true));
    }
}
