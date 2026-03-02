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
    is_admin: bool,
    providers: Vec<ProviderView>,
    error: Option<String>,
    success: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ProvidersQuery {
    pub error: Option<String>,
    pub success: Option<String>,
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

pub async fn providers_index(user: AuthUser, Query(q): Query<ProvidersQuery>, State(state): State<AppState>) -> Response {
    if !user.is_admin {
        return Redirect::to("/chat").into_response();
    }

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
        is_admin: user.is_admin,
        providers,
        error: q.error,
        success: q.success,
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
    user: AuthUser,
    Path(provider_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<SetKeyForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    if form.api_key.trim().is_empty() {
        return Redirect::to("/providers").into_response();
    }

    // Encrypt the API key using the credential store
    let encrypted = match state.credentials.encrypt_raw(form.api_key.trim().as_bytes()) {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to encrypt API key: {e}");
            return Redirect::to("/providers?error=Failed+to+encrypt+API+key.+Please+try+again.").into_response();
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
        return Redirect::to("/providers?error=Failed+to+save+API+key.+Please+try+again.").into_response();
    }

    // Invalidate cached provider so the new key is picked up
    state.provider_cache.invalidate().await;

    Redirect::to("/providers?success=API+key+updated+successfully.").into_response()
}

pub async fn providers_delete_key(
    user: AuthUser,
    Path(provider_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
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

    // Invalidate cached provider so the removed key is reflected
    state.provider_cache.invalidate().await;

    // Return a refreshed card — simplest: redirect via HX-Redirect header
    Redirect::to("/providers").into_response()
}

pub async fn providers_set_region(
    user: AuthUser,
    Path(provider_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<SetRegionForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Err(e) = sqlx::query!(
        "UPDATE llm_providers SET base_url = ? WHERE id = ?",
        form.base_url,
        provider_id
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to update provider region: {e}");
        return Redirect::to("/providers?error=Failed+to+save+settings.+Please+try+again.").into_response();
    }

    // Invalidate cached provider since region/URL changed
    state.provider_cache.invalidate().await;

    Redirect::to("/providers?success=Settings+saved.").into_response()
}
