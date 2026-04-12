/// Agent memory: persistent per-agent, per-user notes that are useful for
/// future interactions but are not personality facts.
///
/// Memories are indexed with SQLite FTS5 and retrieved with BM25 ranking.
use anyhow::{Context, Result};
use sqlx::SqlitePool;
use tracing::debug;
use uuid::Uuid;

use crate::llm::provider::ToolSpec;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AgentMemory {
    pub id: String,
    pub agent_id: String,
    pub user_id: String,
    pub memory: String,
    pub tags: String,
    pub source: String,
    pub category: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct AgentMemoryHit {
    pub id: String,
    pub memory: String,
    pub tags: String,
    pub source: String,
    pub score: f64,
    pub updated_at: String,
}

pub async fn add_memory(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
    memory: &str,
    tags: &str,
    source: &str,
    category: &str,
) -> Result<String> {
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO agent_memories (id, agent_id, user_id, memory_text, tags, source, category) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(agent_id)
    .bind(user_id)
    .bind(memory)
    .bind(tags)
    .bind(source)
    .bind(category)
    .execute(db)
    .await
    .context("Failed to add agent memory")?;

    debug!(agent_id = %agent_id, user_id = %user_id, "Added agent memory");
    Ok(id)
}

pub async fn delete_memory(db: &SqlitePool, user_id: &str, memory_id: &str) -> Result<bool> {
    let result = sqlx::query("DELETE FROM agent_memories WHERE id = ? AND user_id = ?")
        .bind(memory_id)
        .bind(user_id)
        .execute(db)
        .await
        .context("Failed to delete agent memory")?;

    Ok(result.rows_affected() > 0)
}

pub async fn list_memories(db: &SqlitePool, user_id: &str, limit: i64) -> Result<Vec<AgentMemory>> {
    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
        ),
    >(
        r#"SELECT id, agent_id, user_id, memory_text, tags, source, category, created_at, updated_at
           FROM agent_memories
           WHERE user_id = ?
           ORDER BY updated_at DESC, created_at DESC
           LIMIT ?"#,
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(db)
    .await
    .context("Failed to list agent memories")?;

    Ok(rows
        .into_iter()
        .map(
            |(id, agent_id, user_id, memory, tags, source, category, created_at, updated_at)| {
                AgentMemory {
                    id,
                    agent_id,
                    user_id,
                    memory,
                    tags,
                    source,
                    category,
                    created_at,
                    updated_at,
                }
            },
        )
        .collect())
}

fn normalize_fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split_whitespace()
        .map(|term| {
            term.chars()
                .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect::<String>()
        })
        .filter(|term| !term.is_empty())
        .take(8)
        .collect();

    if terms.is_empty() {
        return None;
    }

    Some(
        terms
            .iter()
            .map(|term| format!("\"{}\"", term.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" AND "),
    )
}

pub async fn search_memories(
    db: &SqlitePool,
    user_id: &str,
    query: &str,
    limit: i64,
) -> Result<Vec<AgentMemoryHit>> {
    let Some(fts_query) = normalize_fts_query(query) else {
        return Ok(Vec::new());
    };

    let rows = sqlx::query_as::<_, (String, String, String, String, Option<f64>, String)>(
        r#"SELECT m.id, m.memory_text, m.tags, m.source,
                  bm25(agent_memories_fts, 1.0, 0.35) AS score, m.updated_at
           FROM agent_memories_fts
           JOIN agent_memories m ON m.rowid = agent_memories_fts.rowid
           WHERE m.user_id = ?
             AND agent_memories_fts MATCH ?
           ORDER BY score ASC
           LIMIT ?"#,
    )
    .bind(user_id)
    .bind(fts_query)
    .bind(limit)
    .fetch_all(db)
    .await
    .context("Failed to search agent memories")?;

    Ok(rows
        .into_iter()
        .map(
            |(id, memory, tags, source, score, updated_at)| AgentMemoryHit {
                id,
                memory,
                tags,
                source,
                score: score.unwrap_or(0.0),
                updated_at,
            },
        )
        .collect())
}

pub fn remember_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "memory_remember".to_string(),
        description: "Save a short memory about this user that will be available in all \
             future conversations, across all agents. Use this for stable context, \
             recurring preferences, and project details. Do not store secrets."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "memory": {
                    "type": "string",
                    "description": "A concise memory to store"
                },
                "tags": {
                    "type": "string",
                    "description": "Optional comma-separated tags"
                }
            },
            "required": ["memory"]
        }),
    }
}

pub fn forget_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "memory_forget".to_string(),
        description: "Delete a previously stored memory by its id.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "memory_id": {
                    "type": "string",
                    "description": "The memory id to delete"
                }
            },
            "required": ["memory_id"]
        }),
    }
}

pub fn search_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "memory_search".to_string(),
        description: "Search agent-saved notes and operational knowledge using keyword \
             relevance (BM25). Use this for recalling things you or other agents have \
             noted — NOT for user profile facts (those are always available automatically)."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What to search for"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (1-10)",
                    "minimum": 1,
                    "maximum": 10
                }
            },
            "required": ["query"]
        }),
    }
}

pub fn list_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "memory_list".to_string(),
        description: "List recently stored memories for this user (shared across all agents)."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (1-50)",
                    "minimum": 1,
                    "maximum": 50
                }
            },
            "required": []
        }),
    }
}

pub async fn dispatch_remember(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
    args: &serde_json::Value,
) -> Result<String> {
    let memory = args["memory"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("memory_remember: missing 'memory' string"))?
        .trim();

    if memory.is_empty() {
        return Ok(serde_json::json!({"ok": false, "error": "empty_memory"}).to_string());
    }

    let tags = args["tags"].as_str().unwrap_or("").trim();
    let id = add_memory(db, agent_id, user_id, memory, tags, "agent", "").await?;

    Ok(serde_json::json!({"ok": true, "memory_id": id}).to_string())
}

pub async fn dispatch_forget(
    db: &SqlitePool,
    user_id: &str,
    args: &serde_json::Value,
) -> Result<String> {
    let memory_id = args["memory_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("memory_forget: missing 'memory_id' string"))?;

    let deleted = delete_memory(db, user_id, memory_id).await?;
    Ok(serde_json::json!({"ok": true, "deleted": deleted}).to_string())
}

pub async fn dispatch_search(
    db: &SqlitePool,
    user_id: &str,
    args: &serde_json::Value,
) -> Result<String> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("memory_search: missing 'query' string"))?;

    let limit = args["limit"].as_i64().unwrap_or(5).clamp(1, 10);
    let hits = search_memories(db, user_id, query, limit).await?;

    let entries: Vec<serde_json::Value> = hits
        .into_iter()
        .map(|h| {
            serde_json::json!({
                "id": h.id,
                "memory": h.memory,
                "tags": h.tags,
                "source": h.source,
                "score": h.score,
                "updated_at": h.updated_at,
            })
        })
        .collect();

    Ok(serde_json::json!({"entries": entries}).to_string())
}

pub async fn dispatch_list(
    db: &SqlitePool,
    user_id: &str,
    args: &serde_json::Value,
) -> Result<String> {
    let limit = args["limit"].as_i64().unwrap_or(20).clamp(1, 50);
    let entries = list_memories(db, user_id, limit).await?;

    let items: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "memory": m.memory,
                "tags": m.tags,
                "source": m.source,
                "updated_at": m.updated_at,
            })
        })
        .collect();

    Ok(serde_json::json!({"entries": items}).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE agent_memories (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                memory_text TEXT NOT NULL,
                tags TEXT NOT NULL DEFAULT '',
                source TEXT NOT NULL DEFAULT 'agent',
                category TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE VIRTUAL TABLE agent_memories_fts USING fts5(
                memory_text,
                tags,
                content='agent_memories',
                content_rowid='rowid'
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER agent_memories_ai AFTER INSERT ON agent_memories BEGIN
                INSERT INTO agent_memories_fts(rowid, memory_text, tags)
                VALUES (new.rowid, new.memory_text, new.tags);
             END",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER agent_memories_ad AFTER DELETE ON agent_memories BEGIN
                INSERT INTO agent_memories_fts(agent_memories_fts, rowid, memory_text, tags)
                VALUES ('delete', old.rowid, old.memory_text, old.tags);
             END",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER agent_memories_au AFTER UPDATE ON agent_memories BEGIN
                INSERT INTO agent_memories_fts(agent_memories_fts, rowid, memory_text, tags)
                VALUES ('delete', old.rowid, old.memory_text, old.tags);
                INSERT INTO agent_memories_fts(rowid, memory_text, tags)
                VALUES (new.rowid, new.memory_text, new.tags);
             END",
        )
        .execute(&pool)
        .await
        .unwrap();

        pool
    }

    #[tokio::test]
    async fn memory_search_returns_relevant_entries() {
        let db = test_db().await;
        add_memory(
            &db,
            "agent-1",
            "user-1",
            "User runs backups every Friday",
            "backup,ops",
            "manual",
            "",
        )
        .await
        .unwrap();
        add_memory(
            &db,
            "agent-1",
            "user-1",
            "User prefers concise weekly reports",
            "reports",
            "manual",
            "",
        )
        .await
        .unwrap();

        let hits = search_memories(&db, "user-1", "backup friday", 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].memory.contains("backups"));
    }

    #[tokio::test]
    async fn memory_is_shared_across_agents() {
        let db = test_db().await;
        // Agent A writes a memory
        add_memory(
            &db,
            "agent-a",
            "user-1",
            "User lives in Berlin",
            "location",
            "agent",
            "location",
        )
        .await
        .unwrap();
        // Agent B writes a memory
        add_memory(
            &db,
            "agent-b",
            "user-1",
            "User prefers dark mode",
            "preferences",
            "agent",
            "preferences",
        )
        .await
        .unwrap();

        // User-scoped search returns memories from both agents
        let hits = search_memories(&db, "user-1", "Berlin", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].memory.contains("Berlin"));

        // List returns all memories for the user
        let all = list_memories(&db, "user-1", 50).await.unwrap();
        assert_eq!(all.len(), 2);
    }

}
