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
pub struct BbConfigView {
    pub id: String,
    pub name: String,
    pub server_url: String,
    pub chat_guid: String,
    #[allow(dead_code)]
    pub pipe_id: String,
    pub pipe_name: String,
    pub poll_interval_ms: i64,
    pub active: bool,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct SenderRefView {
    pub id: String,
    #[allow(dead_code)]
    pub user_id: String,
    pub user_display: String,
    #[allow(dead_code)]
    pub pipe_id: String,
    pub pipe_name: String,
    pub sender_ref: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct PipeOption {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct UserOption {
    pub id: String,
    pub display: String,
}

// ── Template ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "bluebubbles.html")]
struct BlueBubblesTemplate {
    active_nav: &'static str,
    configs: Vec<BbConfigView>,
    sender_refs: Vec<SenderRefView>,
    pipes: Vec<PipeOption>,
    users: Vec<UserOption>,
    flash: Option<String>,
    error: Option<String>,
}

// ── Query / Form structs ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct BbQuery {
    pub msg: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateBbConfigForm {
    pub name: String,
    pub server_url: String,
    pub server_password: String,
    pub chat_guid: String,
    pub pipe_id: String,
    pub poll_interval_ms: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CreateSenderRefForm {
    pub user_id: String,
    pub pipe_id: String,
    pub sender_ref: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn bluebubbles_index(
    _user: AuthUser,
    Query(q): Query<BbQuery>,
    State(state): State<AppState>,
) -> Response {
    let configs = load_configs(&state.db).await;
    let sender_refs = load_sender_refs(&state.db).await;
    let pipes = load_pipes(&state.db).await;
    let users = load_users(&state.db).await;

    match (BlueBubblesTemplate {
        active_nav: "bluebubbles",
        configs,
        sender_refs,
        pipes,
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

pub async fn bb_config_create(
    _user: AuthUser,
    State(state): State<AppState>,
    Form(form): Form<CreateBbConfigForm>,
) -> Response {
    if form.name.trim().is_empty()
        || form.server_url.trim().is_empty()
        || form.server_password.is_empty()
        || form.chat_guid.trim().is_empty()
        || form.pipe_id.is_empty()
    {
        return Redirect::to("/bluebubbles?error=All+fields+are+required.").into_response();
    }

    let id = Uuid::new_v4().to_string();
    let poll_ms = form.poll_interval_ms.unwrap_or(2000);

    let result = sqlx::query!(
        "INSERT INTO bluebubbles_configs (id, name, server_url, server_password, chat_guid, pipe_id, poll_interval_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        id,
        form.name,
        form.server_url,
        form.server_password,
        form.chat_guid,
        form.pipe_id,
        poll_ms,
    )
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Redirect::to("/bluebubbles?msg=BlueBubbles+adapter+created.+Restart+to+activate.").into_response(),
        Err(e) => {
            error!("Failed to create BB config: {e}");
            Redirect::to("/bluebubbles?error=Failed+to+create+adapter.+Please+try+again.").into_response()
        }
    }
}

pub async fn bb_config_delete(
    _user: AuthUser,
    Path(config_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if let Err(e) = sqlx::query!(
        "UPDATE bluebubbles_configs SET active = 0 WHERE id = ?",
        config_id
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to deactivate BB config {config_id}: {e}");
    }
    Html("").into_response()
}

pub async fn sender_ref_create(
    _user: AuthUser,
    State(state): State<AppState>,
    Form(form): Form<CreateSenderRefForm>,
) -> Response {
    if form.user_id.is_empty()
        || form.pipe_id.is_empty()
        || form.sender_ref.trim().is_empty()
    {
        return Redirect::to("/bluebubbles?error=All+fields+are+required.").into_response();
    }

    let id = Uuid::new_v4().to_string();

    let result = sqlx::query!(
        "INSERT INTO user_sender_refs (id, user_id, pipe_id, sender_ref) VALUES (?, ?, ?, ?)",
        id,
        form.user_id,
        form.pipe_id,
        form.sender_ref,
    )
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Redirect::to("/bluebubbles?msg=Sender+mapping+created").into_response(),
        Err(e) if e.to_string().contains("UNIQUE") => {
            Redirect::to("/bluebubbles?error=This+sender+is+already+mapped+for+this+pipe.").into_response()
        }
        Err(e) => {
            error!("Failed to create sender ref: {e}");
            Redirect::to("/bluebubbles?error=Failed+to+create+mapping.+Please+try+again.").into_response()
        }
    }
}

pub async fn sender_ref_delete(
    _user: AuthUser,
    Path(ref_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if let Err(e) = sqlx::query!(
        "DELETE FROM user_sender_refs WHERE id = ?",
        ref_id
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to delete sender ref {ref_id}: {e}");
    }
    Html("").into_response()
}

// ── DB helpers ────────────────────────────────────────────────────────────────

async fn load_configs(db: &sqlx::SqlitePool) -> Vec<BbConfigView> {
    sqlx::query!(
        r#"SELECT bc.id, bc.name, bc.server_url, bc.chat_guid, bc.pipe_id,
                  bc.poll_interval_ms, bc.active, bc.created_at,
                  p.name as pipe_name
           FROM bluebubbles_configs bc
           JOIN pipes p ON p.id = bc.pipe_id
           ORDER BY bc.created_at DESC"#
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| BbConfigView {
        id: r.id.unwrap_or_default(),
        name: r.name,
        server_url: r.server_url,
        chat_guid: r.chat_guid,
        pipe_id: r.pipe_id,
        pipe_name: r.pipe_name,
        poll_interval_ms: r.poll_interval_ms,
        active: r.active != 0,
        created_at: r.created_at,
    })
    .collect()
}

async fn load_sender_refs(db: &sqlx::SqlitePool) -> Vec<SenderRefView> {
    sqlx::query!(
        r#"SELECT sr.id, sr.user_id, sr.pipe_id, sr.sender_ref, sr.created_at,
                  u.display_name, u.username,
                  p.name as pipe_name
           FROM user_sender_refs sr
           JOIN users u ON u.id = sr.user_id
           JOIN pipes p ON p.id = sr.pipe_id
           ORDER BY sr.created_at DESC"#
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| SenderRefView {
        id: r.id.unwrap_or_default(),
        user_id: r.user_id,
        user_display: format!("{} ({})", r.display_name, r.username),
        pipe_id: r.pipe_id,
        pipe_name: r.pipe_name,
        sender_ref: r.sender_ref,
        created_at: r.created_at,
    })
    .collect()
}

async fn load_pipes(db: &sqlx::SqlitePool) -> Vec<PipeOption> {
    sqlx::query!("SELECT id, name FROM pipes WHERE active = 1 ORDER BY name")
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| PipeOption {
            id: r.id.unwrap_or_default(),
            name: r.name,
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
