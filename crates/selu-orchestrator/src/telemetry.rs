use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::Row;
use tracing::{info, warn};

use crate::state::AppState;

#[derive(Debug, Serialize)]
struct TelemetryHeartbeatPayload {
    version: String,
}

pub async fn send_heartbeat(state: &AppState) -> Result<()> {
    if !telemetry_enabled(&state.db).await? {
        return Ok(());
    }

    let api_base = telemetry_api_base_url(&state.config.marketplace_url)
        .context("Could not derive telemetry API base URL from marketplace_url")?;

    let instance_id = crate::persistence::db::get_instance_id(&state.db)
        .await
        .context("Failed to load instance_id for telemetry")?;
    let version = current_version(&state.db)
        .await
        .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string());

    let client = reqwest::Client::new();
    let payload = TelemetryHeartbeatPayload {
        version: version.clone(),
    };

    client
        .post(format!("{}/telemetry/installations/heartbeat", api_base))
        .header("x-instance-id", &instance_id)
        .json(&payload)
        .send()
        .await
        .context("Failed to send installation telemetry heartbeat")?
        .error_for_status()
        .context("Installation telemetry heartbeat returned non-success status")?;

    let installed_agents = load_marketplace_agents(&state.db).await?;
    let mut sent_agents = 0usize;
    for (agent_id, agent_version) in installed_agents {
        let version_to_send = if agent_version.trim().is_empty() {
            version.clone()
        } else {
            agent_version
        };
        let agent_payload = TelemetryHeartbeatPayload {
            version: version_to_send,
        };

        let resp = client
            .post(format!(
                "{}/telemetry/agents/{}/heartbeat",
                api_base, agent_id
            ))
            .header("x-instance-id", &instance_id)
            .json(&agent_payload)
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                sent_agents += 1;
            }
            Ok(r) => warn!(
                status = %r.status(),
                "Agent telemetry heartbeat returned non-success status"
            ),
            Err(e) => warn!("Agent telemetry heartbeat failed: {e}"),
        }
    }

    info!(agents = sent_agents, "Telemetry heartbeat sent");

    Ok(())
}

async fn telemetry_enabled(db: &sqlx::SqlitePool) -> Result<bool> {
    let row = sqlx::query(
        "SELECT COALESCE(installation_telemetry_opt_out, 0) AS installation_telemetry_opt_out
         FROM system_update_settings
         WHERE id = 'global'",
    )
    .fetch_optional(db)
    .await
    .context("Failed to load telemetry opt-out setting")?;

    let opted_out = row
        .and_then(|r| r.try_get::<i64, _>("installation_telemetry_opt_out").ok())
        .unwrap_or(0)
        != 0;

    Ok(!opted_out)
}

async fn current_version(db: &sqlx::SqlitePool) -> Result<String> {
    let fallback = env!("CARGO_PKG_VERSION").to_string();
    let row = sqlx::query(
        "SELECT COALESCE(NULLIF(installed_release_version, ''), NULLIF(installed_version, ''), ?) AS version
         FROM system_update_state
         WHERE id = 'global'",
    )
    .bind(&fallback)
    .fetch_optional(db)
    .await
    .context("Failed to load current version for telemetry")?;

    Ok(row
        .and_then(|r| r.try_get::<String, _>("version").ok())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(fallback))
}

async fn load_marketplace_agents(db: &sqlx::SqlitePool) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query(
        "SELECT id, COALESCE(version, '') AS version
         FROM agents
         WHERE id <> 'default' AND setup_complete = 1",
    )
    .fetch_all(db)
    .await
    .context("Failed to load marketplace agents for telemetry")?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.try_get("id").unwrap_or_default();
        let version: String = row.try_get("version").unwrap_or_default();
        if !id.trim().is_empty() {
            out.push((id, version));
        }
    }
    Ok(out)
}

fn telemetry_api_base_url(marketplace_url: &str) -> Option<String> {
    let trimmed = marketplace_url.trim_end_matches('/');
    let marker = "/marketplace/agents";
    trimmed
        .find(marker)
        .map(|idx| trimmed[..idx].to_string())
        .filter(|s| !s.is_empty())
}

pub async fn send_heartbeat_with_logging(state: &AppState) {
    if let Err(e) = send_heartbeat(state).await {
        warn!("Telemetry heartbeat failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::telemetry_api_base_url;

    #[test]
    fn derives_api_base_url_from_marketplace_endpoint() {
        assert_eq!(
            telemetry_api_base_url("https://selu.bot/api/marketplace/agents"),
            Some("https://selu.bot/api".to_string())
        );
    }

    #[test]
    fn fails_for_non_marketplace_url() {
        assert_eq!(
            telemetry_api_base_url("https://selu.bot/something-else"),
            None
        );
    }
}
