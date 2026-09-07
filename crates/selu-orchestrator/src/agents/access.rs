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
