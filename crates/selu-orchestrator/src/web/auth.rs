use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use askama::Template;
use axum::{
    extract::{FromRequestParts, State},
    http::{request::Parts, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use chrono::{Duration, Utc};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use tracing::error;
use uuid::Uuid;

use crate::state::AppState;
use crate::web::{prefixed, prefixed_redirect};

/// Session lifetime: 7 days
const SESSION_TTL_DAYS: i64 = 7;
const SESSION_COOKIE: &str = "selu_session";

// ── Template ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    #[allow(dead_code)]
    active_nav: &'static str,
    error: Option<String>,
    base_path: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
}

/// The authenticated user extracted from the session cookie.
/// Handlers that need auth use `AuthUser` as an extractor.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AuthUser {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub is_admin: bool,
}

/// Axum extractor that validates the session cookie.
/// If invalid/expired/missing, redirects to /login.
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Extract cookie jar
        let jar = CookieJar::from_headers(&parts.headers);

        let session_id = match jar.get(SESSION_COOKIE) {
            Some(c) => c.value().to_string(),
            None => return Err(Redirect::to(&prefixed(state.base_path.as_str(), "/login")).into_response()),
        };

        // Look up session + user
        let row = sqlx::query!(
            r#"SELECT ws.user_id, u.username, u.display_name, u.is_admin
               FROM web_sessions ws
               JOIN users u ON u.id = ws.user_id
               WHERE ws.id = ? AND ws.expires_at > datetime('now')"#,
            session_id
        )
        .fetch_optional(&state.db)
        .await
        .map_err(|e| {
            error!("Session lookup DB error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })?;

        match row {
            Some(r) => Ok(AuthUser {
                user_id: r.user_id,
                username: r.username,
                display_name: r.display_name,
                is_admin: r.is_admin != 0,
            }),
            None => Err(Redirect::to(&prefixed(state.base_path.as_str(), "/login")).into_response()),
        }
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn login_page(State(state): State<AppState>) -> Response {
    // If no users exist, redirect to first-run setup
    let count: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM users")
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);

    if count == 0 {
        return prefixed_redirect(&state, "/setup").into_response();
    }

    let tmpl = LoginTemplate {
        active_nav: "",
        error: None,
        base_path: state.base_path.clone(),
    };
    match tmpl.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn login_submit(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> Response {
    let username = form.username.trim().to_string();
    let bp = &state.base_path;

    // Look up user by username
    let user = sqlx::query!(
        "SELECT id, password_hash FROM users WHERE username = ?",
        username
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let user = match user {
        Some(u) => u,
        None => return render_login_error("Invalid username or password.", bp),
    };

    // Verify password
    let hash = match PasswordHash::new(&user.password_hash) {
        Ok(h) => h,
        Err(_) => return render_login_error("Invalid username or password.", bp),
    };

    if Argon2::default()
        .verify_password(form.password.as_bytes(), &hash)
        .is_err()
    {
        return render_login_error("Invalid username or password.", bp);
    }

    // Create web session
    let session_id = Uuid::new_v4().to_string();
    let user_id = user.id.unwrap_or_default();
    let expires_at = (Utc::now() + Duration::days(SESSION_TTL_DAYS))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    if let Err(e) = sqlx::query!(
        "INSERT INTO web_sessions (id, user_id, expires_at) VALUES (?, ?, ?)",
        session_id,
        user_id,
        expires_at
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to create web session: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Set session cookie
    let cookie_path = if bp.is_empty() { "/".to_string() } else { format!("{}/", bp) };
    let cookie = Cookie::build((SESSION_COOKIE, session_id))
        .path(cookie_path)
        .http_only(true)
        .same_site(axum_extra::extract::cookie::SameSite::Lax)
        .max_age(time::Duration::days(SESSION_TTL_DAYS))
        .build();

    (jar.add(cookie), prefixed_redirect(&state, "/chat")).into_response()
}

pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> Response {
    // Delete session from DB
    if let Some(cookie) = jar.get(SESSION_COOKIE) {
        let session_id = cookie.value().to_string();
        let _ = sqlx::query!("DELETE FROM web_sessions WHERE id = ?", session_id)
            .execute(&state.db)
            .await;
    }

    // Remove cookie
    let bp = &state.base_path;
    let cookie_path = if bp.is_empty() { "/".to_string() } else { format!("{}/", bp) };
    let cookie = Cookie::build((SESSION_COOKIE, ""))
        .path(cookie_path)
        .max_age(time::Duration::ZERO)
        .build();

    (jar.remove(cookie), prefixed_redirect(&state, "/login")).into_response()
}

fn render_login_error(msg: &str, base_path: &str) -> Response {
    let tmpl = LoginTemplate {
        active_nav: "",
        error: Some(msg.to_string()),
        base_path: base_path.to_string(),
    };
    match tmpl.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── First-run setup ───────────────────────────────────────────────────────────
// Only available when no users exist in the database.

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupTemplate {
    error: Option<String>,
    base_path: String,
}

#[derive(Debug, Deserialize)]
pub struct SetupForm {
    pub username: String,
    pub display_name: String,
    pub password: String,
}

/// GET /setup - show the first-run setup page (only if no users exist)
pub async fn setup_page(State(state): State<AppState>) -> Response {
    // If users already exist, redirect to login
    let count: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM users")
        .fetch_one(&state.db)
        .await
        .unwrap_or(1);

    if count > 0 {
        return prefixed_redirect(&state, "/login").into_response();
    }

    let tmpl = SetupTemplate { error: None, base_path: state.base_path.clone() };
    match tmpl.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// POST /setup - create the first admin user (only if no users exist)
pub async fn setup_submit(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(form): Form<SetupForm>,
) -> Response {
    // Safety: only allow setup when no users exist
    let count: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM users")
        .fetch_one(&state.db)
        .await
        .unwrap_or(1);

    if count > 0 {
        return prefixed_redirect(&state, "/login").into_response();
    }

    if form.username.trim().is_empty() || form.password.is_empty() {
        let tmpl = SetupTemplate {
            error: Some("Username and password are required.".to_string()),
            base_path: state.base_path.clone(),
        };
        return match tmpl.render() {
            Ok(html) => Html(html).into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }

    // Hash password with Argon2id
    let sys_rng = SystemRandom::new();
    let mut salt_bytes = [0u8; 16];
    if sys_rng.fill(&mut salt_bytes).is_err() {
        error!("Failed to generate random salt");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let salt = match SaltString::encode_b64(&salt_bytes) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to encode SaltString: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let hash = match Argon2::default().hash_password(form.password.as_bytes(), &salt) {
        Ok(h) => h.to_string(),
        Err(e) => {
            error!("Argon2 hashing failed: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let user_id = Uuid::new_v4().to_string();
    let username = form.username.trim().to_string();
    let display_name = if form.display_name.trim().is_empty() {
        username.clone()
    } else {
        form.display_name.trim().to_string()
    };

    // Create the admin user
    if let Err(e) = sqlx::query!(
        "INSERT INTO users (id, username, display_name, password_hash, is_admin) VALUES (?, ?, ?, ?, 1)",
        user_id,
        username,
        display_name,
        hash,
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to create admin user: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Seed the Bedrock LLM provider entry (region will be set by the user via the providers UI).
    // base_url is left empty so Bedrock doesn't appear as "configured" in the agents page
    // until the user explicitly sets a region.
    let _ = sqlx::query!(
        r#"INSERT OR IGNORE INTO llm_providers (id, display_name, api_key_encrypted, base_url, active)
           VALUES ('bedrock', 'Amazon Bedrock', '', '', 1)"#
    )
    .execute(&state.db)
    .await;

    // Also seed Anthropic and OpenAI entries for convenience
    let _ = sqlx::query!(
        r#"INSERT OR IGNORE INTO llm_providers (id, display_name, api_key_encrypted, base_url, active)
           VALUES ('anthropic', 'Anthropic Claude', '', '', 1)"#
    )
    .execute(&state.db)
    .await;
    let _ = sqlx::query!(
        r#"INSERT OR IGNORE INTO llm_providers (id, display_name, api_key_encrypted, base_url, active)
           VALUES ('openai', 'OpenAI', '', '', 1)"#
    )
    .execute(&state.db)
    .await;

    // Auto-login: create a web session
    let session_id = Uuid::new_v4().to_string();
    let expires_at = (Utc::now() + Duration::days(SESSION_TTL_DAYS))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    let _ = sqlx::query!(
        "INSERT INTO web_sessions (id, user_id, expires_at) VALUES (?, ?, ?)",
        session_id,
        user_id,
        expires_at
    )
    .execute(&state.db)
    .await;

    let bp = &state.base_path;
    let cookie_path = if bp.is_empty() { "/".to_string() } else { format!("{}/", bp) };
    let cookie = Cookie::build((SESSION_COOKIE, session_id))
        .path(cookie_path)
        .http_only(true)
        .same_site(axum_extra::extract::cookie::SameSite::Lax)
        .max_age(time::Duration::days(SESSION_TTL_DAYS))
        .build();

    (jar.add(cookie), prefixed_redirect(&state, "/chat")).into_response()
}
