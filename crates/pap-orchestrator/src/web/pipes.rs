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
pub struct PipeFullView {
    pub id: String,
    pub name: String,
    pub transport: String,
    pub outbound_url: String,
    pub default_agent_id: String,
    pub active: bool,
    pub created_at: String,
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

#[allow(dead_code)]
pub struct PipeCreatedFlash {
    pub pipe_id: String,
    pub inbound_token: String,
    pub inbound_url: String,
}

// ── Template ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pipes.html")]
struct PipesTemplate {
    active_nav: &'static str,
    pipes: Vec<PipeFullView>,
    users: Vec<UserOption>,
    agents: Vec<AgentOption>,
    flash: Option<PipeCreatedFlash>,
    error: Option<String>,
}

// ── Query / Form structs ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PipesQuery {
    pub created: Option<String>,
    pub pipe_id: Option<String>,
    pub token: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreatePipeForm {
    pub user_id: String,
    pub name: String,
    pub transport: Option<String>,
    pub outbound_url: String,
    pub outbound_auth: Option<String>,
    pub default_agent_id: Option<String>,
}

// ── DB helpers ────────────────────────────────────────────────────────────────

async fn db_pipes(db: &sqlx::SqlitePool) -> Vec<PipeFullView> {
    sqlx::query!(
        "SELECT id, name, transport, outbound_url, default_agent_id, active, created_at
         FROM pipes ORDER BY created_at DESC"
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| PipeFullView {
        id: r.id.unwrap_or_default(),
        name: r.name,
        transport: r.transport,
        outbound_url: r.outbound_url,
        default_agent_id: r.default_agent_id.unwrap_or_default(),
        active: r.active != 0,
        created_at: r.created_at,
    })
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

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn pipes_index(
    _user: AuthUser,
    Query(q): Query<PipesQuery>,
    State(state): State<AppState>,
) -> Response {
    let pipes = db_pipes(&state.db).await;
    let users = db_users(&state.db).await;
    let agents = {
        let map = state.agents.read().await;
        let mut v: Vec<AgentOption> = map
            .values()
            .map(|d| AgentOption { id: d.id.clone(), name: d.name.clone() })
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    };

    let flash = if q.created.as_deref() == Some("1") {
        q.pipe_id.zip(q.token).map(|(id, tok)| PipeCreatedFlash {
            inbound_url: format!("/api/pipes/{}/inbound", id),
            pipe_id: id,
            inbound_token: tok,
        })
    } else {
        None
    };

    match (PipesTemplate { active_nav: "pipes", pipes, users, agents, flash, error: q.error }).render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn pipes_create(
    _user: AuthUser,
    State(state): State<AppState>,
    Form(form): Form<CreatePipeForm>,
) -> Response {
    if form.name.trim().is_empty()
        || form.user_id.is_empty()
        || form.outbound_url.trim().is_empty()
    {
        return Redirect::to("/pipes?error=Name%2C+owner%2C+and+outbound+URL+are+required.").into_response();
    }

    let id = Uuid::new_v4().to_string();
    let inbound_token = Uuid::new_v4().to_string().replace('-', "");
    let transport = form
        .transport
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "webhook".to_string());
    let outbound_auth = form.outbound_auth.filter(|v| !v.is_empty());
    let default_agent_id = form.default_agent_id.filter(|v| !v.is_empty());

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
            "/pipes?created=1&pipe_id={}&token={}",
            urlencoding::encode(&id),
            urlencoding::encode(&inbound_token),
        ))
        .into_response(),
        Err(e) => {
            error!("Failed to create pipe: {e}");
            Redirect::to("/pipes?error=Failed+to+create+pipe.+Please+try+again.").into_response()
        }
    }
}

pub async fn pipes_delete(
    _user: AuthUser,
    Path(pipe_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if let Err(e) = sqlx::query!("UPDATE pipes SET active = 0 WHERE id = ?", pipe_id)
        .execute(&state.db)
        .await
    {
        error!("Failed to deactivate pipe {pipe_id}: {e}");
    }
    Html("").into_response()
}
