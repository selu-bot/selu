use anyhow::{Result, anyhow};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::{
    anthropic::AnthropicProvider, bedrock::BedrockProvider, openai::OpenAiProvider,
    provider::LlmProvider,
};
use crate::permissions::store::CredentialStore;

/// Cache key for provider instances: (provider_id, model_id)
type ProviderCacheKey = (String, String);

/// Thread-safe cache for LLM provider instances.
///
/// Providers are keyed by (provider_id, model_id). Since the API key is
/// encrypted in the DB and decryption is not free, caching avoids repeated
/// DB lookups and decryption on every turn.
///
/// The cache is invalidated when the user changes provider settings (see
/// `invalidate` method).
#[derive(Clone)]
pub struct ProviderCache {
    inner: Arc<Mutex<HashMap<ProviderCacheKey, Arc<dyn LlmProvider>>>>,
}

impl ProviderCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get a cached provider or load and cache a new one.
    pub async fn get_or_load(
        &self,
        db: &SqlitePool,
        provider_id: &str,
        model_id: &str,
        cred_store: &CredentialStore,
    ) -> Result<Arc<dyn LlmProvider>> {
        let key = (provider_id.to_string(), model_id.to_string());

        // Fast path: check cache
        {
            let cache = self.inner.lock().await;
            if let Some(provider) = cache.get(&key) {
                return Ok(provider.clone());
            }
        }

        // Slow path: load from DB, then cache
        let provider = load_provider_from_db(db, provider_id, model_id, cred_store).await?;

        {
            let mut cache = self.inner.lock().await;
            cache.insert(key, provider.clone());
        }

        Ok(provider)
    }

    /// Invalidate all cached providers. Call this when provider settings
    /// (API keys, base URLs) are changed.
    pub async fn invalidate(&self) {
        let mut cache = self.inner.lock().await;
        cache.clear();
        tracing::debug!("LLM provider cache invalidated");
    }

    /// Invalidate a specific provider+model combo.
    #[allow(dead_code)]
    pub async fn invalidate_provider(&self, provider_id: &str, model_id: &str) {
        let mut cache = self.inner.lock().await;
        cache.remove(&(provider_id.to_string(), model_id.to_string()));
    }
}

/// Loads an LLM provider from the database by provider ID.
/// API keys are stored encrypted in llm_providers table using AES-256-GCM.
/// The `model_id` parameter overrides the default model for the provider.
///
/// Prefer using `ProviderCache::get_or_load` instead of calling this directly.
pub async fn load_provider(
    db: &SqlitePool,
    provider_id: &str,
    model_id: &str,
    cred_store: &CredentialStore,
) -> Result<Arc<dyn LlmProvider>> {
    load_provider_from_db(db, provider_id, model_id, cred_store).await
}

/// Internal: loads a fresh provider from DB (no caching).
async fn load_provider_from_db(
    db: &SqlitePool,
    provider_id: &str,
    model_id: &str,
    cred_store: &CredentialStore,
) -> Result<Arc<dyn LlmProvider>> {
    let row = sqlx::query!(
        "SELECT id, api_key_encrypted, base_url FROM llm_providers WHERE id = ? AND active = 1",
        provider_id
    )
    .fetch_optional(db)
    .await?
    .ok_or_else(|| anyhow!("LLM provider '{}' not found or inactive", provider_id))?;

    // Decrypt the API key using the credential store's encryption key
    let api_key = match row.api_key_encrypted {
        Some(ref encrypted) if !encrypted.is_empty() => {
            let plaintext_bytes = cred_store.decrypt_raw(encrypted)?;
            String::from_utf8(plaintext_bytes)
                .map_err(|_| anyhow!("LLM API key for '{}' is not valid UTF-8", provider_id))?
        }
        _ => String::new(),
    };

    let base_url = row.base_url;

    let provider: Arc<dyn LlmProvider> = match provider_id {
        "anthropic" => Arc::new(AnthropicProvider::new(api_key, model_id)),
        "openai" => Arc::new(OpenAiProvider::new(api_key, model_id)),
        "bedrock" => {
            // base_url stores the AWS region (e.g. "us-east-1")
            let region = base_url
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| "us-east-1".into());
            Arc::new(BedrockProvider::new(api_key, region, model_id))
        }
        "ollama" => {
            let url = base_url.unwrap_or_else(|| "http://localhost:11434".into());
            // model can be configured in base_url as "http://host:port|model-name"
            let (url, default_model) = if let Some((u, m)) = url.split_once('|') {
                (u.to_string(), m.to_string())
            } else {
                (url, "llama3.2".to_string())
            };
            // Use agent's model_id if specified, otherwise use the one from base_url
            let effective_model = if model_id.is_empty() || model_id == "ollama" {
                default_model
            } else {
                model_id.to_string()
            };
            Arc::new(OpenAiProvider::ollama(url, effective_model))
        }
        other => return Err(anyhow!("Unknown LLM provider: {}", other)),
    };

    Ok(provider)
}
