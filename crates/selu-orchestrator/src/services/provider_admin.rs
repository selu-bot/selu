//! Versioned provider and secret administration service.
//!
//! This module contains no HTTP concerns. Provider secrets are accepted only on
//! writes, encrypted by [`CredentialStore`], and represented on reads only by a
//! boolean. Capability secrets are exposed only as [`SecretMetadata`].

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;

use crate::llm::{models::ModelInfo, registry::ProviderCache};
use crate::permissions::CredentialStore;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("unknown provider")]
    UnknownProvider,
    #[error("resource not found")]
    NotFound,
    #[error("provider is not configured")]
    NotConfigured,
    #[error("invalid request: {0}")]
    Validation(&'static str),
    #[error("provider connection failed: {0}")]
    Connection(String),
    #[error("database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("credential operation failed")]
    Credential(#[source] anyhow::Error),
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Cloud,
    Local,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProviderView {
    pub id: String,
    pub display_name: String,
    pub kind: ProviderKind,
    pub requires_api_key: bool,
    pub requires_base_url: bool,
    pub default_base_url: Option<String>,
    pub configured: bool,
    pub active: bool,
    pub has_api_key: bool,
    pub base_url: Option<String>,
}

/// PUT input. Omitting `api_key` preserves the existing write-only key, while
/// omitting `base_url` applies the provider default (or clears it when there is
/// no default). DELETE is the explicit way to remove the full configuration.
#[derive(Deserialize)]
pub struct ProviderConfigurationInput {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct ProviderMeta {
    id: &'static str,
    display_name: &'static str,
    kind: ProviderKind,
    requires_api_key: bool,
    requires_base_url: bool,
    default_base_url: &'static str,
}

const PROVIDERS: &[ProviderMeta] = &[
    ProviderMeta {
        id: "openai",
        display_name: "OpenAI",
        kind: ProviderKind::Cloud,
        requires_api_key: true,
        requires_base_url: false,
        default_base_url: "",
    },
    ProviderMeta {
        id: "anthropic",
        display_name: "Anthropic Claude",
        kind: ProviderKind::Cloud,
        requires_api_key: true,
        requires_base_url: false,
        default_base_url: "",
    },
    ProviderMeta {
        id: "grok",
        display_name: "xAI Grok",
        kind: ProviderKind::Cloud,
        requires_api_key: true,
        requires_base_url: false,
        default_base_url: "https://api.x.ai",
    },
    ProviderMeta {
        id: "bedrock",
        display_name: "Amazon Bedrock",
        kind: ProviderKind::Cloud,
        requires_api_key: true,
        requires_base_url: true,
        default_base_url: "us-east-1",
    },
    ProviderMeta {
        id: "pico",
        display_name: "Pico AI Server",
        kind: ProviderKind::Local,
        requires_api_key: false,
        requires_base_url: true,
        default_base_url: "",
    },
];

fn provider_meta(provider_id: &str) -> Result<&'static ProviderMeta, ServiceError> {
    PROVIDERS
        .iter()
        .find(|provider| provider.id == provider_id)
        .ok_or(ServiceError::UnknownProvider)
}

fn normalized_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn configured(meta: &ProviderMeta, active: bool, has_key: bool, base_url: Option<&str>) -> bool {
    active
        && (!meta.requires_api_key || has_key)
        && (!meta.requires_base_url || base_url.is_some_and(|value| !value.trim().is_empty()))
}

fn provider_view(
    meta: &ProviderMeta,
    active: bool,
    api_key_encrypted: Option<&str>,
    base_url: Option<String>,
) -> ProviderView {
    let has_api_key = api_key_encrypted.is_some_and(|value| !value.is_empty());
    ProviderView {
        id: meta.id.to_string(),
        display_name: meta.display_name.to_string(),
        kind: meta.kind,
        requires_api_key: meta.requires_api_key,
        requires_base_url: meta.requires_base_url,
        default_base_url: (!meta.default_base_url.is_empty())
            .then(|| meta.default_base_url.to_string()),
        configured: configured(meta, active, has_api_key, base_url.as_deref()),
        active,
        has_api_key,
        base_url,
    }
}

pub async fn provider_catalogue(db: &SqlitePool) -> Result<Vec<ProviderView>, ServiceError> {
    let rows = sqlx::query!(
        "SELECT id, api_key_encrypted, base_url, active FROM llm_providers ORDER BY id"
    )
    .fetch_all(db)
    .await?;

    let rows: HashMap<_, _> = rows
        .into_iter()
        .map(|row| (row.id.clone().unwrap_or_default(), row))
        .collect();

    Ok(PROVIDERS
        .iter()
        .map(|meta| {
            rows.get(meta.id).map_or_else(
                || provider_view(meta, false, None, None),
                |row| {
                    provider_view(
                        meta,
                        row.active != 0,
                        row.api_key_encrypted.as_deref(),
                        row.base_url.clone(),
                    )
                },
            )
        })
        .collect())
}

pub async fn get_provider(
    db: &SqlitePool,
    provider_id: &str,
) -> Result<ProviderView, ServiceError> {
    let meta = provider_meta(provider_id)?;
    let row = sqlx::query!(
        "SELECT api_key_encrypted, base_url, active FROM llm_providers WHERE id = ?",
        provider_id
    )
    .fetch_optional(db)
    .await?;

    Ok(row.map_or_else(
        || provider_view(meta, false, None, None),
        |row| {
            provider_view(
                meta,
                row.active != 0,
                row.api_key_encrypted.as_deref(),
                row.base_url,
            )
        },
    ))
}

/// Atomically replaces the public configuration and activates the provider.
/// The existing encrypted key is retained when the write-only key is omitted.
pub async fn put_provider_configuration(
    db: &SqlitePool,
    credentials: &CredentialStore,
    cache: &ProviderCache,
    provider_id: &str,
    input: ProviderConfigurationInput,
) -> Result<ProviderView, ServiceError> {
    let meta = provider_meta(provider_id)?;
    let mut transaction = db.begin().await?;
    let existing = sqlx::query!(
        "SELECT api_key_encrypted FROM llm_providers WHERE id = ?",
        provider_id
    )
    .fetch_optional(&mut *transaction)
    .await?;

    let api_key_encrypted = match input.api_key {
        Some(value) if value.trim().is_empty() => {
            return Err(ServiceError::Validation(
                "api_key must not be empty; use DELETE to remove configuration",
            ));
        }
        Some(value) => Some(
            credentials
                .encrypt_raw(value.trim().as_bytes())
                .map_err(ServiceError::Credential)?,
        ),
        None => existing.and_then(|row| row.api_key_encrypted),
    };

    let base_url = normalized_optional(input.base_url)
        .or_else(|| (!meta.default_base_url.is_empty()).then(|| meta.default_base_url.to_string()));
    let has_api_key = api_key_encrypted
        .as_deref()
        .is_some_and(|value| !value.is_empty());

    if meta.requires_api_key && !has_api_key {
        return Err(ServiceError::Validation("api_key is required"));
    }
    if meta.requires_base_url && base_url.is_none() {
        return Err(ServiceError::Validation("base_url is required"));
    }

    let display_name = meta.display_name;
    sqlx::query!(
        "INSERT INTO llm_providers (id, display_name, api_key_encrypted, base_url, active)
         VALUES (?, ?, ?, ?, 1)
         ON CONFLICT(id) DO UPDATE SET
             display_name = excluded.display_name,
             api_key_encrypted = excluded.api_key_encrypted,
             base_url = excluded.base_url,
             active = 1",
        provider_id,
        display_name,
        api_key_encrypted,
        base_url
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    cache.invalidate().await;
    get_provider(db, provider_id).await
}

pub async fn delete_provider_configuration(
    db: &SqlitePool,
    cache: &ProviderCache,
    provider_id: &str,
) -> Result<(), ServiceError> {
    provider_meta(provider_id)?;
    sqlx::query!(
        "UPDATE llm_providers
         SET api_key_encrypted = NULL, base_url = NULL, active = 0
         WHERE id = ?",
        provider_id
    )
    .execute(db)
    .await?;
    cache.invalidate().await;
    Ok(())
}

pub async fn provider_models(
    db: &SqlitePool,
    credentials: &CredentialStore,
    provider_id: &str,
) -> Result<Vec<ModelInfo>, ServiceError> {
    provider_meta(provider_id)?;
    Ok(crate::llm::models::list_models(db, credentials, provider_id).await)
}

pub async fn test_provider_connection(
    db: &SqlitePool,
    credentials: &CredentialStore,
    provider_id: &str,
) -> Result<(), ServiceError> {
    let meta = provider_meta(provider_id)?;
    let row = sqlx::query!(
        "SELECT api_key_encrypted, base_url, active FROM llm_providers WHERE id = ?",
        provider_id
    )
    .fetch_optional(db)
    .await?
    .ok_or(ServiceError::NotConfigured)?;

    let has_api_key = row
        .api_key_encrypted
        .as_deref()
        .is_some_and(|value| !value.is_empty());
    if !configured(meta, row.active != 0, has_api_key, row.base_url.as_deref()) {
        return Err(ServiceError::NotConfigured);
    }

    let api_key = match row.api_key_encrypted {
        Some(encrypted) if !encrypted.is_empty() => String::from_utf8(
            credentials
                .decrypt_raw(&encrypted)
                .map_err(ServiceError::Credential)?,
        )
        .map_err(|error| ServiceError::Connection(error.to_string()))?,
        _ => String::new(),
    };
    check_provider_access(meta, &api_key, row.base_url.as_deref().unwrap_or_default()).await
}

async fn check_provider_access(
    meta: &ProviderMeta,
    api_key: &str,
    configured_base_url: &str,
) -> Result<(), ServiceError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| ServiceError::Connection(error.to_string()))?;

    let response = match meta.id {
        "openai" => {
            let base = if configured_base_url.is_empty() {
                "https://api.openai.com"
            } else {
                configured_base_url.trim_end_matches('/')
            };
            client
                .get(format!("{base}/v1/models"))
                .bearer_auth(api_key)
                .send()
                .await
        }
        "anthropic" => {
            client
                .get("https://api.anthropic.com/v1/models?limit=1")
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await
        }
        "grok" => {
            let base = if configured_base_url.is_empty() {
                "https://api.x.ai"
            } else {
                configured_base_url.trim_end_matches('/')
            };
            client
                .get(format!("{base}/v1/models"))
                .bearer_auth(api_key)
                .send()
                .await
        }
        "bedrock" => {
            let region = if configured_base_url.is_empty() {
                "us-east-1"
            } else {
                configured_base_url
            };
            client
                .get(format!(
                    "https://bedrock.{region}.amazonaws.com/foundation-models"
                ))
                .bearer_auth(api_key)
                .send()
                .await
        }
        "pico" => {
            let base = configured_base_url.trim_end_matches('/');
            client.get(format!("{base}/v1/models")).send().await
        }
        _ => return Err(ServiceError::UnknownProvider),
    }
    .map_err(|error| ServiceError::Connection(error.to_string()))?;

    if response.status().is_success() {
        Ok(())
    } else {
        Err(ServiceError::Connection(format!(
            "{} returned HTTP {}",
            meta.display_name,
            response.status()
        )))
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecretScope {
    System,
    User,
}

/// The only read representation of a stored secret.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SecretMetadata {
    pub scope: SecretScope,
    pub capability_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub created_at: String,
}

/// Intentionally deserialize-only so a secret value cannot be serialized into
/// an API response by reusing the request type.
#[derive(Deserialize)]
pub struct SecretValueInput {
    pub value: String,
}

fn validate_secret_parts(
    capability_id: &str,
    name: &str,
    value: Option<&str>,
) -> Result<(), ServiceError> {
    if capability_id.trim().is_empty() {
        return Err(ServiceError::Validation("capability_id must not be empty"));
    }
    if name.trim().is_empty() {
        return Err(ServiceError::Validation("secret name must not be empty"));
    }
    if value.is_some_and(|value| value.trim().is_empty()) {
        return Err(ServiceError::Validation("secret value must not be empty"));
    }
    Ok(())
}

pub async fn list_all_system_secrets(db: &SqlitePool) -> Result<Vec<SecretMetadata>, ServiceError> {
    let rows = sqlx::query!(
        "SELECT capability_id, credential_name, created_at
         FROM system_credentials
         ORDER BY capability_id, credential_name"
    )
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| SecretMetadata {
            scope: SecretScope::System,
            capability_id: row.capability_id,
            name: row.credential_name,
            user_id: None,
            expires_at: None,
            created_at: row.created_at,
        })
        .collect())
}

pub async fn list_system_secrets(
    db: &SqlitePool,
    capability_id: &str,
) -> Result<Vec<SecretMetadata>, ServiceError> {
    validate_secret_parts(capability_id, "metadata", None)?;
    let rows = sqlx::query!(
        "SELECT capability_id, credential_name, created_at
         FROM system_credentials
         WHERE capability_id = ?
         ORDER BY credential_name",
        capability_id
    )
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| SecretMetadata {
            scope: SecretScope::System,
            capability_id: row.capability_id,
            name: row.credential_name,
            user_id: None,
            expires_at: None,
            created_at: row.created_at,
        })
        .collect())
}

async fn get_system_secret(
    db: &SqlitePool,
    capability_id: &str,
    name: &str,
) -> Result<Option<SecretMetadata>, ServiceError> {
    let row = sqlx::query!(
        "SELECT capability_id, credential_name, created_at
         FROM system_credentials
         WHERE capability_id = ? AND credential_name = ?",
        capability_id,
        name
    )
    .fetch_optional(db)
    .await?;

    Ok(row.map(|row| SecretMetadata {
        scope: SecretScope::System,
        capability_id: row.capability_id,
        name: row.credential_name,
        user_id: None,
        expires_at: None,
        created_at: row.created_at,
    }))
}

pub async fn put_system_secret(
    db: &SqlitePool,
    credentials: &CredentialStore,
    capability_id: &str,
    name: &str,
    value: &str,
) -> Result<SecretMetadata, ServiceError> {
    validate_secret_parts(capability_id, name, Some(value))?;
    credentials
        .set_system(capability_id, name, value)
        .await
        .map_err(ServiceError::Credential)?;
    get_system_secret(db, capability_id, name)
        .await?
        .ok_or(ServiceError::NotFound)
}

pub async fn delete_system_secret(
    db: &SqlitePool,
    credentials: &CredentialStore,
    capability_id: &str,
    name: &str,
) -> Result<(), ServiceError> {
    validate_secret_parts(capability_id, name, None)?;
    if get_system_secret(db, capability_id, name).await?.is_none() {
        return Err(ServiceError::NotFound);
    }
    credentials
        .delete_system(capability_id, name)
        .await
        .map_err(ServiceError::Credential)
}

async fn ensure_user(db: &SqlitePool, user_id: &str) -> Result<(), ServiceError> {
    let row = sqlx::query!("SELECT id FROM users WHERE id = ?", user_id)
        .fetch_optional(db)
        .await?;
    row.ok_or(ServiceError::NotFound).map(|_| ())
}

pub async fn list_all_user_secrets(
    db: &SqlitePool,
    user_id: &str,
) -> Result<Vec<SecretMetadata>, ServiceError> {
    ensure_user(db, user_id).await?;
    let rows = sqlx::query!(
        "SELECT user_id, capability_id, credential_name, expires_at, created_at
         FROM user_credentials
         WHERE user_id = ? AND (expires_at IS NULL OR expires_at > datetime('now'))
         ORDER BY capability_id, credential_name",
        user_id
    )
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| SecretMetadata {
            scope: SecretScope::User,
            capability_id: row.capability_id,
            name: row.credential_name,
            user_id: Some(row.user_id),
            expires_at: row.expires_at,
            created_at: row.created_at,
        })
        .collect())
}

pub async fn list_user_secrets(
    db: &SqlitePool,
    user_id: &str,
    capability_id: &str,
) -> Result<Vec<SecretMetadata>, ServiceError> {
    validate_secret_parts(capability_id, "metadata", None)?;
    ensure_user(db, user_id).await?;
    let rows = sqlx::query!(
        "SELECT user_id, capability_id, credential_name, expires_at, created_at
         FROM user_credentials
         WHERE user_id = ? AND capability_id = ?
           AND (expires_at IS NULL OR expires_at > datetime('now'))
         ORDER BY credential_name",
        user_id,
        capability_id
    )
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| SecretMetadata {
            scope: SecretScope::User,
            capability_id: row.capability_id,
            name: row.credential_name,
            user_id: Some(row.user_id),
            expires_at: row.expires_at,
            created_at: row.created_at,
        })
        .collect())
}

async fn get_user_secret(
    db: &SqlitePool,
    user_id: &str,
    capability_id: &str,
    name: &str,
) -> Result<Option<SecretMetadata>, ServiceError> {
    let row = sqlx::query!(
        "SELECT user_id, capability_id, credential_name, expires_at, created_at
         FROM user_credentials
         WHERE user_id = ? AND capability_id = ? AND credential_name = ?
           AND (expires_at IS NULL OR expires_at > datetime('now'))",
        user_id,
        capability_id,
        name
    )
    .fetch_optional(db)
    .await?;

    Ok(row.map(|row| SecretMetadata {
        scope: SecretScope::User,
        capability_id: row.capability_id,
        name: row.credential_name,
        user_id: Some(row.user_id),
        expires_at: row.expires_at,
        created_at: row.created_at,
    }))
}

pub async fn put_user_secret(
    db: &SqlitePool,
    credentials: &CredentialStore,
    user_id: &str,
    capability_id: &str,
    name: &str,
    value: &str,
) -> Result<SecretMetadata, ServiceError> {
    validate_secret_parts(capability_id, name, Some(value))?;
    ensure_user(db, user_id).await?;
    credentials
        .set_user(user_id, capability_id, name, value)
        .await
        .map_err(ServiceError::Credential)?;

    // A replacement is a fresh secret; do not retain an expiry from an older
    // value because CredentialStore intentionally has no expiry parameter.
    sqlx::query!(
        "UPDATE user_credentials SET expires_at = NULL
         WHERE user_id = ? AND capability_id = ? AND credential_name = ?",
        user_id,
        capability_id,
        name
    )
    .execute(db)
    .await?;

    get_user_secret(db, user_id, capability_id, name)
        .await?
        .ok_or(ServiceError::NotFound)
}

pub async fn delete_user_secret(
    db: &SqlitePool,
    credentials: &CredentialStore,
    user_id: &str,
    capability_id: &str,
    name: &str,
) -> Result<(), ServiceError> {
    validate_secret_parts(capability_id, name, None)?;
    ensure_user(db, user_id).await?;
    if get_user_secret(db, user_id, capability_id, name)
        .await?
        .is_none()
    {
        return Err(ServiceError::NotFound);
    }
    credentials
        .delete_user(user_id, capability_id, name)
        .await
        .map_err(ServiceError::Credential)
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;
    use uuid::Uuid;

    use super::*;

    async fn test_db() -> SqlitePool {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        db
    }

    #[test]
    fn provider_requirements_match_supported_backends() {
        let openai = provider_meta("openai").unwrap();
        let bedrock = provider_meta("bedrock").unwrap();
        let pico = provider_meta("pico").unwrap();

        assert!(configured(openai, true, true, None));
        assert!(!configured(openai, true, false, None));
        assert!(!configured(bedrock, true, true, None));
        assert!(configured(bedrock, true, true, Some("us-east-1")));
        assert!(configured(pico, true, false, Some("http://localhost:8090")));
        assert!(!configured(pico, true, false, None));
    }

    #[tokio::test]
    async fn provider_put_encrypts_key_and_delete_removes_configuration() {
        let db = test_db().await;
        let credentials = CredentialStore::new(db.clone(), [0x31; 32]);
        let cache = ProviderCache::new();

        let view = put_provider_configuration(
            &db,
            &credentials,
            &cache,
            "openai",
            ProviderConfigurationInput {
                api_key: Some("top-secret".to_string()),
                base_url: None,
            },
        )
        .await
        .unwrap();
        assert!(view.configured);
        assert!(view.has_api_key);

        let row = sqlx::query!("SELECT api_key_encrypted FROM llm_providers WHERE id = 'openai'")
            .fetch_one(&db)
            .await
            .unwrap();
        let encrypted = row.api_key_encrypted.unwrap();
        assert_ne!(encrypted, "top-secret");
        assert_eq!(credentials.decrypt_raw(&encrypted).unwrap(), b"top-secret");

        delete_provider_configuration(&db, &cache, "openai")
            .await
            .unwrap();
        let view = get_provider(&db, "openai").await.unwrap();
        assert!(!view.active);
        assert!(!view.configured);
        assert!(!view.has_api_key);
    }

    #[tokio::test]
    async fn pico_requires_url_without_mutating_the_database() {
        let db = test_db().await;
        let credentials = CredentialStore::new(db.clone(), [0x42; 32]);
        let cache = ProviderCache::new();

        let result = put_provider_configuration(
            &db,
            &credentials,
            &cache,
            "pico",
            ProviderConfigurationInput {
                api_key: None,
                base_url: None,
            },
        )
        .await;
        assert!(matches!(result, Err(ServiceError::Validation(_))));
        assert!(!get_provider(&db, "pico").await.unwrap().active);
    }

    #[tokio::test]
    async fn secret_writes_return_metadata_only() {
        let db = test_db().await;
        let credentials = CredentialStore::new(db.clone(), [0x53; 32]);
        let user_id = Uuid::new_v4().to_string();
        sqlx::query!(
            "INSERT INTO users (id, username, display_name, password_hash)
             VALUES (?, 'secret-test', 'Secret Test', 'hash')",
            user_id
        )
        .execute(&db)
        .await
        .unwrap();

        let metadata = put_user_secret(
            &db,
            &credentials,
            &user_id,
            "github",
            "token",
            "plain-secret",
        )
        .await
        .unwrap();
        let json = serde_json::to_string(&metadata).unwrap();
        assert!(!json.contains("plain-secret"));
        assert!(!json.contains("encrypted"));
        assert_eq!(metadata.scope, SecretScope::User);
        assert_eq!(
            credentials
                .get_user(&user_id, "github", "token")
                .await
                .unwrap()
                .as_deref(),
            Some("plain-secret")
        );

        let other_user_id = Uuid::new_v4().to_string();
        sqlx::query!(
            "INSERT INTO users (id, username, display_name, password_hash)
             VALUES (?, 'other-secret-test', 'Other Secret Test', 'hash')",
            other_user_id
        )
        .execute(&db)
        .await
        .unwrap();
        put_user_secret(
            &db,
            &credentials,
            &other_user_id,
            "calendar",
            "password",
            "other-secret",
        )
        .await
        .unwrap();
        put_system_secret(&db, &credentials, "search", "api_key", "system-secret")
            .await
            .unwrap();

        let own_metadata = list_all_user_secrets(&db, &user_id).await.unwrap();
        assert_eq!(own_metadata.len(), 1);
        assert_eq!(own_metadata[0].capability_id, "github");
        let own_json = serde_json::to_string(&own_metadata).unwrap();
        assert!(!own_json.contains("plain-secret"));
        assert!(!own_json.contains("other-secret"));

        let system_metadata = list_all_system_secrets(&db).await.unwrap();
        assert_eq!(system_metadata.len(), 1);
        assert_eq!(system_metadata[0].capability_id, "search");
        assert!(
            !serde_json::to_string(&system_metadata)
                .unwrap()
                .contains("system-secret")
        );
    }
}
