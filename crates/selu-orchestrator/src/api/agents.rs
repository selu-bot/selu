use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put},
};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::{
    agents::{
        loader::StepType,
        localization,
        marketplace::{self},
        model,
    },
    api::auth::ApiAdmin,
    capabilities::{
        discovery::{load_discovered_tools, sync_dynamic_tools_for_capability},
        manifest::{CredentialScope, FilesystemPolicy, NetworkMode, ToolSource},
    },
    services::agents as agent_service,
    state::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/agents", get(list))
        .route("/api/v1/agents/defaults", patch(set_defaults))
        .route("/api/v1/agents/check-updates", post(check_updates))
        .route("/api/v1/agents/install", post(install))
        .route("/api/v1/agents/{agent_id}", get(detail).delete(uninstall))
        .route(
            "/api/v1/agents/{agent_id}/setup",
            get(setup).post(complete_setup),
        )
        .route(
            "/api/v1/agents/{agent_id}/setup/test/{step_id}",
            post(test_setup_step),
        )
        .route("/api/v1/agents/{agent_id}/model", patch(set_model))
        .route(
            "/api/v1/agents/{agent_id}/image-model",
            patch(set_image_model),
        )
        .route(
            "/api/v1/agents/{agent_id}/runtime-settings",
            patch(set_runtime),
        )
        .route(
            "/api/v1/agents/{agent_id}/automation",
            patch(toggle_automation),
        )
        .route(
            "/api/v1/agents/{agent_id}/auto-update",
            patch(toggle_auto_update),
        )
        .route(
            "/api/v1/agents/{agent_id}/permissions",
            put(set_permission).delete(reset_permission),
        )
        .route(
            "/api/v1/agents/{agent_id}/network/access",
            put(set_network_access),
        )
        .route(
            "/api/v1/agents/{agent_id}/network/hosts",
            put(set_network_host).delete(remove_network_host),
        )
        .route("/api/v1/agents/{agent_id}/credentials", put(set_credential))
        .route(
            "/api/v1/agents/{agent_id}/credentials/{scope}/{capability_id}/{name}",
            delete(remove_credential),
        )
        .route(
            "/api/v1/agents/{agent_id}/storage/{entry_id}",
            delete(remove_storage),
        )
        .route(
            "/api/v1/agents/{agent_id}/memory/{memory_id}",
            delete(remove_memory),
        )
        .route(
            "/api/v1/agents/{agent_id}/capabilities/{capability_id}/image",
            post(download_image),
        )
        .route(
            "/api/v1/agents/{agent_id}/improvement/{action}",
            post(improvement_action),
        )
        .route("/api/v1/agents/{agent_id}/rating", put(rate))
        .route("/api/v1/agents/updates", post(start_update))
        .route("/api/v1/agents/updates/{job_id}", get(update_status))
}

#[derive(Debug, Serialize)]
struct AgentsResponse {
    installed: Vec<InstalledAgentResponse>,
    marketplace: Vec<MarketplaceAgentResponse>,
    marketplace_error: bool,
    providers: Vec<ProviderResponse>,
    default_model: ModelSelectionResponse,
    default_image_model: ModelSelectionResponse,
}

#[derive(Debug, Serialize)]
struct InstalledAgentResponse {
    id: String,
    name: String,
    version: String,
    provider_id: String,
    model_id: String,
    image_provider_id: String,
    image_model_id: String,
    capability_count: usize,
    is_bundled: bool,
    setup_complete: bool,
    auto_update: bool,
    update_available: bool,
    marketplace_version: String,
}

#[derive(Debug, Serialize)]
struct MarketplaceAgentResponse {
    id: String,
    name: String,
    description: String,
    version: String,
    author: String,
    is_installed: bool,
    installed_version: String,
    update_available: bool,
    entry_json: String,
    average_rating: Option<f64>,
    rating_count: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ProviderResponse {
    id: String,
    display_name: String,
}

#[derive(Debug, Serialize)]
struct ModelSelectionResponse {
    provider_id: String,
    model_id: String,
    temperature: Option<f32>,
}

#[derive(Debug, Serialize)]
struct AgentDetailResponse {
    id: String,
    name: String,
    version: String,
    provider_id: String,
    model_id: String,
    image_provider_id: String,
    image_model_id: String,
    temperature: f32,
    is_bundled: bool,
    setup_complete: bool,
    auto_update: bool,
    overview: OverviewResponse,
    runtime: RuntimeResponse,
    automation: AutomationResponse,
    capabilities: Vec<CapabilityResponse>,
    builtin_permissions: Vec<ToolResponse>,
    storage: Vec<StorageResponse>,
    memory: Vec<MemoryResponse>,
    network_log: Vec<NetworkLogResponse>,
    improvement: ImprovementResponse,
}

#[derive(Debug, Serialize)]
struct OverviewResponse {
    capability_count: usize,
    storage_count: usize,
    memory_count: usize,
    network_request_count: usize,
    permissions_allow_count: usize,
    permissions_ask_count: usize,
    permissions_block_count: usize,
    secrets_set_count: usize,
    secrets_missing_count: usize,
}

#[derive(Debug, Serialize)]
struct RuntimeResponse {
    autonomy_level: String,
    use_advanced_limits: bool,
    max_tool_loop_iterations: u32,
    max_delegation_hops: i32,
    agent_default_autonomy_level: String,
    agent_default_max_tool_loop_iterations: u32,
    agent_default_max_delegation_hops: i32,
    has_user_override: bool,
}

#[derive(Debug, Serialize)]
struct AutomationResponse {
    supported: bool,
    enabled: bool,
    ready: bool,
    missing_required_credentials: bool,
    missing_default_pipe: bool,
    active_schedule_count: usize,
    total_schedule_count: usize,
    presets: Vec<AutomationPresetResponse>,
}

#[derive(Debug, Serialize)]
struct AutomationPresetResponse {
    label: String,
    cron_description: String,
}

#[derive(Debug, Serialize)]
struct CapabilityResponse {
    id: String,
    image_status: String,
    effective_network_mode: String,
    network_access_policy: String,
    host_policies: Vec<HostPolicyResponse>,
    filesystem: String,
    max_memory_mb: u32,
    max_cpu_percent: u32,
    pids_limit: u32,
    tools: Vec<ToolResponse>,
    credentials: Vec<CredentialResponse>,
}

#[derive(Debug, Serialize)]
struct HostPolicyResponse {
    host: String,
    policy: String,
    source: String,
    removable: bool,
}

#[derive(Debug, Serialize)]
struct ToolResponse {
    capability_id: String,
    name: String,
    display_name: String,
    description: String,
    policy: String,
    global_default: String,
    has_override: bool,
}

#[derive(Debug, Serialize)]
struct CredentialResponse {
    capability_id: String,
    name: String,
    scope: String,
    description: String,
    required: bool,
    is_set: bool,
    set_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct StorageResponse {
    id: String,
    user_id: String,
    key: String,
    value: String,
    updated_at: String,
}

#[derive(Debug, Serialize)]
struct MemoryResponse {
    id: String,
    user_id: String,
    memory: String,
    tags: String,
    source: String,
    updated_at: String,
}

#[derive(Debug, Serialize)]
struct NetworkLogResponse {
    capability_id: String,
    method: String,
    host: String,
    port: i32,
    allowed: bool,
    created_at: String,
}

#[derive(Debug, Serialize)]
struct ImprovementResponse {
    signal_count: i64,
    insights: Vec<InsightResponse>,
}

#[derive(Debug, Serialize)]
struct InsightResponse {
    id: String,
    lesson_text: String,
    insight_type: String,
    status: String,
    confidence_percent: u8,
    supporting_signals: i64,
    created_at: String,
}

#[derive(Debug, Serialize)]
struct SetupResponse {
    agent_id: String,
    agent_name: String,
    update_flow: bool,
    steps: Vec<SetupStepResponse>,
    permissions: Vec<SetupPermissionResponse>,
    discovery_warning: bool,
    providers: Vec<ProviderResponse>,
}

#[derive(Debug, Serialize)]
struct SetupStepResponse {
    id: String,
    kind: String,
    label: String,
    description: String,
    default_value: String,
    validation: String,
}

#[derive(Debug, Serialize)]
struct SetupPermissionResponse {
    key: String,
    capability_id: String,
    tool_name: String,
    display_name: String,
    description: String,
    recommended: String,
}

#[derive(Debug, Deserialize)]
struct SetupQuery {
    flow: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SetupSubmitRequest {
    #[serde(default)]
    values: HashMap<String, String>,
    flow: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SetupTestRequest {
    #[serde(default)]
    values: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
struct SetupTestResponse {
    ok: bool,
    status: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct InstallRequest {
    entry_json: String,
}

#[derive(Debug, Deserialize)]
struct ModelRequest {
    provider_id: String,
    model_id: String,
    temperature: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct DefaultsRequest {
    provider_id: Option<String>,
    model_id: Option<String>,
    temperature: Option<f32>,
    image_provider_id: Option<String>,
    image_model_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RuntimeRequest {
    autonomy_level: String,
    use_advanced_limits: bool,
    max_tool_loop_iterations: Option<u32>,
    max_delegation_hops: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct ToggleRequest {
    enabled: bool,
}

#[derive(Debug, Deserialize)]
struct PermissionRequest {
    capability_id: String,
    tool_name: String,
    policy: Option<String>,
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NetworkAccessRequest {
    capability_id: String,
    access: String,
}

#[derive(Debug, Deserialize)]
struct NetworkHostRequest {
    capability_id: String,
    host: String,
    policy: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CredentialRequest {
    capability_id: String,
    credential_name: String,
    scope: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct ImprovementRequest {
    insight_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RatingRequest {
    rating: u8,
}

#[derive(Debug, Serialize)]
struct OperationResponse {
    ok: bool,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
}

fn error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (
        status,
        Json(ErrorEnvelope {
            error: ErrorBody { code, message },
        }),
    )
        .into_response()
}

async fn list(admin: ApiAdmin, State(state): State<AppState>) -> Response {
    let agents_map = state.agents.load();
    let mut installed = Vec::new();
    for definition in agents_map.values() {
        let row = sqlx::query(
            "SELECT provider_id, model_id, image_provider_id, image_model_id, version, \
                    is_bundled, setup_complete, auto_update \
             FROM agents WHERE id = ?",
        )
        .bind(&definition.id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        let string = |row: Option<&sqlx::sqlite::SqliteRow>, name: &str| {
            row.and_then(|value| value.try_get(name).ok())
                .unwrap_or_default()
        };
        installed.push(InstalledAgentResponse {
            id: definition.id.clone(),
            name: definition.localized_name(&admin.language),
            version: string(row.as_ref(), "version"),
            provider_id: string(row.as_ref(), "provider_id"),
            model_id: string(row.as_ref(), "model_id"),
            image_provider_id: string(row.as_ref(), "image_provider_id"),
            image_model_id: string(row.as_ref(), "image_model_id"),
            capability_count: definition.capability_manifests.len(),
            is_bundled: row
                .as_ref()
                .and_then(|value| value.try_get::<i64, _>("is_bundled").ok())
                .unwrap_or(0)
                != 0,
            setup_complete: row
                .as_ref()
                .and_then(|value| value.try_get::<i64, _>("setup_complete").ok())
                .unwrap_or(1)
                != 0,
            auto_update: row
                .as_ref()
                .and_then(|value| value.try_get::<i64, _>("auto_update").ok())
                .unwrap_or(0)
                != 0,
            update_available: false,
            marketplace_version: String::new(),
        });
    }
    drop(agents_map);

    let pending = sqlx::query(
        "SELECT id, display_name, version, is_bundled, auto_update FROM agents WHERE setup_complete = 0",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    for row in pending {
        let id: String = row.try_get("id").unwrap_or_default();
        if installed.iter().any(|agent| agent.id == id) {
            continue;
        }
        installed.push(InstalledAgentResponse {
            id,
            name: row.try_get("display_name").unwrap_or_default(),
            version: row.try_get("version").unwrap_or_default(),
            provider_id: String::new(),
            model_id: String::new(),
            image_provider_id: String::new(),
            image_model_id: String::new(),
            capability_count: 0,
            is_bundled: row.try_get::<i64, _>("is_bundled").unwrap_or(0) != 0,
            setup_complete: false,
            auto_update: row.try_get::<i64, _>("auto_update").unwrap_or(0) != 0,
            update_available: false,
            marketplace_version: String::new(),
        });
    }
    installed.sort_by(|left, right| left.name.cmp(&right.name));

    let installed_versions = installed
        .iter()
        .map(|agent| (agent.id.clone(), agent.version.clone()))
        .collect::<HashMap<_, _>>();
    let (marketplace_agents, marketplace_error) =
        match marketplace::fetch_catalogue(&state.config.marketplace_url).await {
            Ok(catalogue) => {
                let agents = catalogue
                    .agents
                    .iter()
                    .map(|entry| {
                        let installed_version = installed_versions
                            .get(&entry.id)
                            .cloned()
                            .unwrap_or_default();
                        let is_installed = installed_versions.contains_key(&entry.id);
                        let update_available = is_installed
                            && marketplace::is_newer_version(&installed_version, &entry.version);
                        MarketplaceAgentResponse {
                            id: entry.id.clone(),
                            name: entry.localized_name(&admin.language),
                            description: entry.localized_description(&admin.language),
                            version: entry.version.clone(),
                            author: entry.author.clone(),
                            is_installed,
                            installed_version,
                            update_available,
                            entry_json: serde_json::to_string(entry).unwrap_or_default(),
                            average_rating: entry.average_rating,
                            rating_count: entry.rating_count,
                        }
                    })
                    .collect::<Vec<_>>();
                for item in &agents {
                    if let Some(agent) = installed.iter_mut().find(|agent| agent.id == item.id) {
                        agent.update_available = item.update_available;
                        agent.marketplace_version = item.version.clone();
                    }
                }
                (agents, false)
            }
            Err(fetch_error) => {
                tracing::warn!(error = %fetch_error, "Failed to load agent marketplace");
                (Vec::new(), true)
            }
        };

    let default = model::get_global_default(&state.db).await.ok().flatten();
    let default_image = model::get_global_image_default(&state.db)
        .await
        .ok()
        .flatten();
    Json(AgentsResponse {
        installed,
        marketplace: marketplace_agents,
        marketplace_error,
        providers: load_providers(&state).await,
        default_model: default
            .map(|selection| ModelSelectionResponse {
                provider_id: selection.provider_id,
                model_id: selection.model_id,
                temperature: Some(selection.temperature),
            })
            .unwrap_or(ModelSelectionResponse {
                provider_id: String::new(),
                model_id: String::new(),
                temperature: Some(0.7),
            }),
        default_image_model: default_image
            .map(|selection| ModelSelectionResponse {
                provider_id: selection.provider_id,
                model_id: selection.model_id,
                temperature: None,
            })
            .unwrap_or(ModelSelectionResponse {
                provider_id: String::new(),
                model_id: String::new(),
                temperature: None,
            }),
    })
    .into_response()
}

async fn detail(
    admin: ApiAdmin,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    let definition = {
        let agents = state.agents.load();
        agents.get(&agent_id).cloned()
    };
    let Some(definition) = definition else {
        return error(
            StatusCode::NOT_FOUND,
            "agent_not_found",
            "That agent is no longer installed.",
        );
    };
    let metadata = sqlx::query(
        "SELECT version, provider_id, model_id, image_provider_id, image_model_id, temperature, \
                is_bundled, setup_complete, auto_update FROM agents WHERE id = ?",
    )
    .bind(&agent_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let string = |name: &str| {
        metadata
            .as_ref()
            .and_then(|row| row.try_get(name).ok())
            .unwrap_or_default()
    };

    let effective_runtime = crate::agents::runtime_limits::resolve_effective_runtime_settings(
        &state.db,
        &admin.user_id,
        &agent_id,
    )
    .await
    .unwrap_or_else(|_| {
        let level = crate::agents::runtime_limits::AutonomyLevel::Medium;
        crate::agents::runtime_limits::EffectiveRuntimeSettings {
            autonomy_level: level,
            use_advanced_limits: false,
            max_tool_loop_iterations: crate::agents::runtime_limits::limits_for_autonomy(level)
                .max_tool_loop_iterations,
            max_delegation_hops: crate::agents::runtime_limits::limits_for_autonomy(level)
                .max_delegation_hops,
            agent_default_level: level,
            agent_default_limits: crate::agents::runtime_limits::limits_for_autonomy(level),
        }
    });
    let has_runtime_override = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM user_agent_runtime_settings WHERE user_id = ? AND agent_id = ?",
    )
    .bind(&admin.user_id)
    .bind(&agent_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0)
        > 0;

    let global_policies =
        crate::permissions::tool_policy::get_global_policies_for_agent(&state.db, &agent_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|policy| {
                (
                    (policy.capability_id, policy.tool_name),
                    policy.policy.as_str().to_string(),
                )
            })
            .collect::<HashMap<_, _>>();
    let personal_policies = crate::permissions::tool_policy::get_policies_for_agent(
        &state.db,
        &admin.user_id,
        &agent_id,
    )
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|policy| {
        (
            (policy.capability_id, policy.tool_name),
            policy.policy.as_str().to_string(),
        )
    })
    .collect::<HashMap<_, _>>();
    let access_overrides = crate::permissions::network_policy::get_user_access_overrides_for_agent(
        &state.db,
        &admin.user_id,
        &agent_id,
    )
    .await
    .unwrap_or_default();
    let mut host_overrides =
        HashMap::<String, Vec<(String, crate::permissions::network_policy::HostPolicy)>>::new();
    for row in crate::permissions::network_policy::get_user_host_overrides_for_agent(
        &state.db,
        &admin.user_id,
        &agent_id,
    )
    .await
    .unwrap_or_default()
    {
        host_overrides
            .entry(row.capability_id)
            .or_default()
            .push((row.host, row.policy));
    }

    let mut capabilities = Vec::new();
    for (capability_id, manifest) in &definition.capability_manifests {
        let image_status = match state.capabilities.is_image_available(&manifest.image).await {
            Ok(true) => "available",
            Ok(false) => "missing",
            Err(_) => "unknown",
        };
        let override_access = access_overrides.get(capability_id).copied();
        let overrides = host_overrides
            .get(capability_id)
            .cloned()
            .unwrap_or_default();
        let effective = crate::permissions::network_policy::build_effective_policy(
            &manifest.network,
            override_access,
            &overrides,
        );
        let override_map = overrides.into_iter().collect::<HashMap<_, _>>();
        let mut host_policies = manifest
            .network
            .hosts
            .iter()
            .map(|host| {
                let host = host.to_ascii_lowercase();
                HostPolicyResponse {
                    policy: if matches!(
                        override_map.get(&host),
                        Some(crate::permissions::network_policy::HostPolicy::Deny)
                    ) {
                        "deny".to_string()
                    } else {
                        "allow".to_string()
                    },
                    host,
                    source: "default".to_string(),
                    removable: false,
                }
            })
            .collect::<Vec<_>>();
        for (host, policy) in &override_map {
            if manifest
                .network
                .hosts
                .iter()
                .any(|default| default.eq_ignore_ascii_case(host))
            {
                continue;
            }
            host_policies.push(HostPolicyResponse {
                host: host.clone(),
                policy: match policy {
                    crate::permissions::network_policy::HostPolicy::Allow => "allow",
                    crate::permissions::network_policy::HostPolicy::Deny => "deny",
                }
                .to_string(),
                source: "custom".to_string(),
                removable: true,
            });
        }
        host_policies.sort_by(|left, right| left.host.cmp(&right.host));

        let tool_definitions = if manifest.tool_source == ToolSource::Dynamic {
            load_discovered_tools(&state.db, &agent_id, capability_id)
                .await
                .unwrap_or_default()
        } else {
            manifest.tools.clone()
        };
        let tools = tool_definitions
            .iter()
            .map(|tool| {
                tool_response(
                    &definition,
                    capability_id,
                    &tool.name,
                    &tool.description,
                    &admin.language,
                    &global_policies,
                    &personal_policies,
                )
            })
            .collect();

        let mut credentials = Vec::new();
        for credential in &manifest.credentials {
            let (scope, row) = match credential.scope {
                CredentialScope::System => (
                    "system",
                    sqlx::query(
                        "SELECT created_at FROM system_credentials WHERE capability_id = ? AND credential_name = ?",
                    )
                    .bind(capability_id)
                    .bind(&credential.name)
                    .fetch_optional(&state.db)
                    .await
                    .ok()
                    .flatten(),
                ),
                CredentialScope::User => (
                    "user",
                    sqlx::query(
                        "SELECT created_at FROM user_credentials WHERE user_id = ? AND capability_id = ? AND credential_name = ?",
                    )
                    .bind(&admin.user_id)
                    .bind(capability_id)
                    .bind(&credential.name)
                    .fetch_optional(&state.db)
                    .await
                    .ok()
                    .flatten(),
                ),
            };
            let set_at = row.and_then(|row| row.try_get("created_at").ok());
            credentials.push(CredentialResponse {
                capability_id: capability_id.clone(),
                name: credential.name.clone(),
                scope: scope.to_string(),
                description: localization::localized_credential_description(
                    &definition,
                    capability_id,
                    &credential.name,
                    &credential.description,
                    &admin.language,
                ),
                required: credential.required,
                is_set: set_at.is_some(),
                set_at,
            });
        }

        capabilities.push(CapabilityResponse {
            id: capability_id.clone(),
            image_status: image_status.to_string(),
            effective_network_mode: match effective.mode {
                NetworkMode::None => "none",
                NetworkMode::Allowlist => "allowlist",
                NetworkMode::Any => "any",
            }
            .to_string(),
            network_access_policy: match override_access {
                Some(crate::permissions::network_policy::NetworkAccessPolicy::Deny) => "deny",
                _ => "allow",
            }
            .to_string(),
            host_policies,
            filesystem: match manifest.filesystem {
                FilesystemPolicy::None => "none",
                FilesystemPolicy::Temp => "temp",
                FilesystemPolicy::Workspace => "workspace",
            }
            .to_string(),
            max_memory_mb: manifest.resources.max_memory_mb,
            max_cpu_percent: (manifest.resources.max_cpu_fraction * 100.0).round() as u32,
            pids_limit: manifest.resources.pids_limit,
            tools,
            credentials,
        });
    }
    capabilities.sort_by(|left, right| left.id.cmp(&right.id));

    let builtin_permissions = builtin_tools(&agent_id)
        .into_iter()
        .map(|(name, display_name, description)| {
            let capability_id = crate::permissions::tool_policy::BUILTIN_CAPABILITY_ID;
            let key = (capability_id.to_string(), name.to_string());
            let global_default = global_policies
                .get(&key)
                .cloned()
                .unwrap_or_else(|| "not set".to_string());
            let personal = personal_policies.get(&key).cloned();
            ToolResponse {
                capability_id: capability_id.to_string(),
                name: name.to_string(),
                display_name: display_name.to_string(),
                description: description.to_string(),
                policy: personal.clone().unwrap_or_else(|| global_default.clone()),
                global_default,
                has_override: personal.is_some(),
            }
        })
        .collect::<Vec<_>>();

    let storage = load_storage(&state, &agent_id).await;
    let memory = load_memory(&state, &agent_id).await;
    let network_log = load_network_log(
        &state,
        &definition
            .capability_manifests
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
    )
    .await;
    let insights = crate::agents::improvement::list_insights(&state.db, &agent_id, &admin.user_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|insight| InsightResponse {
            id: insight.id,
            lesson_text: insight.lesson_text,
            insight_type: insight.insight_type,
            status: insight.status,
            confidence_percent: (insight.confidence * 100.0).round() as u8,
            supporting_signals: insight.supporting_signals,
            created_at: insight.created_at,
        })
        .collect::<Vec<_>>();
    let signal_count =
        crate::agents::improvement::count_signals(&state.db, &agent_id, &admin.user_id)
            .await
            .unwrap_or(0);

    let automation_presets = definition
        .automation
        .schedules
        .iter()
        .map(|preset| AutomationPresetResponse {
            label: localization::localized_schedule_label(&definition, preset, &admin.language),
            cron_description: localization::localized_schedule_cron_description(
                &definition,
                preset,
                &admin.language,
            ),
        })
        .collect::<Vec<_>>();
    let mut active_schedule_count = 0;
    for preset in &definition.automation.schedules {
        let name = format!("auto.{}.{}", agent_id, preset.id);
        if sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM schedules WHERE user_id = ? AND agent_id = ? AND name = ? AND active = 1",
        )
        .bind(&admin.user_id)
        .bind(&agent_id)
        .bind(name)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0)
            > 0
        {
            active_schedule_count += 1;
        }
    }
    let missing_default_pipe = !definition.automation.schedules.is_empty()
        && sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pipes WHERE user_id = ? AND active = 1 AND default_agent_id = ?",
        )
        .bind(&admin.user_id)
        .bind(&agent_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0)
            == 0;
    let missing_required_credentials = capabilities.iter().any(|capability| {
        capability
            .credentials
            .iter()
            .any(|credential| credential.required && !credential.is_set)
    });
    let total_schedule_count = definition.automation.schedules.len();
    let automation_supported = total_schedule_count > 0;

    let mut allow = 0;
    let mut ask = 0;
    let mut block = 0;
    for tool in capabilities
        .iter()
        .flat_map(|capability| capability.tools.iter())
        .chain(builtin_permissions.iter())
    {
        match tool.policy.as_str() {
            "allow" => allow += 1,
            "ask" => ask += 1,
            _ => block += 1,
        }
    }
    let secrets_set = capabilities
        .iter()
        .flat_map(|capability| capability.credentials.iter())
        .filter(|credential| credential.is_set)
        .count();
    let secrets_total = capabilities
        .iter()
        .map(|capability| capability.credentials.len())
        .sum::<usize>();

    Json(AgentDetailResponse {
        id: agent_id,
        name: definition.localized_name(&admin.language),
        version: string("version"),
        provider_id: string("provider_id"),
        model_id: string("model_id"),
        image_provider_id: string("image_provider_id"),
        image_model_id: string("image_model_id"),
        temperature: metadata
            .as_ref()
            .and_then(|row| row.try_get::<f64, _>("temperature").ok())
            .unwrap_or(0.7) as f32,
        is_bundled: metadata
            .as_ref()
            .and_then(|row| row.try_get::<i64, _>("is_bundled").ok())
            .unwrap_or(0)
            != 0,
        setup_complete: metadata
            .as_ref()
            .and_then(|row| row.try_get::<i64, _>("setup_complete").ok())
            .unwrap_or(1)
            != 0,
        auto_update: metadata
            .as_ref()
            .and_then(|row| row.try_get::<i64, _>("auto_update").ok())
            .unwrap_or(0)
            != 0,
        overview: OverviewResponse {
            capability_count: capabilities.len(),
            storage_count: storage.len(),
            memory_count: memory.len(),
            network_request_count: network_log.len(),
            permissions_allow_count: allow,
            permissions_ask_count: ask,
            permissions_block_count: block,
            secrets_set_count: secrets_set,
            secrets_missing_count: secrets_total.saturating_sub(secrets_set),
        },
        runtime: RuntimeResponse {
            autonomy_level: effective_runtime.autonomy_level.as_str().to_string(),
            use_advanced_limits: effective_runtime.use_advanced_limits,
            max_tool_loop_iterations: effective_runtime.max_tool_loop_iterations,
            max_delegation_hops: effective_runtime.max_delegation_hops,
            agent_default_autonomy_level: effective_runtime
                .agent_default_level
                .as_str()
                .to_string(),
            agent_default_max_tool_loop_iterations: effective_runtime
                .agent_default_limits
                .max_tool_loop_iterations,
            agent_default_max_delegation_hops: effective_runtime
                .agent_default_limits
                .max_delegation_hops,
            has_user_override: has_runtime_override,
        },
        automation: AutomationResponse {
            supported: automation_supported,
            enabled: automation_supported && active_schedule_count == total_schedule_count,
            ready: automation_supported && !missing_default_pipe && !missing_required_credentials,
            missing_required_credentials,
            missing_default_pipe,
            active_schedule_count,
            total_schedule_count,
            presets: automation_presets,
        },
        capabilities,
        builtin_permissions,
        storage,
        memory,
        network_log,
        improvement: ImprovementResponse {
            signal_count,
            insights,
        },
    })
    .into_response()
}

async fn setup(
    admin: ApiAdmin,
    Path(agent_id): Path<String>,
    Query(query): Query<SetupQuery>,
    State(state): State<AppState>,
) -> Response {
    let path = std::path::Path::new(&state.config.installed_agents_dir).join(&agent_id);
    let definition = match crate::agents::loader::load_one(&path).await {
        Ok(definition) => definition,
        Err(load_error) => {
            tracing::warn!(error = %load_error, agent_id, "Failed to load agent setup");
            return error(
                StatusCode::NOT_FOUND,
                "agent_setup_not_found",
                "Setup details for that agent could not be loaded.",
            );
        }
    };
    let steps = definition
        .install_steps
        .iter()
        .map(|step| SetupStepResponse {
            id: step.id.clone(),
            kind: match step.step_type {
                StepType::Input => "input",
                StepType::Test => "test",
            }
            .to_string(),
            label: localization::localized_install_step_label(&definition, step, &admin.language),
            description: localization::localized_install_step_description(
                &definition,
                step,
                &admin.language,
            ),
            default_value: step.default.clone().unwrap_or_default(),
            validation: step.validation.clone().unwrap_or_default(),
        })
        .collect();
    let mut permissions = Vec::new();
    let mut discovery_warning = false;
    for (capability_id, manifest) in &definition.capability_manifests {
        let tools = if manifest.tool_source == ToolSource::Dynamic {
            if sync_dynamic_tools_for_capability(
                &state.db,
                &state.capabilities,
                &state.credentials,
                &agent_id,
                capability_id,
                manifest,
                &definition.capability_manifests,
            )
            .await
            .is_err()
            {
                discovery_warning = true;
            }
            load_discovered_tools(&state.db, &agent_id, capability_id)
                .await
                .unwrap_or_default()
        } else {
            manifest.tools.clone()
        };
        for tool in tools {
            let key = format!("{}_{}", capability_id, tool.name).replace('-', "_");
            permissions.push(SetupPermissionResponse {
                key,
                capability_id: capability_id.clone(),
                tool_name: tool.name.clone(),
                display_name: localization::localized_tool_display_name(
                    &definition,
                    capability_id,
                    &tool.name,
                    &tool.name.replace('_', " "),
                    &admin.language,
                ),
                description: localization::localized_tool_description(
                    &definition,
                    capability_id,
                    &tool.name,
                    &tool.description,
                    &admin.language,
                ),
                recommended: tool.effective_recommended_policy().to_string(),
            });
        }
    }
    for (name, display, description) in builtin_tools(&agent_id) {
        permissions.push(SetupPermissionResponse {
            key: format!("__builtin___{name}"),
            capability_id: "__builtin__".to_string(),
            tool_name: name.to_string(),
            display_name: display.to_string(),
            description: description.to_string(),
            recommended: if name == "delegate_to_agent" {
                "ask"
            } else {
                "allow"
            }
            .to_string(),
        });
    }
    Json(SetupResponse {
        agent_id,
        agent_name: definition.localized_name(&admin.language),
        update_flow: query.flow.as_deref() == Some("update"),
        steps,
        permissions,
        discovery_warning,
        providers: load_providers(&state).await,
    })
    .into_response()
}

async fn install(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Json(request): Json<InstallRequest>,
) -> Response {
    match agent_service::install_agent(&state, &request.entry_json).await {
        Ok(_) => (StatusCode::CREATED, Json(OperationResponse { ok: true })).into_response(),
        Err(error) => ApiError(error).into_response(),
    }
}

async fn complete_setup(
    admin: ApiAdmin,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<SetupSubmitRequest>,
) -> Response {
    let _flow = request.flow;
    operation_response(
        agent_service::complete_setup(&state, &admin.user_id, &agent_id, &request.values).await,
    )
}

async fn test_setup_step(
    _admin: ApiAdmin,
    Path((agent_id, step_id)): Path<(String, String)>,
    State(state): State<AppState>,
    Json(request): Json<SetupTestRequest>,
) -> Response {
    let path = std::path::Path::new(&state.config.installed_agents_dir).join(&agent_id);
    let definition = match crate::agents::loader::load_one(&path).await {
        Ok(definition) => definition,
        Err(_) => {
            return error(
                StatusCode::NOT_FOUND,
                "agent_setup_not_found",
                "Setup details for that agent could not be loaded.",
            );
        }
    };
    let Some(step) = definition
        .install_steps
        .iter()
        .find(|step| step.id == step_id)
    else {
        return error(
            StatusCode::NOT_FOUND,
            "setup_step_not_found",
            "That setup check no longer exists.",
        );
    };
    let Some(test) = step.request.as_ref() else {
        return error(
            StatusCode::BAD_REQUEST,
            "setup_step_not_testable",
            "That setup item does not have a connection check.",
        );
    };
    let mut url = test.url.clone();
    for (key, value) in request.values {
        let key = key.strip_prefix("step_").unwrap_or(&key);
        url = url.replace(&format!("{{{{{key}}}}}"), &value);
    }
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "setup_check_failed",
                "The connection check could not be started.",
            );
        }
    };
    let result = match test.method.to_ascii_uppercase().as_str() {
        "GET" => client.get(url).send().await,
        "POST" => client.post(url).send().await,
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "setup_check_not_supported",
                "That type of connection check is not supported.",
            );
        }
    };
    match result {
        Ok(response) => {
            let status = response.status().as_u16();
            let ok = status == test.expect_status;
            let response = Json(SetupTestResponse {
                ok,
                status: Some(status),
            });
            if ok {
                response.into_response()
            } else {
                (StatusCode::BAD_GATEWAY, response).into_response()
            }
        }
        Err(check_error) => {
            tracing::warn!(error = %check_error, "Agent setup connection check failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(SetupTestResponse {
                    ok: false,
                    status: None,
                }),
            )
                .into_response()
        }
    }
}

async fn set_model(
    _admin: ApiAdmin,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<ModelRequest>,
) -> Response {
    operation_response(
        agent_service::set_agent_model(
            &state.db,
            &agent_id,
            &request.provider_id,
            &request.model_id,
            request.temperature.unwrap_or(0.7),
        )
        .await,
    )
}

async fn set_image_model(
    _admin: ApiAdmin,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<ModelRequest>,
) -> Response {
    operation_response(
        agent_service::set_agent_image_model(
            &state.db,
            &agent_id,
            &request.provider_id,
            &request.model_id,
        )
        .await,
    )
}

async fn set_defaults(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Json(request): Json<DefaultsRequest>,
) -> Response {
    match (request.provider_id, request.model_id) {
        (Some(provider_id), Some(model_id)) => {
            if let Err(error) = agent_service::set_default_model(
                &state.db,
                &provider_id,
                &model_id,
                request.temperature.unwrap_or(0.7),
            )
            .await
            {
                return ApiError(error).into_response();
            }
        }
        (None, None) => {}
        _ => {
            return ApiError(agent_service::ServiceError::Validation(
                "provider and model must be set together",
            ))
            .into_response();
        }
    }
    match (request.image_provider_id, request.image_model_id) {
        (Some(provider_id), Some(model_id)) => operation_response(
            agent_service::set_default_image_model(&state.db, &provider_id, &model_id).await,
        ),
        (None, None) => StatusCode::NO_CONTENT.into_response(),
        _ => ApiError(agent_service::ServiceError::Validation(
            "image provider and model must be set together",
        ))
        .into_response(),
    }
}

async fn set_runtime(
    admin: ApiAdmin,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<RuntimeRequest>,
) -> Response {
    operation_response(
        agent_service::set_runtime_settings(
            &state.db,
            &admin.user_id,
            &agent_id,
            &request.autonomy_level,
            request.use_advanced_limits,
            request.max_tool_loop_iterations,
            request.max_delegation_hops,
        )
        .await,
    )
}

async fn toggle_automation(
    admin: ApiAdmin,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<ToggleRequest>,
) -> Response {
    operation_response(
        agent_service::set_automation_enabled(
            &state,
            &admin.user_id,
            &admin.language,
            &agent_id,
            request.enabled,
        )
        .await,
    )
}

async fn toggle_auto_update(
    _admin: ApiAdmin,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<ToggleRequest>,
) -> Response {
    operation_response(agent_service::set_auto_update(&state.db, &agent_id, request.enabled).await)
}

async fn set_permission(
    admin: ApiAdmin,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<PermissionRequest>,
) -> Response {
    let Some(policy) = request.policy else {
        return ApiError(agent_service::ServiceError::Validation(
            "permission policy is required",
        ))
        .into_response();
    };
    operation_response(
        agent_service::set_permission(
            &state.db,
            &admin.user_id,
            &agent_id,
            &request.capability_id,
            &request.tool_name,
            &policy,
            request.scope.as_deref().unwrap_or("user"),
        )
        .await,
    )
}

async fn reset_permission(
    admin: ApiAdmin,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<PermissionRequest>,
) -> Response {
    operation_response(
        agent_service::reset_permission(
            &state.db,
            &admin.user_id,
            &agent_id,
            &request.capability_id,
            &request.tool_name,
        )
        .await,
    )
}

async fn set_network_access(
    admin: ApiAdmin,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<NetworkAccessRequest>,
) -> Response {
    operation_response(
        agent_service::set_network_access(
            &state,
            &admin.user_id,
            &agent_id,
            &request.capability_id,
            &request.access,
        )
        .await,
    )
}

async fn set_network_host(
    admin: ApiAdmin,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<NetworkHostRequest>,
) -> Response {
    operation_response(
        agent_service::set_network_host(
            &state,
            &admin.user_id,
            &agent_id,
            &request.capability_id,
            &request.host,
            request.policy.as_deref().unwrap_or("allow"),
        )
        .await,
    )
}

async fn remove_network_host(
    admin: ApiAdmin,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<NetworkHostRequest>,
) -> Response {
    operation_response(
        agent_service::remove_network_host(
            &state,
            &admin.user_id,
            &agent_id,
            &request.capability_id,
            &request.host,
        )
        .await,
    )
}

async fn set_credential(
    admin: ApiAdmin,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<CredentialRequest>,
) -> Response {
    operation_response(
        agent_service::set_credential(
            &state,
            &admin.user_id,
            &agent_id,
            &request.capability_id,
            &request.credential_name,
            &request.scope,
            &request.value,
        )
        .await,
    )
}

async fn remove_credential(
    admin: ApiAdmin,
    Path((agent_id, scope, capability_id, name)): Path<(String, String, String, String)>,
    State(state): State<AppState>,
) -> Response {
    operation_response(
        agent_service::remove_credential(
            &state,
            &admin.user_id,
            &agent_id,
            &capability_id,
            &name,
            &scope,
        )
        .await,
    )
}

async fn remove_storage(
    _admin: ApiAdmin,
    Path((agent_id, entry_id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    operation_response(agent_service::remove_storage(&state.db, &agent_id, &entry_id).await)
}

async fn remove_memory(
    _admin: ApiAdmin,
    Path((agent_id, memory_id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    operation_response(agent_service::remove_memory(&state.db, &agent_id, &memory_id).await)
}

async fn download_image(
    _admin: ApiAdmin,
    Path((agent_id, capability_id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    match agent_service::download_capability_image(&state, &agent_id, &capability_id).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => ApiError(error).into_response(),
    }
}

async fn improvement_action(
    admin: ApiAdmin,
    Path((agent_id, action)): Path<(String, String)>,
    State(state): State<AppState>,
    Json(request): Json<ImprovementRequest>,
) -> Response {
    operation_response(
        agent_service::update_improvement(
            &state.db,
            &admin.user_id,
            &agent_id,
            &action,
            request.insight_id.as_deref(),
        )
        .await,
    )
}

async fn rate(
    _admin: ApiAdmin,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<RatingRequest>,
) -> Response {
    operation_response(agent_service::submit_rating(&state, &agent_id, request.rating).await)
}

async fn uninstall(
    _admin: ApiAdmin,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    operation_response(agent_service::uninstall_agent(&state, &agent_id).await)
}

async fn check_updates(_admin: ApiAdmin, State(state): State<AppState>) -> Response {
    match agent_service::check_updates(&state).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => ApiError(error).into_response(),
    }
}

async fn start_update(
    admin: ApiAdmin,
    State(state): State<AppState>,
    Json(request): Json<InstallRequest>,
) -> Response {
    match agent_service::start_update(&state, &admin.user_id, &admin.language, &request.entry_json)
        .await
    {
        Ok(job_id) => Json(serde_json::json!({ "job_id": job_id })).into_response(),
        Err(error) => ApiError(error).into_response(),
    }
}

async fn update_status(
    admin: ApiAdmin,
    Path(job_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    match agent_service::update_status(&state, &admin.user_id, &job_id).await {
        Ok(job) => Json(job).into_response(),
        Err(error) => ApiError(error).into_response(),
    }
}

fn tool_response(
    definition: &crate::agents::loader::AgentDefinition,
    capability_id: &str,
    name: &str,
    description: &str,
    language: &str,
    global: &HashMap<(String, String), String>,
    personal: &HashMap<(String, String), String>,
) -> ToolResponse {
    let key = (capability_id.to_string(), name.to_string());
    let global_default = global
        .get(&key)
        .cloned()
        .unwrap_or_else(|| "not set".to_string());
    let personal_value = personal.get(&key).cloned();
    ToolResponse {
        capability_id: capability_id.to_string(),
        name: name.to_string(),
        display_name: localization::localized_tool_display_name(
            definition,
            capability_id,
            name,
            &name.replace('_', " "),
            language,
        ),
        description: localization::localized_tool_description(
            definition,
            capability_id,
            name,
            description,
            language,
        ),
        policy: personal_value
            .clone()
            .unwrap_or_else(|| global_default.clone()),
        global_default,
        has_override: personal_value.is_some(),
    }
}

fn builtin_tools(agent_id: &str) -> Vec<(&'static str, &'static str, &'static str)> {
    use crate::permissions::tool_policy::*;
    let mut tools = Vec::new();
    if agent_id == "default" {
        tools.push((
            BUILTIN_DELEGATE,
            "Delegate to agent",
            "Hand work to another specialist agent.",
        ));
    }
    tools.extend([
        (
            BUILTIN_STORE_GET,
            "Read saved values",
            "Read values this agent saved earlier.",
        ),
        (
            BUILTIN_STORE_SET,
            "Save values",
            "Save values for a later conversation.",
        ),
        (
            BUILTIN_STORE_DELETE,
            "Delete saved values",
            "Remove values this agent saved.",
        ),
        (
            BUILTIN_STORE_LIST,
            "List saved values",
            "See values this agent has saved.",
        ),
        (
            BUILTIN_MEMORY_REMEMBER,
            "Remember",
            "Save a useful long-term memory.",
        ),
        (BUILTIN_MEMORY_FORGET, "Forget", "Remove a saved memory."),
        (
            BUILTIN_MEMORY_SEARCH,
            "Search memories",
            "Find relevant saved memories.",
        ),
        (BUILTIN_MEMORY_LIST, "List memories", "See saved memories."),
        (
            BUILTIN_SET_SCHEDULE,
            "Create automations",
            "Create recurring work.",
        ),
        (
            BUILTIN_SET_REMINDER,
            "Create reminders",
            "Create one-time reminders.",
        ),
    ]);
    tools
}

fn provider_is_configured(provider_id: &str, has_key: bool, has_url: bool) -> bool {
    match provider_id {
        "bedrock" => has_key && has_url,
        "pico" => has_url,
        _ => has_key,
    }
}

async fn load_providers(state: &AppState) -> Vec<ProviderResponse> {
    sqlx::query(
        "SELECT id, display_name, api_key_encrypted, base_url FROM llm_providers WHERE active = 1 ORDER BY id",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .filter_map(|row| {
        let id: String = row.try_get("id").ok()?;
        let has_key = row
            .try_get::<Option<String>, _>("api_key_encrypted")
            .ok()
            .flatten()
            .is_some_and(|value| !value.is_empty());
        let has_url = row
            .try_get::<Option<String>, _>("base_url")
            .ok()
            .flatten()
            .is_some_and(|value| !value.is_empty());
        provider_is_configured(&id, has_key, has_url).then(|| ProviderResponse {
            id,
            display_name: row.try_get("display_name").unwrap_or_default(),
        })
    })
    .collect()
}

async fn load_storage(state: &AppState, agent_id: &str) -> Vec<StorageResponse> {
    sqlx::query(
        "SELECT id, user_id, key, value, updated_at FROM agent_storage \
         WHERE agent_id = ? ORDER BY updated_at DESC, key LIMIT 500",
    )
    .bind(agent_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|row| StorageResponse {
        id: row.try_get("id").unwrap_or_default(),
        user_id: row.try_get("user_id").unwrap_or_default(),
        key: row.try_get("key").unwrap_or_default(),
        value: row.try_get("value").unwrap_or_default(),
        updated_at: row.try_get("updated_at").unwrap_or_default(),
    })
    .collect()
}

async fn load_memory(state: &AppState, agent_id: &str) -> Vec<MemoryResponse> {
    sqlx::query(
        "SELECT id, user_id, memory_text, tags, source, updated_at FROM agent_memories \
         WHERE agent_id = ? ORDER BY updated_at DESC LIMIT 500",
    )
    .bind(agent_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|row| MemoryResponse {
        id: row.try_get("id").unwrap_or_default(),
        user_id: row.try_get("user_id").unwrap_or_default(),
        memory: row.try_get("memory_text").unwrap_or_default(),
        tags: row.try_get("tags").unwrap_or_default(),
        source: row.try_get("source").unwrap_or_default(),
        updated_at: row.try_get("updated_at").unwrap_or_default(),
    })
    .collect()
}

async fn load_network_log(state: &AppState, capability_ids: &[String]) -> Vec<NetworkLogResponse> {
    let mut output = Vec::new();
    for capability_id in capability_ids {
        for row in sqlx::query(
            "SELECT capability_id, method, host, port, allowed, created_at FROM egress_log \
             WHERE capability_id = ? ORDER BY created_at DESC LIMIT 100",
        )
        .bind(capability_id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        {
            output.push(NetworkLogResponse {
                capability_id: row.try_get("capability_id").unwrap_or_default(),
                method: row.try_get("method").unwrap_or_default(),
                host: row.try_get("host").unwrap_or_default(),
                port: row.try_get("port").unwrap_or(0),
                allowed: row.try_get::<i64, _>("allowed").unwrap_or(0) != 0,
                created_at: row.try_get("created_at").unwrap_or_default(),
            });
        }
    }
    output.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    output.truncate(100);
    output
}

struct ApiError(agent_service::ServiceError);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        use agent_service::ServiceError;
        let (status, code, message) = match &self.0 {
            ServiceError::AgentNotFound => (
                StatusCode::NOT_FOUND,
                "agent_not_found",
                "That agent does not exist.",
            ),
            ServiceError::CapabilityNotFound => (
                StatusCode::NOT_FOUND,
                "capability_not_found",
                "That agent capability does not exist.",
            ),
            ServiceError::SetupNotFound => (
                StatusCode::NOT_FOUND,
                "agent_setup_not_found",
                "Setup details for that agent could not be loaded.",
            ),
            ServiceError::InsightNotFound => (
                StatusCode::NOT_FOUND,
                "insight_not_found",
                "That learned improvement does not exist.",
            ),
            ServiceError::UpdateJobNotFound => (
                StatusCode::NOT_FOUND,
                "update_job_not_found",
                "That update job does not exist.",
            ),
            ServiceError::UpdateJobForbidden => (
                StatusCode::FORBIDDEN,
                "update_job_forbidden",
                "That update job belongs to another user.",
            ),
            ServiceError::Validation(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The request is invalid.",
            ),
            ServiceError::Conflict(_) => (
                StatusCode::CONFLICT,
                "agent_conflict",
                "The agent change cannot be completed in its current state.",
            ),
            ServiceError::DockerUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "docker_unavailable",
                "Docker is unavailable.",
            ),
            ServiceError::Upstream => (
                StatusCode::BAD_GATEWAY,
                "marketplace_unavailable",
                "The agent marketplace request failed.",
            ),
            ServiceError::Database(_) | ServiceError::Operation(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "The agent request could not be completed.",
            ),
        };
        if status.is_server_error() {
            tracing::error!(error = %self.0, "Agents API request failed");
        }
        error(status, code, message)
    }
}

fn operation_response(result: Result<(), agent_service::ServiceError>) -> Response {
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => ApiError(error).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;

    use super::*;

    async fn response_json(response: Response) -> serde_json::Value {
        let body = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    #[test]
    fn provider_configuration_rules_match_the_native_agents_list() {
        assert!(!provider_is_configured("openai", false, true));
        assert!(provider_is_configured("openai", true, false));
        assert!(!provider_is_configured("bedrock", false, true));
        assert!(!provider_is_configured("bedrock", true, false));
        assert!(provider_is_configured("bedrock", true, true));
        assert!(provider_is_configured("pico", false, true));
    }

    #[tokio::test]
    async fn service_errors_have_stable_status_and_safe_envelopes() {
        let response = ApiError(agent_service::ServiceError::AgentNotFound).into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "agent_not_found"
        );

        let response =
            ApiError(agent_service::ServiceError::Validation("private detail")).into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], "invalid_request");
        assert!(!body.to_string().contains("private detail"));
    }

    #[test]
    fn builtin_delegation_is_only_shown_for_the_personal_agent() {
        assert!(
            builtin_tools("default")
                .iter()
                .any(|(name, _, _)| *name == "delegate_to_agent")
        );
        assert!(
            !builtin_tools("specialist")
                .iter()
                .any(|(name, _, _)| *name == "delegate_to_agent")
        );
    }
}
