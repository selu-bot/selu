use askama::Template;
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use chrono::{Duration, Utc};
use serde::Serialize;
use tracing::error;
use uuid::Uuid;

use crate::state::AppState;
use crate::web::auth::AuthUser;
use crate::web::{BasePath, ExternalOrigin};

// ── Template ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "mobile.html")]
struct MobileTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    origin: String,
    username: String,
}

// ── GET /mobile ──────────────────────────────────────────────────────────────

pub async fn mobile_page(
    user: AuthUser,
    BasePath(base_path): BasePath,
    ExternalOrigin(origin): ExternalOrigin,
) -> Response {
    let tmpl = MobileTemplate {
        active_nav: "mobile",
        is_admin: user.is_admin,
        base_path,
        origin,
        username: user.username,
    };
    match tmpl.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── POST /mobile/setup-token ─────────────────────────────────────────────────

#[derive(Serialize)]
struct SetupTokenResponse {
    token: String,
    server_url: String,
    expires_at: String,
}

pub async fn create_setup_token(
    user: AuthUser,
    State(state): State<AppState>,
    ExternalOrigin(origin): ExternalOrigin,
) -> impl IntoResponse {
    let token = Uuid::new_v4().to_string().replace('-', "");
    let user_id = user.user_id.clone();
    let expires_at = (Utc::now() + Duration::minutes(5))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    if let Err(e) = sqlx::query(
        "INSERT INTO mobile_setup_tokens (token, user_id, expires_at) VALUES (?, ?, ?)",
    )
    .bind(&token)
    .bind(&user_id)
    .bind(&expires_at)
    .execute(&state.db)
    .await
    {
        error!("Failed to create setup token: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    Json(SetupTokenResponse {
        token,
        server_url: origin,
        expires_at,
    })
    .into_response()
}
