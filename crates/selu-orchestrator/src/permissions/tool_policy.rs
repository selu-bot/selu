/// Per-user, per-tool permission policies.
///
/// Every capability tool has an associated policy for each user:
///   - `Allow`  — invoke immediately, no prompt
///   - `Ask`    — require confirmation before dispatch
///   - `Block`  — deny the call entirely
///
/// Tools without an explicit policy are **blocked by default**. The
/// agent install wizard forces the user to set a policy for every tool
/// before the agent can be used.
use anyhow::{anyhow, Result};
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

// ── Stored policy record ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ToolPolicyRecord {
    pub user_id: String,
    pub agent_id: String,
    pub capability_id: String,
    pub tool_name: String,
    pub policy: ToolPolicy,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Look up the policy a specific user has set for a specific tool.
///
/// Returns `None` if no policy has been configured — the caller should
/// treat this as **blocked** (secure default).
pub async fn get_policy(
    db: &SqlitePool,
    user_id: &str,
    capability_id: &str,
    tool_name: &str,
) -> Result<Option<ToolPolicy>> {
    let row = sqlx::query(
        "SELECT policy FROM tool_policies WHERE user_id = ? AND capability_id = ? AND tool_name = ?",
    )
    .bind(user_id)
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

/// Retrieve all policies a user has set for a given agent.
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

/// Batch-upsert policies (used by the install wizard submit).
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
             ON CONFLICT(user_id, capability_id, tool_name)
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
        "Saved tool policies"
    );

    Ok(())
}

/// Check whether a user has set a policy for **every** tool declared
/// in the given capability manifests (including built-in tools).
///
/// Returns `true` if all tools have policies set.
pub async fn has_all_policies(
    db: &SqlitePool,
    user_id: &str,
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
    required.push((BUILTIN_CAPABILITY_ID.to_string(), BUILTIN_EMIT_EVENT.to_string()));
    required.push((BUILTIN_CAPABILITY_ID.to_string(), BUILTIN_DELEGATE.to_string()));

    if required.is_empty() {
        return Ok(true);
    }

    let existing = get_policies_for_agent(db, user_id, agent_id).await?;
    let existing_set: std::collections::HashSet<(String, String)> = existing
        .into_iter()
        .map(|r| (r.capability_id, r.tool_name))
        .collect();

    Ok(required.iter().all(|r| existing_set.contains(r)))
}

/// Delete all policies a user has for a given agent.
///
/// Called when an agent is uninstalled.
pub async fn delete_policies_for_agent(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
) -> Result<()> {
    sqlx::query(
        "DELETE FROM tool_policies WHERE user_id = ? AND agent_id = ?",
    )
    .bind(user_id)
    .bind(agent_id)
    .execute(db)
    .await?;

    debug!(user_id = %user_id, agent_id = %agent_id, "Deleted tool policies for uninstalled agent");
    Ok(())
}

// ── Built-in tool identifiers ─────────────────────────────────────────────────

/// Virtual capability ID used for built-in tools (emit_event, delegate_to_agent).
pub const BUILTIN_CAPABILITY_ID: &str = "__builtin__";

/// Tool name for the emit_event built-in.
pub const BUILTIN_EMIT_EVENT: &str = "emit_event";

/// Tool name for the delegate_to_agent built-in.
pub const BUILTIN_DELEGATE: &str = "delegate_to_agent";
