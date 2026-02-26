use anyhow::{anyhow, Result};
use sqlx::SqlitePool;
use std::sync::Arc;

use super::{
    anthropic::AnthropicProvider,
    bedrock::BedrockProvider,
    openai::OpenAiProvider,
    provider::LlmProvider,
};
use crate::permissions::store::CredentialStore;

/// Loads an LLM provider from the database by provider ID.
/// API keys are stored encrypted in llm_providers table using AES-256-GCM.
/// The `model_id` parameter overrides the default model for the provider.
pub async fn load_provider(
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
            let region = base_url.unwrap_or_else(|| "us-east-1".into());
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
