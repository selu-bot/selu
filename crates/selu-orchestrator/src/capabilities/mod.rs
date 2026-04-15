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
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Semaphore};
use tracing::{debug, info, warn};
use uuid::Uuid;

use grpc::CapabilityGrpcClient;
use manifest::CapabilityManifest;
use runner::{CapabilityRunner, RunningCapability};

use crate::permissions::network_policy;
use crate::permissions::{CredentialStore, PermissionError, resolve_credentials};
use crate::state::AppState;

/// Per-session state for a running capability: the Docker container +
/// an already-connected gRPC client.
struct ActiveCapability {
    running: RunningCapability,
    client: CapabilityGrpcClient,
}

#[derive(Clone)]
struct CachedEgressPolicy {
    policy: crate::capabilities::egress_proxy::ContainerEgressPolicy,
    fetched_at: Instant,
}

/// Prefix used for shared (cross-session) container keys in the `active` map.
const SHARED_KEY_PREFIX: &str = "shared::";

/// Timeout for acquiring a shared-capability semaphore permit.
const SHARED_SEMAPHORE_TIMEOUT: Duration = Duration::from_secs(30);

/// The central orchestrator for all capability lifecycle:
///   - start containers on demand (lazy, per tool call)
///   - route tool calls to the correct container via gRPC
///   - tear down containers when the session ends
///   - enforces credential permissions before each invocation
///
/// Keyed by `"{session_id}::{capability_id}"` for exclusive containers,
/// or `"shared::{capability_id}"` for shared (cross-session) containers.
#[derive(Clone)]
pub struct CapabilityEngine {
    runner: CapabilityRunner,
    /// session_capability_key → active container + client
    active: Arc<RwLock<HashMap<String, ActiveCapability>>>,
    /// (user, agent, capability, session) → effective egress policy cache
    network_policy_cache: Arc<RwLock<HashMap<String, CachedEgressPolicy>>>,
    /// Per-capability concurrency limiter for shared containers.
    shared_semaphores: Arc<RwLock<HashMap<String, Arc<Semaphore>>>>,
    /// Tracks the last invocation time for each shared container (for idle reaping).
    shared_last_invoked: Arc<RwLock<HashMap<String, Instant>>>,
}

const NETWORK_POLICY_CACHE_TTL: Duration = Duration::from_secs(30);

impl CapabilityEngine {
    pub fn new(runner: CapabilityRunner) -> Self {
        Self {
            runner,
            active: Arc::new(RwLock::new(HashMap::new())),
            network_policy_cache: Arc::new(RwLock::new(HashMap::new())),
            shared_semaphores: Arc::new(RwLock::new(HashMap::new())),
            shared_last_invoked: Arc::new(RwLock::new(HashMap::new())),
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
        agent_id: &str,
        cred_store: &CredentialStore,
        db: &sqlx::SqlitePool,
    ) -> Result<String> {
        let (cap_id, actual_tool) = resolve_tool(manifests, tool_name)?;

        let manifest = manifests
            .get(&cap_id)
            .ok_or_else(|| anyhow!("Unknown capability '{}'", cap_id))?;

        // Resolve credentials — surfaces a user-visible error if anything required is missing
        let credentials = resolve_credentials(cred_store, manifest, user_id)
            .await
            .map_err(|e: PermissionError| anyhow!("{}", e))?;

        let shared = manifest.is_shared();
        let key = container_key(manifest, session_id);

        // For shared containers, use the manifest's declared network policy directly
        // (per-user overrides would conflict between sessions sharing the container).
        let egress_policy = if shared {
            crate::capabilities::egress_proxy::ContainerEgressPolicy {
                capability_id: cap_id.clone(),
                mode: manifest.network.mode.clone(),
                allowed_hosts: manifest.network.hosts.clone(),
                denied_hosts: Vec::new(),
            }
        } else {
            self.resolve_egress_policy(manifest, &cap_id, session_id, user_id, agent_id, db)
                .await?
        };

        // Acquire concurrency permit for shared containers.
        let _permit = if shared {
            let semaphore = self.get_or_create_semaphore(&cap_id, manifest).await;
            let permit = tokio::time::timeout(SHARED_SEMAPHORE_TIMEOUT, semaphore.acquire_owned())
                .await
                .map_err(|_| {
                    anyhow!(
                        "Shared capability '{}' is at max concurrency — timed out waiting for a slot",
                        cap_id
                    )
                })?
                .map_err(|_| anyhow!("Shared capability '{}' semaphore closed", cap_id))?;
            // Track last invocation time for the idle reaper.
            self.shared_last_invoked
                .write()
                .await
                .insert(cap_id.clone(), Instant::now());
            Some(permit)
        } else {
            None
        };

        let session_for_start = if shared { None } else { Some(session_id) };
        let user_for_start = if shared { None } else { Some(user_id) };

        // Clone args + credentials for potential retry on shared container crash.
        let (args_retry, creds_retry) = if shared {
            (Some(args.clone()), Some(credentials.clone()))
        } else {
            (None, None)
        };

        let client = self
            .get_or_start(manifests, &cap_id, &key, session_for_start, user_for_start, &egress_policy)
            .await?;

        let result = client
            .invoke(&actual_tool, args, credentials, session_id, thread_id)
            .await;

        // Crash recovery for shared containers: if the gRPC call fails with a
        // transport error, remove the stale entry and retry once with a fresh container.
        if shared {
            if let Err(ref e) = result {
                let msg = e.to_string();
                if msg.contains("transport error") || msg.contains("connect") {
                    warn!(capability = %cap_id, "Shared container may have crashed, retrying with fresh container");
                    self.remove_active(&key).await;
                    let client = self
                        .get_or_start(manifests, &cap_id, &key, None, None, &egress_policy)
                        .await?;
                    return client
                        .invoke(
                            &actual_tool,
                            args_retry.unwrap(),
                            creds_retry.unwrap(),
                            session_id,
                            thread_id,
                        )
                        .await;
                }
            }
        }

        result
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
        let manifest = manifests
            .get(capability_id)
            .ok_or_else(|| anyhow!("Unknown capability '{}'", capability_id))?;
        let egress_policy = crate::capabilities::egress_proxy::ContainerEgressPolicy {
            capability_id: capability_id.to_string(),
            mode: manifest.network.mode.clone(),
            allowed_hosts: manifest.network.hosts.clone(),
            denied_hosts: Vec::new(),
        };
        // invoke_direct is used for internal control-plane flows (e.g. discovery)
        // which create ephemeral sessions — always use exclusive keying here.
        let key = format!("{}::{}", session_id, capability_id);
        let client = self
            .get_or_start(
                manifests,
                capability_id,
                &key,
                Some(session_id),
                None, // no user_id for internal flows
                &egress_policy,
            )
            .await?;
        client
            .invoke(tool_name, args, credentials, session_id, thread_id)
            .await
    }

    pub async fn upload_input_artifact(
        &self,
        manifests: &HashMap<String, CapabilityManifest>,
        capability_id: &str,
        session_id: &str,
        user_id: &str,
        agent_id: &str,
        db: &sqlx::SqlitePool,
        filename: &str,
        mime_type: &str,
        data: &[u8],
    ) -> Result<String> {
        let manifest = manifests
            .get(capability_id)
            .ok_or_else(|| anyhow!("Unknown capability '{}'", capability_id))?;
        let shared = manifest.is_shared();
        let key = container_key(manifest, session_id);
        let egress_policy = if shared {
            crate::capabilities::egress_proxy::ContainerEgressPolicy {
                capability_id: capability_id.to_string(),
                mode: manifest.network.mode.clone(),
                allowed_hosts: manifest.network.hosts.clone(),
                denied_hosts: Vec::new(),
            }
        } else {
            self.resolve_egress_policy(manifest, capability_id, session_id, user_id, agent_id, db)
                .await?
        };
        let session_for_start = if shared { None } else { Some(session_id) };
        let user_for_start = if shared { None } else { Some(user_id) };
        let client = self
            .get_or_start(
                manifests,
                capability_id,
                &key,
                session_for_start,
                user_for_start,
                &egress_policy,
            )
            .await?;
        client
            .upload_input_artifact(filename, mime_type, data)
            .await
    }

    pub async fn download_output_artifact(
        &self,
        manifests: &HashMap<String, CapabilityManifest>,
        capability_id: &str,
        session_id: &str,
        user_id: &str,
        agent_id: &str,
        db: &sqlx::SqlitePool,
        capability_artifact_id: &str,
    ) -> Result<Vec<u8>> {
        let manifest = manifests
            .get(capability_id)
            .ok_or_else(|| anyhow!("Unknown capability '{}'", capability_id))?;
        let shared = manifest.is_shared();
        let key = container_key(manifest, session_id);
        let egress_policy = if shared {
            crate::capabilities::egress_proxy::ContainerEgressPolicy {
                capability_id: capability_id.to_string(),
                mode: manifest.network.mode.clone(),
                allowed_hosts: manifest.network.hosts.clone(),
                denied_hosts: Vec::new(),
            }
        } else {
            self.resolve_egress_policy(manifest, capability_id, session_id, user_id, agent_id, db)
                .await?
        };
        let session_for_start = if shared { None } else { Some(session_id) };
        let user_for_start = if shared { None } else { Some(user_id) };
        let client = self
            .get_or_start(
                manifests,
                capability_id,
                &key,
                session_for_start,
                user_for_start,
                &egress_policy,
            )
            .await?;
        client
            .download_output_artifact(capability_artifact_id)
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

        self.network_policy_cache
            .write()
            .await
            .retain(|k, _| !k.ends_with(&format!("::{}", session_id)));
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
        self.network_policy_cache
            .write()
            .await
            .retain(|k, _| !k.contains(&format!("::{}::", capability_id)));
    }

    /// Shut down shared containers that have been idle longer than their
    /// configured `idle_timeout_seconds`. Called periodically by the background reaper.
    pub async fn close_idle_shared(&self, manifests: &HashMap<String, CapabilityManifest>) {
        let now = Instant::now();
        let last_invoked = self.shared_last_invoked.read().await;
        let mut to_close: Vec<(String, Duration)> = Vec::new();

        for (cap_id, last) in last_invoked.iter() {
            let idle_timeout = manifests
                .get(cap_id)
                .and_then(|m| m.sharing_policy())
                .map_or(Duration::from_secs(300), |p| {
                    Duration::from_secs(p.idle_timeout_seconds)
                });
            let elapsed = now.duration_since(*last);
            if elapsed > idle_timeout {
                // Check the semaphore: skip if there are in-flight invocations.
                if let Some(sem) = self.shared_semaphores.read().await.get(cap_id) {
                    let max = manifests
                        .get(cap_id)
                        .and_then(|m| m.sharing_policy())
                        .map_or(1, |p| p.max_concurrent.max(1));
                    if sem.available_permits() < max as usize {
                        debug!(capability = %cap_id, "Skipping idle close — in-flight invocations");
                        continue;
                    }
                }
                to_close.push((cap_id.clone(), elapsed));
            }
        }
        drop(last_invoked);

        if to_close.is_empty() {
            return;
        }

        for (cap_id, elapsed) in &to_close {
            let key = format!("{}{}", SHARED_KEY_PREFIX, cap_id);
            let mut active = self.active.write().await;
            if let Some(ac) = active.remove(&key) {
                self.runner.stop(&ac.running).await;
                info!(
                    capability = %cap_id,
                    idle_secs = elapsed.as_secs(),
                    "Stopped idle shared capability container"
                );
            }
        }

        // Clean up tracking state for closed containers.
        let mut last_invoked = self.shared_last_invoked.write().await;
        let mut semaphores = self.shared_semaphores.write().await;
        for (cap_id, _) in &to_close {
            last_invoked.remove(cap_id);
            semaphores.remove(cap_id);
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
        self.network_policy_cache.write().await.clear();
    }

    pub async fn invalidate_network_policy_cache(
        &self,
        user_id: &str,
        agent_id: &str,
        capability_id: &str,
    ) {
        let prefix = format!("{}::{}::{}::", user_id, agent_id, capability_id);
        self.network_policy_cache
            .write()
            .await
            .retain(|k, _| !k.starts_with(&prefix));
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    async fn get_or_start(
        &self,
        manifests: &HashMap<String, CapabilityManifest>,
        cap_id: &str,
        key: &str,
        session_for_start: Option<&str>,
        user_id: Option<&str>,
        egress_policy: &crate::capabilities::egress_proxy::ContainerEgressPolicy,
    ) -> Result<CapabilityGrpcClient> {
        {
            let active = self.active.read().await;
            if let Some(ac) = active.get(key) {
                let client = ac.client.clone();
                let token = ac.running.egress_token.clone();
                drop(active);
                self.runner
                    .update_policy(&token, egress_policy.clone())
                    .await;
                return Ok(client);
            }
        }

        let manifest = manifests
            .get(cap_id)
            .ok_or_else(|| anyhow!("Unknown capability '{}'", cap_id))?;

        debug!(capability = %cap_id, key = %key, "Starting capability container");
        let running = self
            .runner
            .start(manifest, session_for_start, user_id, egress_policy)
            .await?;
        let client = CapabilityGrpcClient::connect(&running.grpc_addr, cap_id).await?;

        let mut active = self.active.write().await;
        if let Some(existing) = active.get(key) {
            // Another task started the same container while we were booting ours.
            self.runner.stop(&running).await;
            return Ok(existing.client.clone());
        }

        active.insert(
            key.to_string(),
            ActiveCapability {
                running,
                client: client.clone(),
            },
        );
        Ok(client)
    }

    /// Remove an active container entry by key (used for crash recovery).
    async fn remove_active(&self, key: &str) {
        let mut active = self.active.write().await;
        if let Some(ac) = active.remove(key) {
            self.runner.stop(&ac.running).await;
        }
    }

    /// Get or create a concurrency-limiting semaphore for a shared capability.
    async fn get_or_create_semaphore(
        &self,
        cap_id: &str,
        manifest: &CapabilityManifest,
    ) -> Arc<Semaphore> {
        let semaphores = self.shared_semaphores.read().await;
        if let Some(sem) = semaphores.get(cap_id) {
            return sem.clone();
        }
        drop(semaphores);

        let max_concurrent = manifest
            .sharing_policy()
            .map_or(1, |p| p.max_concurrent.max(1));
        let sem = Arc::new(Semaphore::new(max_concurrent as usize));
        self.shared_semaphores
            .write()
            .await
            .entry(cap_id.to_string())
            .or_insert(sem)
            .clone()
    }

    async fn resolve_egress_policy(
        &self,
        manifest: &CapabilityManifest,
        cap_id: &str,
        session_id: &str,
        user_id: &str,
        agent_id: &str,
        db: &sqlx::SqlitePool,
    ) -> Result<crate::capabilities::egress_proxy::ContainerEgressPolicy> {
        let cache_key = network_policy_cache_key(user_id, agent_id, cap_id, session_id);
        if let Some(policy) = {
            let cache = self.network_policy_cache.read().await;
            cache
                .get(&cache_key)
                .filter(|cached| cached.fetched_at.elapsed() <= NETWORK_POLICY_CACHE_TTL)
                .map(|cached| cached.policy.clone())
        } {
            return Ok(policy);
        }

        let effective = network_policy::resolve_effective_policy(
            db,
            user_id,
            agent_id,
            cap_id,
            &manifest.network,
        )
        .await?;
        let resolved = crate::capabilities::egress_proxy::ContainerEgressPolicy {
            capability_id: cap_id.to_string(),
            mode: effective.mode,
            allowed_hosts: effective.allowed_hosts,
            denied_hosts: effective.denied_hosts,
        };
        self.network_policy_cache.write().await.insert(
            cache_key,
            CachedEgressPolicy {
                policy: resolved.clone(),
                fetched_at: Instant::now(),
            },
        );
        Ok(resolved)
    }
}

fn network_policy_cache_key(
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    session_id: &str,
) -> String {
    format!(
        "{}::{}::{}::{}",
        user_id, agent_id, capability_id, session_id
    )
}

/// Given a tool_name from the LLM, find which capability it belongs to and
/// return `(capability_id, bare_tool_name)`.
/// Compute the cache key for a capability container.
///
/// Shared capabilities use `"shared::{capability_id}"` (one container for all sessions).
/// Exclusive capabilities use `"{session_id}::{capability_id}"` (one per session).
fn container_key(manifest: &CapabilityManifest, session_id: &str) -> String {
    if manifest.is_shared() {
        format!("{}{}", SHARED_KEY_PREFIX, manifest.id)
    } else {
        format!("{}::{}", session_id, manifest.id)
    }
}

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

/// Mark cache volume activity for a capability invocation.
///
/// Only capabilities with `cache.enabled: true` are tracked.
/// Creates a row in `cache_volumes` on first use (per user + capability)
/// and touches `last_active_at` on every subsequent invocation.
pub async fn touch_cache_volume_activity(
    db: &sqlx::SqlitePool,
    manifest: &CapabilityManifest,
    user_id: &str,
) -> Result<()> {
    if !manifest.cache.enabled {
        return Ok(());
    }

    let existing = sqlx::query(
        "SELECT id
         FROM cache_volumes
         WHERE user_id = ? AND capability_id = ? AND status = 'active'
         LIMIT 1",
    )
    .bind(user_id)
    .bind(&manifest.id)
    .fetch_optional(db)
    .await?;

    if let Some(row) = existing {
        let cv_id: String = row.get("id");
        sqlx::query(
            "UPDATE cache_volumes
             SET last_active_at = datetime('now')
             WHERE id = ?",
        )
        .bind(cv_id)
        .execute(db)
        .await?;
        return Ok(());
    }

    let cv_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO cache_volumes (id, user_id, capability_id, last_active_at)
         VALUES (?, ?, ?, datetime('now'))",
    )
    .bind(cv_id)
    .bind(user_id)
    .bind(&manifest.id)
    .execute(db)
    .await?;

    Ok(())
}

/// Purge a single cache volume by its database ID.
///
/// Removes the Docker volume and marks the row as `destroyed`.
/// Returns `true` if the volume was found and purged.
pub async fn purge_cache_volume(
    db: &sqlx::SqlitePool,
    cache_id: &str,
    restrict_user_id: Option<&str>,
) -> Result<bool> {
    // Look up the cache volume, optionally restricting to a specific user.
    let row = if let Some(uid) = restrict_user_id {
        sqlx::query(
            "SELECT id, user_id, capability_id
             FROM cache_volumes
             WHERE id = ? AND user_id = ? AND status = 'active'",
        )
        .bind(cache_id)
        .bind(uid)
        .fetch_optional(db)
        .await?
    } else {
        sqlx::query(
            "SELECT id, user_id, capability_id
             FROM cache_volumes
             WHERE id = ? AND status = 'active'",
        )
        .bind(cache_id)
        .fetch_optional(db)
        .await?
    };

    let row = match row {
        Some(r) => r,
        None => return Ok(false),
    };

    let cv_id: String = row.get("id");
    let user_id: String = row.get("user_id");
    let cap_id: String = row.get("capability_id");
    let volume_name = format!("selu-cache-{}-{}", user_id, cap_id);

    match bollard::Docker::connect_with_local_defaults() {
        Ok(docker) => {
            if let Err(e) = docker.remove_volume(&volume_name, None).await {
                warn!(volume = %volume_name, "Failed to remove cache volume: {e}");
            } else {
                info!(volume = %volume_name, "Purged cache volume");
            }
        }
        Err(e) => warn!("Cannot connect to Docker for cache purge: {e}"),
    }

    sqlx::query("UPDATE cache_volumes SET status = 'destroyed' WHERE id = ?")
        .bind(&cv_id)
        .execute(db)
        .await?;

    Ok(true)
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
        CachePolicy, CapabilityClass, CredentialDeclaration, FilesystemPolicy, NetworkPolicy,
        ResourceLimits, ToolSource,
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
            cache: CachePolicy::default(),
            sharing: None,
            prompt: String::new(),
            localized_prompts: HashMap::new(),
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
