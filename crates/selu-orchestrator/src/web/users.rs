use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::CookieJar;
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use tracing::error;
use uuid::Uuid;

use crate::state::AppState;
use crate::web::auth::AuthUser;
use crate::web::{BasePath, prefixed_redirect};

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct UserRow {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub is_admin: bool,
    pub language: String,
    pub created_at: String,
    /// Agent IDs this user has access to. Empty = all agents.
    pub allowed_agents: Vec<String>,
}

impl UserRow {
    pub fn has_agent(&self, agent_id: &String) -> bool {
        self.allowed_agents.contains(agent_id)
    }

    pub fn has_no_agents(&self) -> bool {
        self.allowed_agents
            .iter()
            .any(|id| id == crate::agents::access::NO_AGENTS_SENTINEL)
    }
}

#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
}

// ── Template ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "users.html")]
struct UsersTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    current_user_id: String,
    current_username: String,
    users: Vec<UserRow>,
    all_agents: Vec<AgentInfo>,
    error_key: Option<String>,
    success_key: Option<String>,
}

// ── Query / Form structs ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct UsersQuery {
    pub error: Option<String>,
    pub success: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateUserForm {
    pub username: String,
    pub display_name: String,
    pub password: String,
    pub language: String,
    #[serde(default)]
    pub is_admin: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChangePasswordForm {
    pub current_password: String,
    pub new_password: String,
    pub confirm_password: String,
}

const SESSION_COOKIE: &str = "selu_session";

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn users_index(
    user: AuthUser,
    Query(q): Query<UsersQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    let (users, all_agents) = if user.is_admin {
        // Load all installed agents (excluding "default" which is always accessible)
        let agents_snapshot = state.agents.load();
        let all_agents: Vec<AgentInfo> = agents_snapshot
            .iter()
            .filter(|(id, _)| id.as_str() != "default")
            .map(|(id, def)| AgentInfo {
                id: id.clone(),
                name: def.name.clone(),
            })
            .collect();

        ensure_user_agent_access_table(&state.db).await;

        // Load per-user agent access
        let access_rows = sqlx::query!("SELECT user_id, agent_id FROM user_agent_access")
            .fetch_all(&state.db)
            .await
            .unwrap_or_default();

        let mut user_agents_map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for row in &access_rows {
            user_agents_map
                .entry(row.user_id.clone())
                .or_default()
                .push(row.agent_id.clone());
        }

        let users = sqlx::query!(
            "SELECT id, username, display_name, is_admin, language, created_at FROM users ORDER BY created_at"
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let uid = r.id.unwrap_or_default();
            let allowed_agents = user_agents_map.remove(&uid).unwrap_or_default();
            UserRow {
                id: uid,
                username: r.username,
                display_name: r.display_name,
                is_admin: r.is_admin != 0,
                language: r.language,
                created_at: r.created_at,
                allowed_agents,
            }
        })
        .collect();

        (users, all_agents)
    } else {
        (Vec::new(), Vec::new())
    };

    let error_key = q.error.map(|code| match code.as_str() {
        "duplicate" => "users.error.duplicate".to_string(),
        "hash_failed" => "users.error.hash_failed".to_string(),
        "username_required" => "users.error.username_required".to_string(),
        "create_failed" => "users.error.create_failed".to_string(),
        "agents_save_failed" => "users.error.agents_save_failed".to_string(),
        "current_password_required" => "users.error.current_password_required".to_string(),
        "new_password_required" => "users.error.new_password_required".to_string(),
        "confirm_password_required" => "users.error.confirm_password_required".to_string(),
        "password_mismatch" => "users.error.password_mismatch".to_string(),
        "password_too_short" => "users.error.password_too_short".to_string(),
        "wrong_current_password" => "users.error.wrong_current_password".to_string(),
        "verify_failed" => "users.error.verify_failed".to_string(),
        "password_change_failed" => "users.error.password_change_failed".to_string(),
        _ => "users.error.unexpected".to_string(),
    });
    let success_key = q.success.map(|code| match code.as_str() {
        "password_changed" => "users.success.password_changed".to_string(),
        _ => "users.success.saved".to_string(),
    });

    match (UsersTemplate {
        active_nav: "users",
        is_admin: user.is_admin,
        base_path,
        current_user_id: user.user_id,
        current_username: user.username,
        users,
        all_agents,
        error_key,
        success_key,
    })
    .render()
    {
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
    BasePath(base_path): BasePath,
    Form(form): Form<CreateUserForm>,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }

    if form.username.trim().is_empty() || form.password.is_empty() {
        return Redirect::to(&format!("{}/users?error=username_required", base_path))
            .into_response();
    }

    let display_name = if form.display_name.trim().is_empty() {
        form.username.clone()
    } else {
        form.display_name.trim().to_string()
    };

    let is_admin: i32 = if form.is_admin.as_deref() == Some("on") {
        1
    } else {
        0
    };

    // Hash password with Argon2id
    let sys_rng = SystemRandom::new();
    let mut salt_bytes = [0u8; 16];
    if sys_rng.fill(&mut salt_bytes).is_err() {
        error!("Failed to generate random salt");
        return Redirect::to(&format!("{}/users?error=hash_failed", base_path)).into_response();
    }
    let salt = match SaltString::encode_b64(&salt_bytes) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to encode SaltString: {e}");
            return Redirect::to(&format!("{}/users?error=hash_failed", base_path)).into_response();
        }
    };
    let argon2 = Argon2::default();
    let hash = match argon2.hash_password(form.password.as_bytes(), &salt) {
        Ok(h) => h.to_string(),
        Err(e) => {
            error!("Argon2 hashing failed: {e}");
            return Redirect::to(&format!("{}/users?error=hash_failed", base_path)).into_response();
        }
    };

    let id = Uuid::new_v4().to_string();
    let username = form.username.trim().to_string();
    let language = match form.language.as_str() {
        "en" | "de" => form.language.clone(),
        _ => "de".to_string(),
    };

    let result = sqlx::query!(
        "INSERT INTO users (id, username, display_name, password_hash, is_admin, language) VALUES (?, ?, ?, ?, ?, ?)",
        id,
        username,
        display_name,
        hash,
        is_admin,
        language,
    )
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => {
            // Auto-create the new user's web pipe (used by chat UI and mobile app).
            super::pipes::ensure_web_pipe(&state.db, &id, &display_name).await;
            Redirect::to(&format!("{}/users", base_path)).into_response()
        }
        Err(e) if e.to_string().to_lowercase().contains("unique") => {
            Redirect::to(&format!("{}/users?error=duplicate", base_path)).into_response()
        }
        Err(e) => {
            error!("Failed to create user: {e}");
            Redirect::to(&format!("{}/users?error=create_failed", base_path)).into_response()
        }
    }
}

pub async fn users_change_password(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    jar: CookieJar,
    Form(form): Form<ChangePasswordForm>,
) -> Response {
    let user_id = user.user_id;
    let user_id_ref = user_id.as_str();

    if form.current_password.is_empty() {
        return Redirect::to(&format!(
            "{}/users?error=current_password_required",
            base_path
        ))
        .into_response();
    }
    if form.new_password.is_empty() {
        return Redirect::to(&format!("{}/users?error=new_password_required", base_path))
            .into_response();
    }
    if form.confirm_password.is_empty() {
        return Redirect::to(&format!(
            "{}/users?error=confirm_password_required",
            base_path
        ))
        .into_response();
    }
    if form.new_password != form.confirm_password {
        return Redirect::to(&format!("{}/users?error=password_mismatch", base_path))
            .into_response();
    }
    if form.new_password.len() < 8 {
        return Redirect::to(&format!("{}/users?error=password_too_short", base_path))
            .into_response();
    }

    let user_row = match sqlx::query!("SELECT password_hash FROM users WHERE id = ?", user_id_ref)
        .fetch_optional(&state.db)
        .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            return Redirect::to(&format!("{}/users?error=verify_failed", base_path))
                .into_response();
        }
        Err(e) => {
            error!("Failed to load current user password hash: {e}");
            return Redirect::to(&format!("{}/users?error=verify_failed", base_path))
                .into_response();
        }
    };

    let parsed = match PasswordHash::new(&user_row.password_hash) {
        Ok(v) => v,
        Err(e) => {
            error!("Invalid stored password hash: {e}");
            return Redirect::to(&format!("{}/users?error=verify_failed", base_path))
                .into_response();
        }
    };

    if Argon2::default()
        .verify_password(form.current_password.as_bytes(), &parsed)
        .is_err()
    {
        return Redirect::to(&format!("{}/users?error=wrong_current_password", base_path))
            .into_response();
    }

    let sys_rng = SystemRandom::new();
    let mut salt_bytes = [0u8; 16];
    if sys_rng.fill(&mut salt_bytes).is_err() {
        error!("Failed to generate random salt");
        return Redirect::to(&format!("{}/users?error=hash_failed", base_path)).into_response();
    }
    let salt = match SaltString::encode_b64(&salt_bytes) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to encode SaltString: {e}");
            return Redirect::to(&format!("{}/users?error=hash_failed", base_path)).into_response();
        }
    };
    let hash = match Argon2::default().hash_password(form.new_password.as_bytes(), &salt) {
        Ok(h) => h.to_string(),
        Err(e) => {
            error!("Argon2 hashing failed: {e}");
            return Redirect::to(&format!("{}/users?error=hash_failed", base_path)).into_response();
        }
    };

    let current_session_id = jar.get(SESSION_COOKIE).map(|c| c.value().to_string());

    let mut tx = match state.db.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            error!("Failed to start password change transaction: {e}");
            return Redirect::to(&format!("{}/users?error=password_change_failed", base_path))
                .into_response();
        }
    };

    if let Err(e) = sqlx::query!(
        "UPDATE users SET password_hash = ? WHERE id = ?",
        hash,
        user_id_ref
    )
    .execute(&mut *tx)
    .await
    {
        error!("Failed to update password hash: {e}");
        return Redirect::to(&format!("{}/users?error=password_change_failed", base_path))
            .into_response();
    }

    let sessions_result = if let Some(session_id) = current_session_id {
        sqlx::query!(
            "DELETE FROM web_sessions WHERE user_id = ? AND id != ?",
            user_id_ref,
            session_id
        )
        .execute(&mut *tx)
        .await
    } else {
        sqlx::query!("DELETE FROM web_sessions WHERE user_id = ?", user_id_ref)
            .execute(&mut *tx)
            .await
    };

    if let Err(e) = sessions_result {
        error!("Failed to revoke old sessions after password change: {e}");
        return Redirect::to(&format!("{}/users?error=password_change_failed", base_path))
            .into_response();
    }

    if let Err(e) = tx.commit().await {
        error!("Failed to commit password change transaction: {e}");
        return Redirect::to(&format!("{}/users?error=password_change_failed", base_path))
            .into_response();
    }

    Redirect::to(&format!("{}/users?success=password_changed", base_path)).into_response()
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

    if let Err(e) = sqlx::query!("DELETE FROM users WHERE id = ?", user_id)
        .execute(&state.db)
        .await
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
    BasePath(base_path): BasePath,
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
        Some(r) => {
            if r.is_admin != 0 {
                0
            } else {
                1
            }
        }
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

    let bp = &base_path;

    // Return the updated badge HTML
    if new_val == 1 {
        Html(format!(
            r#"<button class="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium bg-coral/10 text-coral border border-coral/20 cursor-pointer hover:bg-coral/20 transition-colors"
                    hx-post="{bp}/users/{id}/toggle-admin"
                    hx-target="closest td"
                    hx-swap="innerHTML"
                    data-i18n="users.admin">Admin</button>"#,
            bp = bp,
            id = target_id,
        )).into_response()
    } else {
        Html(format!(
            r#"<button class="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium bg-surface-alt text-txt-muted border border-edge cursor-pointer hover:bg-sidebar-hover transition-colors"
                    hx-post="{bp}/users/{id}/toggle-admin"
                    hx-target="closest td"
                    hx-swap="innerHTML"
                    data-i18n="users.user">User</button>"#,
            bp = bp,
            id = target_id,
        )).into_response()
    }
}

// ── Language ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SetLanguageForm {
    pub language: String,
}

/// POST /users/{id}/language — change the output language for a user (HTMX, returns updated select)
pub async fn users_set_language(
    user: AuthUser,
    Path(target_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<SetLanguageForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    let language = match form.language.as_str() {
        "en" | "de" => form.language.clone(),
        _ => "de".to_string(),
    };

    if let Err(e) = sqlx::query!(
        "UPDATE users SET language = ? WHERE id = ?",
        language,
        target_id,
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to set language for user {target_id}: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let bp = &base_path;

    Html(format!(
        r#"<select name="language"
                hx-post="{bp}/users/{id}/language"
                hx-target="closest td"
                hx-swap="innerHTML"
                class="bg-surface-input border border-edge rounded-lg px-2 py-1 text-xs text-txt-heading focus:outline-none focus:border-coral focus:ring-1 focus:ring-coral/50 transition cursor-pointer">
            <option value="de" {de_sel}>Deutsch</option>
            <option value="en" {en_sel}>English</option>
        </select>"#,
        bp = bp,
        id = target_id,
        de_sel = if language == "de" { r#"selected"# } else { "" },
        en_sel = if language == "en" { r#"selected"# } else { "" },
    )).into_response()
}

// ── Agent access ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SetAgentsForm {
    /// Comma-separated agent IDs. Empty means "all agents".
    pub agents: Option<String>,
}

/// POST /users/{id}/agents — set which agents a user can access.
pub async fn users_set_agents(
    user: AuthUser,
    Path(target_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<SetAgentsForm>,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }

    ensure_user_agent_access_table(&state.db).await;

    let agents_snapshot = state.agents.load();
    let all_agent_ids: Vec<String> = agents_snapshot
        .keys()
        .filter(|id| id.as_str() != "default")
        .cloned()
        .collect();

    let selected: Vec<String> = form
        .agents
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if let Err(e) =
        crate::agents::access::set_user_agents(&state.db, &target_id, &selected, &all_agent_ids)
            .await
    {
        error!("Failed to set agents for user {target_id}: {e}");
        return Redirect::to(&format!("{}/users?error=agents_save_failed", base_path))
            .into_response();
    }

    Redirect::to(&format!("{}/users", base_path)).into_response()
}

async fn ensure_user_agent_access_table(db: &sqlx::SqlitePool) {
    if let Err(e) = sqlx::query(
        "CREATE TABLE IF NOT EXISTS user_agent_access (
            user_id  TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            agent_id TEXT NOT NULL,
            PRIMARY KEY (user_id, agent_id)
        )",
    )
    .execute(db)
    .await
    {
        error!("Failed ensuring user_agent_access table exists: {e}");
    }
}
