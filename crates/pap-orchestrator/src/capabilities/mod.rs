pub mod egress_proxy;
pub mod grpc;
pub mod manifest;
pub mod runner;

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use grpc::CapabilityGrpcClient;
use manifest::CapabilityManifest;
use runner::{CapabilityRunner, RunningCapability};

use crate::permissions::{resolve_credentials, CredentialStore, PermissionError};
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
        client.invoke(&actual_tool, args, credentials, session_id).await
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
        let client = CapabilityGrpcClient::connect(running.grpc_port, cap_id).await?;

        let mut active = self.active.write().await;
        if let Some(existing) = active.get(&key) {
            self.runner.stop(&running).await;
            return Ok(existing.client.clone());
        }

        active.insert(key, ActiveCapability { running, client: client.clone() });
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

    Err(anyhow!("No capability found that provides tool '{}'", tool_name))
}

/// Build `ToolSpec` objects from all capability manifests, with tool names
/// namespaced as `"{cap_id}__{tool_name}"`.
///
/// Also returns the set of namespaced tool names that require explicit user
/// confirmation before dispatch (declared via `requires_confirmation: true`
/// in the capability manifest).
pub fn build_tool_specs(
    manifests: &HashMap<String, CapabilityManifest>,
) -> (Vec<crate::llm::provider::ToolSpec>, std::collections::HashSet<String>) {
    let mut specs = Vec::new();
    let mut confirmation_tools = std::collections::HashSet::new();
    for (cap_id, manifest) in manifests {
        for tool in &manifest.tools {
            let namespaced = format!("{}__{}", cap_id, tool.name);
            specs.push(crate::llm::provider::ToolSpec {
                name: namespaced.clone(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
            });
            if tool.requires_confirmation {
                confirmation_tools.insert(namespaced);
            }
        }
    }
    (specs, confirmation_tools)
}

/// Clean up expired workspace volumes.
/// Checks the `workspaces` table for entries whose TTL has expired and
/// removes the corresponding Docker volumes.
pub async fn cleanup_expired_workspaces(state: &AppState) -> Result<()> {
    // Find workspaces that have exceeded their TTL
    let expired = sqlx::query!(
        r#"SELECT id, session_id, capability_id FROM workspaces
           WHERE status = 'active'
             AND datetime(created_at, '+' || ttl_hours || ' hours') < datetime('now')"#
    )
    .fetch_all(&state.db)
    .await?;

    if expired.is_empty() {
        return Ok(());
    }

    info!("Found {} expired workspace(s) to clean up", expired.len());

    for ws in &expired {
        let session_id = &ws.session_id;
        let ws_id = ws.id.as_deref().unwrap_or("?");

        // Close capability containers for this session
        state.capabilities.close_session(session_id).await;

        // Remove the Docker volume
        let volume_name = format!("pap-workspace-{}", session_id);
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

        // Mark workspace as destroyed
        let _ = sqlx::query!(
            "UPDATE workspaces SET status = 'destroyed' WHERE id = ?",
            ws_id
        )
        .execute(&state.db)
        .await;
    }

    Ok(())
}
