use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Redirect, Response},
    Form,
};
use serde::{Deserialize, Serialize};
use tracing::{error, warn};
use uuid::Uuid;

use crate::state::AppState;
use crate::web::auth::AuthUser;
use crate::web::{BasePath, prefixed_redirect};

// ── View structs ──────────────────────────────────────────────────────────────

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

// ── Templates ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pipes_imessage_setup.html")]
struct ImessageSetupTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    users: Vec<UserOption>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "pipes_imessage_detail.html")]
struct ImessageDetailTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    config: ImessageDetail,
    people: Vec<AllowedPerson>,
    users: Vec<UserOption>,
    flash: Option<String>,
    error: Option<String>,
}

// ── Form structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ImessageSetupForm {
    pub name: String,
    pub server_url: String,
    pub server_password: String,
    pub chat_guid: String,
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

// ── BlueBubbles proxy types ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct BbProxyRequest {
    pub server_url: String,
    pub server_password: String,
}

/// A single chat returned to the frontend picker.
#[derive(Debug, Serialize)]
pub struct BbChatInfo {
    pub guid: String,
    pub display_name: String,
    pub participants: Vec<String>,
    pub is_group: bool,
    pub last_message: String,
}

/// Response envelope for the proxy endpoint.
#[derive(Debug, Serialize)]
pub struct BbProxyResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chats: Option<Vec<BbChatInfo>>,
}

// -- BB API response types (internal) --

#[derive(Debug, Deserialize)]
struct BbApiChatResponse {
    #[allow(dead_code)]
    status: u16,
    data: Vec<BbApiChat>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BbApiChat {
    guid: Option<String>,
    display_name: Option<String>,
    #[serde(default)]
    participants: Option<Vec<BbApiHandle>>,
    #[serde(default)]
    group_id: Option<String>,
    /// When queried with ["lastMessage"], BB nests the full message object here.
    #[serde(default)]
    last_message: Option<BbApiLastMessage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BbApiHandle {
    #[serde(default)]
    address: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BbApiLastMessage {
    #[serde(default)]
    text: Option<String>,
}

// ── Handlers: BlueBubbles proxy ───────────────────────────────────────────────

/// Server-side proxy: fetches chats from the user's BlueBubbles server.
/// This avoids CORS issues (browser can't call BB directly) and keeps
/// the BB password out of the browser's network tab.
pub async fn bb_proxy_chats(
    user: AuthUser,
    axum::Json(req): axum::Json<BbProxyRequest>,
) -> Response {
    if !user.is_admin {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    let server_url = req.server_url.trim_end_matches('/');

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    // BlueBubbles uses POST /api/v1/chat/query with a JSON body.
    // Password goes as a query param; the rest is in the body.
    let url = format!("{}/api/v1/chat/query", server_url);

    let body = serde_json::json!({
        "with": ["lastMessage", "participants"],
        "limit": 50,
        "sort": "lastmessage",
    });

    let resp = match http.post(&url)
        .query(&[("password", req.server_password.as_str())])
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("BB proxy: connection failed: {e}");
            let msg = if e.is_timeout() {
                "Connection timed out. Is the BlueBubbles server running?".to_string()
            } else if e.is_connect() {
                format!("Cannot reach {}. Is the server URL correct and the server running? If Selu runs in Docker, use http://host.docker.internal:PORT instead of localhost.", server_url)
            } else {
                format!("Connection failed: {}", e)
            };
            return Json(BbProxyResponse { ok: false, error: Some(msg), chats: None }).into_response();
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        warn!("BB proxy: server returned {status}: {body}");
        let msg = if status.as_u16() == 401 {
            "Invalid password. Check your BlueBubbles server password.".to_string()
        } else if status.as_u16() == 404 {
            "BlueBubbles returned 404. Make sure the server URL includes the correct port (e.g. http://host.docker.internal:1234).".to_string()
        } else {
            format!("BlueBubbles returned error {}.", status)
        };
        return Json(BbProxyResponse { ok: false, error: Some(msg), chats: None }).into_response();
    }

    let raw_body = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            error!("BB proxy: failed to read response body: {e}");
            return Json(BbProxyResponse {
                ok: false,
                error: Some("Failed to read response from BlueBubbles.".to_string()),
                chats: None,
            }).into_response();
        }
    };

    let bb_resp: BbApiChatResponse = match serde_json::from_str(&raw_body) {
        Ok(r) => r,
        Err(e) => {
            error!("BB proxy: failed to parse response: {e}");
            // Log a truncated version of the body for debugging
            let preview = if raw_body.len() > 500 { &raw_body[..500] } else { &raw_body };
            error!("BB proxy: response body preview: {preview}");
            return Json(BbProxyResponse {
                ok: false,
                error: Some("Got an unexpected response from BlueBubbles. Check the Selu logs for details.".to_string()),
                chats: None,
            }).into_response();
        }
    };

    let chats: Vec<BbChatInfo> = bb_resp.data.into_iter()
        .filter_map(|c| {
            let guid = c.guid?;
            let participants: Vec<String> = c.participants
                .unwrap_or_default()
                .into_iter()
                .filter_map(|p| p.address)
                .collect();

            let display_name = c.display_name
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    if participants.is_empty() {
                        "Unknown".to_string()
                    } else {
                        participants.join(", ")
                    }
                });

            let is_group = c.group_id.is_some() || participants.len() > 1;

            let last_message = c.last_message
                .and_then(|m| m.text)
                .unwrap_or_default();

            // Truncate long last messages
            let last_message = if last_message.len() > 80 {
                format!("{}...", &last_message[..77])
            } else {
                last_message
            };

            Some(BbChatInfo {
                guid,
                display_name,
                participants,
                is_group,
                last_message,
            })
        })
        .collect();

    Json(BbProxyResponse { ok: true, error: None, chats: Some(chats) }).into_response()
}

// ── Handlers: iMessage Setup Wizard ───────────────────────────────────────────

pub async fn imessage_setup_page(
    user: AuthUser,
    Query(q): Query<FlashQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }
    let users = load_users(&state.db).await;

    match (ImessageSetupTemplate {
        active_nav: "pipes",
        is_admin: user.is_admin,
        base_path,
        users,
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
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<ImessageSetupForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    // Validate
    if form.name.trim().is_empty()
        || form.server_url.trim().is_empty()
        || form.server_password.is_empty()
        || form.chat_guid.trim().is_empty()
    {
        return Redirect::to(&format!("{}/pipes/imessage/setup?error=Please+fill+in+all+required+fields", base_path)).into_response();
    }

    // 1. Find a user to own the pipe (first user, or from people list)
    let owner_user_id = match find_pipe_owner(&state.db, &form.people).await {
        Some(id) => id,
        None => {
            return Redirect::to(&format!("{}/pipes/imessage/setup?error=You+need+at+least+one+user.+Create+one+in+Users+first.", base_path)).into_response();
        }
    };

    // 2. Auto-create the pipe (no outbound URL needed — BB adapter sends directly)
    let pipe_id = Uuid::new_v4().to_string();
    let inbound_token = Uuid::new_v4().to_string().replace('-', "");
    let default_agent_id: Option<&str> = None; // Always use default agent
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
        return Redirect::to(&format!("{}/pipes/imessage/setup?error=Failed+to+create+pipe", base_path)).into_response();
    }

    // 3. Create the BlueBubbles config
    let bb_config_id = Uuid::new_v4().to_string();

    if let Err(e) = sqlx::query!(
        "INSERT INTO bluebubbles_configs (id, name, server_url, server_password, chat_guid, pipe_id)
         VALUES (?, ?, ?, ?, ?, ?)",
        bb_config_id, form.name, form.server_url, form.server_password, form.chat_guid, pipe_id,
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to create BB config: {e}");
        return Redirect::to(&format!("{}/pipes/imessage/setup?error=Failed+to+create+adapter+config", base_path)).into_response();
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

    // 5. Start the adapter immediately (no restart needed)
    let bp = base_path;
    if let Err(e) = crate::bluebubbles::adapter::start_one(state, &bb_config_id).await {
        error!("Failed to start BlueBubbles adapter: {e}");
        // Non-fatal: the config is saved, adapter will start on next restart
    }

    Redirect::to(&format!(
        "{}/pipes/imessage/{}?msg=iMessage+pipe+set+up+successfully!",
        bp, bb_config_id
    )).into_response()
}

// ── Handlers: iMessage Detail ─────────────────────────────────────────────────

pub async fn imessage_detail(
    user: AuthUser,
    Path(config_id): Path<String>,
    Query(q): Query<FlashQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }
    let config = match load_imessage_config(&state.db, &config_id).await {
        Some(c) => c,
        None => return prefixed_redirect(&base_path, "/pipes").into_response(),
    };

    let people = load_allowed_people(&state.db, &config.pipe_id).await;
    let users = load_users(&state.db).await;

    match (ImessageDetailTemplate {
        active_nav: "pipes",
        is_admin: user.is_admin,
        base_path,
        config,
        people,
        users,
        flash: q.msg,
        error: q.error,
    }).render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn imessage_add_person(
    user: AuthUser,
    Path(config_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<AddPersonForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    if form.user_id.is_empty() || form.sender_ref.trim().is_empty() {
        return Redirect::to(&format!(
            "{}/pipes/imessage/{}?error=Please+fill+in+both+fields", base_path, config_id
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
        None => return prefixed_redirect(&base_path, "/pipes").into_response(),
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
            "{}/pipes/imessage/{}?msg=Person+added", base_path, config_id
        )).into_response(),
        Err(e) if e.to_string().contains("UNIQUE") => Redirect::to(&format!(
            "{}/pipes/imessage/{}?error=This+phone+number+is+already+added", base_path, config_id
        )).into_response(),
        Err(e) => {
            error!("Failed to add person: {e}");
            Redirect::to(&format!(
                "{}/pipes/imessage/{}?error=Failed+to+add+person.+Please+try+again.", base_path, config_id
            )).into_response()
        }
    }
}

pub async fn imessage_remove_person(
    user: AuthUser,
    Path((_config_id, ref_id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    let _ = sqlx::query!("DELETE FROM user_sender_refs WHERE id = ?", ref_id)
        .execute(&state.db)
        .await;
    Html("").into_response()
}

pub async fn imessage_delete(
    user: AuthUser,
    Path(config_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    // Deregister webhook from BlueBubbles before deactivating
    if let Ok(Some(row)) = sqlx::query!(
        "SELECT server_url, server_password, bb_webhook_id
         FROM bluebubbles_configs WHERE id = ?",
        config_id
    )
    .fetch_optional(&state.db)
    .await
    {
        if let Some(webhook_id) = row.bb_webhook_id {
            if let Err(e) = crate::bluebubbles::adapter::deregister_webhook_from_bb(
                &row.server_url,
                &row.server_password,
                &webhook_id,
            ).await {
                warn!("Failed to deregister BB webhook: {e}");
                // Non-fatal: continue with deactivation
            }
        }
    }

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

    Redirect::to(&format!("{}/pipes?msg=Pipe+removed", base_path)).into_response()
}

// ── DB helpers ────────────────────────────────────────────────────────────────

pub async fn load_imessage_configs(db: &sqlx::SqlitePool) -> Vec<ImessageDetail> {
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
