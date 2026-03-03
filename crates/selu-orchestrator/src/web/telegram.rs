use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Redirect, Response},
};
use serde::{Deserialize, Serialize};
use tracing::{error, warn};
use uuid::Uuid;

use crate::state::AppState;
use crate::web::auth::AuthUser;
use crate::web::{BasePath, ExternalOrigin, prefixed_redirect};

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TelegramDetail {
    pub config_id: String,
    pub name: String,
    #[allow(dead_code)]
    pub bot_username: String,
    pub chat_id: String,
    pub active: bool,
    pub pipe_id: String,
    pub agent_name: String,
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

// ── Templates ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pipes_telegram_setup.html")]
struct TelegramSetupTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    users: Vec<UserOption>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "pipes_telegram_detail.html")]
struct TelegramDetailTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    config: TelegramDetail,
    people: Vec<AllowedPerson>,
    users: Vec<UserOption>,
    flash: Option<String>,
    error: Option<String>,
    is_https: bool,
}

// ── Form structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TelegramSetupForm {
    pub name: String,
    pub bot_token: String,
    pub chat_id: String,
    /// Comma-separated list of "tg_user_id:selu_user_id" pairs
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

// ── Proxy types ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TgProxyRequest {
    pub bot_token: String,
}

#[derive(Debug, Serialize)]
pub struct TgProxyResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chats: Option<Vec<TgChatInfo>>,
}

#[derive(Debug, Serialize)]
pub struct TgChatInfo {
    pub chat_id: String,
    pub display_name: String,
    pub chat_type: String,
    pub last_message: String,
}

// ── Handlers: Telegram proxy ─────────────────────────────────────────────────

/// Server-side proxy: verifies the bot token and fetches recent chats
/// by calling Telegram's `getMe` and `getUpdates` APIs.
pub async fn tg_proxy_chats(
    user: AuthUser,
    axum::Json(req): axum::Json<TgProxyRequest>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    let bot_token = req.bot_token.trim();

    // Step 1: Verify bot token
    let bot_username = match crate::telegram::adapter::verify_bot_token(bot_token).await {
        Ok(u) => u,
        Err(e) => {
            warn!("Telegram proxy: token verification failed: {e}");
            return Json(TgProxyResponse {
                ok: false,
                error: Some(format!("{e}")),
                bot_username: None,
                chats: None,
            })
            .into_response();
        }
    };

    // Step 2: Fetch recent chats from updates
    let chats = match crate::telegram::adapter::get_recent_chats(bot_token).await {
        Ok(c) => c,
        Err(e) => {
            warn!("Telegram proxy: getUpdates failed: {e}");
            // Not fatal — bot is valid but no recent messages
            Vec::new()
        }
    };

    let chat_infos: Vec<TgChatInfo> = chats
        .into_iter()
        .map(|c| TgChatInfo {
            chat_id: c.chat_id,
            display_name: c.display_name,
            chat_type: c.chat_type,
            last_message: c.last_message,
        })
        .collect();

    Json(TgProxyResponse {
        ok: true,
        error: None,
        bot_username: Some(bot_username),
        chats: Some(chat_infos),
    })
    .into_response()
}

// ── Handlers: Telegram Setup Wizard ──────────────────────────────────────────

pub async fn telegram_setup_page(
    user: AuthUser,
    Query(q): Query<FlashQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    ExternalOrigin(external_origin): ExternalOrigin,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }

    // Telegram requires HTTPS for webhooks
    if !external_origin.starts_with("https://") {
        return Redirect::to(&format!(
            "{}/pipes?error=Telegram+requires+HTTPS.+Access+Selu+via+HTTPS+or+set+SELU__EXTERNAL_URL+to+an+https://+address.",
            base_path
        )).into_response();
    }

    let users = load_users(&state.db).await;

    match (TelegramSetupTemplate {
        active_nav: "pipes",
        is_admin: user.is_admin,
        base_path,
        users,
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

/// One-click setup: creates pipe + Telegram config + sender mappings.
pub async fn telegram_setup_submit(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    ExternalOrigin(external_origin): ExternalOrigin,
    Form(form): Form<TelegramSetupForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    // Telegram requires HTTPS for webhooks
    if !external_origin.starts_with("https://") {
        return Redirect::to(&format!(
            "{}/pipes?error=Telegram+requires+HTTPS.+Access+Selu+via+HTTPS+or+set+SELU__EXTERNAL_URL+to+an+https://+address.",
            base_path
        )).into_response();
    }

    let bot_token = form.bot_token.trim().to_string();
    let chat_id = form.chat_id.trim().to_string();
    let name = form.name.trim().to_string();

    if name.is_empty() || bot_token.is_empty() || chat_id.is_empty() {
        return Redirect::to(&format!(
            "{}/pipes/telegram/setup?error=Please+fill+in+all+required+fields",
            base_path
        ))
        .into_response();
    }

    // Find a user to own the pipe
    let owner_user_id = match find_pipe_owner(&state.db, &form.people).await {
        Some(id) => id,
        None => {
            return Redirect::to(&format!(
                "{}/pipes/telegram/setup?error=You+need+at+least+one+user.+Create+one+in+Users+first.",
                base_path
            )).into_response();
        }
    };

    // Create the pipe
    let pipe_id = Uuid::new_v4().to_string();
    let inbound_token = Uuid::new_v4().to_string().replace('-', "");
    let default_agent_id: Option<&str> = None;
    let pipe_name = format!("Telegram: {}", name);
    let transport = "webhook";
    let outbound_url = "internal://telegram";

    if let Err(e) = sqlx::query!(
        "INSERT INTO pipes (id, user_id, name, transport, inbound_token, outbound_url, default_agent_id)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        pipe_id, owner_user_id, pipe_name, transport, inbound_token, outbound_url, default_agent_id,
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to create pipe: {e}");
        return Redirect::to(&format!(
            "{}/pipes/telegram/setup?error=Failed+to+create+pipe",
            base_path
        )).into_response();
    }

    // Create the Telegram config
    let tg_config_id = Uuid::new_v4().to_string();

    if let Err(e) = sqlx::query!(
        "INSERT INTO telegram_configs (id, name, bot_token, chat_id, pipe_id)
         VALUES (?, ?, ?, ?, ?)",
        tg_config_id,
        name,
        bot_token,
        chat_id,
        pipe_id,
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to create Telegram config: {e}");
        return Redirect::to(&format!(
            "{}/pipes/telegram/setup?error=Failed+to+create+adapter+config",
            base_path
        ))
        .into_response();
    }

    // Create sender ref mappings
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
                    let _ =
                        sqlx::query!(
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

    // Start the adapter immediately
    let bp = base_path;
    if let Err(e) =
        crate::telegram::adapter::start_one(state, &tg_config_id, Some(&external_origin)).await
    {
        error!("Failed to start Telegram adapter: {e}");
    }

    Redirect::to(&format!(
        "{}/pipes/telegram/{}?msg=Telegram+pipe+set+up+successfully!",
        bp, tg_config_id
    ))
    .into_response()
}

// ── Handlers: Telegram Detail ────────────────────────────────────────────────

pub async fn telegram_detail(
    user: AuthUser,
    Path(config_id): Path<String>,
    Query(q): Query<FlashQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    ExternalOrigin(external_origin): ExternalOrigin,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }
    let config = match load_telegram_config(&state.db, &config_id).await {
        Some(c) => c,
        None => return prefixed_redirect(&base_path, "/pipes").into_response(),
    };

    let people = load_allowed_people(&state.db, &config.pipe_id).await;
    let users = load_users(&state.db).await;
    let is_https = external_origin.starts_with("https://");

    match (TelegramDetailTemplate {
        active_nav: "pipes",
        is_admin: user.is_admin,
        base_path,
        config,
        people,
        users,
        flash: q.msg,
        error: q.error,
        is_https,
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

pub async fn telegram_add_person(
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
            "{}/pipes/telegram/{}?error=Please+fill+in+both+fields",
            base_path, config_id
        ))
        .into_response();
    }

    let pipe_id = match sqlx::query!(
        "SELECT pipe_id FROM telegram_configs WHERE id = ?",
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
            "{}/pipes/telegram/{}?msg=Person+added",
            base_path, config_id
        ))
        .into_response(),
        Err(e) if e.to_string().contains("UNIQUE") => Redirect::to(&format!(
            "{}/pipes/telegram/{}?error=This+Telegram+user+is+already+added",
            base_path, config_id
        ))
        .into_response(),
        Err(e) => {
            error!("Failed to add person: {e}");
            Redirect::to(&format!(
                "{}/pipes/telegram/{}?error=Failed+to+add+person.+Please+try+again.",
                base_path, config_id
            ))
            .into_response()
        }
    }
}

pub async fn telegram_remove_person(
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

pub async fn telegram_delete(
    user: AuthUser,
    Path(config_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    // Delete webhook from Telegram before deactivating
    if let Ok(Some(row)) = sqlx::query!(
        "SELECT bot_token FROM telegram_configs WHERE id = ?",
        config_id
    )
    .fetch_optional(&state.db)
    .await
    {
        if let Err(e) = crate::telegram::adapter::delete_webhook(&row.bot_token).await {
            warn!("Failed to delete Telegram webhook: {e}");
        }
    }

    // Deactivate config and pipe
    let _ = sqlx::query!(
        "UPDATE telegram_configs SET active = 0 WHERE id = ?",
        config_id
    )
    .execute(&state.db)
    .await;
    let _ = sqlx::query!(
        "UPDATE pipes SET active = 0 WHERE id IN (SELECT pipe_id FROM telegram_configs WHERE id = ?)",
        config_id
    )
    .execute(&state.db)
    .await;

    Redirect::to(&format!("{}/pipes?msg=Pipe+removed", base_path)).into_response()
}

// ── Handlers: Troubleshoot ───────────────────────────────────────────────────

/// HTMX handler: check webhook status from Telegram's `getWebhookInfo`.
/// Returns an HTML fragment to be swapped into the troubleshoot section.
pub async fn telegram_check_webhook(
    user: AuthUser,
    Path(config_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    let bot_token = match sqlx::query!(
        "SELECT bot_token FROM telegram_configs WHERE id = ?",
        config_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    {
        Some(r) => r.bot_token,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    match crate::telegram::adapter::get_webhook_info(&bot_token).await {
        Ok(info) => {
            let url_display = if info.url.is_empty() {
                "<span class=\"text-amber-400\" data-i18n=\"tg.troubleshoot.nourl\">No webhook URL set</span>".to_string()
            } else {
                format!(
                    "<code class=\"font-mono text-xs bg-surface-alt px-2 py-1 rounded break-all\">{}</code>",
                    info.url
                )
            };

            let error_html = if let Some(ref msg) = info.last_error_message {
                format!(
                    r#"<div class="flex items-start gap-2 mt-2">
                        <span class="text-rosie text-xs">&#9888;</span>
                        <span class="text-xs text-rosie">{}</span>
                    </div>"#,
                    msg
                )
            } else {
                String::new()
            };

            let pending = info.pending_update_count;

            Html(format!(
                r#"<div class="space-y-2 text-sm animate-fadein">
                    <div class="flex items-center gap-3">
                        <span class="text-txt-muted w-28" data-i18n="tg.troubleshoot.url">Webhook URL</span>
                        <div class="flex-1 min-w-0">{url_display}</div>
                    </div>
                    <div class="flex items-center gap-3">
                        <span class="text-txt-muted w-28" data-i18n="tg.troubleshoot.pending">Pending</span>
                        <span class="text-txt-heading">{pending}</span>
                    </div>
                    {error_html}
                </div>"#
            )).into_response()
        }
        Err(e) => {
            error!("Failed to get webhook info: {e}");
            Html(format!(
                r#"<div class="text-sm text-rosie animate-fadein">{}</div>"#,
                "Could not reach Telegram. Please try again."
            ))
            .into_response()
        }
    }
}

/// HTMX handler: re-register webhook with Telegram.
/// Returns an HTML fragment with the result.
pub async fn telegram_reregister_webhook(
    user: AuthUser,
    Path(config_id): Path<String>,
    State(state): State<AppState>,
    ExternalOrigin(external_origin): ExternalOrigin,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    // Only allow over HTTPS
    if !external_origin.starts_with("https://") {
        return Html(
            r#"<div class="text-sm text-rosie animate-fadein" data-i18n="tg.troubleshoot.httpsonly">Webhook registration requires HTTPS. Access Selu via HTTPS first.</div>"#.to_string()
        ).into_response();
    }

    match crate::telegram::adapter::reregister_webhook(&state, &config_id, &external_origin).await {
        Ok(()) => {
            Html(
                r#"<div class="text-sm text-emerald-400 animate-fadein" data-i18n="tg.troubleshoot.registered">Webhook re-registered successfully!</div>"#.to_string()
            ).into_response()
        }
        Err(e) => {
            error!("Failed to re-register webhook: {e}");
            Html(format!(
                r#"<div class="text-sm text-rosie animate-fadein">{e}</div>"#,
            )).into_response()
        }
    }
}

// ── DB helpers ────────────────────────────────────────────────────────────────

pub async fn load_telegram_configs(db: &sqlx::SqlitePool) -> Vec<TelegramDetail> {
    sqlx::query!(
        r#"SELECT tc.id, tc.name, tc.chat_id, tc.active, tc.pipe_id,
                  COALESCE(p.default_agent_id, 'default') as "agent_name!: String"
           FROM telegram_configs tc
           JOIN pipes p ON p.id = tc.pipe_id
           ORDER BY tc.created_at DESC"#
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| TelegramDetail {
        config_id: r.id.unwrap_or_default(),
        name: r.name,
        bot_username: String::new(), // Not stored in DB
        chat_id: r.chat_id,
        active: r.active != 0,
        pipe_id: r.pipe_id,
        agent_name: r.agent_name,
    })
    .collect()
}

async fn load_telegram_config(db: &sqlx::SqlitePool, config_id: &str) -> Option<TelegramDetail> {
    sqlx::query!(
        r#"SELECT tc.id, tc.name, tc.chat_id, tc.active, tc.pipe_id,
                  COALESCE(p.default_agent_id, 'default') as "agent_name!: String"
           FROM telegram_configs tc
           JOIN pipes p ON p.id = tc.pipe_id
           WHERE tc.id = ?"#,
        config_id
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .map(|r| TelegramDetail {
        config_id: r.id.unwrap_or_default(),
        name: r.name,
        bot_username: String::new(),
        chat_id: r.chat_id,
        active: r.active != 0,
        pipe_id: r.pipe_id,
        agent_name: r.agent_name,
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
