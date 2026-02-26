use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;
use tracing::error;

use crate::state::AppState;
use crate::web::auth::AuthUser;

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SysCredRow {
    pub capability_id: String,
    pub credential_name: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct UserCredRow {
    pub user_id: String,
    pub username: String,
    pub capability_id: String,
    pub credential_name: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct UserOption {
    pub id: String,
    pub display: String,
}

// ── Template ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "credentials.html")]
struct CredentialsTemplate {
    active_nav: &'static str,
    sys_creds: Vec<SysCredRow>,
    user_creds: Vec<UserCredRow>,
    users: Vec<UserOption>,
    error: Option<String>,
    success: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CredentialsQuery {
    pub error: Option<String>,
    pub success: Option<String>,
}

// ── Form structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SetSystemCredForm {
    pub capability_id: String,
    pub credential_name: String,
    pub value: String,
}

#[derive(Debug, Deserialize)]
pub struct SetUserCredForm {
    pub user_id: String,
    pub capability_id: String,
    pub credential_name: String,
    pub value: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn credentials_index(_user: AuthUser, Query(q): Query<CredentialsQuery>, State(state): State<AppState>) -> Response {
    let sys_creds = sqlx::query!(
        "SELECT capability_id, credential_name, created_at
         FROM system_credentials ORDER BY capability_id, credential_name"
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| SysCredRow {
        capability_id: r.capability_id,
        credential_name: r.credential_name,
        created_at: r.created_at,
    })
    .collect();

    let user_creds = sqlx::query!(
        r#"SELECT uc.user_id, u.username, uc.capability_id, uc.credential_name, uc.created_at
           FROM user_credentials uc
           JOIN users u ON uc.user_id = u.id
           ORDER BY u.username, uc.capability_id, uc.credential_name"#
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| UserCredRow {
        user_id: r.user_id,
        username: r.username,
        capability_id: r.capability_id,
        credential_name: r.credential_name,
        created_at: r.created_at,
    })
    .collect();

    let users = sqlx::query!("SELECT id, username, display_name FROM users ORDER BY username")
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| UserOption {
            id: r.id.unwrap_or_default(),
            display: format!("{} ({})", r.display_name, r.username),
        })
        .collect();

    match (CredentialsTemplate { active_nav: "credentials", sys_creds, user_creds, users, error: q.error, success: q.success })
        .render()
    {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn credentials_set_system(
    _user: AuthUser,
    State(state): State<AppState>,
    Form(form): Form<SetSystemCredForm>,
) -> Response {
    if form.capability_id.trim().is_empty()
        || form.credential_name.trim().is_empty()
        || form.value.is_empty()
    {
        return Redirect::to("/credentials?error=All+fields+are+required.").into_response();
    }
    match state
        .credentials
        .set_system(&form.capability_id, &form.credential_name, &form.value)
        .await
    {
        Ok(_) => Redirect::to("/credentials?success=System+credential+saved.").into_response(),
        Err(e) => {
            error!("Failed to set system credential: {e}");
            Redirect::to("/credentials?error=Failed+to+save+credential.+Please+try+again.").into_response()
        }
    }
}

pub async fn credentials_delete_system(
    _user: AuthUser,
    Path((cap_id, name)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    if let Err(e) = state.credentials.delete_system(&cap_id, &name).await {
        error!("Failed to delete system credential: {e}");
    }
    Html("").into_response()
}

pub async fn credentials_set_user(
    _user: AuthUser,
    State(state): State<AppState>,
    Form(form): Form<SetUserCredForm>,
) -> Response {
    if form.user_id.is_empty()
        || form.capability_id.trim().is_empty()
        || form.credential_name.trim().is_empty()
        || form.value.is_empty()
    {
        return Redirect::to("/credentials?error=All+fields+are+required.").into_response();
    }
    match state
        .credentials
        .set_user(&form.user_id, &form.capability_id, &form.credential_name, &form.value)
        .await
    {
        Ok(_) => Redirect::to("/credentials?success=User+credential+saved.").into_response(),
        Err(e) => {
            error!("Failed to set user credential: {e}");
            Redirect::to("/credentials?error=Failed+to+save+credential.+Please+try+again.").into_response()
        }
    }
}

pub async fn credentials_delete_user(
    _user: AuthUser,
    Path((user_id, cap_id, name)): Path<(String, String, String)>,
    State(state): State<AppState>,
) -> Response {
    if let Err(e) = state.credentials.delete_user(&user_id, &cap_id, &name).await {
        error!("Failed to delete user credential: {e}");
    }
    Html("").into_response()
}
