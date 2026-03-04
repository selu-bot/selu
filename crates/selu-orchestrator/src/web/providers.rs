use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use tracing::error;

use crate::state::AppState;
use crate::web::auth::AuthUser;
use crate::web::{BasePath, prefixed_redirect};

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProviderView {
    pub id: String,
    pub display_name: String,
    pub category_key: &'static str,
    pub subtitle_key: &'static str,
}

#[derive(Debug, Clone)]
pub struct ProviderCatalogView {
    pub id: &'static str,
    pub display_name: &'static str,
    pub category_key: &'static str,
    pub subtitle_key: &'static str,
    pub already_active: bool,
}

// ── Template ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "providers.html")]
struct ProvidersTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    providers: Vec<ProviderView>,
    error: Option<String>,
    success: Option<String>,
}

#[derive(Template)]
#[template(path = "providers_new.html")]
struct ProvidersNewTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    providers: Vec<ProviderCatalogView>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "providers_setup.html")]
struct ProvidersSetupTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    provider_id: String,
    provider_display_name: String,
    subtitle_key: &'static str,
    base_url: String,
    has_key: bool,
    requires_api_key: bool,
    requires_base_url: bool,
    api_key_label_key: &'static str,
    base_url_label_key: &'static str,
    api_key_placeholder_key: &'static str,
    base_url_placeholder_key: &'static str,
    base_url_hint_key: &'static str,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ProvidersQuery {
    pub error: Option<String>,
    pub success: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetupQuery {
    pub error: Option<String>,
}

// ── Form structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ProviderSetupForm {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

// ── Provider metadata ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct ProviderMeta {
    id: &'static str,
    display_name: &'static str,
    category_key: &'static str,
    subtitle_key: &'static str,
    requires_api_key: bool,
    requires_base_url: bool,
    api_key_label_key: &'static str,
    base_url_label_key: &'static str,
    api_key_placeholder_key: &'static str,
    base_url_placeholder_key: &'static str,
    base_url_hint_key: &'static str,
    default_base_url: &'static str,
}

const PROVIDER_CATALOG: &[ProviderMeta] = &[
    ProviderMeta {
        id: "openai",
        display_name: "OpenAI",
        category_key: "providers.category.cloud",
        subtitle_key: "providers.provider.openai.subtitle",
        requires_api_key: true,
        requires_base_url: false,
        api_key_label_key: "providers.field.api_key",
        base_url_label_key: "providers.field.base_url",
        api_key_placeholder_key: "providers.provider.openai.key_placeholder",
        base_url_placeholder_key: "providers.provider.openai.base_placeholder",
        base_url_hint_key: "providers.provider.openai.base_hint",
        default_base_url: "",
    },
    ProviderMeta {
        id: "anthropic",
        display_name: "Anthropic Claude",
        category_key: "providers.category.cloud",
        subtitle_key: "providers.provider.anthropic.subtitle",
        requires_api_key: true,
        requires_base_url: false,
        api_key_label_key: "providers.field.api_key",
        base_url_label_key: "providers.field.base_url",
        api_key_placeholder_key: "providers.provider.anthropic.key_placeholder",
        base_url_placeholder_key: "providers.provider.anthropic.base_placeholder",
        base_url_hint_key: "providers.provider.anthropic.base_hint",
        default_base_url: "",
    },
    ProviderMeta {
        id: "bedrock",
        display_name: "Amazon Bedrock",
        category_key: "providers.category.cloud",
        subtitle_key: "providers.provider.bedrock.subtitle",
        requires_api_key: true,
        requires_base_url: true,
        api_key_label_key: "providers.provider.bedrock.token_label",
        base_url_label_key: "providers.provider.bedrock.region_label",
        api_key_placeholder_key: "providers.provider.bedrock.token_placeholder",
        base_url_placeholder_key: "providers.provider.bedrock.region_placeholder",
        base_url_hint_key: "providers.provider.bedrock.region_hint",
        default_base_url: "us-east-1",
    },
    ProviderMeta {
        id: "ollama",
        display_name: "Ollama",
        category_key: "providers.category.local",
        subtitle_key: "providers.provider.ollama.subtitle",
        requires_api_key: false,
        requires_base_url: true,
        api_key_label_key: "providers.provider.ollama.key_label",
        base_url_label_key: "providers.provider.ollama.url_label",
        api_key_placeholder_key: "providers.provider.ollama.key_placeholder",
        base_url_placeholder_key: "providers.provider.ollama.url_placeholder",
        base_url_hint_key: "providers.provider.ollama.url_hint",
        default_base_url: "http://localhost:11434",
    },
];

fn provider_meta(provider_id: &str) -> Option<&'static ProviderMeta> {
    PROVIDER_CATALOG.iter().find(|p| p.id == provider_id)
}

fn is_provider_connected(provider_id: &str, key: Option<&str>, base_url: Option<&str>) -> bool {
    let has_key = key.is_some_and(|k| !k.trim().is_empty());
    let has_base = base_url.is_some_and(|u| !u.trim().is_empty());

    match provider_id {
        "openai" | "anthropic" => has_key,
        "bedrock" => has_key && has_base,
        "ollama" => has_base,
        _ => false,
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn providers_index(
    user: AuthUser,
    Query(q): Query<ProvidersQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }

    let providers = sqlx::query!(
        "SELECT id, display_name, api_key_encrypted, base_url, active
         FROM llm_providers
         WHERE active = 1
         ORDER BY id"
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .filter(|r| {
        is_provider_connected(
            r.id.as_deref().unwrap_or_default(),
            r.api_key_encrypted.as_deref(),
            r.base_url.as_deref(),
        )
    })
    .map(|r| {
        let provider_id = r.id.unwrap_or_default();
        let meta = provider_meta(&provider_id);
        ProviderView {
            id: provider_id,
            display_name: meta.map_or_else(|| r.display_name.clone(), |m| m.display_name.into()),
            category_key: meta.map_or("providers.category.cloud", |m| m.category_key),
            subtitle_key: meta.map_or("providers.provider.custom.subtitle", |m| m.subtitle_key),
        }
    })
    .collect();

    match (ProvidersTemplate {
        active_nav: "providers",
        is_admin: user.is_admin,
        base_path,
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

pub async fn providers_new(
    user: AuthUser,
    Query(q): Query<SetupQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }

    let active_ids: std::collections::HashSet<String> = sqlx::query!(
        "SELECT id, api_key_encrypted, base_url
         FROM llm_providers
         WHERE active = 1"
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .filter(|r| {
        is_provider_connected(
            r.id.as_deref().unwrap_or_default(),
            r.api_key_encrypted.as_deref(),
            r.base_url.as_deref(),
        )
    })
    .map(|r| r.id.unwrap_or_default())
    .collect();

    let providers = PROVIDER_CATALOG
        .iter()
        .map(|p| ProviderCatalogView {
            id: p.id,
            display_name: p.display_name,
            category_key: p.category_key,
            subtitle_key: p.subtitle_key,
            already_active: active_ids.contains(p.id),
        })
        .collect();

    match (ProvidersNewTemplate {
        active_nav: "providers",
        is_admin: user.is_admin,
        base_path,
        providers,
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

pub async fn providers_setup_page(
    user: AuthUser,
    Path(provider_id): Path<String>,
    Query(q): Query<SetupQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }

    let Some(meta) = provider_meta(&provider_id) else {
        return prefixed_redirect(&base_path, "/providers/new").into_response();
    };

    let row = sqlx::query!(
        "SELECT base_url, api_key_encrypted
         FROM llm_providers
         WHERE id = ?",
        provider_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let (base_url, has_key) = if let Some(r) = row {
        (
            r.base_url
                .unwrap_or_else(|| meta.default_base_url.to_string()),
            r.api_key_encrypted.is_some_and(|k| !k.is_empty()),
        )
    } else {
        (meta.default_base_url.to_string(), false)
    };

    match (ProvidersSetupTemplate {
        active_nav: "providers",
        is_admin: user.is_admin,
        base_path,
        provider_id: provider_id.clone(),
        provider_display_name: meta.display_name.to_string(),
        subtitle_key: meta.subtitle_key,
        base_url,
        has_key,
        requires_api_key: meta.requires_api_key,
        requires_base_url: meta.requires_base_url,
        api_key_label_key: meta.api_key_label_key,
        base_url_label_key: meta.base_url_label_key,
        api_key_placeholder_key: meta.api_key_placeholder_key,
        base_url_placeholder_key: meta.base_url_placeholder_key,
        base_url_hint_key: meta.base_url_hint_key,
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

pub async fn providers_setup_submit(
    user: AuthUser,
    Path(provider_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<ProviderSetupForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    let Some(meta) = provider_meta(&provider_id) else {
        return prefixed_redirect(&base_path, "/providers/new").into_response();
    };

    let api_key = form.api_key.unwrap_or_default().trim().to_string();
    let base_url = form
        .base_url
        .unwrap_or_else(|| meta.default_base_url.to_string())
        .trim()
        .to_string();

    if meta.requires_api_key && api_key.is_empty() {
        return Redirect::to(&format!(
            "{}/providers/new/{}?error=Please+add+the+required+key+before+continuing.",
            base_path, provider_id
        ))
        .into_response();
    }

    if meta.requires_base_url && base_url.is_empty() {
        return Redirect::to(&format!(
            "{}/providers/new/{}?error=Please+fill+in+the+required+field+before+continuing.",
            base_path, provider_id
        ))
        .into_response();
    }

    if let Err(e) = check_provider_access(&provider_id, &api_key, &base_url).await {
        error!(provider_id = %provider_id, error = %e, "Provider access check failed");
        return Redirect::to(&format!(
            "{}/providers/new/{}?error=Couldn%27t+connect.+Please+check+your+details+and+try+again.",
            base_path, provider_id
        ))
        .into_response();
    }

    let encrypted = if api_key.is_empty() {
        String::new()
    } else {
        match state.credentials.encrypt_raw(api_key.as_bytes()) {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to encrypt API key: {e}");
                return Redirect::to(&format!(
                    "{}/providers/new/{}?error=Couldn%27t+save+your+key.+Please+try+again.",
                    base_path, provider_id
                ))
                .into_response();
            }
        }
    };

    if let Err(e) = sqlx::query!(
        "INSERT INTO llm_providers (id, display_name, api_key_encrypted, base_url, active)
         VALUES (?, ?, ?, ?, 1)
         ON CONFLICT(id) DO UPDATE SET
           display_name = excluded.display_name,
           api_key_encrypted = excluded.api_key_encrypted,
           base_url = excluded.base_url,
           active = 1",
        meta.id,
        meta.display_name,
        encrypted,
        base_url,
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to save provider settings: {e}");
        return Redirect::to(&format!(
            "{}/providers/new/{}?error=Couldn%27t+save+your+settings.+Please+try+again.",
            base_path, provider_id
        ))
        .into_response();
    }

    state.provider_cache.invalidate().await;

    Redirect::to(&format!("{}/providers?success=connected", base_path)).into_response()
}

pub async fn providers_disconnect(
    user: AuthUser,
    Path(provider_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    if provider_meta(&provider_id).is_none() {
        return prefixed_redirect(&base_path, "/providers").into_response();
    }

    if let Err(e) = sqlx::query!(
        "UPDATE llm_providers
         SET active = 0, api_key_encrypted = '', base_url = ''
         WHERE id = ?",
        provider_id
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to disconnect provider: {e}");
        return Redirect::to(&format!(
            "{}/providers?error=Couldn%27t+disconnect+the+provider.+Please+try+again.",
            base_path
        ))
        .into_response();
    }

    state.provider_cache.invalidate().await;

    Redirect::to(&format!("{}/providers?success=disconnected", base_path)).into_response()
}

// ── Access checks ─────────────────────────────────────────────────────────────

async fn check_provider_access(
    provider_id: &str,
    api_key: &str,
    base_url: &str,
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    match provider_id {
        "openai" => {
            let base = if base_url.trim().is_empty() {
                "https://api.openai.com".to_string()
            } else {
                base_url.trim_end_matches('/').to_string()
            };

            let resp = client
                .get(format!("{base}/v1/models"))
                .bearer_auth(api_key)
                .send()
                .await?;

            if !resp.status().is_success() {
                return Err(anyhow::anyhow!("OpenAI returned HTTP {}", resp.status()));
            }
        }
        "anthropic" => {
            let resp = client
                .get("https://api.anthropic.com/v1/models?limit=1")
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await?;

            if !resp.status().is_success() {
                return Err(anyhow::anyhow!("Anthropic returned HTTP {}", resp.status()));
            }
        }
        "bedrock" => {
            let region = if base_url.trim().is_empty() {
                "us-east-1"
            } else {
                base_url.trim()
            };
            let resp = client
                .get(format!(
                    "https://bedrock.{region}.amazonaws.com/foundation-models"
                ))
                .bearer_auth(api_key)
                .send()
                .await?;

            if !resp.status().is_success() {
                return Err(anyhow::anyhow!("Bedrock returned HTTP {}", resp.status()));
            }
        }
        "ollama" => {
            let base = if base_url.trim().is_empty() {
                "http://localhost:11434".to_string()
            } else {
                base_url.trim_end_matches('/').to_string()
            };

            let mut req = client.get(format!("{base}/api/tags"));
            if !api_key.trim().is_empty() {
                req = req.bearer_auth(api_key);
            }

            let resp = req.send().await?;
            if !resp.status().is_success() {
                return Err(anyhow::anyhow!("Ollama returned HTTP {}", resp.status()));
            }
        }
        _ => return Err(anyhow::anyhow!("Unsupported provider")),
    }

    Ok(())
}
