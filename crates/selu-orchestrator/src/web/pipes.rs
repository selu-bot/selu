use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use std::collections::HashMap;
use tracing::error;
use uuid::Uuid;

use crate::state::AppState;
use crate::web::auth::AuthUser;
use crate::web::{BasePath, ExternalOrigin, prefixed_redirect};

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ImessagePipeView {
    pub config_id: String,
    pub name: String,
    pub server_url: String,
    pub owner_name: String,
}

#[derive(Debug, Clone)]
pub struct TelegramPipeView {
    pub config_id: String,
    pub name: String,
    pub chat_id: String,
    pub owner_name: String,
}

#[derive(Debug, Clone)]
pub struct WhatsappPipeView {
    pub config_id: String,
    pub name: String,
    pub owner_name: String,
}

#[derive(Debug, Clone)]
pub struct OtherPipeView {
    pub id: String,
    pub name: String,
    pub transport: String,
    pub owner_name: String,
}

#[derive(Debug, Clone)]
pub struct PipeCatalogView {
    pub display_name: &'static str,
    pub subtitle_key: &'static str,
    pub href: Option<String>,
    pub action_key: &'static str,
    pub status_key: Option<&'static str>,
    pub available: bool,
}

#[derive(Debug, Clone)]
pub struct UserOption {
    pub id: String,
    pub display: String,
}

#[allow(dead_code)]
pub struct PipeCreatedFlash {
    pub pipe_id: String,
    pub inbound_token: String,
    pub inbound_url: String,
}

// ── Templates ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pipes.html")]
struct PipesTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    imessage_pipes: Vec<ImessagePipeView>,
    telegram_pipes: Vec<TelegramPipeView>,
    whatsapp_pipes: Vec<WhatsappPipeView>,
    other_pipes: Vec<OtherPipeView>,
    flash: Option<PipeCreatedFlash>,
    msg: Option<String>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "pipes_new.html")]
struct PipesNewTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    pipes: Vec<PipeCatalogView>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "pipes_webhook_new.html")]
struct PipesWebhookNewTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    users: Vec<UserOption>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "pipes_web_new.html")]
struct PipesWebNewTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    users: Vec<UserOption>,
    error: Option<String>,
}

// ── Query / Form structs ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PipesQuery {
    pub created: Option<String>,
    pub pipe_id: Option<String>,
    pub token: Option<String>,
    pub error: Option<String>,
    pub msg: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WebhookCreateForm {
    pub user_id: String,
    pub name: String,
    pub outbound_url: String,
    pub outbound_auth: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WebCreateForm {
    pub user_id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct SimpleErrorQuery {
    pub error: Option<String>,
}

const PIPE_TYPES: &[(&str, &str, &str)] = &[
    ("web", "Web UI", "pipes.webui.desc"),
    ("imessage", "iMessage", "pipes.imessage.desc"),
    ("telegram", "Telegram", "pipes.telegram.desc"),
    ("whatsapp", "WhatsApp", "pipes.whatsapp.desc"),
    ("webhook", "Webhook", "pipes.webhook.desc"),
];

// ── DB helpers ────────────────────────────────────────────────────────────────

/// Build a user_id -> display_name lookup map.
async fn user_display_map(db: &sqlx::SqlitePool) -> HashMap<String, String> {
    sqlx::query!("SELECT id, username, display_name FROM users ORDER BY username")
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.id.unwrap_or_default(), r.display_name))
        .collect()
}

/// Build a pipe_id -> user_id lookup map (for resolving iMessage pipe owners).
/// Uses the same query text as api/pipes.rs so the sqlx offline cache is reused.
async fn pipe_owner_map(db: &sqlx::SqlitePool) -> HashMap<String, String> {
    sqlx::query!(
        "SELECT id, user_id, name, transport, outbound_url, default_agent_id, active, created_at
         FROM pipes ORDER BY created_at DESC"
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| (r.id.unwrap_or_default(), r.user_id))
    .collect()
}

async fn db_users(db: &sqlx::SqlitePool) -> Vec<UserOption> {
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

async fn pipe_people_summary_map(db: &sqlx::SqlitePool) -> HashMap<String, String> {
    let rows = sqlx::query!(
        r#"SELECT sr.pipe_id, u.display_name
           FROM user_sender_refs sr
           JOIN users u ON u.id = sr.user_id
           ORDER BY sr.pipe_id, u.display_name"#
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        map.entry(row.pipe_id).or_default().push(row.display_name);
    }

    map.into_iter()
        .map(|(pipe_id, mut names)| {
            names.dedup();
            (pipe_id, names.join(", "))
        })
        .collect()
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// Unified pipes index: shows all active pipes + add-new cards.
pub async fn pipes_index(
    user: AuthUser,
    Query(q): Query<PipesQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }

    // Build lookup maps: user_id -> display_name, pipe_id -> user_id
    let users = user_display_map(&state.db).await;
    let pipe_owners = pipe_owner_map(&state.db).await;
    let pipe_people = pipe_people_summary_map(&state.db).await;

    // Resolve owner name for a pipe_id
    let owner_name = |pipe_id: &str| -> String {
        pipe_owners
            .get(pipe_id)
            .and_then(|uid| users.get(uid))
            .cloned()
            .unwrap_or_default()
    };

    // Load iMessage configs, filter to active only
    let imessage_configs = super::integrations::load_imessage_configs(&state.db).await;
    let bb_pipe_ids: std::collections::HashSet<String> =
        imessage_configs.iter().map(|c| c.pipe_id.clone()).collect();

    let imessage_pipes: Vec<ImessagePipeView> = imessage_configs
        .into_iter()
        .filter(|c| c.active)
        .map(|c| {
            let oname = pipe_people
                .get(&c.pipe_id)
                .cloned()
                .unwrap_or_else(|| owner_name(&c.pipe_id));
            ImessagePipeView {
                config_id: c.config_id,
                name: c.name,
                server_url: c.server_url,
                owner_name: oname,
            }
        })
        .collect();

    // Load Telegram configs, filter to active only
    let telegram_configs = super::telegram::load_telegram_configs(&state.db).await;
    let tg_pipe_ids: std::collections::HashSet<String> =
        telegram_configs.iter().map(|c| c.pipe_id.clone()).collect();

    let telegram_pipes: Vec<TelegramPipeView> = telegram_configs
        .into_iter()
        .filter(|c| c.active)
        .map(|c| {
            let oname = pipe_people
                .get(&c.pipe_id)
                .cloned()
                .unwrap_or_else(|| owner_name(&c.pipe_id));
            TelegramPipeView {
                config_id: c.config_id,
                name: c.name,
                chat_id: c.chat_id,
                owner_name: oname,
            }
        })
        .collect();

    // Load WhatsApp configs, filter to active only
    let whatsapp_configs = super::whatsapp::load_whatsapp_configs(&state.db).await;
    let wa_pipe_ids: std::collections::HashSet<String> =
        whatsapp_configs.iter().map(|c| c.pipe_id.clone()).collect();

    let whatsapp_pipes: Vec<WhatsappPipeView> = whatsapp_configs
        .into_iter()
        .filter(|c| c.active)
        .map(|c| {
            let oname = pipe_people
                .get(&c.pipe_id)
                .cloned()
                .unwrap_or_else(|| owner_name(&c.pipe_id));
            WhatsappPipeView {
                config_id: c.config_id,
                name: c.name,
                owner_name: oname,
            }
        })
        .collect();
    // Load other pipes (webhook, web), active only, excluding iMessage, Telegram, WhatsApp pipes
    let other_pipes: Vec<OtherPipeView> = sqlx::query!(
        "SELECT id, user_id, name, transport, outbound_url, default_agent_id, active, created_at
         FROM pipes ORDER BY created_at DESC"
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| {
        let id = r.id.clone().unwrap_or_default();
        if bb_pipe_ids.contains(&id)
            || tg_pipe_ids.contains(&id)
            || wa_pipe_ids.contains(&id)
            || r.active == 0
        {
            return None;
        }
        let oname = users.get(&r.user_id).cloned().unwrap_or_default();
        Some(OtherPipeView {
            id,
            name: r.name,
            transport: r.transport,
            owner_name: oname,
        })
    })
    .collect();

    let flash = if q.created.as_deref() == Some("1") {
        q.pipe_id.zip(q.token).map(|(id, tok)| PipeCreatedFlash {
            inbound_url: format!("{}/api/pipes/{}/inbound", base_path, id),
            pipe_id: id,
            inbound_token: tok,
        })
    } else {
        None
    };

    match (PipesTemplate {
        active_nav: "pipes",
        is_admin: user.is_admin,
        base_path,
        imessage_pipes,
        telegram_pipes,
        whatsapp_pipes,
        other_pipes,
        flash,
        msg: q.msg,
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

pub async fn pipes_new(
    user: AuthUser,
    Query(q): Query<SimpleErrorQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    ExternalOrigin(external_origin): ExternalOrigin,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }

    let https_available = external_origin.starts_with("https://");
    let whatsapp_configs = super::whatsapp::load_whatsapp_configs(&state.db).await;
    let existing_whatsapp_config_id = whatsapp_configs
        .into_iter()
        .find(|cfg| cfg.active)
        .map(|cfg| cfg.config_id);

    let pipes = PIPE_TYPES
        .iter()
        .map(|(id, display_name, subtitle_key)| {
            let (href, action_key, status_key, available) = match *id {
                "telegram" if !https_available => (
                    None,
                    "pipes.comingsoon",
                    Some("pipes.telegram.https"),
                    false,
                ),
                "whatsapp" => {
                    if let Some(config_id) = existing_whatsapp_config_id.as_ref() {
                        (
                            Some(format!("{}/pipes/whatsapp/{}", base_path, config_id)),
                            "pipes.whatsapp.single.manage",
                            Some("wa.single.limit"),
                            true,
                        )
                    } else {
                        (
                            Some(format!("{}/pipes/new/{}", base_path, id)),
                            "pipes.setup",
                            None,
                            true,
                        )
                    }
                }
                _ => (
                    Some(format!("{}/pipes/new/{}", base_path, id)),
                    "pipes.setup",
                    None,
                    true,
                ),
            };

            PipeCatalogView {
                display_name,
                subtitle_key,
                href,
                action_key,
                status_key,
                available,
            }
        })
        .collect();

    match (PipesNewTemplate {
        active_nav: "pipes",
        is_admin: user.is_admin,
        base_path,
        pipes,
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

pub async fn pipes_new_redirect(
    user: AuthUser,
    Path(pipe_type): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    ExternalOrigin(external_origin): ExternalOrigin,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }

    let target = match pipe_type.as_str() {
        "web" => Some(format!("{}/pipes/web/new", base_path)),
        "webhook" => Some(format!("{}/pipes/webhook/new", base_path)),
        "imessage" => Some(format!("{}/pipes/imessage/setup", base_path)),
        "telegram" if external_origin.starts_with("https://") => {
            Some(format!("{}/pipes/telegram/setup", base_path))
        }
        "telegram" => Some(format!(
            "{}/pipes/new?error=Telegram+requires+HTTPS.",
            base_path
        )),
        "whatsapp" => {
            let existing = super::whatsapp::load_whatsapp_configs(&state.db)
                .await
                .into_iter()
                .find(|cfg| cfg.active)
                .map(|cfg| cfg.config_id);
            Some(if let Some(config_id) = existing {
                format!("{}/pipes/whatsapp/{}", base_path, config_id)
            } else {
                format!("{}/pipes/whatsapp/setup", base_path)
            })
        }
        _ => Some(format!("{}/pipes/new", base_path)),
    };

    Redirect::to(&target.unwrap_or_else(|| format!("{}/pipes/new", base_path))).into_response()
}

/// Webhook pipe creation form page.
pub async fn pipes_webhook_new(
    user: AuthUser,
    Query(q): Query<SimpleErrorQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }

    let users = db_users(&state.db).await;

    match (PipesWebhookNewTemplate {
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

/// Create a webhook pipe from the form.
pub async fn pipes_webhook_create(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<WebhookCreateForm>,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }
    if form.name.trim().is_empty() || form.user_id.is_empty() || form.outbound_url.trim().is_empty()
    {
        return Redirect::to(&format!(
            "{}/pipes/webhook/new?error=Name%2C+owner%2C+and+outbound+URL+are+required.",
            base_path
        ))
        .into_response();
    }

    let id = Uuid::new_v4().to_string();
    let inbound_token = Uuid::new_v4().to_string().replace('-', "");
    let transport = "webhook";
    let outbound_auth = form.outbound_auth.filter(|v| !v.is_empty());
    let default_agent_id: Option<&str> = None; // Always use default agent

    let result = sqlx::query!(
        "INSERT INTO pipes (id, user_id, name, transport, inbound_token, outbound_url, outbound_auth, default_agent_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        id,
        form.user_id,
        form.name,
        transport,
        inbound_token,
        form.outbound_url,
        outbound_auth,
        default_agent_id,
    )
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Redirect::to(&format!(
            "{}/pipes?created=1&pipe_id={}&token={}",
            base_path,
            urlencoding::encode(&id),
            urlencoding::encode(&inbound_token),
        ))
        .into_response(),
        Err(e) => {
            error!("Failed to create pipe: {e}");
            Redirect::to(&format!(
                "{}/pipes/webhook/new?error=Failed+to+create+pipe.+Please+try+again.",
                base_path
            ))
            .into_response()
        }
    }
}

/// Web UI pipe creation form page.
pub async fn pipes_web_new(
    user: AuthUser,
    Query(q): Query<SimpleErrorQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }

    let users = db_users(&state.db).await;

    match (PipesWebNewTemplate {
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

/// Create a web UI pipe.
pub async fn pipes_web_create(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<WebCreateForm>,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }
    if form.name.trim().is_empty() || form.user_id.is_empty() {
        return Redirect::to(&format!(
            "{}/pipes/web/new?error=Name+and+owner+are+required.",
            base_path
        ))
        .into_response();
    }

    let id = Uuid::new_v4().to_string();
    let inbound_token = Uuid::new_v4().to_string().replace('-', "");
    let transport = "web";
    let outbound_url = "internal://web"; // Marker: replies are shown in the browser
    let outbound_auth: Option<&str> = None;
    let default_agent_id: Option<&str> = None; // Always use default agent

    let result = sqlx::query!(
        "INSERT INTO pipes (id, user_id, name, transport, inbound_token, outbound_url, outbound_auth, default_agent_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        id,
        form.user_id,
        form.name,
        transport,
        inbound_token,
        outbound_url,
        outbound_auth,
        default_agent_id,
    )
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Redirect::to(&format!(
            "{}/pipes?msg=Web+pipe+%22{}%22+created.+Go+to+Chat+to+start+talking.",
            base_path,
            urlencoding::encode(&form.name),
        ))
        .into_response(),
        Err(e) => {
            error!("Failed to create web pipe: {e}");
            Redirect::to(&format!(
                "{}/pipes/web/new?error=Failed+to+create+pipe.+Please+try+again.",
                base_path
            ))
            .into_response()
        }
    }
}

/// Deactivate a pipe (HTMX delete).
pub async fn pipes_delete(
    user: AuthUser,
    Path(pipe_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Err(e) = sqlx::query!("UPDATE pipes SET active = 0 WHERE id = ?", pipe_id)
        .execute(&state.db)
        .await
    {
        error!("Failed to deactivate pipe {pipe_id}: {e}");
    }
    Html("").into_response()
}
