use askama::Template;
use axum::{
    extract::{Path, State},
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
pub struct ProviderView {
    pub id: String,
    pub display_name: String,
    pub has_key: bool,
    pub base_url: String,
    pub active: bool,
}

// ── Template ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "providers.html")]
struct ProvidersTemplate {
    active_nav: &'static str,
    providers: Vec<ProviderView>,
}

// ── Form structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SetKeyForm {
    pub api_key: String,
}

#[derive(Debug, Deserialize)]
pub struct SetRegionForm {
    pub base_url: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn providers_index(_user: AuthUser, State(state): State<AppState>) -> Response {
    let providers = sqlx::query!(
        "SELECT id, display_name, api_key_encrypted, base_url, active
         FROM llm_providers ORDER BY id"
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| ProviderView {
        id: r.id.unwrap_or_default(),
        display_name: r.display_name,
        has_key: r.api_key_encrypted.as_ref().map_or(false, |k| !k.is_empty()),
        base_url: r.base_url.unwrap_or_default(),
        active: r.active != 0,
    })
    .collect();

    match (ProvidersTemplate {
        active_nav: "providers",
        providers,
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

pub async fn providers_set_key(
    _user: AuthUser,
    Path(provider_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<SetKeyForm>,
) -> Response {
    if form.api_key.trim().is_empty() {
        return Redirect::to("/providers").into_response();
    }

    // Encrypt the API key using the credential store
    let encrypted = match state.credentials.encrypt_raw(form.api_key.trim().as_bytes()) {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to encrypt API key: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if let Err(e) = sqlx::query!(
        "UPDATE llm_providers SET api_key_encrypted = ? WHERE id = ?",
        encrypted,
        provider_id
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to update provider key: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    Redirect::to("/providers").into_response()
}

pub async fn providers_delete_key(
    _user: AuthUser,
    Path(provider_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    let empty = String::new();
    if let Err(e) = sqlx::query!(
        "UPDATE llm_providers SET api_key_encrypted = ? WHERE id = ?",
        empty,
        provider_id
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to remove provider key: {e}");
    }

    // Return a refreshed card — simplest: redirect via HX-Redirect header
    Redirect::to("/providers").into_response()
}

pub async fn providers_set_region(
    _user: AuthUser,
    Path(provider_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<SetRegionForm>,
) -> Response {
    if let Err(e) = sqlx::query!(
        "UPDATE llm_providers SET base_url = ? WHERE id = ?",
        form.base_url,
        provider_id
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to update provider region: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    Redirect::to("/providers").into_response()
}
