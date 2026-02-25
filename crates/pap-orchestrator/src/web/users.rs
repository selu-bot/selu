use argon2::{
    password_hash::{PasswordHasher, SaltString},
    Argon2,
};
use ring::rand::{SecureRandom, SystemRandom};
use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;
use tracing::error;
use uuid::Uuid;

use crate::state::AppState;
use crate::web::auth::AuthUser;

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct UserRow {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub created_at: String,
}

// ── Template ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "users.html")]
struct UsersTemplate {
    active_nav: &'static str,
    users: Vec<UserRow>,
    error: Option<String>,
}

// ── Query / Form structs ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct UsersQuery {
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateUserForm {
    pub username: String,
    pub display_name: String,
    pub password: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn users_index(
    _user: AuthUser,
    Query(q): Query<UsersQuery>,
    State(state): State<AppState>,
) -> Response {
    let users = sqlx::query!(
        "SELECT id, username, display_name, created_at FROM users ORDER BY created_at"
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| UserRow {
        id: r.id.unwrap_or_default(),
        username: r.username,
        display_name: r.display_name,
        created_at: r.created_at,
    })
    .collect();

    let error = q.error.map(|code| match code.as_str() {
        "duplicate" => "Username is already taken. Please choose a different username.".to_string(),
        "hash_failed" => "Password hashing failed. Please try again.".to_string(),
        _ => "An unexpected error occurred.".to_string(),
    });

    match (UsersTemplate { active_nav: "users", users, error }).render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn users_create(
    _user: AuthUser,
    State(state): State<AppState>,
    Form(form): Form<CreateUserForm>,
) -> Response {
    if form.username.trim().is_empty() || form.password.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let display_name = if form.display_name.trim().is_empty() {
        form.username.clone()
    } else {
        form.display_name.trim().to_string()
    };

    // Hash password with Argon2id
    let sys_rng = SystemRandom::new();
    let mut salt_bytes = [0u8; 16];
    if sys_rng.fill(&mut salt_bytes).is_err() {
        error!("Failed to generate random salt");
        return Redirect::to("/users?error=hash_failed").into_response();
    }
    let salt = match SaltString::encode_b64(&salt_bytes) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to encode SaltString: {e}");
            return Redirect::to("/users?error=hash_failed").into_response();
        }
    };
    let argon2 = Argon2::default();
    let hash = match argon2.hash_password(form.password.as_bytes(), &salt) {
        Ok(h) => h.to_string(),
        Err(e) => {
            error!("Argon2 hashing failed: {e}");
            return Redirect::to("/users?error=hash_failed").into_response();
        }
    };

    let id = Uuid::new_v4().to_string();
    let username = form.username.trim().to_string();

    let result = sqlx::query!(
        "INSERT INTO users (id, username, display_name, password_hash) VALUES (?, ?, ?, ?)",
        id,
        username,
        display_name,
        hash,
    )
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Redirect::to("/users").into_response(),
        Err(e) if e.to_string().to_lowercase().contains("unique") => {
            Redirect::to("/users?error=duplicate").into_response()
        }
        Err(e) => {
            error!("Failed to create user: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn users_delete(
    _user: AuthUser,
    Path(user_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if let Err(e) =
        sqlx::query!("DELETE FROM users WHERE id = ?", user_id).execute(&state.db).await
    {
        error!("Failed to delete user {user_id}: {e}");
    }
    Html("").into_response()
}
