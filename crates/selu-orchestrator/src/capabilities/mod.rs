pub mod discovery;
pub mod egress_proxy;
pub mod grpc;
pub mod manifest;
pub mod runner;

use anyhow::{Result, anyhow};
use serde_json::Value;
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

use grpc::CapabilityGrpcClient;
use manifest::CapabilityManifest;
use runner::{CapabilityRunner, RunningCapability};

use crate::permissions::{CredentialStore, PermissionError, resolve_credentials};
use crate::state::AppState;

/// Per-session state for a running capability: the Docker container +
/// an already-connected gRPC client.
struct ActiveCapability {
    running: RunningCapability,
    client: CapabilityGrpcClient,
}

/// The central orchestrator for all capability lifecycle:
///   - start containers on demand (lazy, per tool call)
///   - route tool calls to the correct container via gRPC
///   - tear down containers when the session ends
///   - enforces credential permissions before each invocation
///
/// Keyed by `"{session_id}::{capability_id}"`.
#[derive(Clone)]
pub struct CapabilityEngine {
    runner: CapabilityRunner,
    /// session_capability_key → active container + client
    active: Arc<RwLock<HashMap<String, ActiveCapability>>>,
}

impl CapabilityEngine {
    pub fn new(runner: CapabilityRunner) -> Self {
        Self {
            runner,
            active: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Invoke a tool by name. Starts the capability container if not already running.
    ///
    /// `manifests`    — capability manifests available to the current agent  
    /// `tool_name`    — e.g. `"weather__get_forecast"`  
    /// `args`         — JSON args from the LLM  
    /// `session_id`   — active session (used for workspace volumes + container naming)  
    /// `thread_id`    — conversation thread within the session; forwarded to the  
    ///                  container so capabilities can isolate per-thread state  
    /// `user_id`      — the acting user (for credential lookup)  
    /// `cred_store`   — credential store for resolving declared credentials  
    ///
    /// Returns `Err` with a user-visible message if a required credential is missing.
    pub async fn invoke(
        &self,
        manifests: &HashMap<String, CapabilityManifest>,
        tool_name: &str,
        args: Value,
        session_id: &str,
        thread_id: &str,
        user_id: &str,
        cred_store: &CredentialStore,
    ) -> Result<String> {
        let (cap_id, actual_tool) = resolve_tool(manifests, tool_name)?;

        let manifest = manifests
            .get(&cap_id)
            .ok_or_else(|| anyhow!("Unknown capability '{}'", cap_id))?;

        // Resolve credentials — surfaces a user-visible error if anything required is missing
        let credentials = resolve_credentials(cred_store, manifest, user_id)
            .await
            .map_err(|e: PermissionError| anyhow!("{}", e))?;

        let client = self.get_or_start(manifests, &cap_id, session_id).await?;
        client
            .invoke(&actual_tool, args, credentials, session_id, thread_id)
            .await
    }

    /// Invoke a specific tool on a specific capability with explicit
    /// credentials. This is used by internal control-plane flows
    /// (e.g. dynamic tool discovery) and bypasses tool-name resolution.
    pub async fn invoke_direct(
        &self,
        manifests: &HashMap<String, CapabilityManifest>,
        capability_id: &str,
        tool_name: &str,
        args: Value,
        credentials: Value,
        session_id: &str,
        thread_id: &str,
    ) -> Result<String> {
        if !manifests.contains_key(capability_id) {
            return Err(anyhow!("Unknown capability '{}'", capability_id));
        }
        let client = self
            .get_or_start(manifests, capability_id, session_id)
            .await?;
        client
            .invoke(tool_name, args, credentials, session_id, thread_id)
            .await
    }

    /// Shut down all containers that belong to a specific session.
    pub async fn close_session(&self, session_id: &str) {
        let mut active = self.active.write().await;
        let prefix = format!("{}::", session_id);

        let keys_to_remove: Vec<String> = active
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();

        for key in keys_to_remove {
            if let Some(ac) = active.remove(&key) {
                self.runner.stop(&ac.running).await;
                info!(session = %session_id, capability = %ac.running.capability_id, "Stopped capability for closed session");
            }
        }
    }

    /// Shut down all containers for a specific capability (across all sessions).
    /// Used during agent updates to stop old containers before replacing files.
    pub async fn close_capability(&self, capability_id: &str) {
        let mut active = self.active.write().await;

        let keys_to_remove: Vec<String> = active
            .keys()
            .filter(|k| k.ends_with(&format!("::{}", capability_id)))
            .cloned()
            .collect();

        for key in keys_to_remove {
            if let Some(ac) = active.remove(&key) {
                self.runner.stop(&ac.running).await;
                info!(capability = %capability_id, "Stopped capability container for update");
            }
        }
    }

    /// Shut down **all** active containers (used during graceful shutdown).
    pub async fn close_all(&self) {
        let mut active = self.active.write().await;
        let count = active.len();
        if count == 0 {
            return;
        }
        info!("Shutting down {} active capability container(s)...", count);
        let entries: Vec<(String, ActiveCapability)> = active.drain().collect();
        // Drop the lock before doing potentially slow Docker calls
        drop(active);
        for (key, ac) in entries {
            self.runner.stop(&ac.running).await;
            info!(key = %key, capability = %ac.running.capability_id, "Stopped capability container (shutdown)");
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    async fn get_or_start(
        &self,
        manifests: &HashMap<String, CapabilityManifest>,
        cap_id: &str,
        session_id: &str,
    ) -> Result<CapabilityGrpcClient> {
        let key = format!("{}::{}", session_id, cap_id);

        {
            let active = self.active.read().await;
            if let Some(ac) = active.get(&key) {
                return Ok(ac.client.clone());
            }
        }

        let manifest = manifests
            .get(cap_id)
            .ok_or_else(|| anyhow!("Unknown capability '{}'", cap_id))?;

        debug!(capability = %cap_id, session = %session_id, "Starting capability container");
        let running = self.runner.start(manifest, session_id).await?;
        let client = CapabilityGrpcClient::connect(&running.grpc_addr, cap_id).await?;

        let mut active = self.active.write().await;
        if let Some(existing) = active.get(&key) {
            self.runner.stop(&running).await;
            return Ok(existing.client.clone());
        }

        active.insert(
            key,
            ActiveCapability {
                running,
                client: client.clone(),
            },
        );
        Ok(client)
    }
}

/// Given a tool_name from the LLM, find which capability it belongs to and
/// return `(capability_id, bare_tool_name)`.
fn resolve_tool(
    manifests: &HashMap<String, CapabilityManifest>,
    tool_name: &str,
) -> Result<(String, String)> {
    if let Some((cap_id, bare)) = tool_name.split_once("__") {
        if manifests.contains_key(cap_id) {
            return Ok((cap_id.to_string(), bare.to_string()));
        }
    }

    for (cap_id, manifest) in manifests {
        if manifest.tools.iter().any(|t| t.name == tool_name) {
            return Ok((cap_id.clone(), tool_name.to_string()));
        }
    }

    Err(anyhow!(
        "No capability found that provides tool '{}'",
        tool_name
    ))
}

/// Build `ToolSpec` objects from all capability manifests, with tool names
/// namespaced as `"{cap_id}__{tool_name}"`.
///
/// Tool-call authorization (allow / ask / block) is now handled by the
/// per-user tool policy system rather than the manifest's
/// `requires_confirmation` flag. This function only builds the specs
/// that are exposed to the LLM.
pub fn build_tool_specs(
    manifests: &HashMap<String, CapabilityManifest>,
) -> Vec<crate::llm::provider::ToolSpec> {
    let mut specs = Vec::new();
    for (cap_id, manifest) in manifests {
        for tool in &manifest.tools {
            let namespaced = format!("{}__{}", cap_id, tool.name);
            specs.push(crate::llm::provider::ToolSpec {
                name: namespaced.clone(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
            });
        }
    }
    specs
}

/// Mark workspace activity for an environment capability invocation.
///
/// Only capabilities with `class: environment` and `filesystem: workspace`
/// are tracked. A row is created in `workspaces` on first use and touched
/// (`last_active_at`) on every subsequent invocation.
pub async fn touch_workspace_activity(
    db: &sqlx::SqlitePool,
    manifest: &CapabilityManifest,
    session_id: &str,
) -> Result<()> {
    use crate::capabilities::manifest::{CapabilityClass, FilesystemPolicy};

    if manifest.class != CapabilityClass::Environment
        || manifest.filesystem != FilesystemPolicy::Workspace
    {
        return Ok(());
    }

    let existing = sqlx::query(
        "SELECT id
         FROM workspaces
         WHERE session_id = ? AND capability_id = ? AND status != 'destroyed'
         ORDER BY created_at DESC
         LIMIT 1",
    )
    .bind(session_id)
    .bind(&manifest.id)
    .fetch_optional(db)
    .await?;

    if let Some(row) = existing {
        let ws_id: String = row.get("id");
        sqlx::query(
            "UPDATE workspaces
             SET status = 'active',
                 suspended_at = NULL,
                 last_active_at = datetime('now')
             WHERE id = ?",
        )
        .bind(ws_id)
        .execute(db)
        .await?;
        return Ok(());
    }

    let ws_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO workspaces (id, session_id, capability_id, status, last_active_at)
         VALUES (?, ?, ?, 'active', datetime('now'))",
    )
    .bind(ws_id)
    .bind(session_id)
    .bind(&manifest.id)
    .execute(db)
    .await?;

    Ok(())
}

/// Clean up expired workspace volumes.
/// Checks the `workspaces` table for entries whose TTL has expired and
/// removes the corresponding Docker volumes.
pub async fn cleanup_expired_workspaces(state: &AppState) -> Result<()> {
    // Workspace volumes are keyed by session_id. Expire sessions where all
    // active workspace rows for that session are beyond TTL.
    let expired_sessions = sqlx::query(
        r#"SELECT session_id
           FROM workspaces
           WHERE status = 'active'
           GROUP BY session_id
           HAVING MAX(datetime(COALESCE(last_active_at, created_at), '+' || ttl_hours || ' hours')) < datetime('now')"#,
    )
    .fetch_all(&state.db)
    .await?;

    if expired_sessions.is_empty() {
        return Ok(());
    }

    info!(
        "Found {} expired workspace session(s) to clean up",
        expired_sessions.len()
    );

    for row in &expired_sessions {
        let session_id: String = row.get("session_id");
        // Close capability containers for this session
        state.capabilities.close_session(&session_id).await;

        // Remove the Docker volume
        let volume_name = format!("selu-workspace-{}", session_id);
        match bollard::Docker::connect_with_local_defaults() {
            Ok(docker) => {
                if let Err(e) = docker.remove_volume(&volume_name, None).await {
                    warn!(volume = %volume_name, "Failed to remove workspace volume: {e}");
                } else {
                    info!(volume = %volume_name, "Removed expired workspace volume");
                }
            }
            Err(e) => {
                warn!("Cannot connect to Docker for volume cleanup: {e}");
            }
        }

        // Mark all active workspaces for this session as destroyed.
        let _ = sqlx::query(
            "UPDATE workspaces
             SET status = 'destroyed'
             WHERE session_id = ? AND status = 'active'",
        )
        .bind(&session_id)
        .execute(&state.db)
        .await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::manifest::{
        CapabilityClass, CredentialDeclaration, FilesystemPolicy, NetworkPolicy, ResourceLimits,
        ToolSource,
    };
    use sqlx::SqlitePool;

    fn env_manifest(cap_id: &str, filesystem: FilesystemPolicy) -> CapabilityManifest {
        CapabilityManifest {
            id: cap_id.to_string(),
            class: CapabilityClass::Environment,
            image: "test:latest".to_string(),
            tool_source: ToolSource::Manifest,
            discovery_tool_name: "list_tools".to_string(),
            tools: vec![],
            network: NetworkPolicy::default(),
            filesystem,
            credentials: Vec::<CredentialDeclaration>::new(),
            resources: ResourceLimits::default(),
            prompt: String::new(),
        }
    }

    async fn setup_db() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE workspaces (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                capability_id TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                ttl_hours INTEGER NOT NULL DEFAULT 24,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                suspended_at TEXT,
                last_active_at TEXT
            )",
        )
        .execute(&db)
        .await
        .unwrap();
        db
    }

    #[tokio::test]
    async fn touch_workspace_activity_creates_and_reuses_workspace_row() {
        let db = setup_db().await;
        let session_id = "session-1";
        let manifest = env_manifest("coding", FilesystemPolicy::Workspace);

        touch_workspace_activity(&db, &manifest, session_id)
            .await
            .unwrap();
        touch_workspace_activity(&db, &manifest, session_id)
            .await
            .unwrap();

        let count: i64 = sqlx::query(
            "SELECT COUNT(*) AS cnt
             FROM workspaces
             WHERE session_id = ? AND capability_id = ? AND status = 'active'",
        )
        .bind(session_id)
        .bind("coding")
        .fetch_one(&db)
        .await
        .unwrap()
        .get("cnt");

        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn touch_workspace_activity_skips_non_workspace_filesystem() {
        let db = setup_db().await;
        let manifest = env_manifest("coding", FilesystemPolicy::Temp);

        touch_workspace_activity(&db, &manifest, "session-1")
            .await
            .unwrap();

        let count: i64 = sqlx::query("SELECT COUNT(*) AS cnt FROM workspaces")
            .fetch_one(&db)
            .await
            .unwrap()
            .get("cnt");

        assert_eq!(count, 0);
    }
}
