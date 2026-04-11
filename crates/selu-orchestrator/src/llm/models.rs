/// Model catalogue for LLM providers.
///
/// Fetches available models dynamically from each provider's list-models API.
/// Falls back to a static catalogue when the API call fails (no key, network
/// error, etc.).
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::{debug, warn};

use crate::permissions::store::CredentialStore;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Fetch available models for a provider.
///
/// Tries the provider's live API first. Falls back to the static catalogue on
/// any error (missing key, network failure, unexpected response, etc.).
pub async fn list_models(
    db: &SqlitePool,
    cred_store: &CredentialStore,
    provider_id: &str,
) -> Vec<ModelInfo> {
    match fetch_live(db, cred_store, provider_id).await {
        Ok(models) if !models.is_empty() => {
            debug!(
                provider = provider_id,
                count = models.len(),
                "Fetched models from live API"
            );
            models
        }
        Ok(_) => {
            warn!(
                provider = provider_id,
                "Live API returned no models, using static fallback"
            );
            static_models(provider_id)
        }
        Err(e) => {
            warn!(provider = provider_id, error = %e, "Live model fetch failed, using static fallback");
            static_models(provider_id)
        }
    }
}

/// Return the static fallback model list for a provider.
///
/// Used both as fallback when live API fails and for resolving display names
/// without making API calls (e.g. in page rendering).
pub fn static_fallback(provider_id: &str) -> Vec<ModelInfo> {
    static_models(provider_id)
}

// ── Live fetching ─────────────────────────────────────────────────────────────

async fn fetch_live(
    db: &SqlitePool,
    cred_store: &CredentialStore,
    provider_id: &str,
) -> Result<Vec<ModelInfo>> {
    let row = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT api_key_encrypted, base_url FROM llm_providers WHERE id = ? AND active = 1",
    )
    .bind(provider_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found or inactive", provider_id))?;

    let api_key = match row.0 {
        Some(ref encrypted) if !encrypted.is_empty() => {
            let bytes = cred_store.decrypt_raw(encrypted)?;
            String::from_utf8(bytes)?
        }
        _ => String::new(),
    };

    let base_url = row.1.unwrap_or_default();

    match provider_id {
        "openai" => fetch_openai(&api_key, &base_url).await,
        "grok" => fetch_grok(&api_key, &base_url).await,
        "anthropic" => fetch_anthropic(&api_key).await,
        "bedrock" => fetch_bedrock(&api_key, &base_url).await,
        "pico" => fetch_pico(&base_url).await,
        _ => Ok(vec![]),
    }
}

/// OpenAI: GET /v1/models
async fn fetch_openai(api_key: &str, base_url: &str) -> Result<Vec<ModelInfo>> {
    if api_key.is_empty() {
        return Err(anyhow::anyhow!("No API key configured for OpenAI"));
    }

    let url = if base_url.is_empty() {
        "https://api.openai.com"
    } else {
        base_url.trim_end_matches('/')
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let resp = client
        .get(format!("{url}/v1/models"))
        .bearer_auth(api_key)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "OpenAI /v1/models returned HTTP {status}: {body}"
        ));
    }

    #[derive(Deserialize)]
    struct ListResp {
        data: Vec<OaiModel>,
    }
    #[derive(Deserialize)]
    struct OaiModel {
        id: String,
    }

    let body: ListResp = resp.json().await?;

    // Exclude non-chat models by filtering OUT known non-chat prefixes.
    // This is more future-proof than whitelisting — new model families
    // (gpt-5, o5, etc.) will be included automatically.
    let exclude_prefixes = [
        "text-embedding",
        "text-search",
        "text-similarity",
        "text-moderation",
        "dall-e",
        "whisper",
        "tts",
        "babbage",
        "davinci",
        "curie",
        "ada",
        "code-",
        "text-davinci",
        "text-curie",
        "text-babbage",
        "text-ada",
        "moderation",
        "canary-",
        "codex-",
    ];

    let exclude_contains = [
        "realtime",
        "audio",
        "search",
        "transcribe",
        "tts",
        "embedding",
        "moderation",
        "-instruct",
    ];

    let mut models: Vec<ModelInfo> = body
        .data
        .into_iter()
        .filter(|m| !exclude_prefixes.iter().any(|p| m.id.starts_with(p)))
        .filter(|m| !exclude_contains.iter().any(|c| m.id.contains(c)))
        .map(|m| {
            let name = humanize_openai_id(&m.id);
            ModelInfo { id: m.id, name }
        })
        .collect();

    models.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(models)
}

/// xAI Grok: GET /v1/models (OpenAI-compatible)
async fn fetch_grok(api_key: &str, base_url: &str) -> Result<Vec<ModelInfo>> {
    if api_key.is_empty() {
        return Err(anyhow::anyhow!("No API key configured for Grok"));
    }

    let url = if base_url.is_empty() {
        "https://api.x.ai"
    } else {
        base_url.trim_end_matches('/')
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let resp = client
        .get(format!("{url}/v1/models"))
        .bearer_auth(api_key)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "Grok /v1/models returned HTTP {status}: {body}"
        ));
    }

    #[derive(Deserialize)]
    struct ListResp {
        data: Vec<GrokModel>,
    }
    #[derive(Deserialize)]
    struct GrokModel {
        id: String,
    }

    let body: ListResp = resp.json().await?;

    let exclude_contains = ["audio", "embedding", "moderation"];
    let mut models: Vec<ModelInfo> = body
        .data
        .into_iter()
        .filter(|m| !exclude_contains.iter().any(|c| m.id.contains(c)))
        .map(|m| ModelInfo {
            name: humanize_grok_id(&m.id),
            id: m.id,
        })
        .collect();

    models.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(models)
}

/// Anthropic: GET /v1/models (paginated)
async fn fetch_anthropic(api_key: &str) -> Result<Vec<ModelInfo>> {
    if api_key.is_empty() {
        return Err(anyhow::anyhow!("No API key configured for Anthropic"));
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let mut all_models = Vec::new();
    let mut after_id: Option<String> = None;

    // Paginate through all results
    loop {
        let mut url = "https://api.anthropic.com/v1/models?limit=100".to_string();
        if let Some(ref cursor) = after_id {
            url.push_str(&format!("&after_id={cursor}"));
        }

        let resp = client
            .get(&url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Anthropic /v1/models returned HTTP {status}: {body}"
            ));
        }

        #[derive(Deserialize)]
        struct ListResp {
            data: Vec<AnthModel>,
            has_more: Option<bool>,
            last_id: Option<String>,
        }
        #[derive(Deserialize)]
        struct AnthModel {
            id: String,
            display_name: Option<String>,
        }

        let body: ListResp = resp.json().await?;
        let has_more = body.has_more.unwrap_or(false);

        for m in body.data {
            let name = m
                .display_name
                .unwrap_or_else(|| humanize_anthropic_id(&m.id));
            all_models.push(ModelInfo { id: m.id, name });
        }

        if has_more {
            after_id = body.last_id;
        } else {
            break;
        }
    }

    all_models.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(all_models)
}

/// Bedrock: fetch cross-region inference profiles + foundation models.
///
/// The cross-region inference profiles (`us.anthropic.claude-...`) are what Selu
/// actually uses in the Converse API. The `/foundation-models` endpoint only
/// returns base model IDs without the region prefix and misses newer models
/// that are only available as inference profiles.
///
/// We call both endpoints and merge, preferring inference profiles.
async fn fetch_bedrock(api_key: &str, region: &str) -> Result<Vec<ModelInfo>> {
    let region = if region.is_empty() {
        "us-east-1"
    } else {
        region
    };

    if api_key.is_empty() {
        return Err(anyhow::anyhow!("No API key / token configured for Bedrock"));
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let mut models: Vec<ModelInfo> = Vec::new();

    // 1. Fetch inference profiles (cross-region model IDs like us.anthropic.claude-...)
    let profiles_url = format!("https://bedrock.{region}.amazonaws.com/inference-profiles");

    match client.get(&profiles_url).bearer_auth(api_key).send().await {
        Ok(resp) if resp.status().is_success() => {
            #[derive(Deserialize)]
            struct ProfilesResp {
                #[serde(rename = "inferenceProfileSummaries", default)]
                summaries: Vec<ProfileSummary>,
            }
            #[derive(Deserialize)]
            struct ProfileSummary {
                #[serde(rename = "inferenceProfileId")]
                id: String,
                #[serde(rename = "inferenceProfileName")]
                name: Option<String>,
                #[serde(rename = "status", default)]
                status: Option<String>,
            }

            if let Ok(body) = resp.json::<ProfilesResp>().await {
                for p in body.summaries {
                    // Only include active system-defined profiles
                    let is_active = p.status.as_deref() == Some("ACTIVE") || p.status.is_none();
                    if !is_active {
                        continue;
                    }
                    let name = p.name.unwrap_or_else(|| humanize_bedrock_profile_id(&p.id));
                    models.push(ModelInfo { id: p.id, name });
                }
            }
        }
        Ok(resp) => {
            let status = resp.status();
            debug!("Bedrock /inference-profiles returned HTTP {status}, skipping");
        }
        Err(e) => {
            debug!("Bedrock /inference-profiles request failed: {e}, skipping");
        }
    }

    // 2. Fetch foundation models as a fallback / supplement
    let fm_url = format!("https://bedrock.{region}.amazonaws.com/foundation-models");

    match client.get(&fm_url).bearer_auth(api_key).send().await {
        Ok(resp) if resp.status().is_success() => {
            #[derive(Deserialize)]
            struct FmResp {
                #[serde(rename = "modelSummaries", default)]
                model_summaries: Vec<FmModel>,
            }
            #[derive(Deserialize)]
            struct FmModel {
                #[serde(rename = "modelId")]
                model_id: String,
                #[serde(rename = "modelName")]
                model_name: Option<String>,
                #[serde(rename = "providerName")]
                provider_name: Option<String>,
                #[serde(rename = "outputModalities", default)]
                output_modalities: Vec<String>,
                #[serde(rename = "inferenceTypesSupported", default)]
                inference_types: Vec<String>,
            }

            if let Ok(body) = resp.json::<FmResp>().await {
                // Only add foundation models that aren't already covered by
                // an inference profile (avoid duplicates)
                let existing_ids: std::collections::HashSet<String> =
                    models.iter().map(|m| m.id.clone()).collect();

                for m in body.model_summaries {
                    if !m.output_modalities.iter().any(|o| o == "TEXT") {
                        continue;
                    }
                    if !m.inference_types.iter().any(|t| t == "ON_DEMAND") {
                        continue;
                    }
                    if existing_ids.contains(m.model_id.as_str()) {
                        continue;
                    }
                    // Also skip if a cross-region variant already exists
                    // (e.g. skip "anthropic.claude-..." if "us.anthropic.claude-..." is present)
                    let has_profile = existing_ids.iter().any(|id| id.ends_with(&m.model_id));
                    if has_profile {
                        continue;
                    }
                    let name = match (&m.model_name, &m.provider_name) {
                        (Some(n), Some(p)) => format!("{p} — {n}"),
                        (Some(n), None) => n.clone(),
                        _ => m.model_id.clone(),
                    };
                    models.push(ModelInfo {
                        id: m.model_id,
                        name,
                    });
                }
            }
        }
        Ok(resp) => {
            let status = resp.status();
            debug!("Bedrock /foundation-models returned HTTP {status}, skipping");
        }
        Err(e) => {
            debug!("Bedrock /foundation-models request failed: {e}, skipping");
        }
    }

    if models.is_empty() {
        return Err(anyhow::anyhow!(
            "Both Bedrock endpoints returned no usable models"
        ));
    }

    models.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(models)
}

/// Pico AI Server: GET /v1/models (OpenAI-compatible)
async fn fetch_pico(base_url: &str) -> Result<Vec<ModelInfo>> {
    if base_url.is_empty() {
        return Err(anyhow::anyhow!("No base URL configured for Pico AI Server"));
    }

    let url = base_url.trim_end_matches('/');

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let resp = client.get(format!("{url}/v1/models")).send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "Pico /v1/models returned HTTP {status}: {body}"
        ));
    }

    #[derive(Deserialize)]
    struct ListResp {
        data: Vec<PicoModel>,
    }
    #[derive(Deserialize)]
    struct PicoModel {
        id: String,
    }

    let body: ListResp = resp.json().await?;

    let models: Vec<ModelInfo> = body
        .data
        .into_iter()
        .map(|m| {
            let name = m.id.clone();
            ModelInfo { id: m.id, name }
        })
        .collect();

    Ok(models)
}

// ── Static fallback ───────────────────────────────────────────────────────────

fn static_models(provider_id: &str) -> Vec<ModelInfo> {
    match provider_id {
        "anthropic" => vec![
            ModelInfo {
                id: "claude-opus-4-20250514".into(),
                name: "Claude Opus 4".into(),
            },
            ModelInfo {
                id: "claude-sonnet-4-20250514".into(),
                name: "Claude Sonnet 4".into(),
            },
            ModelInfo {
                id: "claude-haiku-3-5-20241022".into(),
                name: "Claude 3.5 Haiku".into(),
            },
        ],
        "openai" => vec![
            ModelInfo {
                id: "gpt-4o".into(),
                name: "GPT-4o".into(),
            },
            ModelInfo {
                id: "gpt-4o-mini".into(),
                name: "GPT-4o Mini".into(),
            },
            ModelInfo {
                id: "o3".into(),
                name: "o3".into(),
            },
            ModelInfo {
                id: "o3-mini".into(),
                name: "o3 Mini".into(),
            },
            ModelInfo {
                id: "o4-mini".into(),
                name: "o4 Mini".into(),
            },
        ],
        "grok" => vec![
            ModelInfo {
                id: "grok-3".into(),
                name: "Grok 3".into(),
            },
            ModelInfo {
                id: "grok-3-mini".into(),
                name: "Grok 3 Mini".into(),
            },
            ModelInfo {
                id: "grok-2".into(),
                name: "Grok 2".into(),
            },
        ],
        "bedrock" => vec![
            ModelInfo {
                id: "us.anthropic.claude-opus-4-20250514-v1:0".into(),
                name: "Claude Opus 4".into(),
            },
            ModelInfo {
                id: "us.anthropic.claude-sonnet-4-20250514-v1:0".into(),
                name: "Claude Sonnet 4".into(),
            },
            ModelInfo {
                id: "us.anthropic.claude-haiku-3-5-20241022-v1:0".into(),
                name: "Claude 3.5 Haiku".into(),
            },
        ],
        _ => vec![],
    }
}

// ── Humanization helpers ──────────────────────────────────────────────────────

fn humanize_openai_id(id: &str) -> String {
    // Return something readable from model IDs like "gpt-4o-2024-08-06"
    // or "o3-mini-2025-01-31"
    // Strategy: capitalize known prefixes, strip date suffixes if present
    let id_lower = id.to_lowercase();

    // Strip date suffix (e.g. "-2024-08-06")
    let base = strip_date_suffix(id);

    // Known mappings for clean names
    let name = base.replace("gpt-", "GPT-").replace("chatgpt-", "ChatGPT-");

    // If it starts with "o" followed by a digit, capitalize (o3 → O3)
    if name.starts_with('o') && name.chars().nth(1).map_or(false, |c| c.is_ascii_digit()) {
        return format!("O{}", &name[1..]);
    }

    // If still lowercase, title-case it
    if name == id_lower {
        return title_case(&name);
    }

    name
}

fn humanize_grok_id(id: &str) -> String {
    title_case(strip_date_suffix(id))
}

fn humanize_anthropic_id(id: &str) -> String {
    // "claude-sonnet-4-20250514" → "Claude Sonnet 4"
    let base = strip_date_suffix(id);
    title_case(base)
}

/// Humanize a Bedrock inference profile ID.
/// "us.anthropic.claude-opus-4-20250514-v1:0" → "Claude Opus 4"
fn humanize_bedrock_profile_id(id: &str) -> String {
    // Strip region + provider prefix: "us.anthropic." / "eu.anthropic." / "anthropic."
    let stripped = id
        .strip_prefix("us.")
        .or_else(|| id.strip_prefix("eu."))
        .or_else(|| id.strip_prefix("ap."))
        .unwrap_or(id);
    let stripped = stripped
        .strip_prefix("anthropic.")
        .or_else(|| stripped.strip_prefix("meta."))
        .or_else(|| stripped.strip_prefix("amazon."))
        .or_else(|| stripped.strip_prefix("mistral."))
        .or_else(|| stripped.strip_prefix("cohere."))
        .or_else(|| stripped.strip_prefix("ai21."))
        .unwrap_or(stripped);

    // Strip version suffix: "-v1:0", "-v2:0", etc.
    let without_version = if let Some(pos) = stripped.find("-v") {
        let after = &stripped[pos + 2..];
        if after.contains(':') || after.chars().next().map_or(false, |c| c.is_ascii_digit()) {
            &stripped[..pos]
        } else {
            stripped
        }
    } else {
        stripped
    };

    // Strip date suffix and title-case
    let base = strip_date_suffix(without_version);
    title_case(base)
}

/// Strip a trailing date component like "-20250514" or "-2025-01-31"
fn strip_date_suffix(id: &str) -> &str {
    // Try YYYYMMDD (8 digits after last dash)
    if let Some((prefix, suffix)) = id.rsplit_once('-') {
        if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_digit()) {
            return prefix;
        }
        // Try YYYY-MM-DD (check if last 3 segments are a date)
        // e.g. "gpt-4o-2024-08-06" → strip "-2024-08-06"
        if suffix.len() == 2 && suffix.chars().all(|c| c.is_ascii_digit()) {
            if let Some((prefix2, mm)) = prefix.rsplit_once('-') {
                if mm.len() == 2 && mm.chars().all(|c| c.is_ascii_digit()) {
                    if let Some((prefix3, yyyy)) = prefix2.rsplit_once('-') {
                        if yyyy.len() == 4 && yyyy.chars().all(|c| c.is_ascii_digit()) {
                            return prefix3;
                        }
                    }
                }
            }
        }
    }
    id
}

fn title_case(s: &str) -> String {
    s.split('-')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => format!("{}{}", f.to_uppercase(), c.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
