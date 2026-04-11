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

/// Resolved optional image model configuration for an agent turn.
///
/// Image features are optional in Selu. `None` means no image model is
/// configured and image generation/editing should be disabled gracefully.
#[allow(dead_code)]
pub struct ResolvedImageModel {
    pub provider_id: String,
    pub model_id: String,
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

/// Resolve the optional image model to use for a given agent.
///
/// Precedence:
///   1. Per-agent override in the `agents` table (image_provider_id IS NOT NULL)
///   2. Global default from `default_image_model_config`
///   3. None (image features disabled)
pub async fn resolve_image_model(
    db: &SqlitePool,
    agent_id: &str,
) -> Result<Option<ResolvedImageModel>> {
    // 1. Check per-agent override
    let agent_row = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT image_provider_id, image_model_id FROM agents WHERE id = ?",
    )
    .bind(agent_id)
    .fetch_optional(db)
    .await
    .context("Failed to query agent image model config")?;

    if let Some((Some(provider), Some(model))) = agent_row {
        if !provider.is_empty() && !model.is_empty() {
            return Ok(Some(ResolvedImageModel {
                provider_id: provider,
                model_id: model,
            }));
        }
    }

    // 2. Fall back to global default
    let global = sqlx::query_as::<_, (String, String)>(
        "SELECT provider_id, model_id FROM default_image_model_config WHERE id = 'global'",
    )
    .fetch_optional(db)
    .await
    .context("Failed to query global default image model config")?;

    Ok(global
        .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
        .map(|(provider_id, model_id)| ResolvedImageModel {
            provider_id,
            model_id,
        }))
}

/// Get the current global default image model config.
#[allow(dead_code)]
pub async fn get_global_image_default(db: &SqlitePool) -> Result<Option<ResolvedImageModel>> {
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT provider_id, model_id FROM default_image_model_config WHERE id = 'global'",
    )
    .fetch_optional(db)
    .await
    .context("Failed to query global default image model")?;

    Ok(row
        .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
        .map(|(provider_id, model_id)| ResolvedImageModel {
            provider_id,
            model_id,
        }))
}

/// Set the global default image model config.
#[allow(dead_code)]
pub async fn set_global_image_default(
    db: &SqlitePool,
    provider_id: &str,
    model_id: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO default_image_model_config (id, provider_id, model_id, updated_at) \
         VALUES ('global', ?, ?, datetime('now')) \
         ON CONFLICT(id) DO UPDATE SET provider_id = excluded.provider_id, \
         model_id = excluded.model_id, updated_at = excluded.updated_at",
    )
    .bind(provider_id)
    .bind(model_id)
    .execute(db)
    .await
    .context("Failed to set global default image model")?;

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
    sqlx::query("UPDATE agents SET provider_id = ?, model_id = ?, temperature = ? WHERE id = ?")
        .bind(provider_id)
        .bind(model_id)
        .bind(temperature as f64)
        .bind(agent_id)
        .execute(db)
        .await
        .context("Failed to set agent model")?;

    Ok(())
}

/// Set the image model override for a specific agent.
#[allow(dead_code)]
pub async fn set_agent_image_model(
    db: &SqlitePool,
    agent_id: &str,
    provider_id: &str,
    model_id: &str,
) -> Result<()> {
    sqlx::query("UPDATE agents SET image_provider_id = ?, image_model_id = ? WHERE id = ?")
        .bind(provider_id)
        .bind(model_id)
        .bind(agent_id)
        .execute(db)
        .await
        .context("Failed to set agent image model")?;

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

/// Clear the image model override for a specific agent (revert to global
/// optional image default).
#[allow(dead_code)]
pub async fn clear_agent_image_model(db: &SqlitePool, agent_id: &str) -> Result<()> {
    sqlx::query("UPDATE agents SET image_provider_id = NULL, image_model_id = NULL WHERE id = ?")
        .bind(agent_id)
        .execute(db)
        .await
        .context("Failed to clear agent image model")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_test_db() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("connect sqlite memory db");

        sqlx::query(
            "CREATE TABLE agents (
                id TEXT PRIMARY KEY,
                provider_id TEXT,
                model_id TEXT,
                temperature REAL NOT NULL DEFAULT 0.7,
                image_provider_id TEXT,
                image_model_id TEXT
            )",
        )
        .execute(&db)
        .await
        .expect("create agents");

        sqlx::query(
            "CREATE TABLE default_image_model_config (
                id TEXT PRIMARY KEY,
                provider_id TEXT NOT NULL DEFAULT '',
                model_id TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .execute(&db)
        .await
        .expect("create default_image_model_config");

        sqlx::query("INSERT INTO default_image_model_config (id) VALUES ('global')")
            .execute(&db)
            .await
            .expect("seed default_image_model_config");

        db
    }

    #[tokio::test]
    async fn resolve_image_model_prefers_agent_override() {
        let db = setup_test_db().await;
        sqlx::query("INSERT INTO agents (id, image_provider_id, image_model_id) VALUES (?, ?, ?)")
            .bind("agent-1")
            .bind("grok")
            .bind("grok-imagine-fast")
            .execute(&db)
            .await
            .expect("insert agent");

        set_global_image_default(&db, "openai", "gpt-image-1")
            .await
            .expect("set global image default");

        let resolved = resolve_image_model(&db, "agent-1")
            .await
            .expect("resolve image model")
            .expect("some image model");

        assert_eq!(resolved.provider_id, "grok");
        assert_eq!(resolved.model_id, "grok-imagine-fast");
    }

    #[tokio::test]
    async fn resolve_image_model_falls_back_to_global() {
        let db = setup_test_db().await;
        sqlx::query("INSERT INTO agents (id) VALUES (?)")
            .bind("agent-2")
            .execute(&db)
            .await
            .expect("insert agent");

        set_global_image_default(&db, "openai", "gpt-image-1")
            .await
            .expect("set global image default");

        let resolved = resolve_image_model(&db, "agent-2")
            .await
            .expect("resolve image model")
            .expect("some image model");

        assert_eq!(resolved.provider_id, "openai");
        assert_eq!(resolved.model_id, "gpt-image-1");
    }

    #[tokio::test]
    async fn resolve_image_model_returns_none_when_unconfigured() {
        let db = setup_test_db().await;
        sqlx::query("INSERT INTO agents (id) VALUES (?)")
            .bind("agent-3")
            .execute(&db)
            .await
            .expect("insert agent");

        let resolved = resolve_image_model(&db, "agent-3")
            .await
            .expect("resolve image model");

        assert!(resolved.is_none());
    }
}
