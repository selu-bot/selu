/// Tool permission policies with global defaults and per-user overrides.
///
/// Every capability tool has a **global default** policy set by the admin
/// during agent setup.  Individual users can optionally override the
/// default for their own sessions.
///
/// Policy resolution order:
///   1. Per-user override (`tool_policies` table)
///   2. Global default    (`global_tool_policies` table)
///   3. Block             (secure fallback when nothing is configured)
///
/// The three policy levels:
///   - `Allow`  — invoke immediately, no prompt
///   - `Ask`    — require confirmation before dispatch
///   - `Block`  — deny the call entirely
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use tracing::debug;
use uuid::Uuid;

// ── Policy enum ───────────────────────────────────────────────────────────────

/// The three possible tool-call policies a user can set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicy {
    Allow,
    Ask,
    Block,
}

impl ToolPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Block => "block",
        }
    }

    pub fn from_str(s: &str) -> Result<Self> {
        match s {
            "allow" => Ok(Self::Allow),
            "ask" => Ok(Self::Ask),
            "block" => Ok(Self::Block),
            other => Err(anyhow!("Invalid tool policy: '{}'", other)),
        }
    }
}

impl std::fmt::Display for ToolPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Stored policy records ─────────────────────────────────────────────────────

/// A per-user override record (from `tool_policies`).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ToolPolicyRecord {
    pub user_id: String,
    pub agent_id: String,
    pub capability_id: String,
    pub tool_name: String,
    pub policy: ToolPolicy,
}

/// A global default record (from `global_tool_policies`).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GlobalToolPolicyRecord {
    pub agent_id: String,
    pub capability_id: String,
    pub tool_name: String,
    pub policy: ToolPolicy,
}

// ── Public API ────────────────────────────────────────────────────────────────

// ── Effective policy (the main runtime entry point) ───────────────────────────

/// Resolve the effective policy for a tool call.
///
/// Checks the per-user override first, then the global default.
/// Returns `None` if neither exists — the caller should treat this as
/// **blocked** (secure default).
pub async fn get_effective_policy(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    tool_name: &str,
) -> Result<Option<ToolPolicy>> {
    // 1. Check per-user override
    if let Some(policy) = get_user_override(db, user_id, agent_id, capability_id, tool_name).await?
    {
        return Ok(Some(policy));
    }

    // 2. Fall back to global default
    get_global_policy(db, agent_id, capability_id, tool_name).await
}

// ── Per-user overrides (tool_policies table) ──────────────────────────────────

/// Look up a per-user override for a specific tool.
///
/// Returns `None` if the user has not overridden the global default.
pub async fn get_user_override(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    tool_name: &str,
) -> Result<Option<ToolPolicy>> {
    let row = sqlx::query(
        "SELECT policy FROM tool_policies WHERE user_id = ? AND agent_id = ? AND capability_id = ? AND tool_name = ?",
    )
    .bind(user_id)
    .bind(agent_id)
    .bind(capability_id)
    .bind(tool_name)
    .fetch_optional(db)
    .await?;

    match row {
        Some(r) => {
            let policy: String = r.get("policy");
            Ok(Some(ToolPolicy::from_str(&policy)?))
        }
        None => Ok(None),
    }
}

/// Look up the policy a specific user has set for a specific tool.
///
/// Returns `None` if no policy has been configured — the caller should
/// treat this as **blocked** (secure default).
///
/// **Deprecated:** prefer [`get_effective_policy`] which checks both
/// per-user overrides and global defaults.
#[allow(dead_code)]
pub async fn get_policy(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    tool_name: &str,
) -> Result<Option<ToolPolicy>> {
    get_effective_policy(db, user_id, agent_id, capability_id, tool_name).await
}

/// Retrieve all per-user override policies for a given agent.
pub async fn get_policies_for_agent(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
) -> Result<Vec<ToolPolicyRecord>> {
    let rows = sqlx::query(
        "SELECT capability_id, tool_name, policy FROM tool_policies WHERE user_id = ? AND agent_id = ?",
    )
    .bind(user_id)
    .bind(agent_id)
    .fetch_all(db)
    .await?;

    rows.iter()
        .map(|r| {
            Ok(ToolPolicyRecord {
                user_id: user_id.to_string(),
                agent_id: agent_id.to_string(),
                capability_id: r.get("capability_id"),
                tool_name: r.get("tool_name"),
                policy: ToolPolicy::from_str(&r.get::<String, _>("policy"))?,
            })
        })
        .collect()
}

/// Batch-upsert per-user override policies.
///
/// Each entry in `policies` is `(capability_id, tool_name, policy)`.
pub async fn set_policies(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
    policies: &[(String, String, ToolPolicy)],
) -> Result<()> {
    for (capability_id, tool_name, policy) in policies {
        let id = Uuid::new_v4().to_string();
        let policy_str = policy.as_str();
        sqlx::query(
            "INSERT INTO tool_policies (id, user_id, agent_id, capability_id, tool_name, policy)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(user_id, agent_id, capability_id, tool_name)
             DO UPDATE SET policy = excluded.policy, updated_at = datetime('now')",
        )
        .bind(&id)
        .bind(user_id)
        .bind(agent_id)
        .bind(capability_id)
        .bind(tool_name)
        .bind(policy_str)
        .execute(db)
        .await?;
    }

    debug!(
        user_id = %user_id,
        agent_id = %agent_id,
        count = policies.len(),
        "Saved per-user tool policy overrides"
    );

    Ok(())
}

/// Delete a single per-user override, reverting to the global default.
pub async fn delete_user_policy(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    tool_name: &str,
) -> Result<()> {
    sqlx::query(
        "DELETE FROM tool_policies WHERE user_id = ? AND agent_id = ? AND capability_id = ? AND tool_name = ?",
    )
    .bind(user_id)
    .bind(agent_id)
    .bind(capability_id)
    .bind(tool_name)
    .execute(db)
    .await?;

    debug!(
        user_id = %user_id,
        agent_id = %agent_id,
        capability_id = %capability_id,
        tool_name = %tool_name,
        "Deleted per-user tool policy override (reverted to global default)"
    );

    Ok(())
}

/// Delete all per-user override policies for a given agent.
///
/// Called when an agent is uninstalled.
#[allow(dead_code)]
pub async fn delete_policies_for_agent(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM tool_policies WHERE user_id = ? AND agent_id = ?")
        .bind(user_id)
        .bind(agent_id)
        .execute(db)
        .await?;

    debug!(user_id = %user_id, agent_id = %agent_id, "Deleted per-user tool policies for uninstalled agent");
    Ok(())
}

// ── Global defaults (global_tool_policies table) ──────────────────────────────

/// Look up the global default policy for a specific tool.
///
/// Returns `None` if no global default has been configured.
pub async fn get_global_policy(
    db: &SqlitePool,
    agent_id: &str,
    capability_id: &str,
    tool_name: &str,
) -> Result<Option<ToolPolicy>> {
    let row = sqlx::query(
        "SELECT policy FROM global_tool_policies WHERE agent_id = ? AND capability_id = ? AND tool_name = ?",
    )
    .bind(agent_id)
    .bind(capability_id)
    .bind(tool_name)
    .fetch_optional(db)
    .await?;

    match row {
        Some(r) => {
            let policy: String = r.get("policy");
            Ok(Some(ToolPolicy::from_str(&policy)?))
        }
        None => Ok(None),
    }
}

/// Retrieve all global default policies for a given agent.
pub async fn get_global_policies_for_agent(
    db: &SqlitePool,
    agent_id: &str,
) -> Result<Vec<GlobalToolPolicyRecord>> {
    let rows = sqlx::query(
        "SELECT capability_id, tool_name, policy FROM global_tool_policies WHERE agent_id = ?",
    )
    .bind(agent_id)
    .fetch_all(db)
    .await?;

    rows.iter()
        .map(|r| {
            Ok(GlobalToolPolicyRecord {
                agent_id: agent_id.to_string(),
                capability_id: r.get("capability_id"),
                tool_name: r.get("tool_name"),
                policy: ToolPolicy::from_str(&r.get::<String, _>("policy"))?,
            })
        })
        .collect()
}

/// Batch-upsert global default policies (used by the install wizard).
///
/// Each entry in `policies` is `(capability_id, tool_name, policy)`.
pub async fn set_global_policies(
    db: &SqlitePool,
    agent_id: &str,
    policies: &[(String, String, ToolPolicy)],
) -> Result<()> {
    for (capability_id, tool_name, policy) in policies {
        let id = Uuid::new_v4().to_string();
        let policy_str = policy.as_str();
        sqlx::query(
            "INSERT INTO global_tool_policies (id, agent_id, capability_id, tool_name, policy)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(agent_id, capability_id, tool_name)
             DO UPDATE SET policy = excluded.policy, updated_at = datetime('now')",
        )
        .bind(&id)
        .bind(agent_id)
        .bind(capability_id)
        .bind(tool_name)
        .bind(policy_str)
        .execute(db)
        .await?;
    }

    debug!(
        agent_id = %agent_id,
        count = policies.len(),
        "Saved global tool policies"
    );

    Ok(())
}

/// Delete all global default policies for a given agent.
///
/// Called when an agent is uninstalled.
#[allow(dead_code)]
pub async fn delete_global_policies_for_agent(db: &SqlitePool, agent_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM global_tool_policies WHERE agent_id = ?")
        .bind(agent_id)
        .execute(db)
        .await?;

    debug!(agent_id = %agent_id, "Deleted global tool policies for uninstalled agent");
    Ok(())
}

// ── Completeness check ───────────────────────────────────────────────────────

/// Check whether global default policies exist for **every** tool declared
/// in the given capability manifests (including built-in tools).
///
/// Returns `true` if all tools have global policies set.
#[allow(dead_code)]
pub async fn has_all_policies(
    db: &SqlitePool,
    _user_id: &str,
    agent_id: &str,
    manifests: &HashMap<String, crate::capabilities::manifest::CapabilityManifest>,
) -> Result<bool> {
    // Collect all (capability_id, tool_name) pairs from manifests
    let mut required: Vec<(String, String)> = Vec::new();
    for (cap_id, manifest) in manifests {
        for tool in &manifest.tools {
            required.push((cap_id.clone(), tool.name.clone()));
        }
    }

    // Add built-in tools
    required.push((
        BUILTIN_CAPABILITY_ID.to_string(),
        BUILTIN_EMIT_EVENT.to_string(),
    ));
    required.push((
        BUILTIN_CAPABILITY_ID.to_string(),
        BUILTIN_DELEGATE.to_string(),
    ));
    required.push((
        BUILTIN_CAPABILITY_ID.to_string(),
        BUILTIN_STORE_GET.to_string(),
    ));
    required.push((
        BUILTIN_CAPABILITY_ID.to_string(),
        BUILTIN_STORE_SET.to_string(),
    ));
    required.push((
        BUILTIN_CAPABILITY_ID.to_string(),
        BUILTIN_STORE_DELETE.to_string(),
    ));
    required.push((
        BUILTIN_CAPABILITY_ID.to_string(),
        BUILTIN_STORE_LIST.to_string(),
    ));
    required.push((
        BUILTIN_CAPABILITY_ID.to_string(),
        BUILTIN_SET_SCHEDULE.to_string(),
    ));
    required.push((
        BUILTIN_CAPABILITY_ID.to_string(),
        BUILTIN_SET_REMINDER.to_string(),
    ));
    required.push((
        BUILTIN_CAPABILITY_ID.to_string(),
        BUILTIN_MEMORY_REMEMBER.to_string(),
    ));
    required.push((
        BUILTIN_CAPABILITY_ID.to_string(),
        BUILTIN_MEMORY_FORGET.to_string(),
    ));
    required.push((
        BUILTIN_CAPABILITY_ID.to_string(),
        BUILTIN_MEMORY_SEARCH.to_string(),
    ));
    required.push((
        BUILTIN_CAPABILITY_ID.to_string(),
        BUILTIN_MEMORY_LIST.to_string(),
    ));

    if required.is_empty() {
        return Ok(true);
    }

    let existing = get_global_policies_for_agent(db, agent_id).await?;
    let existing_set: std::collections::HashSet<(String, String)> = existing
        .into_iter()
        .map(|r| (r.capability_id, r.tool_name))
        .collect();

    Ok(required.iter().all(|r| existing_set.contains(r)))
}

// ── Built-in tool identifiers ─────────────────────────────────────────────────

/// Virtual capability ID used for built-in tools (emit_event, delegate_to_agent, store_*).
pub const BUILTIN_CAPABILITY_ID: &str = "__builtin__";

/// Tool name for the emit_event built-in.
pub const BUILTIN_EMIT_EVENT: &str = "emit_event";

/// Tool name for the delegate_to_agent built-in.
pub const BUILTIN_DELEGATE: &str = "delegate_to_agent";

/// Tool name for the store_get built-in (persistent agent storage).
pub const BUILTIN_STORE_GET: &str = "store_get";

/// Tool name for the store_set built-in (persistent agent storage).
pub const BUILTIN_STORE_SET: &str = "store_set";

/// Tool name for the store_delete built-in (persistent agent storage).
pub const BUILTIN_STORE_DELETE: &str = "store_delete";

/// Tool name for the store_list built-in (persistent agent storage).
pub const BUILTIN_STORE_LIST: &str = "store_list";

/// Tool name for the set_reminder built-in (one-shot schedule reminders).
pub const BUILTIN_SET_REMINDER: &str = "set_reminder";

/// Tool name for the set_schedule built-in (recurring schedules).
pub const BUILTIN_SET_SCHEDULE: &str = "set_schedule";

/// Tool name for the memory_remember built-in (BM25 long-term memory).
pub const BUILTIN_MEMORY_REMEMBER: &str = "memory_remember";

/// Tool name for the memory_forget built-in (BM25 long-term memory).
pub const BUILTIN_MEMORY_FORGET: &str = "memory_forget";

/// Tool name for the memory_search built-in (BM25 long-term memory).
pub const BUILTIN_MEMORY_SEARCH: &str = "memory_search";

/// Tool name for the memory_list built-in (BM25 long-term memory).
pub const BUILTIN_MEMORY_LIST: &str = "memory_list";
