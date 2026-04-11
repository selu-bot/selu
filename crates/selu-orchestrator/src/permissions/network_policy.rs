use anyhow::{Result, anyhow};
use sqlx::{Row, SqlitePool};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use crate::capabilities::manifest::{NetworkMode, NetworkPolicy};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkAccessPolicy {
    Allow,
    Deny,
}

impl NetworkAccessPolicy {
    pub fn from_str(value: &str) -> Result<Self> {
        match value {
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            _ => Err(anyhow!("Invalid network access policy")),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPolicy {
    Allow,
    Deny,
}

impl HostPolicy {
    pub fn from_str(value: &str) -> Result<Self> {
        match value {
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            _ => Err(anyhow!("Invalid host policy")),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone)]
pub struct UserHostPolicyRecord {
    pub capability_id: String,
    pub host: String,
    pub policy: HostPolicy,
}

#[derive(Debug, Clone)]
pub struct EffectiveNetworkPolicy {
    pub mode: NetworkMode,
    pub allowed_hosts: Vec<String>,
    pub denied_hosts: Vec<String>,
}

pub fn normalize_host_entry(raw: &str) -> Result<String> {
    let host = raw.trim().to_ascii_lowercase();
    if host.is_empty() {
        return Err(anyhow!("Host is required"));
    }
    if host.contains("://") || host.contains('/') || host.contains(' ') {
        return Err(anyhow!("Use only host or host:port"));
    }
    if host.starts_with(':') || host.ends_with(':') {
        return Err(anyhow!("Invalid host format"));
    }
    if let Some((h, p)) = host.rsplit_once(':') {
        if h.is_empty() || p.is_empty() || p.parse::<u16>().is_err() {
            return Err(anyhow!("Invalid host:port format"));
        }
    }
    Ok(host)
}

pub async fn get_user_access_overrides_for_agent(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
) -> Result<HashMap<String, NetworkAccessPolicy>> {
    let rows = sqlx::query(
        "SELECT capability_id, access
         FROM user_network_access_policies
         WHERE user_id = ? AND agent_id = ?",
    )
    .bind(user_id)
    .bind(agent_id)
    .fetch_all(db)
    .await?;

    let mut map = HashMap::new();
    for row in rows {
        let cap_id: String = row.get("capability_id");
        let access: String = row.get("access");
        map.insert(cap_id, NetworkAccessPolicy::from_str(&access)?);
    }
    Ok(map)
}

pub async fn get_user_host_overrides_for_agent(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
) -> Result<Vec<UserHostPolicyRecord>> {
    let rows = sqlx::query(
        "SELECT capability_id, host, policy
         FROM user_network_host_policies
         WHERE user_id = ? AND agent_id = ?",
    )
    .bind(user_id)
    .bind(agent_id)
    .fetch_all(db)
    .await?;

    rows.into_iter()
        .map(|row| {
            let policy: String = row.get("policy");
            Ok(UserHostPolicyRecord {
                capability_id: row.get("capability_id"),
                host: row.get("host"),
                policy: HostPolicy::from_str(&policy)?,
            })
        })
        .collect()
}

pub async fn set_user_access_override(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    access: NetworkAccessPolicy,
) -> Result<()> {
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO user_network_access_policies (id, user_id, agent_id, capability_id, access)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(user_id, agent_id, capability_id)
         DO UPDATE SET access = excluded.access, updated_at = datetime('now')",
    )
    .bind(id)
    .bind(user_id)
    .bind(agent_id)
    .bind(capability_id)
    .bind(access.as_str())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn set_user_host_override(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    host: &str,
    policy: HostPolicy,
) -> Result<()> {
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO user_network_host_policies (id, user_id, agent_id, capability_id, host, policy)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(user_id, agent_id, capability_id, host)
         DO UPDATE SET policy = excluded.policy, updated_at = datetime('now')",
    )
    .bind(id)
    .bind(user_id)
    .bind(agent_id)
    .bind(capability_id)
    .bind(host)
    .bind(policy.as_str())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete_user_host_override(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    host: &str,
) -> Result<()> {
    sqlx::query(
        "DELETE FROM user_network_host_policies
         WHERE user_id = ? AND agent_id = ? AND capability_id = ? AND host = ?",
    )
    .bind(user_id)
    .bind(agent_id)
    .bind(capability_id)
    .bind(host)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn resolve_effective_policy(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    manifest_network: &NetworkPolicy,
) -> Result<EffectiveNetworkPolicy> {
    let access = sqlx::query(
        "SELECT access FROM user_network_access_policies
         WHERE user_id = ? AND agent_id = ? AND capability_id = ?",
    )
    .bind(user_id)
    .bind(agent_id)
    .bind(capability_id)
    .fetch_optional(db)
    .await?
    .map(|row| row.get::<String, _>("access"))
    .map(|s| NetworkAccessPolicy::from_str(&s))
    .transpose()?;

    let host_rows = sqlx::query(
        "SELECT host, policy
         FROM user_network_host_policies
         WHERE user_id = ? AND agent_id = ? AND capability_id = ?",
    )
    .bind(user_id)
    .bind(agent_id)
    .bind(capability_id)
    .fetch_all(db)
    .await?;

    let mut host_overrides: Vec<(String, HostPolicy)> = Vec::with_capacity(host_rows.len());
    for row in host_rows {
        let host: String = row.get("host");
        let policy: String = row.get("policy");
        host_overrides.push((host, HostPolicy::from_str(&policy)?));
    }

    Ok(build_effective_policy(
        manifest_network,
        access,
        &host_overrides,
    ))
}

pub fn build_effective_policy(
    manifest_network: &NetworkPolicy,
    access_override: Option<NetworkAccessPolicy>,
    host_overrides: &[(String, HostPolicy)],
) -> EffectiveNetworkPolicy {
    if access_override == Some(NetworkAccessPolicy::Deny) {
        return EffectiveNetworkPolicy {
            mode: NetworkMode::None,
            allowed_hosts: Vec::new(),
            denied_hosts: Vec::new(),
        };
    }

    let mut mode = manifest_network.mode.clone();
    let mut allow_set: HashSet<String> = manifest_network
        .hosts
        .iter()
        .map(|h| h.to_ascii_lowercase())
        .collect();
    let mut deny_set: HashSet<String> = HashSet::new();

    for (host, policy) in host_overrides {
        let normalized = host.to_ascii_lowercase();
        match policy {
            HostPolicy::Allow => {
                allow_set.insert(normalized);
            }
            HostPolicy::Deny => {
                deny_set.insert(normalized.clone());
                allow_set.remove(&normalized);
            }
        }
    }

    if access_override == Some(NetworkAccessPolicy::Allow)
        && mode == NetworkMode::None
        && !allow_set.is_empty()
    {
        mode = NetworkMode::Allowlist;
    }
    if mode == NetworkMode::None && !allow_set.is_empty() {
        mode = NetworkMode::Allowlist;
    }

    let mut allowed_hosts: Vec<String> = allow_set.into_iter().collect();
    let mut denied_hosts: Vec<String> = deny_set.into_iter().collect();
    allowed_hosts.sort();
    denied_hosts.sort();

    EffectiveNetworkPolicy {
        mode,
        allowed_hosts,
        denied_hosts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_access_override_blocks_everything() {
        let manifest = NetworkPolicy {
            mode: NetworkMode::Any,
            hosts: vec!["api.example.com".to_string()],
        };
        let effective = build_effective_policy(
            &manifest,
            Some(NetworkAccessPolicy::Deny),
            &[("api.example.com".to_string(), HostPolicy::Allow)],
        );
        assert_eq!(effective.mode, NetworkMode::None);
        assert!(effective.allowed_hosts.is_empty());
    }

    #[test]
    fn user_allow_host_enables_allowlist_when_manifest_is_none() {
        let manifest = NetworkPolicy {
            mode: NetworkMode::None,
            hosts: Vec::new(),
        };
        let effective = build_effective_policy(
            &manifest,
            None,
            &[("pypi.org:443".to_string(), HostPolicy::Allow)],
        );
        assert_eq!(effective.mode, NetworkMode::Allowlist);
        assert_eq!(effective.allowed_hosts, vec!["pypi.org:443"]);
    }

    #[test]
    fn deny_host_removes_manifest_allow_host() {
        let manifest = NetworkPolicy {
            mode: NetworkMode::Allowlist,
            hosts: vec!["api.example.com:443".to_string()],
        };
        let effective = build_effective_policy(
            &manifest,
            None,
            &[("api.example.com:443".to_string(), HostPolicy::Deny)],
        );
        assert_eq!(effective.mode, NetworkMode::Allowlist);
        assert!(effective.allowed_hosts.is_empty());
        assert_eq!(effective.denied_hosts, vec!["api.example.com:443"]);
    }
}
