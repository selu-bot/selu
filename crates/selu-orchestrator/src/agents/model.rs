/// Per-agent model resolution with global default fallback.
///
/// Each installed agent can optionally have a provider/model override in the
/// `agents` table. If not set, the global default from `default_model_config`
/// is used.
use anyhow::{Context, Result};
use sqlx::SqlitePool;

/// Resolved model configuration for an agent turn.
pub struct ResolvedModel {
    pub provider_id: String,
    pub model_id: String,
    pub temperature: f32,
}

/// Resolve the model to use for a given agent.
///
/// Precedence:
///   1. Per-agent override in the `agents` table (provider_id IS NOT NULL)
///   2. Global default from `default_model_config`
pub async fn resolve_model(db: &SqlitePool, agent_id: &str) -> Result<ResolvedModel> {
    // 1. Check per-agent override
    let agent_row = sqlx::query_as::<_, (Option<String>, Option<String>, f64)>(
        "SELECT provider_id, model_id, temperature FROM agents WHERE id = ?",
    )
    .bind(agent_id)
    .fetch_optional(db)
    .await
    .context("Failed to query agent model config")?;

    if let Some((Some(provider), Some(model), temp)) = agent_row {
        if !provider.is_empty() && !model.is_empty() {
            return Ok(ResolvedModel {
                provider_id: provider,
                model_id: model,
                temperature: temp as f32,
            });
        }
    }

    // 2. Fall back to global default
    let global = sqlx::query_as::<_, (String, String, f64)>(
        "SELECT provider_id, model_id, temperature FROM default_model_config WHERE id = 'global'",
    )
    .fetch_optional(db)
    .await
    .context("Failed to query global default model config")?;

    match global {
        Some((provider, model, temp)) if !provider.is_empty() && !model.is_empty() => {
            Ok(ResolvedModel {
                provider_id: provider,
                model_id: model,
                temperature: temp as f32,
            })
        }
        _ => Err(anyhow::anyhow!(
            "No model configured for agent '{}' and no global default model set. \
             Please configure a default model in Settings or assign a model to this agent.",
            agent_id
        )),
    }
}

/// Get the current global default model config.
pub async fn get_global_default(db: &SqlitePool) -> Result<Option<ResolvedModel>> {
    let row = sqlx::query_as::<_, (String, String, f64)>(
        "SELECT provider_id, model_id, temperature FROM default_model_config WHERE id = 'global'",
    )
    .fetch_optional(db)
    .await
    .context("Failed to query global default model")?;

    Ok(row
        .filter(|(p, m, _)| !p.is_empty() && !m.is_empty())
        .map(|(provider, model, temp)| ResolvedModel {
            provider_id: provider,
            model_id: model,
            temperature: temp as f32,
        }))
}

/// Set the global default model config.
pub async fn set_global_default(
    db: &SqlitePool,
    provider_id: &str,
    model_id: &str,
    temperature: f32,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO default_model_config (id, provider_id, model_id, temperature, updated_at) \
         VALUES ('global', ?, ?, ?, datetime('now')) \
         ON CONFLICT(id) DO UPDATE SET provider_id = excluded.provider_id, \
         model_id = excluded.model_id, temperature = excluded.temperature, \
         updated_at = excluded.updated_at",
    )
    .bind(provider_id)
    .bind(model_id)
    .bind(temperature as f64)
    .execute(db)
    .await
    .context("Failed to set global default model")?;

    Ok(())
}

/// Set the model override for a specific agent.
pub async fn set_agent_model(
    db: &SqlitePool,
    agent_id: &str,
    provider_id: &str,
    model_id: &str,
    temperature: f32,
) -> Result<()> {
    sqlx::query(
        "UPDATE agents SET provider_id = ?, model_id = ?, temperature = ? WHERE id = ?",
    )
    .bind(provider_id)
    .bind(model_id)
    .bind(temperature as f64)
    .bind(agent_id)
    .execute(db)
    .await
    .context("Failed to set agent model")?;

    Ok(())
}

/// Clear the model override for a specific agent (revert to global default).
#[allow(dead_code)]
pub async fn clear_agent_model(db: &SqlitePool, agent_id: &str) -> Result<()> {
    sqlx::query("UPDATE agents SET provider_id = NULL, model_id = NULL WHERE id = ?")
        .bind(agent_id)
        .execute(db)
        .await
        .context("Failed to clear agent model")?;

    Ok(())
}
