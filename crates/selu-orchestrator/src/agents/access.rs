use crate::agents::loader::AgentDefinition;
use std::collections::HashMap;
use std::sync::Arc;

pub const NO_AGENTS_SENTINEL: &str = "__none__";

/// Filter the global agent map to only agents visible to the given user.
///
/// Rules:
/// - If the user has **no rows** in `user_agent_access`, they see **all** agents
///   (backward compatible, zero config).
/// - If any rows exist, only listed agent IDs are visible. A sentinel row
///   (`__none__`) means "no selectable agents" and therefore exposes only
///   the always-available `"default"` fallback.
/// - The `"default"` agent is **always** included (safety fallback).
pub async fn visible_agents(
    db: &sqlx::SqlitePool,
    user_id: &str,
    all_agents: &HashMap<String, Arc<AgentDefinition>>,
) -> HashMap<String, Arc<AgentDefinition>> {
    let rows = sqlx::query_scalar!(
        "SELECT agent_id FROM user_agent_access WHERE user_id = ?",
        user_id,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    if rows.is_empty() {
        return all_agents.clone();
    }

    let mut visible: HashMap<String, Arc<AgentDefinition>> = all_agents
        .iter()
        .filter(|(id, _)| rows.contains(id) || *id == "default")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Ensure "default" is always present
    if let Some(def) = all_agents.get("default") {
        visible
            .entry("default".to_string())
            .or_insert_with(|| def.clone());
    }

    visible
}

/// Replace the set of allowed agents for a user.
/// Storage model:
/// - all selected => no rows (unrestricted)
/// - explicit none selected => sentinel row (`__none__`)
/// - subset selected => one row per selected agent
pub async fn set_user_agents(
    db: &sqlx::SqlitePool,
    user_id: &str,
    agent_ids: &[String],
    all_agent_ids: &[String],
) -> Result<(), sqlx::Error> {
    let all_selected =
        !all_agent_ids.is_empty() && all_agent_ids.iter().all(|id| agent_ids.contains(id));

    let mut tx = db.begin().await?;

    sqlx::query!("DELETE FROM user_agent_access WHERE user_id = ?", user_id)
        .execute(&mut *tx)
        .await?;

    if agent_ids.is_empty() {
        if !all_agent_ids.is_empty() {
            sqlx::query!(
                "INSERT INTO user_agent_access (user_id, agent_id) VALUES (?, ?)",
                user_id,
                NO_AGENTS_SENTINEL,
            )
            .execute(&mut *tx)
            .await?;
        }
    } else if !all_selected {
        for aid in agent_ids {
            sqlx::query!(
                "INSERT INTO user_agent_access (user_id, agent_id) VALUES (?, ?)",
                user_id,
                aid,
            )
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;
    Ok(())
}
