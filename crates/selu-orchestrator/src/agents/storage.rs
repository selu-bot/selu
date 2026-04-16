/// Persistent key-value storage for agents, scoped per agent + user.
///
/// Allows agents to store state that survives container restarts, session
/// resets, and orchestrator reboots.  Common use cases include last-sync
/// timestamps, checkpoint data for polling workflows, and user preferences
/// that are agent-specific.
///
/// Storage is exposed to agents via four built-in tools:
///   - `store_get`    — retrieve a value by key
///   - `store_set`    — upsert a key-value pair
///   - `store_delete` — remove a key
///   - `store_list`   — list all stored key-value pairs
///
/// All operations are scoped to `(agent_id, user_id)` — each user's agent
/// state is fully isolated.
use anyhow::Result;
use sqlx::SqlitePool;
use tracing::debug;
use uuid::Uuid;

use crate::llm::provider::ToolSpec;

// ── CRUD operations ──────────────────────────────────────────────────────────

/// Retrieve a single value by key.
///
/// Returns `None` if the key does not exist.
pub async fn get(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
    key: &str,
) -> Result<Option<String>> {
    let row = sqlx::query!(
        "SELECT value FROM agent_storage WHERE agent_id = ? AND user_id = ? AND key = ?",
        agent_id,
        user_id,
        key,
    )
    .fetch_optional(db)
    .await?;

    Ok(row.map(|r| r.value))
}

/// Upsert a key-value pair.
///
/// If the key already exists, its value and `updated_at` timestamp are updated.
pub async fn set(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
    key: &str,
    value: &str,
) -> Result<()> {
    let id = Uuid::new_v4().to_string();
    sqlx::query!(
        "INSERT INTO agent_storage (id, agent_id, user_id, key, value)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(agent_id, user_id, key)
         DO UPDATE SET value = excluded.value, updated_at = datetime('now')",
        id,
        agent_id,
        user_id,
        key,
        value,
    )
    .execute(db)
    .await?;

    debug!(agent_id = %agent_id, key = %key, "Agent storage: set");
    Ok(())
}

/// Delete a key.
///
/// Returns `true` if the key existed and was deleted, `false` if it was not found.
pub async fn delete(db: &SqlitePool, agent_id: &str, user_id: &str, key: &str) -> Result<bool> {
    let result = sqlx::query!(
        "DELETE FROM agent_storage WHERE agent_id = ? AND user_id = ? AND key = ?",
        agent_id,
        user_id,
        key,
    )
    .execute(db)
    .await?;

    let deleted = result.rows_affected() > 0;
    debug!(agent_id = %agent_id, key = %key, deleted = deleted, "Agent storage: delete");
    Ok(deleted)
}

/// List all key-value pairs for an agent + user.
///
/// Returns a `serde_json::Map` for direct JSON serialisation.
pub async fn list(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
) -> Result<(serde_json::Map<String, serde_json::Value>, bool)> {
    const LIMIT: usize = 100;

    let rows = sqlx::query!(
        "SELECT key, value FROM agent_storage WHERE agent_id = ? AND user_id = ? ORDER BY key LIMIT ?",
        agent_id,
        user_id,
        // Fetch one extra row to detect whether more entries exist.
        (LIMIT + 1) as i64,
    )
    .fetch_all(db)
    .await?;

    let truncated = rows.len() > LIMIT;
    let mut map = serde_json::Map::new();
    for row in rows.into_iter().take(LIMIT) {
        map.insert(row.key, serde_json::Value::String(row.value));
    }
    Ok((map, truncated))
}

/// Delete all storage for a given agent (called on agent uninstall).
pub async fn delete_all_for_agent(db: &SqlitePool, agent_id: &str) -> Result<()> {
    sqlx::query!("DELETE FROM agent_storage WHERE agent_id = ?", agent_id)
        .execute(db)
        .await?;

    debug!(agent_id = %agent_id, "Agent storage: deleted all entries for agent");
    Ok(())
}

// ── Built-in tool specs ──────────────────────────────────────────────────────

/// Tool spec for `store_get` — retrieve a stored value by key.
pub fn store_get_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "store_get".to_string(),
        description: "Retrieve a previously stored value by key. \
             Use this to recall persistent state such as last sync timestamps, \
             bookmarks, or preferences that were saved with store_set."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key to look up"
                }
            },
            "required": ["key"]
        }),
    }
}

/// Tool spec for `store_set` — store a key-value pair persistently.
pub fn store_set_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "store_set".to_string(),
        description: "Store a key-value pair persistently. The value survives across \
             sessions, container restarts, and reboots. Use this to save sync \
             checkpoints, preferences, or any state you need to recall later."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key to store under"
                },
                "value": {
                    "type": "string",
                    "description": "The value to store (must be a string; serialise complex data as JSON)"
                }
            },
            "required": ["key", "value"]
        }),
    }
}

/// Tool spec for `store_delete` — delete a stored key.
pub fn store_delete_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "store_delete".to_string(),
        description: "Delete a previously stored key-value pair. \
             Returns whether the key existed."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key to delete"
                }
            },
            "required": ["key"]
        }),
    }
}

/// Tool spec for `store_list` — list all stored key-value pairs.
pub fn store_list_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "store_list".to_string(),
        description: "List persistently stored key-value pairs (max 100). \
             Returns a JSON object with an \"entries\" map and a \"truncated\" flag. \
             If truncated is true, use store_get to fetch specific keys. \
             Returns an empty object if nothing has been stored yet."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
    }
}

// ── Dispatch handlers ────────────────────────────────────────────────────────

/// Handle a `store_get` tool call.
pub async fn dispatch_store_get(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
    args: &serde_json::Value,
) -> Result<String> {
    let key = args["key"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("store_get: missing 'key' string"))?;

    match get(db, agent_id, user_id, key).await? {
        Some(value) => Ok(serde_json::json!({
            "found": true,
            "key": key,
            "value": value,
        })
        .to_string()),
        None => Ok(serde_json::json!({
            "found": false,
            "key": key,
        })
        .to_string()),
    }
}

/// Handle a `store_set` tool call.
pub async fn dispatch_store_set(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
    args: &serde_json::Value,
) -> Result<String> {
    let key = args["key"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("store_set: missing 'key' string"))?;
    let value = args["value"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("store_set: missing 'value' string"))?;

    set(db, agent_id, user_id, key, value).await?;
    Ok(serde_json::json!({"ok": true, "key": key}).to_string())
}

/// Handle a `store_delete` tool call.
pub async fn dispatch_store_delete(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
    args: &serde_json::Value,
) -> Result<String> {
    let key = args["key"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("store_delete: missing 'key' string"))?;

    let deleted = delete(db, agent_id, user_id, key).await?;
    Ok(serde_json::json!({"ok": true, "deleted": deleted}).to_string())
}

/// Handle a `store_list` tool call.
pub async fn dispatch_store_list(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
    _args: &serde_json::Value,
) -> Result<String> {
    let (entries, truncated) = list(db, agent_id, user_id).await?;
    Ok(serde_json::json!({"entries": entries, "truncated": truncated}).to_string())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE agent_storage (
                id         TEXT PRIMARY KEY NOT NULL,
                agent_id   TEXT NOT NULL,
                user_id    TEXT NOT NULL,
                key        TEXT NOT NULL,
                value      TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(agent_id, user_id, key)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn test_set_and_get() {
        let db = test_db().await;
        set(&db, "agent1", "user1", "last_sync", "2026-03-01T00:00:00Z")
            .await
            .unwrap();
        let val = get(&db, "agent1", "user1", "last_sync").await.unwrap();
        assert_eq!(val, Some("2026-03-01T00:00:00Z".to_string()));
    }

    #[tokio::test]
    async fn test_get_missing_key() {
        let db = test_db().await;
        let val = get(&db, "agent1", "user1", "nonexistent").await.unwrap();
        assert_eq!(val, None);
    }

    #[tokio::test]
    async fn test_upsert() {
        let db = test_db().await;
        set(&db, "agent1", "user1", "key1", "value1").await.unwrap();
        set(&db, "agent1", "user1", "key1", "value2").await.unwrap();
        let val = get(&db, "agent1", "user1", "key1").await.unwrap();
        assert_eq!(val, Some("value2".to_string()));
    }

    #[tokio::test]
    async fn test_isolation_between_agents() {
        let db = test_db().await;
        set(&db, "agent1", "user1", "key", "from_agent1")
            .await
            .unwrap();
        set(&db, "agent2", "user1", "key", "from_agent2")
            .await
            .unwrap();
        assert_eq!(
            get(&db, "agent1", "user1", "key").await.unwrap(),
            Some("from_agent1".to_string())
        );
        assert_eq!(
            get(&db, "agent2", "user1", "key").await.unwrap(),
            Some("from_agent2".to_string())
        );
    }

    #[tokio::test]
    async fn test_isolation_between_users() {
        let db = test_db().await;
        set(&db, "agent1", "user1", "key", "from_user1")
            .await
            .unwrap();
        set(&db, "agent1", "user2", "key", "from_user2")
            .await
            .unwrap();
        assert_eq!(
            get(&db, "agent1", "user1", "key").await.unwrap(),
            Some("from_user1".to_string())
        );
        assert_eq!(
            get(&db, "agent1", "user2", "key").await.unwrap(),
            Some("from_user2".to_string())
        );
    }

    #[tokio::test]
    async fn test_delete() {
        let db = test_db().await;
        set(&db, "agent1", "user1", "key", "value").await.unwrap();
        let deleted = delete(&db, "agent1", "user1", "key").await.unwrap();
        assert!(deleted);
        let val = get(&db, "agent1", "user1", "key").await.unwrap();
        assert_eq!(val, None);
    }

    #[tokio::test]
    async fn test_delete_nonexistent() {
        let db = test_db().await;
        let deleted = delete(&db, "agent1", "user1", "key").await.unwrap();
        assert!(!deleted);
    }

    #[tokio::test]
    async fn test_list() {
        let db = test_db().await;
        set(&db, "agent1", "user1", "alpha", "1").await.unwrap();
        set(&db, "agent1", "user1", "beta", "2").await.unwrap();
        set(&db, "agent1", "user1", "gamma", "3").await.unwrap();

        let (entries, truncated) = list(&db, "agent1", "user1").await.unwrap();
        assert_eq!(entries.len(), 3);
        assert!(!truncated);
        assert_eq!(entries["alpha"], "1");
        assert_eq!(entries["beta"], "2");
        assert_eq!(entries["gamma"], "3");
    }

    #[tokio::test]
    async fn test_list_empty() {
        let db = test_db().await;
        let (entries, truncated) = list(&db, "agent1", "user1").await.unwrap();
        assert!(entries.is_empty());
        assert!(!truncated);
    }

    #[tokio::test]
    async fn test_list_truncation() {
        let db = test_db().await;
        for i in 0..105 {
            set(&db, "agent1", "user1", &format!("key_{i:03}"), &format!("val_{i}"))
                .await
                .unwrap();
        }
        let (entries, truncated) = list(&db, "agent1", "user1").await.unwrap();
        assert_eq!(entries.len(), 100);
        assert!(truncated);
    }

    #[tokio::test]
    async fn test_delete_all_for_agent() {
        let db = test_db().await;
        set(&db, "agent1", "user1", "k1", "v1").await.unwrap();
        set(&db, "agent1", "user2", "k2", "v2").await.unwrap();
        set(&db, "agent2", "user1", "k3", "v3").await.unwrap();

        delete_all_for_agent(&db, "agent1").await.unwrap();

        assert_eq!(get(&db, "agent1", "user1", "k1").await.unwrap(), None);
        assert_eq!(get(&db, "agent1", "user2", "k2").await.unwrap(), None);
        // agent2 data should be untouched
        assert_eq!(
            get(&db, "agent2", "user1", "k3").await.unwrap(),
            Some("v3".to_string())
        );
    }
}
