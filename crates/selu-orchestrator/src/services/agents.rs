//! Native domain operations for agent administration.
//!
//! This module intentionally contains no Axum extractors, responses, redirects,
//! or form handling. Both the versioned API and background update jobs use the
//! same typed operations.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;
use sqlx::SqlitePool;
use thiserror::Error;
use uuid::Uuid;

use crate::agents::{
    loader::StepType,
    localization,
    marketplace::{self, MarketplaceEntry},
    model,
    runtime_limits::{self, AutonomyLevel},
};
use crate::capabilities::discovery::sync_dynamic_tools_for_capability;
use crate::capabilities::manifest::{CredentialScope, ToolSource};
use crate::permissions::{network_policy, tool_policy};
use crate::state::{AgentUpdateJob, AppState};

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("agent not found")]
    AgentNotFound,
    #[error("capability not found")]
    CapabilityNotFound,
    #[error("agent setup not found")]
    SetupNotFound,
    #[error("improvement insight not found")]
    InsightNotFound,
    #[error("agent update job not found")]
    UpdateJobNotFound,
    #[error("agent update job belongs to another user")]
    UpdateJobForbidden,
    #[error("invalid request: {0}")]
    Validation(&'static str),
    #[error("operation cannot be completed: {0}")]
    Conflict(&'static str),
    #[error("Docker is unavailable")]
    DockerUnavailable,
    #[error("marketplace request failed")]
    Upstream,
    #[error("database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("agent operation failed")]
    Operation(#[source] anyhow::Error),
}

impl ServiceError {
    fn operation(error: impl Into<anyhow::Error>) -> Self {
        Self::Operation(error.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallOutcome {
    pub setup_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageDownloadOutcome {
    Downloaded,
    AlreadyAvailable,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UpdateJobView {
    pub job_id: String,
    pub agent_id: String,
    pub agent_name: String,
    pub target_version: String,
    pub progress: u8,
    pub message_key: String,
    pub done: bool,
    pub success: bool,
    pub redirect_to: Option<String>,
    pub error_key: Option<String>,
}

#[derive(Debug, Serialize)]
struct SubmitRatingRequest {
    rating: u8,
}

pub async fn install_agent(
    state: &AppState,
    entry_json: &str,
) -> Result<InstallOutcome, ServiceError> {
    let entry = parse_marketplace_entry(entry_json)?;
    let docker = bollard::Docker::connect_with_local_defaults()
        .map_err(|_| ServiceError::DockerUnavailable)?;
    let definition = marketplace::install_agent(
        &entry,
        &state.config.installed_agents_dir,
        &state.db,
        &state.agents,
        &docker,
        &state.capabilities,
        &state.credentials,
        &state.docker_storage,
    )
    .await
    .map_err(ServiceError::operation)?;
    let setup_required =
        marketplace::agent_requires_setup(&state.db, &state.credentials, &entry.id, &definition)
            .await
            .map_err(ServiceError::operation)?;
    Ok(InstallOutcome { setup_required })
}

pub async fn complete_setup(
    state: &AppState,
    user_id: &str,
    agent_id: &str,
    values: &HashMap<String, String>,
) -> Result<(), ServiceError> {
    ensure_safe_agent_id(agent_id)?;

    // Setup is fail-closed: a retry or a discovery error must never leave a
    // previously loaded definition active while setup_complete says otherwise.
    sqlx::query("UPDATE agents SET setup_complete = 0 WHERE id = ?")
        .bind(agent_id)
        .execute(&state.db)
        .await?;
    let current = state.agents.load();
    let mut inactive = (**current).clone();
    inactive.remove(agent_id);
    state.agents.store(std::sync::Arc::new(inactive));

    let path = std::path::Path::new(&state.config.installed_agents_dir).join(agent_id);
    let definition = crate::agents::loader::load_one(&path)
        .await
        .map_err(|_| ServiceError::SetupNotFound)?;

    for step in &definition.install_steps {
        if step.step_type != StepType::Input {
            continue;
        }
        let Some(target) = step.store_as.as_ref() else {
            continue;
        };
        let Some(value) = values.get(&format!("step_{}", step.id)) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match target.scope.as_str() {
            "system_credential" => state
                .credentials
                .set_system(&target.capability_id, &target.credential_name, value)
                .await
                .map_err(ServiceError::operation)?,
            "user_credential" => state
                .credentials
                .set_user(
                    user_id,
                    &target.capability_id,
                    &target.credential_name,
                    value,
                )
                .await
                .map_err(ServiceError::operation)?,
            _ => return Err(ServiceError::Validation("invalid credential target")),
        }
    }

    // Unlike startup synchronization, setup discovery is not best-effort. The
    // agent remains absent from the active map and setup_complete remains false
    // until every runnable dynamic capability has discovered valid tools.
    for (capability_id, manifest) in &definition.capability_manifests {
        if manifest.tool_source != ToolSource::Dynamic {
            continue;
        }
        sync_dynamic_tools_for_capability(
            &state.db,
            &state.capabilities,
            &state.credentials,
            agent_id,
            capability_id,
            manifest,
            &definition.capability_manifests,
        )
        .await
        .map_err(ServiceError::operation)?;
    }

    let mut policies = Vec::new();
    for key in values
        .keys()
        .filter_map(|key| key.strip_prefix("policy_val_"))
    {
        let Some(capability_id) = values.get(&format!("policy_cap_{key}")) else {
            return Err(ServiceError::Validation("missing policy capability"));
        };
        let Some(tool_name) = values.get(&format!("policy_tool_{key}")) else {
            return Err(ServiceError::Validation("missing policy tool"));
        };
        let policy = values
            .get(&format!("policy_val_{key}"))
            .ok_or(ServiceError::Validation("missing policy value"))
            .and_then(|value| {
                tool_policy::ToolPolicy::from_str(value)
                    .map_err(|_| ServiceError::Validation("invalid tool policy"))
            })?;
        policies.push((capability_id.clone(), tool_name.clone(), policy));
    }
    if !policies.is_empty() {
        tool_policy::set_global_policies(&state.db, agent_id, &policies)
            .await
            .map_err(ServiceError::operation)?;
    }

    marketplace::complete_setup(
        agent_id,
        &state.config.installed_agents_dir,
        &state.db,
        &state.agents,
    )
    .await
    .map_err(ServiceError::operation)
}

pub async fn set_agent_model(
    db: &SqlitePool,
    agent_id: &str,
    provider_id: &str,
    model_id: &str,
    temperature: f32,
) -> Result<(), ServiceError> {
    validate_model(provider_id, model_id, temperature)?;
    ensure_agent_exists(db, agent_id).await?;
    model::set_agent_model(db, agent_id, provider_id, model_id, temperature)
        .await
        .map_err(ServiceError::operation)
}

pub async fn set_agent_image_model(
    db: &SqlitePool,
    agent_id: &str,
    provider_id: &str,
    model_id: &str,
) -> Result<(), ServiceError> {
    ensure_agent_exists(db, agent_id).await?;
    let provider_id = provider_id.trim();
    let model_id = model_id.trim();
    if provider_id.is_empty() != model_id.is_empty() {
        return Err(ServiceError::Validation(
            "image provider and model must be set together",
        ));
    }
    if provider_id.is_empty() {
        model::clear_agent_image_model(db, agent_id)
            .await
            .map_err(ServiceError::operation)
    } else {
        model::set_agent_image_model(db, agent_id, provider_id, model_id)
            .await
            .map_err(ServiceError::operation)
    }
}

pub async fn set_default_model(
    db: &SqlitePool,
    provider_id: &str,
    model_id: &str,
    temperature: f32,
) -> Result<(), ServiceError> {
    validate_model(provider_id, model_id, temperature)?;
    model::set_global_default(db, provider_id.trim(), model_id.trim(), temperature)
        .await
        .map_err(ServiceError::operation)
}

pub async fn set_default_image_model(
    db: &SqlitePool,
    provider_id: &str,
    model_id: &str,
) -> Result<(), ServiceError> {
    let provider_id = provider_id.trim();
    let model_id = model_id.trim();
    if provider_id.is_empty() != model_id.is_empty() {
        return Err(ServiceError::Validation(
            "image provider and model must be set together",
        ));
    }
    model::set_global_image_default(db, provider_id, model_id)
        .await
        .map_err(ServiceError::operation)
}

pub async fn set_runtime_settings(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
    autonomy_level: &str,
    use_advanced_limits: bool,
    max_tool_loop_iterations: Option<u32>,
    max_delegation_hops: Option<i32>,
) -> Result<(), ServiceError> {
    ensure_agent_exists(db, agent_id).await?;
    let level = AutonomyLevel::parse(autonomy_level)
        .ok_or(ServiceError::Validation("invalid autonomy level"))?;
    let defaults = runtime_limits::limits_for_autonomy(level);
    let iterations = if use_advanced_limits {
        max_tool_loop_iterations.ok_or(ServiceError::Validation("missing iteration limit"))?
    } else {
        defaults.max_tool_loop_iterations
    };
    let delegation_hops = if use_advanced_limits {
        let value =
            max_delegation_hops.ok_or(ServiceError::Validation("missing delegation limit"))?;
        if value < 0 {
            return Err(ServiceError::Validation(
                "delegation limit cannot be negative",
            ));
        }
        value
    } else {
        defaults.max_delegation_hops
    };

    sqlx::query(
        "INSERT INTO user_agent_runtime_settings (
            user_id, agent_id, autonomy_level, use_advanced_limits,
            max_tool_loop_iterations, max_delegation_hops, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, datetime('now'))
         ON CONFLICT(user_id, agent_id) DO UPDATE SET
            autonomy_level = excluded.autonomy_level,
            use_advanced_limits = excluded.use_advanced_limits,
            max_tool_loop_iterations = excluded.max_tool_loop_iterations,
            max_delegation_hops = excluded.max_delegation_hops,
            updated_at = datetime('now')",
    )
    .bind(user_id)
    .bind(agent_id)
    .bind(level.as_str())
    .bind(i64::from(use_advanced_limits))
    .bind(i64::from(iterations))
    .bind(i64::from(delegation_hops))
    .execute(db)
    .await?;
    Ok(())
}

pub async fn set_automation_enabled(
    state: &AppState,
    user_id: &str,
    language: &str,
    agent_id: &str,
    enabled: bool,
) -> Result<(), ServiceError> {
    ensure_safe_agent_id(agent_id)?;
    let agent = {
        let agents = state.agents.load();
        agents
            .get(agent_id)
            .cloned()
            .ok_or(ServiceError::AgentNotFound)?
    };
    if agent.automation.schedules.is_empty() {
        return Err(ServiceError::Conflict("automation is not supported"));
    }

    if !enabled {
        for preset in &agent.automation.schedules {
            sqlx::query(
                "UPDATE schedules SET active = 0
                 WHERE user_id = ? AND agent_id = ? AND name = ?",
            )
            .bind(user_id)
            .bind(agent_id)
            .bind(automation_schedule_name(agent_id, &preset.id))
            .execute(&state.db)
            .await?;
        }
        return Ok(());
    }

    let pipe_ids = sqlx::query_scalar::<_, String>(
        "SELECT id FROM pipes
         WHERE user_id = ? AND active = 1 AND default_agent_id = ? ORDER BY name",
    )
    .bind(user_id)
    .bind(agent_id)
    .fetch_all(&state.db)
    .await?;
    if pipe_ids.is_empty() {
        return Err(ServiceError::Conflict("automation needs an active pipe"));
    }

    for (capability_id, manifest) in &agent.capability_manifests {
        for credential in manifest
            .credentials
            .iter()
            .filter(|credential| credential.required)
        {
            let present = match credential.scope {
                CredentialScope::System => state
                    .credentials
                    .get_system(capability_id, &credential.name)
                    .await
                    .map_err(ServiceError::operation)?
                    .is_some(),
                CredentialScope::User => state
                    .credentials
                    .get_user(user_id, capability_id, &credential.name)
                    .await
                    .map_err(ServiceError::operation)?
                    .is_some(),
            };
            if !present {
                return Err(ServiceError::Conflict(
                    "automation needs required credentials",
                ));
            }
        }
    }

    let timezone = sqlx::query_scalar::<_, String>("SELECT timezone FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(&state.db)
        .await?
        .unwrap_or_else(|| "UTC".to_string());
    let now = chrono::Utc::now();

    for preset in &agent.automation.schedules {
        crate::schedules::validate_cron(&preset.cron_expression)
            .map_err(|_| ServiceError::Validation("invalid automation schedule"))?;
        let next_run = crate::schedules::compute_next_run(&preset.cron_expression, &timezone, now)
            .map_err(|_| ServiceError::Validation("invalid automation schedule"))?
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let prompt = localization::localized_schedule_prompt(&agent, preset, language);
        let description =
            localization::localized_schedule_cron_description(&agent, preset, language);
        let name = automation_schedule_name(agent_id, &preset.id);
        let existing = sqlx::query_scalar::<_, String>(
            "SELECT id FROM schedules
             WHERE user_id = ? AND agent_id = ? AND name = ? LIMIT 1",
        )
        .bind(user_id)
        .bind(agent_id)
        .bind(&name)
        .fetch_optional(&state.db)
        .await?;

        let schedule_id = if let Some(schedule_id) = existing {
            sqlx::query(
                "UPDATE schedules SET prompt = ?, cron_expression = ?, cron_description = ?,
                 active = 1, one_shot = 0, next_run_at = ?, agent_id = ? WHERE id = ?",
            )
            .bind(&prompt)
            .bind(&preset.cron_expression)
            .bind(&description)
            .bind(&next_run)
            .bind(agent_id)
            .bind(&schedule_id)
            .execute(&state.db)
            .await?;
            sqlx::query("DELETE FROM schedule_pipes WHERE schedule_id = ?")
                .bind(&schedule_id)
                .execute(&state.db)
                .await?;
            for pipe_id in &pipe_ids {
                sqlx::query("INSERT INTO schedule_pipes (schedule_id, pipe_id) VALUES (?, ?)")
                    .bind(&schedule_id)
                    .bind(pipe_id)
                    .execute(&state.db)
                    .await?;
            }
            schedule_id
        } else {
            crate::schedules::create_schedule(
                &state.db,
                user_id,
                Some(agent_id),
                &name,
                &prompt,
                &preset.cron_expression,
                &description,
                &timezone,
                &pipe_ids,
            )
            .await
            .map_err(ServiceError::operation)?
        };
        tracing::debug!(agent_id, schedule_id, "Enabled agent automation schedule");
    }
    Ok(())
}

pub async fn set_auto_update(
    db: &SqlitePool,
    agent_id: &str,
    enabled: bool,
) -> Result<(), ServiceError> {
    ensure_safe_agent_id(agent_id)?;
    let result = sqlx::query("UPDATE agents SET auto_update = ? WHERE id = ? AND is_bundled = 0")
        .bind(i64::from(enabled))
        .bind(agent_id)
        .execute(db)
        .await?;
    if result.rows_affected() == 0 {
        return match sqlx::query_scalar::<_, i64>("SELECT is_bundled FROM agents WHERE id = ?")
            .bind(agent_id)
            .fetch_optional(db)
            .await?
        {
            Some(_) => Err(ServiceError::Conflict(
                "bundled agents cannot enable automatic updates",
            )),
            None => Err(ServiceError::AgentNotFound),
        };
    }
    Ok(())
}

pub async fn set_permission(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    tool_name: &str,
    policy: &str,
    scope: &str,
) -> Result<(), ServiceError> {
    ensure_agent_exists(db, agent_id).await?;
    let policy = tool_policy::ToolPolicy::from_str(policy)
        .map_err(|_| ServiceError::Validation("invalid tool policy"))?;
    match scope {
        "global" => tool_policy::set_global_policies(
            db,
            agent_id,
            &[(capability_id.to_string(), tool_name.to_string(), policy)],
        )
        .await
        .map_err(ServiceError::operation),
        "user" => tool_policy::set_policies(
            db,
            user_id,
            agent_id,
            &[(capability_id.to_string(), tool_name.to_string(), policy)],
        )
        .await
        .map_err(ServiceError::operation),
        _ => Err(ServiceError::Validation("invalid permission scope")),
    }
}

pub async fn reset_permission(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    tool_name: &str,
) -> Result<(), ServiceError> {
    ensure_agent_exists(db, agent_id).await?;
    tool_policy::delete_user_policy(db, user_id, agent_id, capability_id, tool_name)
        .await
        .map_err(ServiceError::operation)
}

pub async fn set_network_access(
    state: &AppState,
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    access: &str,
) -> Result<(), ServiceError> {
    ensure_capability_exists(state, agent_id, capability_id)?;
    let access = network_policy::NetworkAccessPolicy::from_str(access)
        .map_err(|_| ServiceError::Validation("invalid network access policy"))?;
    network_policy::set_user_access_override(&state.db, user_id, agent_id, capability_id, access)
        .await
        .map_err(ServiceError::operation)?;
    state
        .capabilities
        .invalidate_network_policy_cache(user_id, agent_id, capability_id)
        .await;
    Ok(())
}

pub async fn set_network_host(
    state: &AppState,
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    host: &str,
    policy: &str,
) -> Result<(), ServiceError> {
    ensure_capability_exists(state, agent_id, capability_id)?;
    let host = network_policy::normalize_host_entry(host)
        .map_err(|_| ServiceError::Validation("invalid network host"))?;
    let policy = network_policy::HostPolicy::from_str(policy)
        .map_err(|_| ServiceError::Validation("invalid network host policy"))?;
    network_policy::set_user_host_override(
        &state.db,
        user_id,
        agent_id,
        capability_id,
        &host,
        policy,
    )
    .await
    .map_err(ServiceError::operation)?;
    state
        .capabilities
        .invalidate_network_policy_cache(user_id, agent_id, capability_id)
        .await;
    Ok(())
}

pub async fn remove_network_host(
    state: &AppState,
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    host: &str,
) -> Result<(), ServiceError> {
    ensure_capability_exists(state, agent_id, capability_id)?;
    let host = network_policy::normalize_host_entry(host)
        .map_err(|_| ServiceError::Validation("invalid network host"))?;
    network_policy::delete_user_host_override(&state.db, user_id, agent_id, capability_id, &host)
        .await
        .map_err(ServiceError::operation)?;
    state
        .capabilities
        .invalidate_network_policy_cache(user_id, agent_id, capability_id)
        .await;
    Ok(())
}

fn validate_credential_target(
    state: &AppState,
    agent_id: &str,
    capability_id: &str,
    credential_name: &str,
    scope: &str,
) -> Result<(), ServiceError> {
    ensure_safe_agent_id(agent_id)?;
    let agents = state.agents.load();
    let agent = agents.get(agent_id).ok_or(ServiceError::AgentNotFound)?;
    let manifest = agent
        .capability_manifests
        .get(capability_id)
        .ok_or(ServiceError::CapabilityNotFound)?;
    let credential = manifest
        .credentials
        .iter()
        .find(|credential| credential.name == credential_name)
        .ok_or(ServiceError::CapabilityNotFound)?;
    let declared_scope = match credential.scope {
        CredentialScope::System => "system",
        CredentialScope::User => "user",
    };
    if scope != declared_scope {
        return Err(ServiceError::Validation(
            "credential scope does not match manifest",
        ));
    }
    Ok(())
}

pub async fn set_credential(
    state: &AppState,
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    credential_name: &str,
    scope: &str,
    value: &str,
) -> Result<(), ServiceError> {
    validate_credential_target(state, agent_id, capability_id, credential_name, scope)?;
    if value.is_empty() {
        return Err(ServiceError::Validation("credential value is required"));
    }
    match scope {
        "system" => state
            .credentials
            .set_system(capability_id, credential_name, value)
            .await
            .map_err(ServiceError::operation),
        "user" => state
            .credentials
            .set_user(user_id, capability_id, credential_name, value)
            .await
            .map_err(ServiceError::operation),
        _ => Err(ServiceError::Validation("invalid credential scope")),
    }
}

pub async fn remove_credential(
    state: &AppState,
    user_id: &str,
    agent_id: &str,
    capability_id: &str,
    name: &str,
    scope: &str,
) -> Result<(), ServiceError> {
    validate_credential_target(state, agent_id, capability_id, name, scope)?;
    match scope {
        "system" => state
            .credentials
            .delete_system(capability_id, name)
            .await
            .map_err(ServiceError::operation),
        "user" => state
            .credentials
            .delete_user(user_id, capability_id, name)
            .await
            .map_err(ServiceError::operation),
        _ => Err(ServiceError::Validation("invalid credential scope")),
    }
}

pub async fn remove_storage(
    db: &SqlitePool,
    agent_id: &str,
    entry_id: &str,
) -> Result<(), ServiceError> {
    ensure_safe_agent_id(agent_id)?;
    let result = sqlx::query("DELETE FROM agent_storage WHERE id = ? AND agent_id = ?")
        .bind(entry_id)
        .bind(agent_id)
        .execute(db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(ServiceError::AgentNotFound);
    }
    Ok(())
}

pub async fn remove_memory(
    db: &SqlitePool,
    agent_id: &str,
    memory_id: &str,
) -> Result<(), ServiceError> {
    ensure_safe_agent_id(agent_id)?;
    let result = sqlx::query("DELETE FROM agent_memories WHERE id = ? AND agent_id = ?")
        .bind(memory_id)
        .bind(agent_id)
        .execute(db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(ServiceError::AgentNotFound);
    }
    Ok(())
}

pub async fn download_capability_image(
    state: &AppState,
    agent_id: &str,
    capability_id: &str,
) -> Result<ImageDownloadOutcome, ServiceError> {
    ensure_safe_agent_id(agent_id)?;
    let image = {
        let agents = state.agents.load();
        let agent = agents.get(agent_id).ok_or(ServiceError::AgentNotFound)?;
        agent
            .capability_manifests
            .get(capability_id)
            .ok_or(ServiceError::CapabilityNotFound)?
            .image
            .clone()
    };
    match state
        .capabilities
        .ensure_image_available(agent_id, &image)
        .await
    {
        Ok(true) => Ok(ImageDownloadOutcome::Downloaded),
        Ok(false) => Ok(ImageDownloadOutcome::AlreadyAvailable),
        Err(error) => Err(ServiceError::operation(error)),
    }
}

pub async fn update_improvement(
    db: &SqlitePool,
    user_id: &str,
    agent_id: &str,
    action: &str,
    insight_id: Option<&str>,
) -> Result<(), ServiceError> {
    ensure_safe_agent_id(agent_id)?;
    if action == "reset" {
        return crate::agents::improvement::reset_all(db, agent_id, user_id)
            .await
            .map_err(ServiceError::operation);
    }
    let status = match action {
        "pause" => "paused",
        "activate" => "active",
        "reject" => "rejected",
        _ => return Err(ServiceError::Validation("invalid improvement action")),
    };
    let insight_id = insight_id.ok_or(ServiceError::Validation("insight id is required"))?;
    let changed = crate::agents::improvement::update_insight_status(
        db, insight_id, agent_id, user_id, status,
    )
    .await
    .map_err(ServiceError::operation)?;
    if !changed {
        return Err(ServiceError::InsightNotFound);
    }
    Ok(())
}

pub async fn submit_rating(
    state: &AppState,
    agent_id: &str,
    rating: u8,
) -> Result<(), ServiceError> {
    ensure_safe_agent_id(agent_id)?;
    if !(1..=5).contains(&rating) {
        return Err(ServiceError::Validation("rating must be between 1 and 5"));
    }
    let installed = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM agents WHERE id = ? AND setup_complete = 1 LIMIT 1",
    )
    .bind(agent_id)
    .fetch_optional(&state.db)
    .await?
    .is_some();
    if !installed {
        return Err(ServiceError::AgentNotFound);
    }
    let instance_id = crate::persistence::db::get_instance_id(&state.db)
        .await
        .map_err(ServiceError::operation)?;
    let base = ratings_api_base_url(&state.config.marketplace_url)
        .ok_or(ServiceError::Conflict("ratings are unavailable"))?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| ServiceError::Upstream)?;
    let response = client
        .post(format!("{base}/ratings/agents/{agent_id}"))
        .header("x-instance-id", instance_id)
        .json(&SubmitRatingRequest { rating })
        .send()
        .await
        .map_err(|_| ServiceError::Upstream)?;
    if !response.status().is_success() {
        return Err(ServiceError::Upstream);
    }
    Ok(())
}

pub async fn uninstall_agent(state: &AppState, agent_id: &str) -> Result<(), ServiceError> {
    ensure_agent_exists(&state.db, agent_id).await?;
    marketplace::uninstall_agent(
        agent_id,
        &state.config.installed_agents_dir,
        &state.db,
        &state.agents,
        &state.capabilities,
        &state.docker_storage,
    )
    .await
    .map_err(ServiceError::operation)
}

pub async fn check_updates(state: &AppState) -> Result<usize, ServiceError> {
    marketplace::auto_update_agents(
        &state.config.marketplace_url,
        &state.config.installed_agents_dir,
        &state.db,
        &state.agents,
        &state.capabilities,
        &state.credentials,
        &state.docker_storage,
    )
    .await
    .map_err(ServiceError::operation)
}

pub async fn start_update(
    state: &AppState,
    owner_user_id: &str,
    language: &str,
    entry_json: &str,
) -> Result<String, ServiceError> {
    let entry = parse_marketplace_entry(entry_json)?;
    ensure_agent_exists(&state.db, &entry.id).await?;
    {
        let jobs = state.agent_update_jobs.lock().await;
        if let Some((job_id, _)) = jobs.iter().find(|(_, job)| {
            job.owner_user_id == owner_user_id && job.agent_id == entry.id && !job.done
        }) {
            return Ok(job_id.clone());
        }
    }

    let job_id = Uuid::new_v4().to_string();
    state.agent_update_jobs.lock().await.insert(
        job_id.clone(),
        AgentUpdateJob {
            owner_user_id: owner_user_id.to_string(),
            agent_id: entry.id.clone(),
            agent_name: entry.localized_name(language),
            target_version: entry.version.clone(),
            progress: 5,
            message_key: "agents.update.phase.preparing".to_string(),
            done: false,
            success: false,
            redirect_to: None,
            error_key: None,
            updated_at: Instant::now(),
        },
    );

    let update_state = state.clone();
    let update_job_id = job_id.clone();
    tokio::spawn(async move {
        run_update_job(update_state, entry, update_job_id).await;
    });
    Ok(job_id)
}

pub async fn update_status(
    state: &AppState,
    owner_user_id: &str,
    job_id: &str,
) -> Result<UpdateJobView, ServiceError> {
    let jobs = state.agent_update_jobs.lock().await;
    let job = jobs.get(job_id).ok_or(ServiceError::UpdateJobNotFound)?;
    if job.owner_user_id != owner_user_id {
        return Err(ServiceError::UpdateJobForbidden);
    }
    Ok(UpdateJobView {
        job_id: job_id.to_string(),
        agent_id: job.agent_id.clone(),
        agent_name: job.agent_name.clone(),
        target_version: job.target_version.clone(),
        progress: job.progress,
        message_key: job.message_key.clone(),
        done: job.done,
        success: job.success,
        redirect_to: job.redirect_to.clone(),
        error_key: job.error_key.clone(),
    })
}

fn parse_marketplace_entry(entry_json: &str) -> Result<MarketplaceEntry, ServiceError> {
    let entry: MarketplaceEntry = serde_json::from_str(entry_json)
        .map_err(|_| ServiceError::Validation("invalid marketplace entry"))?;
    ensure_safe_agent_id(&entry.id)?;
    Ok(entry)
}

fn ensure_safe_agent_id(agent_id: &str) -> Result<(), ServiceError> {
    marketplace::validate_agent_id(agent_id)
        .map_err(|_| ServiceError::Validation("invalid agent id"))
}

fn validate_model(provider_id: &str, model_id: &str, temperature: f32) -> Result<(), ServiceError> {
    if provider_id.trim().is_empty() || model_id.trim().is_empty() {
        return Err(ServiceError::Validation("provider and model are required"));
    }
    if !temperature.is_finite() || !(0.0..=2.0).contains(&temperature) {
        return Err(ServiceError::Validation(
            "temperature must be between 0 and 2",
        ));
    }
    Ok(())
}

async fn ensure_agent_exists(db: &SqlitePool, agent_id: &str) -> Result<(), ServiceError> {
    ensure_safe_agent_id(agent_id)?;
    let exists = sqlx::query_scalar::<_, i64>("SELECT 1 FROM agents WHERE id = ? LIMIT 1")
        .bind(agent_id)
        .fetch_optional(db)
        .await?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(ServiceError::AgentNotFound)
    }
}

fn ensure_capability_exists(
    state: &AppState,
    agent_id: &str,
    capability_id: &str,
) -> Result<(), ServiceError> {
    ensure_safe_agent_id(agent_id)?;
    let agents = state.agents.load();
    let agent = agents.get(agent_id).ok_or(ServiceError::AgentNotFound)?;
    if agent.capability_manifests.contains_key(capability_id) {
        Ok(())
    } else {
        Err(ServiceError::CapabilityNotFound)
    }
}

fn automation_schedule_name(agent_id: &str, preset_id: &str) -> String {
    format!("auto.{agent_id}.{preset_id}")
}

fn ratings_api_base_url(marketplace_url: &str) -> Option<String> {
    let trimmed = marketplace_url.trim_end_matches('/');
    let index = trimmed.find("/marketplace/agents")?;
    Some(trimmed[..index].to_string())
}

async fn set_update_progress(state: &AppState, job_id: &str, progress: u8, message_key: &str) {
    if let Some(job) = state.agent_update_jobs.lock().await.get_mut(job_id) {
        if !job.done {
            job.progress = progress;
            job.message_key = message_key.to_string();
            job.updated_at = Instant::now();
        }
    }
}

async fn run_update_job(state: AppState, entry: MarketplaceEntry, job_id: String) {
    set_update_progress(&state, &job_id, 10, "agents.update.phase.preparing").await;
    let docker = match bollard::Docker::connect_with_local_defaults() {
        Ok(docker) => docker,
        Err(error) => {
            tracing::error!(%error, "Failed to connect to Docker for agent update");
            finish_update_error(&state, &job_id, "agents.update.error.docker").await;
            return;
        }
    };

    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::unbounded_channel::<marketplace::PullProgress>();
    let progress_state = state.clone();
    let progress_job_id = job_id.clone();
    let progress_task = tokio::spawn(async move {
        while let Some(progress) = progress_rx.recv().await {
            tracing::debug!(
                image = %progress.image,
                overall_fraction = progress.overall_fraction,
                "Agent capability image pull progress"
            );
            let percent = (20.0 + progress.overall_fraction.clamp(0.0, 1.0) * 70.0).round() as u8;
            let mut jobs = progress_state.agent_update_jobs.lock().await;
            let Some(job) = jobs.get_mut(&progress_job_id) else {
                break;
            };
            if job.done {
                break;
            }
            job.progress = job.progress.max(percent.min(90));
            job.message_key = "agents.update.phase.downloading".to_string();
            job.updated_at = Instant::now();
        }
    });

    let result = marketplace::update_agent_with_progress(
        &entry,
        &state.config.installed_agents_dir,
        &state.db,
        &state.agents,
        &docker,
        &state.capabilities,
        &state.credentials,
        &state.docker_storage,
        Some(progress_tx),
    )
    .await;
    progress_task.abort();

    match result {
        Ok(definition) => {
            let setup_required = marketplace::agent_requires_setup(
                &state.db,
                &state.credentials,
                &entry.id,
                &definition,
            )
            .await
            .unwrap_or(true);
            if let Some(job) = state.agent_update_jobs.lock().await.get_mut(&job_id) {
                job.progress = 100;
                job.message_key = if setup_required {
                    "agents.update.success.setup_required"
                } else {
                    "agents.update.success.updated"
                }
                .to_string();
                job.done = true;
                job.success = true;
                job.redirect_to = setup_required.then(|| format!("{}/app/agents", state.base_path));
                job.updated_at = Instant::now();
            }
        }
        Err(error) => {
            tracing::error!(%error, agent_id = %entry.id, "Agent update failed");
            finish_update_error(&state, &job_id, "agents.update.error.failed").await;
        }
    }
}

async fn finish_update_error(state: &AppState, job_id: &str, error_key: &str) {
    if let Some(job) = state.agent_update_jobs.lock().await.get_mut(job_id) {
        job.progress = 100;
        job.message_key = error_key.to_string();
        job.done = true;
        job.success = false;
        job.error_key = Some(error_key.to_string());
        job.updated_at = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn migrated_db() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, display_name, password_hash)
             VALUES ('user-1', 'admin', 'Admin', 'hash')",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agents (id, display_name, version, is_bundled, setup_complete)
             VALUES ('agent-1', 'Agent', '1.0.0', 0, 1)",
        )
        .execute(&db)
        .await
        .unwrap();
        db
    }

    #[test]
    fn ratings_url_is_derived_only_from_the_marketplace_endpoint() {
        assert_eq!(
            ratings_api_base_url("https://example.test/marketplace/agents"),
            Some("https://example.test".to_string())
        );
        assert_eq!(
            ratings_api_base_url("https://example.test/marketplace/agents/"),
            Some("https://example.test".to_string())
        );
        assert_eq!(ratings_api_base_url("https://example.test/catalogue"), None);
    }

    #[test]
    fn model_validation_rejects_partial_and_unsafe_values() {
        assert!(matches!(
            validate_model("", "model", 0.7),
            Err(ServiceError::Validation(_))
        ));
        assert!(matches!(
            validate_model("provider", "model", f32::NAN),
            Err(ServiceError::Validation(_))
        ));
        assert!(matches!(
            validate_model("provider", "model", 2.1),
            Err(ServiceError::Validation(_))
        ));
        assert!(validate_model("provider", "model", 0.7).is_ok());
    }

    #[tokio::test]
    async fn runtime_settings_use_profile_defaults_unless_advanced() {
        let db = migrated_db().await;
        set_runtime_settings(&db, "user-1", "agent-1", "low", false, Some(999), Some(999))
            .await
            .unwrap();
        let row: sqlx::sqlite::SqliteRow = sqlx::query(
            "SELECT autonomy_level, use_advanced_limits, max_tool_loop_iterations,
                    max_delegation_hops
             FROM user_agent_runtime_settings WHERE user_id = 'user-1' AND agent_id = 'agent-1'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        use sqlx::Row;
        let defaults = runtime_limits::limits_for_autonomy(AutonomyLevel::Low);
        assert_eq!(row.get::<String, _>("autonomy_level"), "low");
        assert_eq!(row.get::<i64, _>("use_advanced_limits"), 0);
        assert_eq!(
            row.get::<i64, _>("max_tool_loop_iterations"),
            i64::from(defaults.max_tool_loop_iterations)
        );
        assert_eq!(
            row.get::<i64, _>("max_delegation_hops"),
            i64::from(defaults.max_delegation_hops)
        );
    }

    #[tokio::test]
    async fn bundled_agents_cannot_change_auto_update() {
        let db = migrated_db().await;
        sqlx::query("UPDATE agents SET is_bundled = 1 WHERE id = 'agent-1'")
            .execute(&db)
            .await
            .unwrap();
        assert!(matches!(
            set_auto_update(&db, "agent-1", true).await,
            Err(ServiceError::Conflict(_))
        ));
        assert!(matches!(
            set_auto_update(&db, "missing", true).await,
            Err(ServiceError::AgentNotFound)
        ));
    }
}
