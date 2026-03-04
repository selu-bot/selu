use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::Value;
use sqlx::SqlitePool;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::capabilities::manifest::{CapabilityManifest, ToolDefinition, ToolSource};
use crate::permissions::CredentialStore;
use crate::state::AgentMap;

use super::CapabilityEngine;

#[derive(Debug, Clone, Deserialize)]
struct DiscoveryTool {
    name: String,
    description: String,
    input_schema: Value,
    #[serde(default)]
    recommended_policy: Option<String>,
}

pub async fn load_discovered_tools(
    db: &SqlitePool,
    agent_id: &str,
    capability_id: &str,
) -> Result<Vec<ToolDefinition>> {
    let rows = sqlx::query(
        "SELECT tool_name, description, input_schema_json, recommended_policy \
         FROM discovered_tools \
         WHERE agent_id = ? AND capability_id = ? \
         ORDER BY tool_name ASC",
    )
    .bind(agent_id)
    .bind(capability_id)
    .fetch_all(db)
    .await?;

    let mut tools = Vec::with_capacity(rows.len());
    for row in rows {
        use sqlx::Row;

        let tool_name: String = row.get("tool_name");
        let description: String = row.get("description");
        let schema_json: String = row.get("input_schema_json");
        let recommended: Option<String> = row.get("recommended_policy");

        let input_schema: Value = match serde_json::from_str(&schema_json) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    agent_id = %agent_id,
                    capability_id = %capability_id,
                    tool_name = %tool_name,
                    "Invalid cached input_schema_json: {e}"
                );
                continue;
            }
        };

        tools.push(ToolDefinition {
            name: tool_name,
            description,
            input_schema,
            requires_confirmation: false,
            recommended_policy: recommended,
        });
    }

    Ok(tools)
}

pub async fn hydrate_dynamic_tools_for_agent(
    db: &SqlitePool,
    agent_id: &str,
    manifests: &mut HashMap<String, CapabilityManifest>,
) -> Result<()> {
    for (cap_id, manifest) in manifests.iter_mut() {
        if manifest.tool_source != ToolSource::Dynamic {
            continue;
        }
        manifest.tools = load_discovered_tools(db, agent_id, cap_id).await?;
    }

    Ok(())
}

pub async fn sync_dynamic_tools_for_agent(
    db: &SqlitePool,
    engine: &CapabilityEngine,
    cred_store: &CredentialStore,
    agent_id: &str,
    manifests: &HashMap<String, CapabilityManifest>,
) {
    for (cap_id, manifest) in manifests {
        if manifest.tool_source != ToolSource::Dynamic {
            continue;
        }

        if let Err(e) = sync_dynamic_tools_for_capability(
            db, engine, cred_store, agent_id, cap_id, manifest, manifests,
        )
        .await
        {
            warn!(
                agent_id = %agent_id,
                capability_id = %cap_id,
                "Dynamic tool sync failed: {e}"
            );
        }
    }
}

pub async fn sync_dynamic_tools_for_all_agents(
    db: &SqlitePool,
    engine: &CapabilityEngine,
    cred_store: &CredentialStore,
    agents: &AgentMap,
) {
    for (agent_id, agent) in agents {
        sync_dynamic_tools_for_agent(
            db,
            engine,
            cred_store,
            agent_id,
            &agent.capability_manifests,
        )
        .await;
    }
}

pub async fn sync_dynamic_tools_for_capability(
    db: &SqlitePool,
    engine: &CapabilityEngine,
    cred_store: &CredentialStore,
    agent_id: &str,
    capability_id: &str,
    manifest: &CapabilityManifest,
    manifests: &HashMap<String, CapabilityManifest>,
) -> Result<()> {
    if manifest.tool_source != ToolSource::Dynamic {
        return Ok(());
    }

    let old_rows = sqlx::query(
        "SELECT tool_name FROM discovered_tools WHERE agent_id = ? AND capability_id = ?",
    )
    .bind(agent_id)
    .bind(capability_id)
    .fetch_all(db)
    .await?;

    let mut old_names = HashSet::new();
    for row in old_rows {
        use sqlx::Row;
        old_names.insert(row.get::<String, _>("tool_name"));
    }

    let mut last_error: Option<anyhow::Error> = None;
    let mut discovered: Option<Vec<DiscoveryTool>> = None;

    for _attempt in 0..2 {
        match discover_tools_once(engine, cred_store, manifest, manifests, capability_id).await {
            Ok(tools) => {
                discovered = Some(tools);
                break;
            }
            Err(e) => {
                last_error = Some(e);
            }
        }
    }

    let discovered = match discovered {
        Some(v) => v,
        None => {
            let err_text = last_error
                .as_ref()
                .map(|e| format!("{e:#}"))
                .unwrap_or_else(|| "unknown discovery error".to_string());
            sqlx::query(
                "INSERT INTO tool_discovery_state (agent_id, capability_id, last_sync_at, last_sync_status, last_sync_error) \
                 VALUES (?, ?, datetime('now'), 'error', ?) \
                 ON CONFLICT(agent_id, capability_id) DO UPDATE SET \
                   last_sync_at = datetime('now'), \
                   last_sync_status = 'error', \
                   last_sync_error = excluded.last_sync_error",
            )
            .bind(agent_id)
            .bind(capability_id)
            .bind(err_text)
            .execute(db)
            .await?;

            return Err(last_error.unwrap_or_else(|| anyhow!("unknown discovery error")));
        }
    };

    let mut seen = HashSet::new();
    for tool in &discovered {
        if !seen.insert(tool.name.clone()) {
            return Err(anyhow!(
                "Discovery payload contains duplicate tool name '{}'",
                tool.name
            ));
        }
    }

    let new_names: HashSet<String> = discovered.iter().map(|t| t.name.clone()).collect();
    let added: Vec<String> = new_names.difference(&old_names).cloned().collect();
    let removed: Vec<String> = old_names.difference(&new_names).cloned().collect();

    let mut tx = db.begin().await?;

    for tool in &discovered {
        let schema_json = serde_json::to_string(&tool.input_schema)?;
        let recommended = normalize_policy(tool.recommended_policy.as_deref());

        sqlx::query(
            "INSERT INTO discovered_tools \
               (agent_id, capability_id, tool_name, description, input_schema_json, recommended_policy, discovered_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, datetime('now'), datetime('now')) \
             ON CONFLICT(agent_id, capability_id, tool_name) DO UPDATE SET \
               description = excluded.description, \
               input_schema_json = excluded.input_schema_json, \
               recommended_policy = excluded.recommended_policy, \
               updated_at = datetime('now')",
        )
        .bind(agent_id)
        .bind(capability_id)
        .bind(&tool.name)
        .bind(&tool.description)
        .bind(schema_json)
        .bind(recommended)
        .execute(&mut *tx)
        .await?;
    }

    for tool_name in &added {
        let policy = discovered
            .iter()
            .find(|t| t.name == *tool_name)
            .and_then(|t| normalize_policy(t.recommended_policy.as_deref()))
            .unwrap_or_else(|| "block".to_string());

        sqlx::query(
            "INSERT INTO global_tool_policies (id, agent_id, capability_id, tool_name, policy) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(agent_id, capability_id, tool_name) DO NOTHING",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(agent_id)
        .bind(capability_id)
        .bind(tool_name)
        .bind(policy)
        .execute(&mut *tx)
        .await?;
    }

    for tool_name in &removed {
        sqlx::query(
            "DELETE FROM global_tool_policies WHERE agent_id = ? AND capability_id = ? AND tool_name = ?",
        )
        .bind(agent_id)
        .bind(capability_id)
        .bind(tool_name)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "DELETE FROM tool_policies WHERE agent_id = ? AND capability_id = ? AND tool_name = ?",
        )
        .bind(agent_id)
        .bind(capability_id)
        .bind(tool_name)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "DELETE FROM discovered_tools WHERE agent_id = ? AND capability_id = ? AND tool_name = ?",
        )
        .bind(agent_id)
        .bind(capability_id)
        .bind(tool_name)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query(
        "INSERT INTO tool_discovery_state (agent_id, capability_id, last_sync_at, last_sync_status, last_sync_error) \
         VALUES (?, ?, datetime('now'), 'ok', NULL) \
         ON CONFLICT(agent_id, capability_id) DO UPDATE SET \
           last_sync_at = datetime('now'), \
           last_sync_status = 'ok', \
           last_sync_error = NULL",
    )
    .bind(agent_id)
    .bind(capability_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    debug!(
        agent_id = %agent_id,
        capability_id = %capability_id,
        added = added.len(),
        removed = removed.len(),
        total = new_names.len(),
        "Dynamic tools reconciled"
    );

    Ok(())
}

async fn discover_tools_once(
    engine: &CapabilityEngine,
    cred_store: &CredentialStore,
    manifest: &CapabilityManifest,
    manifests: &HashMap<String, CapabilityManifest>,
    capability_id: &str,
) -> Result<Vec<DiscoveryTool>> {
    let mut creds = serde_json::Map::new();
    for decl in &manifest.credentials {
        if decl.scope != crate::capabilities::manifest::CredentialScope::System {
            continue;
        }

        let value = cred_store
            .get_system(&manifest.id, &decl.name)
            .await
            .with_context(|| {
                format!(
                    "Failed to read system credential '{}' for capability '{}'",
                    decl.name, manifest.id
                )
            })?;

        if let Some(v) = value {
            creds.insert(decl.name.clone(), Value::String(v));
        } else if decl.required {
            return Err(anyhow!(
                "Missing required system credential '{}' for capability '{}'",
                decl.name,
                manifest.id
            ));
        }
    }

    let session_id = format!("discovery-{}-{}", capability_id, Uuid::new_v4().simple());
    let invoke_result = engine
        .invoke_direct(
            manifests,
            capability_id,
            &manifest.discovery_tool_name,
            serde_json::json!({}),
            Value::Object(creds),
            &session_id,
            "tool-discovery",
        )
        .await;

    engine.close_session(&session_id).await;

    let payload = invoke_result?;
    parse_discovery_payload(&payload)
}

fn parse_discovery_payload(payload: &str) -> Result<Vec<DiscoveryTool>> {
    let value: Value = serde_json::from_str(payload)
        .with_context(|| "Discovery tool returned invalid JSON payload".to_string())?;

    let array = match value {
        Value::Array(a) => a,
        _ => {
            return Err(anyhow!("Discovery payload must be a JSON array of tools"));
        }
    };

    let mut tools = Vec::with_capacity(array.len());
    for item in array {
        let tool: DiscoveryTool = serde_json::from_value(item)
            .with_context(|| "Discovery payload item is invalid".to_string())?;
        if tool.name.trim().is_empty() {
            return Err(anyhow!("Discovery tool contains an empty tool name"));
        }
        tools.push(tool);
    }

    Ok(tools)
}

fn normalize_policy(value: Option<&str>) -> Option<String> {
    match value {
        Some("allow") => Some("allow".to_string()),
        Some("ask") => Some("ask".to_string()),
        Some("block") => Some("block".to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_policy, parse_discovery_payload};

    #[test]
    fn parse_discovery_payload_accepts_array() {
        let payload = r#"
        [
          {
            "name": "saveItem",
            "description": "Save an item",
            "input_schema": { "type": "object", "properties": {} },
            "recommended_policy": "ask"
          }
        ]
        "#;

        let tools = parse_discovery_payload(payload).expect("payload should parse");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "saveItem");
        assert_eq!(tools[0].recommended_policy.as_deref(), Some("ask"));
    }

    #[test]
    fn parse_discovery_payload_rejects_non_array() {
        let payload = r#"{ "tools": [] }"#;
        let err = parse_discovery_payload(payload).expect_err("payload should fail");
        assert!(err.to_string().contains("JSON array"));
    }

    #[test]
    fn normalize_policy_only_accepts_known_values() {
        assert_eq!(normalize_policy(Some("allow")).as_deref(), Some("allow"));
        assert_eq!(normalize_policy(Some("ask")).as_deref(), Some("ask"));
        assert_eq!(normalize_policy(Some("block")).as_deref(), Some("block"));
        assert_eq!(normalize_policy(Some("invalid")), None);
        assert_eq!(normalize_policy(None), None);
    }
}
