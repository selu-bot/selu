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
#[allow(dead_code)]
pub struct IntegrationOverview {
    pub id: String,
    pub name: String,
    pub status: String,        // "connected", "not_configured"
    pub detail: String,        // e.g. "2 users, 3 threads today"
}

#[derive(Debug, Clone)]
pub struct ImessageDetail {
    pub config_id: String,
    pub name: String,
    pub server_url: String,
    pub chat_guid: String,
    pub active: bool,
    pub pipe_id: String,
    pub agent_name: String,
}

#[derive(Debug, Clone)]
pub struct AllowedPerson {
    pub ref_id: String,
    pub display_name: String,
    #[allow(dead_code)]
    pub username: String,
    pub sender_ref: String,
    pub initials: String,
}

#[derive(Debug, Clone)]
pub struct UserOption {
    pub id: String,
    pub display: String,
}

#[derive(Debug, Clone)]
pub struct AgentOption {
    pub id: String,
    pub name: String,
}

// ── Templates ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "integrations.html")]
struct IntegrationsTemplate {
    active_nav: &'static str,
    imessage_configs: Vec<ImessageDetail>,
}

#[derive(Template)]
#[template(path = "integrations_imessage_setup.html")]
struct ImessageSetupTemplate {
    active_nav: &'static str,
    users: Vec<UserOption>,
    agents: Vec<AgentOption>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "integrations_imessage_detail.html")]
struct ImessageDetailTemplate {
    active_nav: &'static str,
    config: ImessageDetail,
    people: Vec<AllowedPerson>,
    users: Vec<UserOption>,
    flash: Option<String>,
}

// ── Form structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ImessageSetupForm {
    pub name: String,
    pub server_url: String,
    pub server_password: String,
    pub chat_guid: String,
    pub default_agent_id: Option<String>,
    /// Comma-separated list of "phone:user_id" pairs
    /// e.g. "+14155551234:uuid1,+14155555678:uuid2"
    pub people: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddPersonForm {
    pub user_id: String,
    pub sender_ref: String,
}

#[derive(Debug, Deserialize)]
pub struct FlashQuery {
    pub msg: Option<String>,
    pub error: Option<String>,
}

// ── Handlers: Overview ────────────────────────────────────────────────────────

pub async fn integrations_index(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Response {
    let imessage_configs = load_imessage_configs(&state.db).await;

    match (IntegrationsTemplate {
        active_nav: "integrations",
        imessage_configs,
    }).render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── Handlers: iMessage Setup Wizard ───────────────────────────────────────────

pub async fn imessage_setup_page(
    _user: AuthUser,
    Query(q): Query<FlashQuery>,
    State(state): State<AppState>,
) -> Response {
    let users = load_users(&state.db).await;
    let agents = load_agents(&state).await;

    match (ImessageSetupTemplate {
        active_nav: "integrations",
        users,
        agents,
        error: q.error,
    }).render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// One-click setup: creates pipe + BB config + sender mappings in one go.
pub async fn imessage_setup_submit(
    _user: AuthUser,
    State(state): State<AppState>,
    Form(form): Form<ImessageSetupForm>,
) -> Response {
    // Validate
    if form.name.trim().is_empty()
        || form.server_url.trim().is_empty()
        || form.server_password.is_empty()
        || form.chat_guid.trim().is_empty()
    {
        return Redirect::to("/integrations/imessage/setup?error=Please+fill+in+all+required+fields").into_response();
    }

    // 1. Find a user to own the pipe (first user, or from people list)
    let owner_user_id = match find_pipe_owner(&state.db, &form.people).await {
        Some(id) => id,
        None => {
            return Redirect::to("/integrations/imessage/setup?error=You+need+at+least+one+user.+Create+one+in+Users+first.").into_response();
        }
    };

    // 2. Auto-create the pipe (no outbound URL needed — BB adapter sends directly)
    let pipe_id = Uuid::new_v4().to_string();
    let inbound_token = Uuid::new_v4().to_string().replace('-', "");
    let default_agent_id = form.default_agent_id.as_deref().filter(|s| !s.is_empty());
    let pipe_name = format!("iMessage: {}", form.name.trim());
    let transport = "webhook";
    let outbound_url = "internal://bluebubbles"; // Marker: handled by integrated adapter

    if let Err(e) = sqlx::query!(
        "INSERT INTO pipes (id, user_id, name, transport, inbound_token, outbound_url, default_agent_id)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        pipe_id, owner_user_id, pipe_name, transport, inbound_token, outbound_url, default_agent_id,
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to create pipe: {e}");
        return Redirect::to("/integrations/imessage/setup?error=Failed+to+create+pipe").into_response();
    }

    // 3. Create the BlueBubbles config
    let bb_config_id = Uuid::new_v4().to_string();
    let poll_ms: i64 = 2000;

    if let Err(e) = sqlx::query!(
        "INSERT INTO bluebubbles_configs (id, name, server_url, server_password, chat_guid, pipe_id, poll_interval_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        bb_config_id, form.name, form.server_url, form.server_password, form.chat_guid, pipe_id, poll_ms,
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to create BB config: {e}");
        return Redirect::to("/integrations/imessage/setup?error=Failed+to+create+adapter+config").into_response();
    }

    // 4. Create sender ref mappings from the people list
    if let Some(ref people_str) = form.people {
        for entry in people_str.split(',') {
            let entry = entry.trim();
            if entry.is_empty() { continue; }
            let parts: Vec<&str> = entry.splitn(2, ':').collect();
            if parts.len() == 2 {
                let sender_ref = parts[0].trim();
                let user_id = parts[1].trim();
                if !sender_ref.is_empty() && !user_id.is_empty() {
                    let ref_id = Uuid::new_v4().to_string();
                    let _ = sqlx::query!(
                        "INSERT OR IGNORE INTO user_sender_refs (id, user_id, pipe_id, sender_ref)
                         VALUES (?, ?, ?, ?)",
                        ref_id, user_id, pipe_id, sender_ref,
                    )
                    .execute(&state.db)
                    .await;
                }
            }
        }
    }

    Redirect::to(&format!(
        "/integrations/imessage/{}?msg=iMessage+integration+set+up+successfully!+Restart+PAP+to+start+the+adapter.",
        bb_config_id
    )).into_response()
}

// ── Handlers: iMessage Detail ─────────────────────────────────────────────────

pub async fn imessage_detail(
    _user: AuthUser,
    Path(config_id): Path<String>,
    Query(q): Query<FlashQuery>,
    State(state): State<AppState>,
) -> Response {
    let config = match load_imessage_config(&state.db, &config_id).await {
        Some(c) => c,
        None => return Redirect::to("/integrations").into_response(),
    };

    let people = load_allowed_people(&state.db, &config.pipe_id).await;
    let users = load_users(&state.db).await;

    match (ImessageDetailTemplate {
        active_nav: "integrations",
        config,
        people,
        users,
        flash: q.msg,
    }).render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn imessage_add_person(
    _user: AuthUser,
    Path(config_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<AddPersonForm>,
) -> Response {
    if form.user_id.is_empty() || form.sender_ref.trim().is_empty() {
        return Redirect::to(&format!(
            "/integrations/imessage/{}?msg=Please+fill+in+both+fields", config_id
        )).into_response();
    }

    // Get pipe_id from config
    let pipe_id = match sqlx::query!("SELECT pipe_id FROM bluebubbles_configs WHERE id = ?", config_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
    {
        Some(r) => r.pipe_id,
        None => return Redirect::to("/integrations").into_response(),
    };

    let ref_id = Uuid::new_v4().to_string();
    match sqlx::query!(
        "INSERT INTO user_sender_refs (id, user_id, pipe_id, sender_ref) VALUES (?, ?, ?, ?)",
        ref_id, form.user_id, pipe_id, form.sender_ref,
    )
    .execute(&state.db)
    .await
    {
        Ok(_) => Redirect::to(&format!(
            "/integrations/imessage/{}?msg=Person+added", config_id
        )).into_response(),
        Err(e) if e.to_string().contains("UNIQUE") => Redirect::to(&format!(
            "/integrations/imessage/{}?msg=This+phone+number+is+already+added", config_id
        )).into_response(),
        Err(e) => {
            error!("Failed to add person: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn imessage_remove_person(
    _user: AuthUser,
    Path((_config_id, ref_id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let _ = sqlx::query!("DELETE FROM user_sender_refs WHERE id = ?", ref_id)
        .execute(&state.db)
        .await;
    Html("").into_response()
}

pub async fn imessage_delete(
    _user: AuthUser,
    Path(config_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    // Deactivate the BB config and its pipe
    let _ = sqlx::query!("UPDATE bluebubbles_configs SET active = 0 WHERE id = ?", config_id)
        .execute(&state.db)
        .await;
    // Also deactivate the associated pipe
    let _ = sqlx::query!(
        "UPDATE pipes SET active = 0 WHERE id IN (SELECT pipe_id FROM bluebubbles_configs WHERE id = ?)",
        config_id
    )
    .execute(&state.db)
    .await;

    Redirect::to("/integrations?msg=Integration+removed").into_response()
}

// ── DB helpers ────────────────────────────────────────────────────────────────

async fn load_imessage_configs(db: &sqlx::SqlitePool) -> Vec<ImessageDetail> {
    sqlx::query!(
        r#"SELECT bc.id, bc.name, bc.server_url, bc.chat_guid, bc.active, bc.pipe_id,
                  COALESCE(p.default_agent_id, 'default') as "agent_name!: String"
           FROM bluebubbles_configs bc
           JOIN pipes p ON p.id = bc.pipe_id
           ORDER BY bc.created_at DESC"#
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| ImessageDetail {
        config_id: r.id.unwrap_or_default(),
        name: r.name,
        server_url: r.server_url,
        chat_guid: r.chat_guid,
        active: r.active != 0,
        pipe_id: r.pipe_id,
        agent_name: r.agent_name,
    })
    .collect()
}

async fn load_imessage_config(db: &sqlx::SqlitePool, config_id: &str) -> Option<ImessageDetail> {
    sqlx::query!(
        r#"SELECT bc.id, bc.name, bc.server_url, bc.chat_guid, bc.active, bc.pipe_id,
                  COALESCE(p.default_agent_id, 'default') as "agent_name!: String"
           FROM bluebubbles_configs bc
           JOIN pipes p ON p.id = bc.pipe_id
           WHERE bc.id = ?"#,
        config_id
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .map(|r| ImessageDetail {
        config_id: r.id.unwrap_or_default(),
        name: r.name,
        server_url: r.server_url,
        chat_guid: r.chat_guid,
        active: r.active != 0,
        pipe_id: r.pipe_id,
        agent_name: r.agent_name,
    })
}

async fn load_allowed_people(db: &sqlx::SqlitePool, pipe_id: &str) -> Vec<AllowedPerson> {
    sqlx::query!(
        r#"SELECT sr.id, sr.sender_ref, u.display_name, u.username
           FROM user_sender_refs sr
           JOIN users u ON u.id = sr.user_id
           WHERE sr.pipe_id = ?
           ORDER BY u.display_name"#,
        pipe_id
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        let initials = r.display_name.chars()
            .filter(|c| c.is_alphabetic())
            .take(2)
            .collect::<String>()
            .to_uppercase();
        AllowedPerson {
            ref_id: r.id.unwrap_or_default(),
            display_name: r.display_name,
            username: r.username,
            sender_ref: r.sender_ref,
            initials: if initials.is_empty() { "?".to_string() } else { initials },
        }
    })
    .collect()
}

async fn load_users(db: &sqlx::SqlitePool) -> Vec<UserOption> {
    sqlx::query!("SELECT id, username, display_name FROM users ORDER BY username")
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| UserOption {
            id: r.id.unwrap_or_default(),
            display: format!("{} ({})", r.display_name, r.username),
        })
        .collect()
}

async fn load_agents(state: &AppState) -> Vec<AgentOption> {
    let map = state.agents.read().await;
    let mut v: Vec<AgentOption> = map
        .values()
        .map(|d| AgentOption { id: d.id.clone(), name: d.name.clone() })
        .collect();
    v.sort_by(|a, b| a.id.cmp(&b.id));
    v
}

async fn find_pipe_owner(db: &sqlx::SqlitePool, people: &Option<String>) -> Option<String> {
    // Try to get the first user from the people list
    if let Some(people_str) = people {
        for entry in people_str.split(',') {
            let parts: Vec<&str> = entry.trim().splitn(2, ':').collect();
            if parts.len() == 2 && !parts[1].trim().is_empty() {
                return Some(parts[1].trim().to_string());
            }
        }
    }
    // Fall back to first user in DB
    sqlx::query!("SELECT id FROM users ORDER BY created_at LIMIT 1")
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.id)
}
