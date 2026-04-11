use anyhow::{Context, Result};
use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Redirect, Response},
};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::state::AppState;
use crate::updater::client::SidecarUpdaterClient;
use crate::updater::types::{
    SidecarEnsureWhatsappBridgeRequest, SidecarStopWhatsappBridgeRequest,
    SidecarWhatsappBridgeChatsResponse, SidecarWhatsappBridgeStatusResponse,
};
use crate::web::auth::AuthUser;
use crate::web::{BasePath, prefixed_redirect};

const DEFAULT_BRIDGE_OUTBOUND_URL_DOCKER: &str =
    "http://selu-whatsapp-bridge:3200/webhooks/selu/outbound";
const DEFAULT_BRIDGE_OUTBOUND_URL_LOCAL: &str = "http://127.0.0.1:3200/webhooks/selu/outbound";
const DEFAULT_SELU_INBOUND_ORIGIN_DOCKER: &str = "http://selu";
const DEFAULT_SELU_INBOUND_ORIGIN_LOCAL: &str = "http://host.docker.internal";

#[derive(Debug, Clone)]
pub struct WhatsappDetail {
    pub config_id: String,
    pub name: String,
    pub bridge_url: String,
    pub active: bool,
    pub pipe_id: String,
    pub agent_name: String,
    pub inbound_token: String,
}

#[derive(Debug, Clone)]
pub struct AllowedPerson {
    pub ref_id: String,
    pub display_name: String,
    pub sender_ref: String,
    pub initials: String,
}

#[derive(Debug, Clone)]
pub struct UserOption {
    pub id: String,
    pub display: String,
}

#[derive(Debug, Clone)]
pub struct BridgeStatusView {
    pub running: bool,
    pub connection_state: Option<String>,
    pub connection_state_key: Option<String>,
    pub requires_qr: bool,
    pub qr_data_url: Option<String>,
    pub jid: Option<String>,
    pub last_error: Option<String>,
    pub message: Option<String>,
    pub message_key: Option<String>,
}

#[derive(Template)]
#[template(path = "pipes_whatsapp_setup.html")]
struct WhatsappSetupTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    already_configured: bool,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "pipes_whatsapp_detail.html")]
struct WhatsappDetailTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    config: WhatsappDetail,
    people: Vec<AllowedPerson>,
    users: Vec<UserOption>,
    inbound_url: String,
    bridge_status: Option<BridgeStatusView>,
    flash: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WhatsappSetupForm {
    pub name: String,
    pub outbound_auth: Option<String>,
    /// Comma-separated list of "sender_ref:user_id" pairs
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

#[derive(Debug, Deserialize)]
pub struct ChatsSearchQuery {
    pub q: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChatOption {
    pub sender_ref: String,
    pub label: String,
}

#[derive(Debug, Serialize)]
pub struct ChatSearchResponse {
    pub ok: bool,
    pub running: bool,
    pub connection_state: Option<String>,
    pub message: Option<String>,
    pub chats: Vec<ChatOption>,
}

pub async fn whatsapp_setup_page(
    user: AuthUser,
    Query(q): Query<FlashQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }

    let already_configured = has_active_whatsapp_pipe(&state.db).await;

    match (WhatsappSetupTemplate {
        active_nav: "pipes",
        is_admin: user.is_admin,
        base_path,
        already_configured,
        error: q.error,
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

pub async fn whatsapp_search_chats(
    user: AuthUser,
    State(state): State<AppState>,
    Query(q): Query<ChatsSearchQuery>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    let client = match SidecarUpdaterClient::from_config(&state.config) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to create updater client for WhatsApp chat search: {e}");
            return Json(ChatSearchResponse {
                ok: false,
                running: false,
                connection_state: None,
                message: Some("Could not reach updates service".to_string()),
                chats: Vec::new(),
            })
            .into_response();
        }
    };

    match client
        .whatsapp_bridge_chats(q.q.as_deref().unwrap_or_default())
        .await
    {
        Ok(resp) => Json(map_chat_search_response(resp)).into_response(),
        Err(e) => {
            warn!("Failed to fetch WhatsApp chats from updater: {e:#}");
            Json(ChatSearchResponse {
                ok: false,
                running: false,
                connection_state: None,
                message: Some("Could not load chats right now".to_string()),
                chats: Vec::new(),
            })
            .into_response()
        }
    }
}

pub async fn whatsapp_setup_submit(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<WhatsappSetupForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    let name = form.name.trim().to_string();
    let bridge_url = managed_bridge_outbound_url(&state.config);

    if name.is_empty() {
        return Redirect::to(&format!(
            "{}/pipes/whatsapp/setup?error=Please+fill+in+all+required+fields",
            base_path
        ))
        .into_response();
    }

    if has_active_whatsapp_pipe(&state.db).await {
        return Redirect::to(&format!(
            "{}/pipes/whatsapp/setup?error=WhatsApp+is+already+set+up.+Remove+the+existing+WhatsApp+pipe+first.",
            base_path
        ))
        .into_response();
    }

    let owner_user_id = match find_pipe_owner(&state.db, &form.people).await {
        Some(id) => id,
        None => {
            return Redirect::to(&format!(
                "{}/pipes/whatsapp/setup?error=You+need+at+least+one+user.+Create+one+in+Users+first.",
                base_path
            ))
            .into_response();
        }
    };

    let pipe_id = Uuid::new_v4().to_string();
    let inbound_token = Uuid::new_v4().to_string().replace('-', "");
    let default_agent_id: Option<&str> = None;
    let pipe_name = format!("WhatsApp: {}", name);
    let transport = "webhook";
    let outbound_auth = form.outbound_auth.filter(|s| !s.trim().is_empty());
    let inbound_url = managed_bridge_inbound_url(&state.config, &state.public_base_url(), &pipe_id);

    // Make sidecar startup transparent: ensure bridge image is pulled and running.
    // If this fails, block setup with a user-friendly action message.
    let channel = std::env::var("SELU_RELEASE_CHANNEL").unwrap_or_else(|_| "stable".to_string());
    if let Ok(client) = SidecarUpdaterClient::from_config(&state.config) {
        let ack = client
            .ensure_whatsapp_bridge(&SidecarEnsureWhatsappBridgeRequest {
                request_id: Uuid::new_v4().to_string(),
                channel,
                inbound_url,
                inbound_token: inbound_token.clone(),
                outbound_auth: outbound_auth.clone().unwrap_or_default(),
            })
            .await;
        match ack {
            Ok(a) if a.accepted => {}
            Ok(a) => {
                let msg = a.message.unwrap_or_else(|| {
                    "Could not start WhatsApp bridge. Please try again.".to_string()
                });
                return Redirect::to(&format!(
                    "{}/pipes/whatsapp/setup?error={}",
                    base_path,
                    urlencoding::encode(&msg),
                ))
                .into_response();
            }
            Err(e) => {
                warn!("Failed to ensure WhatsApp bridge via updater: {e:#}");
                return Redirect::to(&format!(
                    "{}/pipes/whatsapp/setup?error={}",
                    base_path,
                    urlencoding::encode(&format!("Couldn't start WhatsApp bridge. {}", e)),
                ))
                .into_response();
            }
        }
    } else {
        return Redirect::to(&format!(
            "{}/pipes/whatsapp/setup?error=Couldn%27t+start+WhatsApp+bridge.+Please+check+updates+service+and+try+again.",
            base_path
        ))
        .into_response();
    }

    if let Err(e) = sqlx::query!(
        "INSERT INTO pipes (id, user_id, name, transport, inbound_token, outbound_url, outbound_auth, default_agent_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        pipe_id,
        owner_user_id,
        pipe_name,
        transport,
        inbound_token,
        bridge_url,
        outbound_auth,
        default_agent_id,
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to create pipe: {e}");
        return Redirect::to(&format!(
            "{}/pipes/whatsapp/setup?error=Failed+to+create+pipe",
            base_path
        ))
        .into_response();
    }

    let wa_config_id = Uuid::new_v4().to_string();
    if let Err(e) = sqlx::query!(
        "INSERT INTO whatsapp_configs (id, name, bridge_url, pipe_id)
         VALUES (?, ?, ?, ?)",
        wa_config_id,
        name,
        bridge_url,
        pipe_id,
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to create WhatsApp config: {e}");
        return Redirect::to(&format!(
            "{}/pipes/whatsapp/setup?error=Failed+to+create+WhatsApp+config",
            base_path
        ))
        .into_response();
    }

    if let Some(ref people_str) = form.people {
        for entry in people_str.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let parts: Vec<&str> = entry.splitn(2, ':').collect();
            if parts.len() == 2 {
                let sender_ref = parts[0].trim();
                let user_id = parts[1].trim();
                if !sender_ref.is_empty() && !user_id.is_empty() {
                    let ref_id = Uuid::new_v4().to_string();
                    let _ = sqlx::query!(
                        "INSERT OR IGNORE INTO user_sender_refs (id, user_id, pipe_id, sender_ref)
                         VALUES (?, ?, ?, ?)",
                        ref_id,
                        user_id,
                        pipe_id,
                        sender_ref,
                    )
                    .execute(&state.db)
                    .await;
                }
            }
        }
    }

    Redirect::to(&format!(
        "{}/pipes/whatsapp/{}?msg=WhatsApp+pipe+set+up+successfully!",
        base_path, wa_config_id
    ))
    .into_response()
}

pub async fn whatsapp_detail(
    user: AuthUser,
    Path(config_id): Path<String>,
    Query(q): Query<FlashQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }

    let config = match load_whatsapp_config(&state.db, &config_id).await {
        Some(c) => c,
        None => return prefixed_redirect(&base_path, "/pipes").into_response(),
    };

    let people = load_allowed_people(&state.db, &config.pipe_id).await;
    let users = load_users(&state.db).await;
    let inbound_url =
        managed_bridge_inbound_url(&state.config, &state.public_base_url(), &config.pipe_id);
    let bridge_status = load_bridge_status(&state).await;

    match (WhatsappDetailTemplate {
        active_nav: "pipes",
        is_admin: user.is_admin,
        base_path,
        config,
        people,
        users,
        inbound_url,
        bridge_status,
        flash: q.msg,
        error: q.error,
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

pub async fn whatsapp_add_person(
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
            "{}/pipes/whatsapp/{}?error=wa.detail.person.missing",
            base_path, config_id
        ))
        .into_response();
    }

    let pipe_id = match sqlx::query!(
        "SELECT pipe_id FROM whatsapp_configs WHERE id = ?",
        config_id
    )
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
        ref_id,
        form.user_id,
        pipe_id,
        form.sender_ref,
    )
    .execute(&state.db)
    .await
    {
        Ok(_) => Redirect::to(&format!(
            "{}/pipes/whatsapp/{}?msg=pipes.person.added",
            base_path, config_id
        ))
        .into_response(),
        Err(e) if e.to_string().contains("UNIQUE") => Redirect::to(&format!(
            "{}/pipes/whatsapp/{}?error=wa.detail.person.exists",
            base_path, config_id
        ))
        .into_response(),
        Err(e) => {
            error!("Failed to add person: {e}");
            Redirect::to(&format!(
                "{}/pipes/whatsapp/{}?error=pipes.person.add.error",
                base_path, config_id
            ))
            .into_response()
        }
    }
}

pub async fn whatsapp_remove_person(
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

pub async fn whatsapp_delete(
    user: AuthUser,
    Path(config_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    let pipe_id = match sqlx::query!(
        "SELECT pipe_id FROM whatsapp_configs WHERE id = ?",
        config_id
    )
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(row)) => row.pipe_id,
        Ok(None) => {
            return Redirect::to(&format!(
                "{}/pipes?error={}",
                base_path,
                urlencoding::encode("Couldn't find that WhatsApp pipe.")
            ))
            .into_response();
        }
        Err(e) => {
            error!(config_id = %config_id, "Failed to load WhatsApp pipe for deletion: {e:#}");
            return Redirect::to(&format!(
                "{}/pipes?error={}",
                base_path,
                urlencoding::encode(
                    "Couldn't remove the WhatsApp pipe right now. Please try again."
                )
            ))
            .into_response();
        }
    };

    if let Err(e) = deactivate_whatsapp_pipe(&state, &config_id, &pipe_id).await {
        error!(
            config_id = %config_id,
            pipe_id = %pipe_id,
            "Failed to deactivate WhatsApp pipe: {e:#}"
        );
        return Redirect::to(&format!(
            "{}/pipes?error={}",
            base_path,
            urlencoding::encode("Couldn't remove the WhatsApp pipe right now. Please try again.")
        ))
        .into_response();
    }

    if let Err(e) = stop_bridge_if_no_active_pipe(&state).await {
        error!(
            config_id = %config_id,
            pipe_id = %pipe_id,
            "WhatsApp pipe was removed but bridge cleanup failed: {e:#}"
        );
        return Redirect::to(&format!(
            "{}/pipes?error={}",
            base_path,
            urlencoding::encode(
                "The WhatsApp pipe was removed, but the old phone connection could not be cleared. Please try again or restart updates service."
            )
        ))
        .into_response();
    }

    Redirect::to(&format!("{}/pipes?msg=Pipe+removed", base_path)).into_response()
}

pub async fn load_whatsapp_configs(db: &sqlx::SqlitePool) -> Vec<WhatsappDetail> {
    sqlx::query!(
        r#"SELECT wc.id, wc.name, wc.bridge_url, wc.active, wc.pipe_id,
                  p.inbound_token,
                  COALESCE(p.default_agent_id, 'default') as "agent_name!: String"
           FROM whatsapp_configs wc
           JOIN pipes p ON p.id = wc.pipe_id
           ORDER BY wc.created_at DESC"#
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| WhatsappDetail {
        config_id: r.id.unwrap_or_default(),
        name: r.name,
        bridge_url: r.bridge_url,
        active: r.active != 0,
        pipe_id: r.pipe_id,
        agent_name: r.agent_name,
        inbound_token: r.inbound_token,
    })
    .collect()
}

async fn load_whatsapp_config(db: &sqlx::SqlitePool, config_id: &str) -> Option<WhatsappDetail> {
    sqlx::query!(
        r#"SELECT wc.id, wc.name, wc.bridge_url, wc.active, wc.pipe_id,
                  p.inbound_token,
                  COALESCE(p.default_agent_id, 'default') as "agent_name!: String"
           FROM whatsapp_configs wc
           JOIN pipes p ON p.id = wc.pipe_id
           WHERE wc.id = ?"#,
        config_id
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .map(|r| WhatsappDetail {
        config_id: r.id.unwrap_or_default(),
        name: r.name,
        bridge_url: r.bridge_url,
        active: r.active != 0,
        pipe_id: r.pipe_id,
        agent_name: r.agent_name,
        inbound_token: r.inbound_token,
    })
}

async fn load_allowed_people(db: &sqlx::SqlitePool, pipe_id: &str) -> Vec<AllowedPerson> {
    sqlx::query!(
        r#"SELECT sr.id, sr.sender_ref, u.display_name
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
        let initials = r
            .display_name
            .chars()
            .filter(|c| c.is_alphabetic())
            .take(2)
            .collect::<String>()
            .to_uppercase();
        AllowedPerson {
            ref_id: r.id.unwrap_or_default(),
            display_name: r.display_name,
            sender_ref: r.sender_ref,
            initials: if initials.is_empty() {
                "?".to_string()
            } else {
                initials
            },
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
    if let Some(people_str) = people {
        for entry in people_str.split(',') {
            let parts: Vec<&str> = entry.trim().splitn(2, ':').collect();
            if parts.len() == 2 && !parts[1].trim().is_empty() {
                return Some(parts[1].trim().to_string());
            }
        }
    }

    sqlx::query!("SELECT id FROM users ORDER BY created_at LIMIT 1")
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.id)
}

fn managed_bridge_outbound_url(config: &AppConfig) -> String {
    let updater = config.updater_url.to_ascii_lowercase();
    if updater.contains("localhost") || updater.contains("127.0.0.1") {
        return DEFAULT_BRIDGE_OUTBOUND_URL_LOCAL.to_string();
    }
    DEFAULT_BRIDGE_OUTBOUND_URL_DOCKER.to_string()
}

fn managed_bridge_inbound_url(config: &AppConfig, public_base_url: &str, pipe_id: &str) -> String {
    format!(
        "{}/api/pipes/{}/inbound",
        managed_bridge_inbound_origin(config, public_base_url),
        pipe_id
    )
}

fn managed_bridge_inbound_origin(config: &AppConfig, public_base_url: &str) -> String {
    let external = public_base_url.trim();
    let lower = external.to_ascii_lowercase();
    if !external.is_empty() && !lower.contains("localhost") && !lower.contains("127.0.0.1") {
        return external.trim_end_matches('/').to_string();
    }

    let updater = config.updater_url.to_ascii_lowercase();
    let origin = if updater.contains("localhost") || updater.contains("127.0.0.1") {
        format!(
            "{}:{}",
            DEFAULT_SELU_INBOUND_ORIGIN_LOCAL, config.server.port
        )
    } else {
        format!(
            "{}:{}",
            DEFAULT_SELU_INBOUND_ORIGIN_DOCKER, config.server.port
        )
    };

    if config.base_path().is_empty() {
        origin
    } else {
        format!("{}{}", origin, config.base_path())
    }
}

fn whatsapp_bridge_message_key(message: &str) -> Option<&'static str> {
    match message {
        "WhatsApp bridge is disabled" => Some("wa.status.bridge_disabled"),
        "WhatsApp bridge is not running" => Some("wa.status.bridge_not_running"),
        "WhatsApp bridge is running" => Some("wa.status.bridge_running"),
        "WhatsApp bridge is running, but session status is unavailable" => {
            Some("wa.status.bridge_running_session_unavailable")
        }
        "WhatsApp chats loaded" => Some("wa.status.chats_loaded"),
        "WhatsApp bridge is running, but chats are unavailable" => {
            Some("wa.status.bridge_running_chats_unavailable")
        }
        _ => None,
    }
}

fn whatsapp_connection_state_key(state: &str) -> Option<&'static str> {
    match state.trim().to_ascii_lowercase().as_str() {
        "open" => Some("wa.connection.open"),
        "connected" => Some("wa.connection.connected"),
        "connecting" => Some("wa.connection.connecting"),
        "close" => Some("wa.connection.closed"),
        "closed" => Some("wa.connection.closed"),
        "disconnected" => Some("wa.connection.disconnected"),
        "qr" => Some("wa.connection.qr"),
        "scan" => Some("wa.connection.qr"),
        "pairing" => Some("wa.connection.pairing"),
        _ => None,
    }
}

fn map_chat_search_response(resp: SidecarWhatsappBridgeChatsResponse) -> ChatSearchResponse {
    ChatSearchResponse {
        ok: true,
        running: resp.running,
        connection_state: resp.connection_state,
        message: resp.message,
        chats: resp
            .chats
            .into_iter()
            .map(|c| ChatOption {
                sender_ref: c.sender_ref,
                label: c.label,
            })
            .collect(),
    }
}

async fn has_active_whatsapp_pipe(db: &sqlx::SqlitePool) -> bool {
    sqlx::query!(
        r#"SELECT COUNT(1) as "count!: i64"
           FROM whatsapp_configs wc
           JOIN pipes p ON p.id = wc.pipe_id
           WHERE wc.active = 1 AND p.active = 1"#
    )
    .fetch_one(db)
    .await
    .map(|r| r.count > 0)
    .unwrap_or(false)
}

async fn load_bridge_status(state: &AppState) -> Option<BridgeStatusView> {
    let client = SidecarUpdaterClient::from_config(&state.config).ok()?;
    let status: SidecarWhatsappBridgeStatusResponse = client.whatsapp_bridge_status().await.ok()?;
    Some(BridgeStatusView {
        running: status.running,
        connection_state_key: status
            .connection_state
            .as_deref()
            .and_then(whatsapp_connection_state_key)
            .map(str::to_string),
        connection_state: status.connection_state,
        requires_qr: status.requires_qr,
        qr_data_url: status.qr_data_url,
        jid: status.jid,
        last_error: status.last_error,
        message_key: status
            .message
            .as_deref()
            .and_then(whatsapp_bridge_message_key)
            .map(str::to_string),
        message: status.message,
    })
}

async fn deactivate_whatsapp_pipe(state: &AppState, config_id: &str, pipe_id: &str) -> Result<()> {
    let mut tx = state
        .db
        .begin()
        .await
        .context("start WhatsApp delete transaction")?;

    sqlx::query!(
        "UPDATE whatsapp_configs SET active = 0 WHERE id = ?",
        config_id
    )
    .execute(&mut *tx)
    .await
    .context("deactivate WhatsApp config")?;

    sqlx::query!("UPDATE pipes SET active = 0 WHERE id = ?", pipe_id)
        .execute(&mut *tx)
        .await
        .context("deactivate WhatsApp pipe")?;

    tx.commit()
        .await
        .context("commit WhatsApp delete transaction")?;

    Ok(())
}

async fn stop_bridge_if_no_active_pipe(state: &AppState) -> Result<()> {
    if has_active_whatsapp_pipe(&state.db).await {
        return Ok(());
    }

    let client = match SidecarUpdaterClient::from_config(&state.config) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to create updater client for WhatsApp bridge shutdown: {e}");
            return Err(e).context("create updater client for WhatsApp bridge shutdown");
        }
    };

    let req = SidecarStopWhatsappBridgeRequest {
        request_id: Uuid::new_v4().to_string(),
    };
    match client.stop_whatsapp_bridge(&req).await {
        Ok(a) if a.accepted => {
            info!("Stopped WhatsApp bridge (no active WhatsApp pipes)");
            Ok(())
        }
        Ok(a) => {
            warn!(message = ?a.message, "Updater rejected WhatsApp bridge shutdown");
            Err(anyhow::anyhow!(
                "{}",
                a.message.unwrap_or_else(
                    || "Updates service rejected WhatsApp bridge cleanup".to_string()
                )
            ))
        }
        Err(e) => {
            warn!("Failed to stop WhatsApp bridge: {e}");
            Err(e).context("stop WhatsApp bridge")
        }
    }
}

/// Best-effort startup hook:
/// if an active WhatsApp pipe exists, ensure the bridge container is running.
pub async fn ensure_bridge_for_active_pipe(state: &AppState) {
    let row = match sqlx::query!(
        r#"SELECT wc.id as config_id, wc.pipe_id, p.inbound_token, p.outbound_auth
           FROM whatsapp_configs wc
           JOIN pipes p ON p.id = wc.pipe_id
           WHERE wc.active = 1 AND p.active = 1
           ORDER BY wc.created_at DESC
           LIMIT 1"#
    )
    .fetch_optional(&state.db)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to check active WhatsApp pipes at startup: {e}");
            return;
        }
    };

    let Some(active) = row else {
        if let Err(e) = stop_bridge_if_no_active_pipe(state).await {
            warn!("Failed to clean up WhatsApp bridge at startup: {e:#}");
        }
        return;
    };

    let channel = std::env::var("SELU_RELEASE_CHANNEL").unwrap_or_else(|_| "stable".to_string());
    let inbound_url = format!(
        "{}",
        managed_bridge_inbound_url(&state.config, &state.public_base_url(), &active.pipe_id)
    );

    let client = match SidecarUpdaterClient::from_config(&state.config) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to create updater client for WhatsApp bridge startup: {e}");
            return;
        }
    };

    match client
        .ensure_whatsapp_bridge(&SidecarEnsureWhatsappBridgeRequest {
            request_id: Uuid::new_v4().to_string(),
            channel,
            inbound_url,
            inbound_token: active.inbound_token,
            outbound_auth: active.outbound_auth.unwrap_or_default(),
        })
        .await
    {
        Ok(a) if a.accepted => {
            info!(
                config_id = ?active.config_id,
                "Ensured WhatsApp bridge for active pipe at startup"
            );
        }
        Ok(a) => {
            warn!(
                config_id = ?active.config_id,
                message = ?a.message,
                "Updater rejected WhatsApp bridge ensure at startup"
            );
        }
        Err(e) => {
            warn!(
                config_id = ?active.config_id,
                "Failed to ensure WhatsApp bridge at startup: {e}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{managed_bridge_inbound_origin, managed_bridge_inbound_url};
    use crate::config::{AppConfig, DatabaseConfig, ServerConfig};

    fn test_config() -> AppConfig {
        AppConfig {
            server: ServerConfig {
                host: "0.0.0.0".to_string(),
                port: 3000,
            },
            database: DatabaseConfig {
                url: "sqlite://selu.db?mode=rwc".to_string(),
            },
            marketplace_url: "https://example.test/marketplace".to_string(),
            installed_agents_dir: "./installed_agents".to_string(),
            encryption_key: "test".to_string(),
            egress_proxy_addr: "0.0.0.0:8888".to_string(),
            max_chain_depth: 3,
            max_tool_loop_iterations: 32,
            external_url: None,
            base_path: None,
            release_metadata_url: "https://example.test/releases".to_string(),
            updater_url: "http://selu-updater:8090".to_string(),
            updater_shared_secret: String::new(),
            updater_request_timeout_secs: 30,
        }
    }

    #[test]
    fn bridge_uses_internal_selu_origin_in_compose() {
        let cfg = test_config();
        assert_eq!(
            managed_bridge_inbound_origin(&cfg, "http://localhost:3000"),
            "http://selu:3000"
        );
        assert_eq!(
            managed_bridge_inbound_url(&cfg, "http://localhost:3000", "pipe-123"),
            "http://selu:3000/api/pipes/pipe-123/inbound"
        );
    }

    #[test]
    fn bridge_uses_host_gateway_for_local_updater() {
        let mut cfg = test_config();
        cfg.updater_url = "http://127.0.0.1:8090".to_string();
        assert_eq!(
            managed_bridge_inbound_origin(&cfg, "http://localhost:3000"),
            "http://host.docker.internal:3000"
        );
    }

    #[test]
    fn bridge_prefers_public_external_url_when_configured() {
        let mut cfg = test_config();
        cfg.external_url = Some("https://selu.example.com".to_string());
        cfg.base_path = Some("/selu".to_string());
        assert_eq!(
            managed_bridge_inbound_origin(&cfg, "https://selu.example.com/selu"),
            "https://selu.example.com/selu"
        );
    }
}
