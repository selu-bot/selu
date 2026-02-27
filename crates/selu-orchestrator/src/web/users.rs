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
    pub is_admin: bool,
    pub created_at: String,
}

// ── Template ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "users.html")]
struct UsersTemplate {
    active_nav: &'static str,
    is_admin: bool,
    current_user_id: String,
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
    #[serde(default)]
    pub is_admin: Option<String>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn users_index(
    user: AuthUser,
    Query(q): Query<UsersQuery>,
    State(state): State<AppState>,
) -> Response {
    if !user.is_admin {
        return Redirect::to("/chat").into_response();
    }

    let users = sqlx::query!(
        "SELECT id, username, display_name, is_admin, created_at FROM users ORDER BY created_at"
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| UserRow {
        id: r.id.unwrap_or_default(),
        username: r.username,
        display_name: r.display_name,
        is_admin: r.is_admin != 0,
        created_at: r.created_at,
    })
    .collect();

    let error = q.error.map(|code| match code.as_str() {
        "duplicate" => "Username is already taken. Please choose a different username.".to_string(),
        "hash_failed" => "Password hashing failed. Please try again.".to_string(),
        "username_required" => "Username and password are required.".to_string(),
        "create_failed" => "Failed to create user. Please try again.".to_string(),
        _ => "An unexpected error occurred.".to_string(),
    });

    match (UsersTemplate { active_nav: "users", is_admin: user.is_admin, current_user_id: user.user_id, users, error }).render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn users_create(
    user: AuthUser,
    State(state): State<AppState>,
    Form(form): Form<CreateUserForm>,
) -> Response {
    if !user.is_admin {
        return Redirect::to("/chat").into_response();
    }

    if form.username.trim().is_empty() || form.password.is_empty() {
        return Redirect::to("/users?error=username_required").into_response();
    }

    let display_name = if form.display_name.trim().is_empty() {
        form.username.clone()
    } else {
        form.display_name.trim().to_string()
    };

    let is_admin: i32 = if form.is_admin.as_deref() == Some("on") { 1 } else { 0 };

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
        "INSERT INTO users (id, username, display_name, password_hash, is_admin) VALUES (?, ?, ?, ?, ?)",
        id,
        username,
        display_name,
        hash,
        is_admin,
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
            Redirect::to("/users?error=create_failed").into_response()
        }
    }
}

pub async fn users_delete(
    user: AuthUser,
    Path(user_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    // Prevent self-deletion
    if user_id == user.user_id {
        return StatusCode::FORBIDDEN.into_response();
    }

    if let Err(e) =
        sqlx::query!("DELETE FROM users WHERE id = ?", user_id).execute(&state.db).await
    {
        error!("Failed to delete user {user_id}: {e}");
    }
    Html("").into_response()
}

/// POST /users/{id}/toggle-admin — flip the is_admin flag (HTMX, returns updated badge)
pub async fn users_toggle_admin(
    user: AuthUser,
    Path(target_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    // Prevent admins from changing their own admin status
    if target_id == user.user_id {
        return StatusCode::FORBIDDEN.into_response();
    }

    // Read current value and flip it
    let current = sqlx::query!("SELECT is_admin FROM users WHERE id = ?", target_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();

    let new_val: i32 = match current {
        Some(r) => if r.is_admin != 0 { 0 } else { 1 },
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Err(e) = sqlx::query!(
        "UPDATE users SET is_admin = ? WHERE id = ?",
        new_val,
        target_id,
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to toggle admin for user {target_id}: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Return the updated badge HTML
    if new_val == 1 {
        Html(format!(
            r#"<button class="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium bg-coral/10 text-coral border border-coral/20 cursor-pointer hover:bg-coral/20 transition-colors"
                    hx-post="/users/{id}/toggle-admin"
                    hx-target="closest td"
                    hx-swap="innerHTML"
                    data-i18n="users.admin">Admin</button>"#,
            id = target_id,
        )).into_response()
    } else {
        Html(format!(
            r#"<button class="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium bg-surface-alt text-txt-muted border border-edge cursor-pointer hover:bg-sidebar-hover transition-colors"
                    hx-post="/users/{id}/toggle-admin"
                    hx-target="closest td"
                    hx-swap="innerHTML"
                    data-i18n="users.user">User</button>"#,
            id = target_id,
        )).into_response()
    }
}
