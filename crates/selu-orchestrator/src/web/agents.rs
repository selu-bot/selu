use askama::Template;
use axum::{
    Form, Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::{Deserialize, Serialize};
use tracing::error;
use uuid::Uuid;

use crate::agents::{
    loader::StepType,
    localization,
    marketplace::{self, MarketplaceEntry},
    model,
};
use crate::capabilities::discovery::{
    load_discovered_tools, sync_dynamic_tools_for_agent, sync_dynamic_tools_for_capability,
};
use crate::capabilities::manifest::ToolSource;
use crate::state::AppState;
use crate::web::auth::AuthUser;
use crate::web::{BasePath, prefixed_redirect};

const DEFAULT_AGENT_ID: &str = "default";

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct InstalledAgentView {
    pub id: String,
    pub name: String,
    pub version: String,
    pub provider_id: String,
    pub model_id: String,
    pub model_display_name: String,
    pub image_provider_id: String,
    pub image_model_id: String,
    pub image_model_display_name: String,
    pub capability_count: usize,
    pub is_bundled: bool,
    pub setup_complete: bool,
    pub auto_update: bool,
    /// True if a newer version is available in the marketplace
    pub update_available: bool,
    /// The marketplace version (if update is available)
    pub marketplace_version: String,
}

#[derive(Debug, Clone)]
pub struct MarketplaceAgentView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub author: String,
    pub is_installed: bool,
    /// If installed, the currently installed version (empty string if not installed)
    pub installed_version: String,
    /// True when the marketplace version is newer than the installed version
    pub update_available: bool,
    /// JSON-encoded MarketplaceEntry for the install/update form
    pub entry_json: String,
    pub average_rating: Option<f64>,
    pub rating_count: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ProviderOption {
    pub id: String,
    pub display_name: String,
}

#[derive(Debug, Clone)]
pub struct SetupStepView {
    pub id: String,
    pub step_type: String,
    pub label: String,
    pub description: String,
    pub default_value: String,
    pub validation: String,
}

/// A tool that needs a permission policy during agent setup.
#[derive(Debug, Clone)]
pub struct ToolPolicyView {
    /// Unique key for form field names (capability_id + "__" + tool_name, sanitized)
    pub key: String,
    pub capability_id: String,
    pub tool_name: String,
    /// Human-readable tool name (bare, without capability prefix)
    pub display_name: String,
    pub description: String,
    /// The agent author's recommended policy: "allow", "ask", or "block"
    pub recommended: String,
}

fn show_delegate_permission(agent_id: &str) -> bool {
    agent_id == DEFAULT_AGENT_ID
}

// ── Templates ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "agents.html")]
struct AgentsTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    agents: Vec<InstalledAgentView>,
    marketplace_agents: Vec<MarketplaceAgentView>,
    providers: Vec<ProviderOption>,
    global_provider: String,
    global_model: String,
    global_model_display_name: String,
    global_temperature: String,
    global_image_provider: String,
    global_image_model: String,
    global_image_model_display_name: String,
    marketplace_error: String,
    error: Option<String>,
    success: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AgentsQuery {
    pub error: Option<String>,
    pub success: Option<String>,
}

#[derive(Template)]
#[template(path = "agents_setup.html")]
struct AgentSetupTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    agent_id: String,
    agent_name: String,
    is_update_flow: bool,
    steps: Vec<SetupStepView>,
    tool_policies: Vec<ToolPolicyView>,
    discovery_warning: Option<String>,
    providers: Vec<ProviderOption>,
}

#[derive(Template)]
#[template(path = "agents_update.html")]
struct AgentUpdateTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    agent_name: String,
    installed_version: String,
    target_version: String,
    entry_json: String,
}

// ── Agent detail page ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CredentialView {
    pub capability_id: String,
    pub name: String,
    pub scope: String,
    pub description: String,
    pub required: bool,
    pub is_set: bool,
    pub set_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CapabilityView {
    pub id: String,
    pub effective_network_mode: String,
    pub network_access_policy: String,
    pub host_policies: Vec<NetworkHostPolicyView>,
    pub filesystem: String,
    pub max_memory_mb: u32,
    pub max_cpu_fraction: String,
    pub pids_limit: u32,
    pub tools: Vec<ToolView>,
    pub credentials: Vec<CredentialView>,
}

#[derive(Debug, Clone)]
pub struct NetworkHostPolicyView {
    pub host: String,
    pub policy: String,
    pub source: String,
    pub removable: bool,
}

#[derive(Debug, Clone)]
pub struct ToolView {
    pub name: String,
    pub display_name: String,
    pub description: String,
    /// The effective policy for the current viewer ("allow", "ask", "block", or "not set").
    pub policy: String,
    pub capability_id: String,
    /// The global default policy ("allow", "ask", "block", or "not set").
    pub global_default: String,
    /// Whether the current user has a personal override.
    pub has_override: bool,
}

#[derive(Debug, Clone)]
pub struct EgressEntryView {
    pub capability_id: String,
    pub method: String,
    pub host: String,
    pub port: i32,
    pub allowed: bool,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct StorageEntryView {
    pub id: String,
    pub user_id: String,
    pub key: String,
    pub value: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct MemoryEntryView {
    pub id: String,
    pub user_id: String,
    pub memory: String,
    pub tags: String,
    pub source: String,
    pub updated_at: String,
}

/// A built-in tool (delegate_to_agent, emit_event) with its current policy.
#[derive(Debug, Clone)]
pub struct BuiltinPolicyView {
    pub capability_id: String,
    pub tool_name: String,
    pub display_name: String,
    /// The effective policy for the current viewer.
    pub policy: String,
    /// Whether the current user has a personal override.
    pub has_override: bool,
}

#[derive(Template)]
#[template(path = "agents_detail.html")]
struct AgentDetailTemplate {
    active_nav: &'static str,
    active_tab: String,
    agent_id: String,
    agent_name: String,
    is_admin: bool,
    base_path: String,
    overview: AgentOverviewStats,
    runtime_settings: AgentRuntimeSettingsView,
    automation: AgentAutomationView,
    capabilities: Vec<CapabilityView>,
    storage_entries: Vec<StorageEntryView>,
    memory_entries: Vec<MemoryEntryView>,
    builtin_policies: Vec<BuiltinPolicyView>,
    egress_entries: Vec<EgressEntryView>,
    improvement_insights: Vec<InsightView>,
    improvement_signal_count: i64,
    error: Option<String>,
    success: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct InsightView {
    pub id: String,
    pub lesson_text: String,
    pub insight_type: String,
    pub status: String,
    pub confidence_pct: u8,
    pub supporting_signals: i64,
    pub promotion_threshold: i64,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct AgentOverviewStats {
    pub capability_count: usize,
    pub storage_count: i64,
    pub memory_count: i64,
    pub network_request_count: i64,
    pub permissions_allow_count: usize,
    pub permissions_ask_count: usize,
    pub permissions_block_count: usize,
    pub secrets_set_count: usize,
    pub secrets_missing_count: usize,
}

#[derive(Debug, Clone)]
pub struct AutomationPresetView {
    pub label: String,
    pub cron_description: String,
}

#[derive(Debug, Clone)]
pub struct AgentAutomationView {
    pub supported: bool,
    pub enabled: bool,
    pub ready: bool,
    pub missing_required_credentials: bool,
    pub missing_default_pipe: bool,
    pub active_schedule_count: usize,
    pub total_schedule_count: usize,
    pub presets: Vec<AutomationPresetView>,
}

#[derive(Debug, Clone)]
pub struct AgentRuntimeSettingsView {
    pub autonomy_level: String,
    pub use_advanced_limits: bool,
    pub max_tool_loop_iterations: u32,
    pub max_delegation_hops: i32,
    pub agent_default_autonomy_level: String,
    pub agent_default_max_tool_loop_iterations: u32,
    pub agent_default_max_delegation_hops: i32,
    pub has_user_override: bool,
}

// ── Form structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SetAgentModelForm {
    pub provider_id: String,
    pub model_id: String,
    #[serde(default = "default_temp_str")]
    pub temperature: String,
}
fn default_temp_str() -> String {
    "0.7".into()
}

#[derive(Debug, Deserialize)]
pub struct SetToolPolicyForm {
    pub capability_id: String,
    pub tool_name: String,
    pub policy: String,
    /// `"global"` to set the global default (admin only), `"user"` for a
    /// personal override.  Defaults to `"user"` if omitted.
    #[serde(default = "default_scope_user")]
    pub scope: String,
}
fn default_scope_user() -> String {
    "user".into()
}

#[derive(Debug, Deserialize)]
pub struct ResetToolPolicyForm {
    pub capability_id: String,
    pub tool_name: String,
    pub return_to: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetNetworkAccessForm {
    pub capability_id: String,
    pub access: String,
}

#[derive(Debug, Deserialize)]
pub struct SetNetworkHostPolicyForm {
    pub capability_id: String,
    pub host: String,
    pub policy: String,
}

#[derive(Debug, Deserialize)]
pub struct DeleteNetworkHostForm {
    pub capability_id: String,
    pub host: String,
}

#[derive(Debug, Deserialize)]
pub struct SetAgentCredentialForm {
    pub capability_id: String,
    pub credential_name: String,
    pub scope: String,
    pub value: String,
    pub return_to: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AgentDetailQuery {
    pub error: Option<String>,
    pub success: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteStorageForm {
    pub entry_id: String,
}

#[derive(Debug, Deserialize)]
pub struct DeleteMemoryForm {
    pub memory_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CredentialDeleteQuery {
    pub return_to: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetDefaultModelForm {
    pub provider_id: String,
    pub model_id: String,
    #[serde(default = "default_temp_str")]
    pub temperature: String,
}

#[derive(Debug, Deserialize)]
pub struct SetDefaultImageModelForm {
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SetAgentImageModelForm {
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ToggleAutomationForm {
    pub action: String,
}

#[derive(Debug, Deserialize)]
pub struct SetRuntimeSettingsForm {
    pub autonomy_level: String,
    pub use_advanced_limits: Option<String>,
    pub iterations: Option<String>,
    pub delegation_hops: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InstallForm {
    /// JSON-encoded MarketplaceEntry (passed as hidden field)
    pub entry_json: String,
}

#[derive(Debug, Serialize)]
pub struct StartAgentUpdateResponse {
    pub job_id: String,
}

#[derive(Debug, Serialize)]
pub struct AgentUpdateStatusResponse {
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

#[derive(Debug, Deserialize)]
pub struct ToggleAutoUpdateForm {
    pub auto_update: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RateAgentForm {
    pub rating: u8,
}

#[derive(Debug, Serialize)]
struct SubmitRatingRequest {
    rating: u8,
}

#[derive(Debug, Deserialize)]
pub struct SetupSubmitForm {
    /// Step values as step_<id>=<value> pairs
    #[serde(flatten)]
    pub values: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct SetupFlowQuery {
    pub flow: Option<String>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// Main agents page: installed agents + marketplace catalogue
pub async fn agents_index(
    user: AuthUser,
    Query(q): Query<AgentsQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }
    // Installed agents from in-memory map + DB metadata
    let agents_map = state.agents.load();
    let mut agents: Vec<InstalledAgentView> = Vec::new();

    for def in agents_map.values() {
        let db_row = sqlx::query_as::<
            _,
            (
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                f64,
                i32,
                i32,
                String,
                i32,
            ),
        >(
            "SELECT provider_id, model_id, image_provider_id, image_model_id, \
             temperature, is_bundled, setup_complete, version, auto_update \
             FROM agents WHERE id = ?",
        )
        .bind(&def.id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();

        let (
            provider_id,
            model_id,
            image_provider_id,
            image_model_id,
            _temp,
            is_bundled,
            setup_complete,
            version,
            auto_update,
        ) = db_row.unwrap_or((None, None, None, None, 0.7, 0, 1, "0.1.0".into(), 0));

        let prov_id = provider_id.unwrap_or_default();
        let mod_id = model_id.unwrap_or_default();
        let model_display_name = resolve_model_display_name(&prov_id, &mod_id);
        let image_prov_id = image_provider_id.unwrap_or_default();
        let image_mod_id = image_model_id.unwrap_or_default();
        let image_model_display_name = resolve_model_display_name(&image_prov_id, &image_mod_id);

        agents.push(InstalledAgentView {
            id: def.id.clone(),
            name: def.localized_name(&user.language),
            version,
            provider_id: prov_id,
            model_id: mod_id,
            model_display_name,
            image_provider_id: image_prov_id,
            image_model_id: image_mod_id,
            image_model_display_name,
            capability_count: def.capability_manifests.len(),
            is_bundled: is_bundled != 0,
            setup_complete: setup_complete != 0,
            auto_update: auto_update != 0,
            update_available: false,
            marketplace_version: String::new(),
        });
    }
    drop(agents_map);
    agents.sort_by(|a, b| a.id.cmp(&b.id));

    // Also include agents with pending setup (not yet in memory)
    let pending_rows = sqlx::query_as::<_, (String, String)>(
        "SELECT id, display_name FROM agents WHERE setup_complete = 0",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    for (id, name) in pending_rows {
        if !agents.iter().any(|a| a.id == id) {
            let localized_name = crate::agents::loader::load_one(
                &std::path::Path::new(&state.config.installed_agents_dir).join(&id),
            )
            .await
            .map(|agent| agent.localized_name(&user.language))
            .unwrap_or(name);
            agents.push(InstalledAgentView {
                id,
                name: localized_name,
                version: String::new(),
                provider_id: String::new(),
                model_id: String::new(),
                model_display_name: String::new(),
                image_provider_id: String::new(),
                image_model_id: String::new(),
                image_model_display_name: String::new(),
                capability_count: 0,
                is_bundled: false,
                setup_complete: false,
                auto_update: false,
                update_available: false,
                marketplace_version: String::new(),
            });
        }
    }

    // Marketplace catalogue
    let marketplace_url = state.config.marketplace_url.clone();
    let (marketplace_agents, marketplace_error) =
        match marketplace::fetch_catalogue(&marketplace_url).await {
            Ok(catalogue) => {
                // Build a map of installed agent ID -> installed version
                let installed_versions: std::collections::HashMap<String, String> = agents
                    .iter()
                    .map(|a| (a.id.clone(), a.version.clone()))
                    .collect();

                let views: Vec<MarketplaceAgentView> = catalogue
                    .agents
                    .iter()
                    .map(|entry| {
                        let entry_json = serde_json::to_string(entry).unwrap_or_default();
                        let is_installed = installed_versions.contains_key(&entry.id);
                        let installed_version = installed_versions
                            .get(&entry.id)
                            .cloned()
                            .unwrap_or_default();
                        let update_available = is_installed
                            && marketplace::is_newer_version(&installed_version, &entry.version);

                        // Also update the installed agents list with marketplace info
                        MarketplaceAgentView {
                            id: entry.id.clone(),
                            name: entry.localized_name(&user.language),
                            description: entry.localized_description(&user.language),
                            version: entry.version.clone(),
                            author: entry.author.clone(),
                            is_installed,
                            installed_version,
                            update_available,
                            entry_json,
                            average_rating: entry.average_rating,
                            rating_count: entry.rating_count,
                        }
                    })
                    .collect();

                // Back-fill update_available into the installed agents list
                for mv in &views {
                    if mv.update_available {
                        if let Some(agent) = agents.iter_mut().find(|a| a.id == mv.id) {
                            agent.update_available = true;
                            agent.marketplace_version = mv.version.clone();
                        }
                    }
                }

                (views, String::new())
            }
            Err(e) => {
                tracing::warn!("Failed to fetch marketplace: {e}");
                (vec![], format!("Failed to fetch marketplace: {e}"))
            }
        };

    // Available providers for model assignment
    let providers = load_provider_options(&state).await;

    // Global default model
    let global = model::get_global_default(&state.db).await.ok().flatten();
    let (global_provider, global_model, global_temperature) = match global {
        Some(m) => (m.provider_id, m.model_id, format!("{:.1}", m.temperature)),
        None => (String::new(), String::new(), "0.7".into()),
    };
    let global_model_display_name = resolve_model_display_name(&global_provider, &global_model);
    let global_image = model::get_global_image_default(&state.db)
        .await
        .ok()
        .flatten();
    let (global_image_provider, global_image_model) = match global_image {
        Some(m) => (m.provider_id, m.model_id),
        None => (String::new(), String::new()),
    };
    let global_image_model_display_name =
        resolve_model_display_name(&global_image_provider, &global_image_model);

    match (AgentsTemplate {
        active_nav: "agents",
        is_admin: user.is_admin,
        base_path: base_path.clone(),
        agents,
        marketplace_agents,
        providers,
        global_provider,
        global_model,
        global_model_display_name,
        global_temperature,
        global_image_provider,
        global_image_model,
        global_image_model_display_name,
        marketplace_error,
        error: q.error,
        success: q.success,
    })
    .render()
    {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Install an agent from the marketplace
pub async fn install_agent(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<InstallForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    let entry: MarketplaceEntry = match serde_json::from_str(&form.entry_json) {
        Ok(e) => e,
        Err(e) => {
            error!("Invalid entry JSON: {e}");
            return Redirect::to(&format!(
                "{}/agents?error=Invalid+agent+data.+Please+try+again.",
                base_path
            ))
            .into_response();
        }
    };

    let docker = match bollard::Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to connect to Docker: {e}");
            return Redirect::to(&format!(
                "{}/agents?error=Cannot+connect+to+Docker.+Is+Docker+running%3F",
                base_path
            ))
            .into_response();
        }
    };

    match marketplace::install_agent(
        &entry,
        &state.config.installed_agents_dir,
        &state.db,
        &state.agents,
        &docker,
        &state.capabilities,
        &state.credentials,
    )
    .await
    {
        Ok(agent_def) => {
            let setup_required = marketplace::agent_requires_setup(
                &state.db,
                &state.credentials,
                &entry.id,
                &agent_def,
            )
            .await
            .unwrap_or(true);
            if setup_required {
                Redirect::to(&format!("{}/agents/{}/setup", base_path, entry.id)).into_response()
            } else {
                prefixed_redirect(&base_path, "/agents").into_response()
            }
        }
        Err(e) => {
            error!("Agent installation failed: {e}");
            let text = format!("Agent installation failed: {e}");
            let msg = urlencoding::encode(&text);
            Redirect::to(&format!("{}/agents?error={msg}", base_path)).into_response()
        }
    }
}

/// Show the agent update wizard.
pub async fn update_wizard(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<InstallForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    let entry: MarketplaceEntry = match serde_json::from_str(&form.entry_json) {
        Ok(e) => e,
        Err(e) => {
            error!("Invalid entry JSON: {e}");
            return Redirect::to(&format!(
                "{}/agents?error=Invalid+agent+data.+Please+try+again.",
                base_path
            ))
            .into_response();
        }
    };

    let installed_version =
        sqlx::query_scalar::<_, String>("SELECT version FROM agents WHERE id = ?")
            .bind(&entry.id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();

    match (AgentUpdateTemplate {
        active_nav: "agents",
        is_admin: user.is_admin,
        base_path,
        agent_name: entry.localized_name(&user.language),
        installed_version,
        target_version: entry.version.clone(),
        entry_json: form.entry_json,
    })
    .render()
    {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Show the setup wizard for an agent
pub async fn setup_wizard(
    user: AuthUser,
    Path(agent_id): Path<String>,
    Query(flow): Query<SetupFlowQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&base_path, "/chat").into_response();
    }
    let installed_dir = &state.config.installed_agents_dir;
    let agent_dir = std::path::Path::new(installed_dir).join(&agent_id);

    let agent_def = match crate::agents::loader::load_one(&agent_dir).await {
        Ok(a) => a,
        Err(e) => {
            error!("Failed to load agent for setup: {e}");
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    let steps: Vec<SetupStepView> = agent_def
        .install_steps
        .iter()
        .map(|s| SetupStepView {
            id: s.id.clone(),
            step_type: match s.step_type {
                StepType::Input => "input".to_string(),
                StepType::Test => "test".to_string(),
            },
            label: localization::localized_install_step_label(&agent_def, s, &user.language),
            description: localization::localized_install_step_description(
                &agent_def,
                s,
                &user.language,
            ),
            default_value: s.default.clone().unwrap_or_default(),
            validation: s.validation.clone().unwrap_or_default(),
        })
        .collect();

    let providers = load_provider_options(&state).await;

    // Build tool policy views from capability manifests
    let mut tool_policies: Vec<ToolPolicyView> = Vec::new();
    let mut discovery_failed_caps: Vec<String> = Vec::new();
    for (cap_id, manifest) in &agent_def.capability_manifests {
        let tools = if manifest.tool_source == ToolSource::Dynamic {
            if let Err(e) = sync_dynamic_tools_for_capability(
                &state.db,
                &state.capabilities,
                &state.credentials,
                &agent_id,
                cap_id,
                manifest,
                &agent_def.capability_manifests,
            )
            .await
            {
                error!(
                    agent_id = %agent_id,
                    capability_id = %cap_id,
                    "Dynamic tool discovery failed in setup: {e}"
                );
                discovery_failed_caps.push(cap_id.clone());
            }

            load_discovered_tools(&state.db, &agent_id, cap_id)
                .await
                .unwrap_or_default()
        } else {
            manifest.tools.clone()
        };

        for tool in &tools {
            let key = format!("{}_{}", cap_id, tool.name).replace('-', "_");
            tool_policies.push(ToolPolicyView {
                key,
                capability_id: cap_id.clone(),
                tool_name: tool.name.clone(),
                display_name: localization::localized_tool_display_name(
                    &agent_def,
                    cap_id,
                    &tool.name,
                    &tool.name.replace('_', " "),
                    &user.language,
                ),
                description: localization::localized_tool_description(
                    &agent_def,
                    cap_id,
                    &tool.name,
                    &tool.description,
                    &user.language,
                ),
                recommended: tool.effective_recommended_policy().to_string(),
            });
        }
    }
    // Add built-in tools
    if show_delegate_permission(&agent_id) {
        tool_policies.push(ToolPolicyView {
            key: "__builtin___delegate_to_agent".to_string(),
            capability_id: "__builtin__".to_string(),
            tool_name: "delegate_to_agent".to_string(),
            display_name: "Delegate to Agent".to_string(),
            description: "Allows the agent to hand off tasks to other specialist agents."
                .to_string(),
            recommended: "ask".to_string(),
        });
    }
    tool_policies.push(ToolPolicyView {
        key: "__builtin___memory_remember".to_string(),
        capability_id: "__builtin__".to_string(),
        tool_name: "memory_remember".to_string(),
        display_name: "Memory Remember".to_string(),
        description: "Allows the agent to save long-term memory notes for future conversations."
            .to_string(),
        recommended: "allow".to_string(),
    });
    tool_policies.push(ToolPolicyView {
        key: "__builtin___memory_forget".to_string(),
        capability_id: "__builtin__".to_string(),
        tool_name: "memory_forget".to_string(),
        display_name: "Memory Forget".to_string(),
        description: "Allows the agent to remove stored memory notes.".to_string(),
        recommended: "allow".to_string(),
    });
    tool_policies.push(ToolPolicyView {
        key: "__builtin___memory_search".to_string(),
        capability_id: "__builtin__".to_string(),
        tool_name: "memory_search".to_string(),
        display_name: "Memory Search".to_string(),
        description: "Allows the agent to search saved memory using BM25 relevance.".to_string(),
        recommended: "allow".to_string(),
    });
    tool_policies.push(ToolPolicyView {
        key: "__builtin___memory_list".to_string(),
        capability_id: "__builtin__".to_string(),
        tool_name: "memory_list".to_string(),
        display_name: "Memory List".to_string(),
        description: "Allows the agent to list saved memory notes.".to_string(),
        recommended: "allow".to_string(),
    });

    match (AgentSetupTemplate {
        active_nav: "agents",
        is_admin: user.is_admin,
        base_path,
        agent_id,
        agent_name: agent_def.localized_name(&user.language),
        is_update_flow: flow.flow.as_deref() == Some("update"),
        steps,
        tool_policies,
        discovery_warning: if discovery_failed_caps.is_empty() {
            None
        } else {
            Some(discovery_failed_caps.join(", "))
        },
        providers,
    })
    .render()
    {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Submit setup wizard values
pub async fn setup_submit(
    user: AuthUser,
    Path(agent_id): Path<String>,
    Query(flow): Query<SetupFlowQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<SetupSubmitForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    let installed_dir = &state.config.installed_agents_dir;
    let agent_dir = std::path::Path::new(installed_dir).join(&agent_id);

    let agent_def = match crate::agents::loader::load_one(&agent_dir).await {
        Ok(a) => a,
        Err(e) => {
            error!("Failed to load agent for setup: {e}");
            let text = format!("Failed to load agent: {e}");
            let msg = urlencoding::encode(&text);
            return Redirect::to(&format!("{}/agents?error={msg}", base_path)).into_response();
        }
    };

    // Process input steps: store credentials
    for step in &agent_def.install_steps {
        if step.step_type != StepType::Input {
            continue;
        }

        let key = format!("step_{}", step.id);
        let value = match form.values.get(&key) {
            Some(v) => v.trim().to_string(),
            None => continue,
        };

        if let Some(ref target) = step.store_as {
            let encrypted = match state.credentials.encrypt_raw(value.as_bytes()) {
                Ok(e) => e,
                Err(e) => {
                    error!("Failed to encrypt credential: {e}");
                    let msg = urlencoding::encode("encrypt_failed");
                    return Redirect::to(&format!(
                        "{}/agents/{agent_id}/setup?error={msg}",
                        base_path
                    ))
                    .into_response();
                }
            };

            if target.scope == "system_credential" {
                let id = uuid::Uuid::new_v4().to_string();
                if let Err(e) = sqlx::query(
                    "INSERT INTO system_credentials (id, capability_id, credential_name, encrypted_value) \
                     VALUES (?, ?, ?, ?) \
                     ON CONFLICT(capability_id, credential_name) DO UPDATE SET encrypted_value = excluded.encrypted_value",
                )
                .bind(&id)
                .bind(&target.capability_id)
                .bind(&target.credential_name)
                .bind(&encrypted)
                .execute(&state.db)
                .await
                {
                    error!("Failed to store system credential: {e}");
                    let msg = urlencoding::encode("store_cred_failed");
                    return Redirect::to(&format!("{}/agents/{agent_id}/setup?error={msg}", base_path)).into_response();
                }
            }
        }
    }

    // Process tool policies: extract policy_cap_*, policy_tool_*, policy_val_* fields
    // These are saved as GLOBAL defaults (not per-user) — they apply to all users.
    sync_dynamic_tools_for_agent(
        &state.db,
        &state.capabilities,
        &state.credentials,
        &agent_id,
        &agent_def.capability_manifests,
    )
    .await;

    {
        use crate::permissions::tool_policy::{self, ToolPolicy};

        let mut policies: Vec<(String, String, ToolPolicy)> = Vec::new();

        // Collect all unique keys from form values that match the pattern
        let keys: Vec<String> = form
            .values
            .keys()
            .filter(|k| k.starts_with("policy_val_"))
            .map(|k| k.strip_prefix("policy_val_").unwrap().to_string())
            .collect();

        for key in &keys {
            let cap_id = match form.values.get(&format!("policy_cap_{key}")) {
                Some(v) => v.clone(),
                None => continue,
            };
            let tool_name = match form.values.get(&format!("policy_tool_{key}")) {
                Some(v) => v.clone(),
                None => continue,
            };
            let policy_str = match form.values.get(&format!("policy_val_{key}")) {
                Some(v) => v.clone(),
                None => continue,
            };
            let policy = match ToolPolicy::from_str(&policy_str) {
                Ok(p) => p,
                Err(_) => continue,
            };
            policies.push((cap_id, tool_name, policy));
        }

        if !policies.is_empty() {
            if let Err(e) = tool_policy::set_global_policies(&state.db, &agent_id, &policies).await
            {
                error!("Failed to save global tool policies: {e}");
                let msg = urlencoding::encode("save_perms_failed");
                return Redirect::to(&format!(
                    "{}/agents/{agent_id}/setup?error={msg}",
                    base_path
                ))
                .into_response();
            }
        }
    }

    // Mark setup complete + load agent into memory
    if let Err(e) =
        marketplace::complete_setup(&agent_id, installed_dir, &state.db, &state.agents).await
    {
        error!("Failed to complete setup: {e}");
        let text = format!("Failed to complete setup: {e}");
        let msg = urlencoding::encode(&text);
        return Redirect::to(&format!("{}/agents?error={msg}", base_path)).into_response();
    }

    let success_key = if flow.flow.as_deref() == Some("update") {
        "updated"
    } else {
        "setup_complete"
    };
    let msg = urlencoding::encode(success_key);
    Redirect::to(&format!("{}/agents?success={msg}", base_path)).into_response()
}

/// Execute a test step during setup (HTMX endpoint)
pub async fn setup_test(
    user: AuthUser,
    Path((agent_id, step_id)): Path<(String, String)>,
    State(state): State<AppState>,
    Form(form): Form<SetupSubmitForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    let installed_dir = &state.config.installed_agents_dir;
    let agent_dir = std::path::Path::new(installed_dir).join(&agent_id);

    let agent_def = match crate::agents::loader::load_one(&agent_dir).await {
        Ok(a) => a,
        Err(e) => {
            return Html(format!(
                r#"<span class="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium bg-red-500/10 text-red-400 border border-red-500/20">Error: {e}</span>"#
            ))
            .into_response();
        }
    };

    let step = match agent_def.install_steps.iter().find(|s| s.id == step_id) {
        Some(s) => s,
        None => {
            return Html(r#"<span class="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium bg-red-500/10 text-red-400 border border-red-500/20">Step not found</span>"#.to_string())
                .into_response();
        }
    };

    let request = match &step.request {
        Some(r) => r,
        None => {
            return Html(
                r#"<span class="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium bg-red-500/10 text-red-400 border border-red-500/20">No test request defined</span>"#.to_string(),
            )
            .into_response();
        }
    };

    // Resolve template variables in the URL
    let mut url = request.url.clone();
    for (key, value) in &form.values {
        let step_id_key = key.strip_prefix("step_").unwrap_or(key);
        url = url.replace(&format!("{{{{{}}}}}", step_id_key), value);
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    let result = match request.method.to_uppercase().as_str() {
        "GET" => client.get(&url).send().await,
        "POST" => client.post(&url).send().await,
        _ => {
            return Html(format!(
                r#"<span class="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium bg-red-500/10 text-red-400 border border-red-500/20">Unsupported method: {}</span>"#,
                request.method
            ))
            .into_response();
        }
    };

    match result {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if status == request.expect_status {
                Html(format!(
                    r#"<span class="inline-flex items-center gap-1 px-2 py-0.5 rounded-full text-xs font-medium bg-emerald-500/10 text-emerald-400 border border-emerald-500/20"><span class="w-1 h-1 rounded-full bg-emerald-400"></span>Success (HTTP {status})</span>"#
                ))
                .into_response()
            } else {
                let body = resp.text().await.unwrap_or_default();
                let truncated = if body.len() > 200 {
                    format!("{}...", &body[..200])
                } else {
                    body
                };
                Html(format!(
                    r#"<span class="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium bg-red-500/10 text-red-400 border border-red-500/20">Failed: HTTP {status}</span><br><small class="text-xs text-slate-500">{truncated}</small>"#
                ))
                .into_response()
            }
        }
        Err(e) => Html(format!(
            r#"<span class="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium bg-red-500/10 text-red-400 border border-red-500/20">Connection failed: {e}</span>"#
        ))
        .into_response(),
    }
}

/// Agent detail page: capabilities, network policy, tools, egress log
pub async fn agent_detail(
    user: AuthUser,
    Path(agent_id): Path<String>,
    Query(q): Query<AgentDetailQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    render_agent_detail_page(user, agent_id, q, state, base_path, "overview").await
}

pub async fn agent_detail_storage(
    user: AuthUser,
    Path(agent_id): Path<String>,
    Query(q): Query<AgentDetailQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    render_agent_detail_page(user, agent_id, q, state, base_path, "storage").await
}

pub async fn agent_detail_memory(
    user: AuthUser,
    Path(agent_id): Path<String>,
    Query(q): Query<AgentDetailQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    render_agent_detail_page(user, agent_id, q, state, base_path, "memory").await
}

pub async fn agent_detail_network(
    user: AuthUser,
    Path(agent_id): Path<String>,
    Query(q): Query<AgentDetailQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    render_agent_detail_page(user, agent_id, q, state, base_path, "network").await
}

pub async fn agent_detail_permissions(
    user: AuthUser,
    Path(agent_id): Path<String>,
    Query(q): Query<AgentDetailQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    render_agent_detail_page(user, agent_id, q, state, base_path, "permissions").await
}

pub async fn agent_detail_improvement(
    user: AuthUser,
    Path(agent_id): Path<String>,
    Query(q): Query<AgentDetailQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    render_agent_detail_page(user, agent_id, q, state, base_path, "improvement").await
}

pub async fn agent_detail_secrets(
    user: AuthUser,
    Path(agent_id): Path<String>,
    Query(q): Query<AgentDetailQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    render_agent_detail_page(user, agent_id, q, state, base_path, "secrets").await
}

pub async fn set_runtime_settings_handler(
    user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<SetRuntimeSettingsForm>,
) -> Response {
    let return_to = format!("{}/agents/{}", base_path, agent_id);
    let Some(autonomy_level) =
        crate::agents::runtime_limits::AutonomyLevel::parse(&form.autonomy_level)
    else {
        return Redirect::to(&format!("{}?error=runtime_settings_invalid", return_to))
            .into_response();
    };
    let use_advanced_limits = form.use_advanced_limits.is_some();
    let base_limits = crate::agents::runtime_limits::limits_for_autonomy(autonomy_level);
    let iterations = if use_advanced_limits {
        match form
            .iterations
            .as_deref()
            .unwrap_or_default()
            .trim()
            .parse::<i64>()
        {
            Ok(v) if v >= 0 => v,
            _ => {
                return Redirect::to(&format!("{}?error=runtime_settings_invalid", return_to))
                    .into_response();
            }
        }
    } else {
        base_limits.max_tool_loop_iterations as i64
    };
    let delegation_hops = if use_advanced_limits {
        match form
            .delegation_hops
            .as_deref()
            .unwrap_or_default()
            .trim()
            .parse::<i64>()
        {
            Ok(v) if v >= 0 => v,
            _ => {
                return Redirect::to(&format!("{}?error=runtime_settings_invalid", return_to))
                    .into_response();
            }
        }
    } else {
        base_limits.max_delegation_hops as i64
    };

    if let Err(e) = sqlx::query(
        "INSERT INTO user_agent_runtime_settings (
            user_id, agent_id, autonomy_level, use_advanced_limits,
            max_tool_loop_iterations, max_delegation_hops, updated_at
         )
         VALUES (?, ?, ?, ?, ?, ?, datetime('now'))
         ON CONFLICT(user_id, agent_id) DO UPDATE
         SET autonomy_level = excluded.autonomy_level,
             use_advanced_limits = excluded.use_advanced_limits,
             max_tool_loop_iterations = excluded.max_tool_loop_iterations,
             max_delegation_hops = excluded.max_delegation_hops,
             updated_at = datetime('now')",
    )
    .bind(&user.user_id)
    .bind(&agent_id)
    .bind(autonomy_level.as_str())
    .bind(if use_advanced_limits { 1 } else { 0 })
    .bind(iterations)
    .bind(delegation_hops)
    .execute(&state.db)
    .await
    {
        error!("Failed to save runtime settings: {e}");
        return Redirect::to(&format!("{}?error=runtime_settings_save_failed", return_to))
            .into_response();
    }

    Redirect::to(&format!("{}?success=runtime_settings_saved", return_to)).into_response()
}

pub async fn agent_automation_toggle(
    user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<ToggleAutomationForm>,
) -> Response {
    let return_to = format!("{}/agents/{}", base_path, agent_id);

    let agents_map = state.agents.load();
    let agent = match agents_map.get(&agent_id) {
        Some(a) => a.clone(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    drop(agents_map);

    if agent.automation.schedules.is_empty() {
        return Redirect::to(&format!("{}?error=automation_not_supported", return_to))
            .into_response();
    }

    let action = form.action.trim().to_lowercase();
    if action == "disable" {
        for preset in &agent.automation.schedules {
            let schedule_name = automation_schedule_name(&agent_id, &preset.id);
            let _ = sqlx::query(
                "UPDATE schedules SET active = 0 WHERE user_id = ? AND agent_id = ? AND name = ?",
            )
            .bind(&user.user_id)
            .bind(&agent_id)
            .bind(&schedule_name)
            .execute(&state.db)
            .await;
        }
        return Redirect::to(&format!("{}?success=automation_disabled", return_to)).into_response();
    }

    if action != "enable" {
        return Redirect::to(&format!("{}?error=automation_invalid_action", return_to))
            .into_response();
    }

    let pipe_ids = sqlx::query_scalar::<_, String>(
        "SELECT id FROM pipes WHERE user_id = ? AND active = 1 AND default_agent_id = ? ORDER BY name",
    )
    .bind(&user.user_id)
    .bind(&agent_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    if pipe_ids.is_empty() {
        return Redirect::to(&format!("{}?error=automation_missing_pipe", return_to))
            .into_response();
    }

    for (cap_id, manifest) in &agent.capability_manifests {
        for cred in &manifest.credentials {
            if !cred.required {
                continue;
            }
            let present = match cred.scope {
                crate::capabilities::manifest::CredentialScope::System => state
                    .credentials
                    .get_system(cap_id, &cred.name)
                    .await
                    .ok()
                    .flatten()
                    .is_some(),
                crate::capabilities::manifest::CredentialScope::User => state
                    .credentials
                    .get_user(&user.user_id, cap_id, &cred.name)
                    .await
                    .ok()
                    .flatten()
                    .is_some(),
            };
            if !present {
                return Redirect::to(&format!(
                    "{}?error=automation_missing_credentials",
                    return_to
                ))
                .into_response();
            }
        }
    }

    let timezone = sqlx::query_scalar::<_, String>("SELECT timezone FROM users WHERE id = ?")
        .bind(&user.user_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "UTC".to_string());
    let now = chrono::Utc::now();

    for preset in &agent.automation.schedules {
        if crate::schedules::validate_cron(&preset.cron_expression).is_err() {
            return Redirect::to(&format!("{}?error=automation_invalid_cron", return_to))
                .into_response();
        }
        let next_run =
            match crate::schedules::compute_next_run(&preset.cron_expression, &timezone, now) {
                Ok(next) => next.format("%Y-%m-%d %H:%M:%S").to_string(),
                Err(_) => {
                    return Redirect::to(&format!("{}?error=automation_invalid_cron", return_to))
                        .into_response();
                }
            };
        let localized_prompt =
            localization::localized_schedule_prompt(&agent, preset, &user.language);
        let localized_cron_description =
            localization::localized_schedule_cron_description(&agent, preset, &user.language);

        let schedule_name = automation_schedule_name(&agent_id, &preset.id);
        let existing = sqlx::query_scalar::<_, String>(
            "SELECT id FROM schedules WHERE user_id = ? AND agent_id = ? AND name = ? LIMIT 1",
        )
        .bind(&user.user_id)
        .bind(&agent_id)
        .bind(&schedule_name)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();

        let schedule_id = if let Some(existing_id) = existing {
            let _ = sqlx::query(
                "UPDATE schedules
                 SET prompt = ?, cron_expression = ?, cron_description = ?, active = 1, one_shot = 0, next_run_at = ?, agent_id = ?
                 WHERE id = ?",
            )
            .bind(&localized_prompt)
            .bind(&preset.cron_expression)
            .bind(&localized_cron_description)
            .bind(&next_run)
            .bind(&agent_id)
            .bind(&existing_id)
            .execute(&state.db)
            .await;
            existing_id
        } else {
            match crate::schedules::create_schedule(
                &state.db,
                &user.user_id,
                Some(&agent_id),
                &schedule_name,
                &localized_prompt,
                &preset.cron_expression,
                &localized_cron_description,
                &timezone,
                &pipe_ids,
            )
            .await
            {
                Ok(id) => id,
                Err(_) => {
                    return Redirect::to(&format!("{}?error=automation_save_failed", return_to))
                        .into_response();
                }
            }
        };

        let _ = sqlx::query("DELETE FROM schedule_pipes WHERE schedule_id = ?")
            .bind(&schedule_id)
            .execute(&state.db)
            .await;
        for pipe_id in &pipe_ids {
            let _ = sqlx::query(
                "INSERT OR IGNORE INTO schedule_pipes (schedule_id, pipe_id) VALUES (?, ?)",
            )
            .bind(&schedule_id)
            .bind(pipe_id)
            .execute(&state.db)
            .await;
        }
    }

    Redirect::to(&format!("{}?success=automation_enabled", return_to)).into_response()
}

async fn render_agent_detail_page(
    user: AuthUser,
    agent_id: String,
    q: AgentDetailQuery,
    state: AppState,
    base_path: String,
    active_tab: &str,
) -> Response {
    // Load agent definition
    let agents_map = state.agents.load();
    let agent = match agents_map.get(&agent_id) {
        Some(a) => a.clone(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    drop(agents_map);

    let automation_presets: Vec<AutomationPresetView> = agent
        .automation
        .schedules
        .iter()
        .map(|preset| AutomationPresetView {
            label: localization::localized_schedule_label(&agent, preset, &user.language),
            cron_description: localization::localized_schedule_cron_description(
                &agent,
                preset,
                &user.language,
            ),
        })
        .collect();
    let agent_name = agent.localized_name(&user.language);
    let effective_runtime = crate::agents::runtime_limits::resolve_effective_runtime_settings(
        &state.db,
        &user.user_id,
        &agent_id,
    )
    .await
    .unwrap_or_else(|_| {
        let level = crate::agents::runtime_limits::AutonomyLevel::Medium;
        let limits = crate::agents::runtime_limits::limits_for_autonomy(level);
        crate::agents::runtime_limits::EffectiveRuntimeSettings {
            autonomy_level: level,
            use_advanced_limits: false,
            max_tool_loop_iterations: limits.max_tool_loop_iterations,
            max_delegation_hops: limits.max_delegation_hops,
            agent_default_level: level,
            agent_default_limits: limits,
        }
    });
    let has_user_override = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM user_agent_runtime_settings WHERE user_id = ? AND agent_id = ?",
    )
    .bind(&user.user_id)
    .bind(&agent_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0)
        > 0;
    let runtime_settings = AgentRuntimeSettingsView {
        autonomy_level: effective_runtime.autonomy_level.as_str().to_string(),
        use_advanced_limits: effective_runtime.use_advanced_limits,
        max_tool_loop_iterations: effective_runtime.max_tool_loop_iterations,
        max_delegation_hops: effective_runtime.max_delegation_hops,
        agent_default_autonomy_level: effective_runtime.agent_default_level.as_str().to_string(),
        agent_default_max_tool_loop_iterations: effective_runtime
            .agent_default_limits
            .max_tool_loop_iterations,
        agent_default_max_delegation_hops: effective_runtime
            .agent_default_limits
            .max_delegation_hops,
        has_user_override,
    };

    let automation_supported = !agent.automation.schedules.is_empty();
    let pipe_count = if automation_supported {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pipes WHERE user_id = ? AND active = 1 AND default_agent_id = ?",
        )
        .bind(&user.user_id)
        .bind(&agent_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0)
    } else {
        0
    };
    let missing_default_pipe = automation_supported && pipe_count == 0;

    let mut missing_required_credentials = false;
    if automation_supported {
        'cred_check: for (cap_id, manifest) in &agent.capability_manifests {
            for cred in &manifest.credentials {
                if !cred.required {
                    continue;
                }
                let present = match cred.scope {
                    crate::capabilities::manifest::CredentialScope::System => state
                        .credentials
                        .get_system(cap_id, &cred.name)
                        .await
                        .ok()
                        .flatten()
                        .is_some(),
                    crate::capabilities::manifest::CredentialScope::User => state
                        .credentials
                        .get_user(&user.user_id, cap_id, &cred.name)
                        .await
                        .ok()
                        .flatten()
                        .is_some(),
                };
                if !present {
                    missing_required_credentials = true;
                    break 'cred_check;
                }
            }
        }
    }

    let mut active_schedule_count = 0usize;
    if automation_supported {
        for preset in &agent.automation.schedules {
            let schedule_name = automation_schedule_name(&agent_id, &preset.id);
            let active = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM schedules WHERE user_id = ? AND agent_id = ? AND name = ? AND active = 1",
            )
            .bind(&user.user_id)
            .bind(&agent_id)
            .bind(&schedule_name)
            .fetch_one(&state.db)
            .await
            .unwrap_or(0);
            if active > 0 {
                active_schedule_count += 1;
            }
        }
    }

    let automation_ready =
        automation_supported && !missing_default_pipe && !missing_required_credentials;
    let automation_enabled = automation_supported
        && active_schedule_count == agent.automation.schedules.len()
        && !agent.automation.schedules.is_empty();

    let automation = AgentAutomationView {
        supported: automation_supported,
        enabled: automation_enabled,
        ready: automation_ready,
        missing_required_credentials,
        missing_default_pipe,
        active_schedule_count,
        total_schedule_count: agent.automation.schedules.len(),
        presets: automation_presets,
    };

    // Build capability views
    let mut capabilities: Vec<CapabilityView> = Vec::new();

    // Get global default policies for this agent
    let global_policies =
        crate::permissions::tool_policy::get_global_policies_for_agent(&state.db, &agent_id)
            .await
            .unwrap_or_default();

    let global_policy_map: std::collections::HashMap<(String, String), String> = global_policies
        .into_iter()
        .map(|p| {
            (
                (p.capability_id, p.tool_name),
                p.policy.as_str().to_string(),
            )
        })
        .collect();

    // Get per-user override policies (if any)
    let user_overrides = crate::permissions::tool_policy::get_policies_for_agent(
        &state.db,
        &user.user_id,
        &agent_id,
    )
    .await
    .unwrap_or_default();

    let override_map: std::collections::HashMap<(String, String), String> = user_overrides
        .into_iter()
        .map(|p| {
            (
                (p.capability_id, p.tool_name),
                p.policy.as_str().to_string(),
            )
        })
        .collect();

    let network_access_overrides =
        crate::permissions::network_policy::get_user_access_overrides_for_agent(
            &state.db,
            &user.user_id,
            &agent_id,
        )
        .await
        .unwrap_or_default();

    let network_host_override_rows =
        crate::permissions::network_policy::get_user_host_overrides_for_agent(
            &state.db,
            &user.user_id,
            &agent_id,
        )
        .await
        .unwrap_or_default();
    let mut network_host_overrides: std::collections::HashMap<
        String,
        Vec<(String, crate::permissions::network_policy::HostPolicy)>,
    > = std::collections::HashMap::new();
    for row in network_host_override_rows {
        network_host_overrides
            .entry(row.capability_id)
            .or_default()
            .push((row.host, row.policy));
    }

    for (cap_id, manifest) in &agent.capability_manifests {
        let access_override = network_access_overrides.get(cap_id).copied();
        let host_overrides = network_host_overrides
            .get(cap_id)
            .cloned()
            .unwrap_or_default();
        let effective_network = crate::permissions::network_policy::build_effective_policy(
            &manifest.network,
            access_override,
            &host_overrides,
        );
        let effective_network_mode = match effective_network.mode {
            crate::capabilities::manifest::NetworkMode::None => "none",
            crate::capabilities::manifest::NetworkMode::Allowlist => "allowlist",
            crate::capabilities::manifest::NetworkMode::Any => "any",
        };
        let network_access_policy = match access_override {
            Some(crate::permissions::network_policy::NetworkAccessPolicy::Allow) => "allow",
            Some(crate::permissions::network_policy::NetworkAccessPolicy::Deny) => "deny",
            None => "allow",
        };
        let host_override_map: std::collections::HashMap<
            String,
            crate::permissions::network_policy::HostPolicy,
        > = host_overrides.into_iter().collect();
        let mut host_policies: Vec<NetworkHostPolicyView> = manifest
            .network
            .hosts
            .iter()
            .map(|host| {
                let normalized = host.to_ascii_lowercase();
                let override_policy = host_override_map.get(&normalized);
                let policy = match override_policy {
                    Some(crate::permissions::network_policy::HostPolicy::Deny) => "deny",
                    _ => "allow",
                };
                NetworkHostPolicyView {
                    host: normalized,
                    policy: policy.to_string(),
                    source: "default".to_string(),
                    removable: false,
                }
            })
            .collect();

        for (host, policy) in &host_override_map {
            if manifest
                .network
                .hosts
                .iter()
                .any(|h| h.eq_ignore_ascii_case(host))
            {
                continue;
            }
            host_policies.push(NetworkHostPolicyView {
                host: host.clone(),
                policy: match policy {
                    crate::permissions::network_policy::HostPolicy::Allow => "allow".to_string(),
                    crate::permissions::network_policy::HostPolicy::Deny => "deny".to_string(),
                },
                source: "custom".to_string(),
                removable: true,
            });
        }
        host_policies.sort_by(|a, b| a.host.cmp(&b.host));

        let filesystem = match manifest.filesystem {
            crate::capabilities::manifest::FilesystemPolicy::None => "none",
            crate::capabilities::manifest::FilesystemPolicy::Temp => "temp",
            crate::capabilities::manifest::FilesystemPolicy::Workspace => "workspace",
        };

        let tool_defs = if manifest.tool_source == ToolSource::Dynamic {
            load_discovered_tools(&state.db, &agent_id, cap_id)
                .await
                .unwrap_or_default()
        } else {
            manifest.tools.clone()
        };

        let tools: Vec<ToolView> = tool_defs
            .iter()
            .map(|t| {
                let key = (cap_id.clone(), t.name.clone());
                let global_default = global_policy_map
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| "not set".to_string());
                let user_override = override_map.get(&key);
                let has_override = user_override.is_some();
                // Effective policy for "Your Permissions": override if set, else global default
                let policy = user_override
                    .cloned()
                    .unwrap_or_else(|| global_default.clone());
                let desc = if t.description.len() > 120 {
                    let localized = localization::localized_tool_description(
                        &agent,
                        cap_id,
                        &t.name,
                        &t.description,
                        &user.language,
                    );
                    if localized.len() > 120 {
                        format!("{}...", &localized[..117])
                    } else {
                        localized
                    }
                } else {
                    localization::localized_tool_description(
                        &agent,
                        cap_id,
                        &t.name,
                        &t.description,
                        &user.language,
                    )
                };
                ToolView {
                    name: t.name.clone(),
                    display_name: localization::localized_tool_display_name(
                        &agent,
                        cap_id,
                        &t.name,
                        &t.name.replace('_', " "),
                        &user.language,
                    ),
                    description: desc,
                    policy,
                    capability_id: cap_id.clone(),
                    global_default,
                    has_override,
                }
            })
            .collect();

        // Build credential views from manifest declarations
        let mut credential_views: Vec<CredentialView> = Vec::new();
        for cred_decl in &manifest.credentials {
            let scope_str = match cred_decl.scope {
                crate::capabilities::manifest::CredentialScope::System => "system",
                crate::capabilities::manifest::CredentialScope::User => "user",
            };

            // Check if credential is already set
            let (is_set, set_at) = match cred_decl.scope {
                crate::capabilities::manifest::CredentialScope::System => {
                    let row = sqlx::query(
                        "SELECT created_at FROM system_credentials WHERE capability_id = ? AND credential_name = ?",
                    )
                    .bind(cap_id)
                    .bind(&cred_decl.name)
                    .fetch_optional(&state.db)
                    .await
                    .ok()
                    .flatten();
                    match row {
                        Some(r) => {
                            use sqlx::Row;
                            (true, r.try_get::<String, _>("created_at").ok())
                        }
                        None => (false, None),
                    }
                }
                crate::capabilities::manifest::CredentialScope::User => {
                    let row = sqlx::query(
                        "SELECT created_at FROM user_credentials WHERE user_id = ? AND capability_id = ? AND credential_name = ?",
                    )
                    .bind(&user.user_id)
                    .bind(cap_id)
                    .bind(&cred_decl.name)
                    .fetch_optional(&state.db)
                    .await
                    .ok()
                    .flatten();
                    match row {
                        Some(r) => {
                            use sqlx::Row;
                            (true, r.try_get::<String, _>("created_at").ok())
                        }
                        None => (false, None),
                    }
                }
            };

            credential_views.push(CredentialView {
                capability_id: cap_id.clone(),
                name: cred_decl.name.clone(),
                scope: scope_str.to_string(),
                description: localization::localized_credential_description(
                    &agent,
                    cap_id,
                    &cred_decl.name,
                    &cred_decl.description,
                    &user.language,
                ),
                required: cred_decl.required,
                is_set,
                set_at,
            });
        }

        capabilities.push(CapabilityView {
            id: cap_id.clone(),
            effective_network_mode: effective_network_mode.to_string(),
            network_access_policy: network_access_policy.to_string(),
            host_policies,
            filesystem: filesystem.to_string(),
            max_memory_mb: manifest.resources.max_memory_mb,
            max_cpu_fraction: format!("{:.0}%", manifest.resources.max_cpu_fraction * 100.0),
            pids_limit: manifest.resources.pids_limit,
            tools,
            credentials: credential_views,
        });
    }

    // Fetch egress log entries for this agent's capabilities
    let cap_ids: Vec<String> = agent.capability_manifests.keys().cloned().collect();
    let mut egress_entries: Vec<EgressEntryView> = Vec::new();
    let mut network_request_count = 0_i64;

    // Query egress log for all capabilities of this agent (last 100)
    for cap_id in &cap_ids {
        let count_row = sqlx::query("SELECT COUNT(*) AS c FROM egress_log WHERE capability_id = ?")
            .bind(cap_id)
            .fetch_one(&state.db)
            .await
            .ok();
        if let Some(row) = count_row {
            use sqlx::Row;
            network_request_count += row.try_get::<i64, _>("c").unwrap_or(0);
        }

        let rows = sqlx::query(
            "SELECT capability_id, method, host, port, allowed, created_at
             FROM egress_log
             WHERE capability_id = ?
             ORDER BY created_at DESC
             LIMIT 100",
        )
        .bind(cap_id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

        for row in rows {
            use sqlx::Row;
            egress_entries.push(EgressEntryView {
                capability_id: row.get("capability_id"),
                method: row.get("method"),
                host: row.get("host"),
                port: row.get("port"),
                allowed: row.get::<i32, _>("allowed") == 1,
                created_at: row.try_get::<String, _>("created_at").unwrap_or_default(),
            });
        }
    }

    // Sort by timestamp descending and limit to 100 total
    egress_entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    egress_entries.truncate(100);

    // Agent persistent storage entries.
    let storage_entries = if user.is_admin {
        let rows = sqlx::query(
            "SELECT id, user_id, key, value, updated_at
             FROM agent_storage
             WHERE agent_id = ?
             ORDER BY updated_at DESC, key ASC
             LIMIT 500",
        )
        .bind(&agent_id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

        rows.into_iter()
            .map(|row| {
                use sqlx::Row;
                StorageEntryView {
                    id: row.get("id"),
                    user_id: row.get("user_id"),
                    key: row.get("key"),
                    value: row.get("value"),
                    updated_at: row.try_get::<String, _>("updated_at").unwrap_or_default(),
                }
            })
            .collect()
    } else {
        let rows = sqlx::query(
            "SELECT id, user_id, key, value, updated_at
             FROM agent_storage
             WHERE agent_id = ? AND user_id = ?
             ORDER BY updated_at DESC, key ASC
             LIMIT 500",
        )
        .bind(&agent_id)
        .bind(&user.user_id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

        rows.into_iter()
            .map(|row| {
                use sqlx::Row;
                StorageEntryView {
                    id: row.get("id"),
                    user_id: row.get("user_id"),
                    key: row.get("key"),
                    value: row.get("value"),
                    updated_at: row.try_get::<String, _>("updated_at").unwrap_or_default(),
                }
            })
            .collect()
    };

    let storage_count_row = if user.is_admin {
        sqlx::query("SELECT COUNT(*) AS c FROM agent_storage WHERE agent_id = ?")
            .bind(&agent_id)
            .fetch_one(&state.db)
            .await
            .ok()
    } else {
        sqlx::query("SELECT COUNT(*) AS c FROM agent_storage WHERE agent_id = ? AND user_id = ?")
            .bind(&agent_id)
            .bind(&user.user_id)
            .fetch_one(&state.db)
            .await
            .ok()
    };
    let storage_count = storage_count_row
        .and_then(|row| {
            use sqlx::Row;
            row.try_get::<i64, _>("c").ok()
        })
        .unwrap_or(0);

    let memory_entries = if user.is_admin {
        let rows = sqlx::query(
            "SELECT id, user_id, memory_text, tags, source, updated_at
             FROM agent_memories
             WHERE agent_id = ?
             ORDER BY updated_at DESC
             LIMIT 500",
        )
        .bind(&agent_id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

        rows.into_iter()
            .map(|row| {
                use sqlx::Row;
                MemoryEntryView {
                    id: row.get("id"),
                    user_id: row.get("user_id"),
                    memory: row.get("memory_text"),
                    tags: row.get("tags"),
                    source: row.get("source"),
                    updated_at: row.try_get::<String, _>("updated_at").unwrap_or_default(),
                }
            })
            .collect()
    } else {
        let rows = sqlx::query(
            "SELECT id, user_id, memory_text, tags, source, updated_at
             FROM agent_memories
             WHERE agent_id = ? AND user_id = ?
             ORDER BY updated_at DESC
             LIMIT 500",
        )
        .bind(&agent_id)
        .bind(&user.user_id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

        rows.into_iter()
            .map(|row| {
                use sqlx::Row;
                MemoryEntryView {
                    id: row.get("id"),
                    user_id: row.get("user_id"),
                    memory: row.get("memory_text"),
                    tags: row.get("tags"),
                    source: row.get("source"),
                    updated_at: row.try_get::<String, _>("updated_at").unwrap_or_default(),
                }
            })
            .collect()
    };

    let memory_count_row = if user.is_admin {
        sqlx::query("SELECT COUNT(*) AS c FROM agent_memories WHERE agent_id = ?")
            .bind(&agent_id)
            .fetch_one(&state.db)
            .await
            .ok()
    } else {
        sqlx::query("SELECT COUNT(*) AS c FROM agent_memories WHERE agent_id = ? AND user_id = ?")
            .bind(&agent_id)
            .bind(&user.user_id)
            .fetch_one(&state.db)
            .await
            .ok()
    };
    let memory_count = memory_count_row
        .and_then(|row| {
            use sqlx::Row;
            row.try_get::<i64, _>("c").ok()
        })
        .unwrap_or(0);

    // Build built-in tool policy views
    use crate::permissions::tool_policy::{
        BUILTIN_CAPABILITY_ID, BUILTIN_DELEGATE, BUILTIN_MEMORY_FORGET, BUILTIN_MEMORY_LIST,
        BUILTIN_MEMORY_REMEMBER, BUILTIN_MEMORY_SEARCH, BUILTIN_SET_REMINDER, BUILTIN_SET_SCHEDULE,
        BUILTIN_STORE_DELETE, BUILTIN_STORE_GET, BUILTIN_STORE_LIST, BUILTIN_STORE_SET,
    };
    let mut builtin_tools: Vec<(&str, &str, &str)> = Vec::new();
    if show_delegate_permission(&agent_id) {
        builtin_tools.push((
            BUILTIN_DELEGATE,
            "Delegate to Agent",
            "Allows the agent to hand off tasks to other specialist agents.",
        ));
    }
    builtin_tools.extend([
        (
            BUILTIN_STORE_GET,
            "Store Get",
            "Allows the agent to retrieve persistently stored values.",
        ),
        (
            BUILTIN_STORE_SET,
            "Store Set",
            "Allows the agent to persistently store key-value pairs.",
        ),
        (
            BUILTIN_STORE_DELETE,
            "Store Delete",
            "Allows the agent to delete persistently stored values.",
        ),
        (
            BUILTIN_STORE_LIST,
            "Store List",
            "Allows the agent to list all persistently stored key-value pairs.",
        ),
        (
            BUILTIN_MEMORY_REMEMBER,
            "Memory Remember",
            "Allows the agent to save long-term memory notes for future conversations.",
        ),
        (
            BUILTIN_MEMORY_FORGET,
            "Memory Forget",
            "Allows the agent to remove stored memory notes.",
        ),
        (
            BUILTIN_MEMORY_SEARCH,
            "Memory Search",
            "Allows the agent to search saved memory using BM25 relevance.",
        ),
        (
            BUILTIN_MEMORY_LIST,
            "Memory List",
            "Allows the agent to list saved memory notes.",
        ),
        (
            BUILTIN_SET_SCHEDULE,
            "Set Schedule",
            "Allows the agent to create recurring schedules from natural-language user requests.",
        ),
        (
            BUILTIN_SET_REMINDER,
            "Set Reminder",
            "Allows the agent to create one-time reminders for future actions.",
        ),
    ]);
    let builtin_policies: Vec<BuiltinPolicyView> = builtin_tools
        .iter()
        .map(|(tool_name, display, _desc)| {
            let key = (BUILTIN_CAPABILITY_ID.to_string(), tool_name.to_string());
            let global_default = global_policy_map
                .get(&key)
                .cloned()
                .unwrap_or_else(|| "not set".to_string());
            let user_override = override_map.get(&key);
            let has_override = user_override.is_some();
            let policy = user_override
                .cloned()
                .unwrap_or_else(|| global_default.clone());
            BuiltinPolicyView {
                capability_id: BUILTIN_CAPABILITY_ID.to_string(),
                tool_name: tool_name.to_string(),
                display_name: display.to_string(),
                policy,
                has_override,
            }
        })
        .collect();

    let mut permissions_allow_count = 0_usize;
    let mut permissions_ask_count = 0_usize;
    let mut permissions_block_count = 0_usize;

    for cap in &capabilities {
        for tool in &cap.tools {
            match tool.policy.as_str() {
                "allow" => permissions_allow_count += 1,
                "ask" => permissions_ask_count += 1,
                _ => permissions_block_count += 1,
            }
        }
    }
    for tool in &builtin_policies {
        match tool.policy.as_str() {
            "allow" => permissions_allow_count += 1,
            "ask" => permissions_ask_count += 1,
            _ => permissions_block_count += 1,
        }
    }

    let mut secrets_set_count = 0_usize;
    let mut secrets_missing_count = 0_usize;
    for cap in &capabilities {
        for cred in &cap.credentials {
            if cred.is_set {
                secrets_set_count += 1;
            } else {
                secrets_missing_count += 1;
            }
        }
    }

    // Agent self-improvement insights
    let improvement_views: Vec<InsightView> =
        crate::agents::improvement::list_insights(&state.db, &agent_id, &user.user_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|i| InsightView {
                id: i.id,
                lesson_text: i.lesson_text,
                insight_type: i.insight_type,
                status: i.status,
                confidence_pct: (i.confidence * 100.0).round() as u8,
                supporting_signals: i.supporting_signals,
                promotion_threshold: i.promotion_threshold,
                created_at: i.created_at,
            })
            .collect();
    let improvement_signal_count =
        crate::agents::improvement::count_signals(&state.db, &agent_id, &user.user_id)
            .await
            .unwrap_or(0);

    match (AgentDetailTemplate {
        active_nav: "agents",
        active_tab: active_tab.to_string(),
        agent_id,
        agent_name,
        is_admin: user.is_admin,
        base_path,
        overview: AgentOverviewStats {
            capability_count: capabilities.len(),
            storage_count,
            memory_count,
            network_request_count,
            permissions_allow_count,
            permissions_ask_count,
            permissions_block_count,
            secrets_set_count,
            secrets_missing_count,
        },
        runtime_settings,
        automation,
        capabilities,
        storage_entries,
        memory_entries,
        builtin_policies,
        egress_entries,
        improvement_insights: improvement_views,
        improvement_signal_count,
        error: q.error,
        success: q.success,
    })
    .render()
    {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── Agent credential management (inline on detail page) ──────────────────────

/// HTMX: set a credential for a capability from the agent detail page.
/// Returns an HTML fragment that replaces the credential row.
pub async fn agent_credential_set(
    user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<SetAgentCredentialForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    let return_to = safe_return_path(
        form.return_to.as_deref(),
        &base_path,
        &format!("/agents/{agent_id}/secrets"),
    );

    if form.value.is_empty() {
        return Redirect::to(&format!("{}?error=secret_empty", return_to)).into_response();
    }

    let result = match form.scope.as_str() {
        "system" => {
            state
                .credentials
                .set_system(&form.capability_id, &form.credential_name, &form.value)
                .await
        }
        "user" => {
            state
                .credentials
                .set_user(
                    &user.user_id,
                    &form.capability_id,
                    &form.credential_name,
                    &form.value,
                )
                .await
        }
        _ => {
            return Redirect::to(&format!("{}?error=invalid_scope", return_to)).into_response();
        }
    };

    match result {
        Ok(_) => Redirect::to(&format!("{}?success=secret_saved", return_to)).into_response(),
        Err(e) => {
            error!("Failed to set agent credential: {e}");
            Redirect::to(&format!("{}?error=secret_save_failed", return_to)).into_response()
        }
    }
}

/// HTMX: delete a credential for a capability from the agent detail page.
pub async fn agent_credential_delete(
    user: AuthUser,
    Path((agent_id, scope, cap_id, name)): Path<(String, String, String, String)>,
    Query(q): Query<CredentialDeleteQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    let return_to = safe_return_path(
        q.return_to.as_deref(),
        &base_path,
        &format!("/agents/{agent_id}/secrets"),
    );

    let result = match scope.as_str() {
        "system" => state.credentials.delete_system(&cap_id, &name).await,
        "user" => {
            state
                .credentials
                .delete_user(&user.user_id, &cap_id, &name)
                .await
        }
        _ => {
            return Redirect::to(&format!("{}?error=invalid_scope", return_to)).into_response();
        }
    };

    if let Err(e) = result {
        error!("Failed to delete agent credential: {e}");
    }
    Redirect::to(&format!("{}?success=secret_removed", return_to)).into_response()
}

/// Delete one persistent storage entry for the current agent.
pub async fn agent_storage_delete(
    user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<DeleteStorageForm>,
) -> Response {
    if user.is_admin {
        if let Err(e) = sqlx::query("DELETE FROM agent_storage WHERE id = ? AND agent_id = ?")
            .bind(&form.entry_id)
            .bind(&agent_id)
            .execute(&state.db)
            .await
        {
            error!("Failed to delete agent storage entry (admin): {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    } else if let Err(e) =
        sqlx::query("DELETE FROM agent_storage WHERE id = ? AND agent_id = ? AND user_id = ?")
            .bind(&form.entry_id)
            .bind(&agent_id)
            .bind(&user.user_id)
            .execute(&state.db)
            .await
    {
        error!("Failed to delete agent storage entry (user): {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    StatusCode::OK.into_response()
}

/// Delete one persistent memory entry for the current agent.
pub async fn agent_memory_delete(
    user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<DeleteMemoryForm>,
) -> Response {
    if user.is_admin {
        if let Err(e) = sqlx::query("DELETE FROM agent_memories WHERE id = ? AND agent_id = ?")
            .bind(&form.memory_id)
            .bind(&agent_id)
            .execute(&state.db)
            .await
        {
            error!("Failed to delete agent memory entry (admin): {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    } else if let Err(e) =
        sqlx::query("DELETE FROM agent_memories WHERE id = ? AND agent_id = ? AND user_id = ?")
            .bind(&form.memory_id)
            .bind(&agent_id)
            .bind(&user.user_id)
            .execute(&state.db)
            .await
    {
        error!("Failed to delete agent memory entry (user): {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    StatusCode::OK.into_response()
}

/// Set model for a specific agent
pub async fn set_agent_model_handler(
    user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<SetAgentModelForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    let temp: f32 = form.temperature.parse().unwrap_or(0.7);

    if let Err(e) = model::set_agent_model(
        &state.db,
        &agent_id,
        &form.provider_id,
        &form.model_id,
        temp,
    )
    .await
    {
        error!("Failed to set agent model: {e}");
        return Redirect::to(&format!(
            "{}/agents?error=Failed+to+update+model+assignment.+Please+try+again.",
            base_path
        ))
        .into_response();
    }

    Redirect::to(&format!(
        "{}/agents?success=Model+updated+successfully.",
        base_path
    ))
    .into_response()
}

/// Set image model for a specific agent (optional override).
pub async fn set_agent_image_model_handler(
    user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<SetAgentImageModelForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    let result = if form.provider_id.trim().is_empty() || form.model_id.trim().is_empty() {
        model::clear_agent_image_model(&state.db, &agent_id).await
    } else {
        model::set_agent_image_model(&state.db, &agent_id, &form.provider_id, &form.model_id).await
    };

    if let Err(e) = result {
        error!("Failed to set agent image model: {e}");
        return Redirect::to(&format!(
            "{}/agents?error=Failed+to+update+image+model+assignment.+Please+try+again.",
            base_path
        ))
        .into_response();
    }

    Redirect::to(&format!("{}/agents?success=image_model_updated", base_path)).into_response()
}

/// HTMX endpoint: set per-user network access override for a capability.
pub async fn set_network_access_handler(
    user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<SetNetworkAccessForm>,
) -> Response {
    let access =
        match crate::permissions::network_policy::NetworkAccessPolicy::from_str(&form.access) {
            Ok(v) => v,
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        };

    if let Err(e) = crate::permissions::network_policy::set_user_access_override(
        &state.db,
        &user.user_id,
        &agent_id,
        &form.capability_id,
        access,
    )
    .await
    {
        error!("Failed to set network access override: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    state
        .capabilities
        .invalidate_network_policy_cache(&user.user_id, &agent_id, &form.capability_id)
        .await;

    StatusCode::OK.into_response()
}

/// HTMX endpoint: set per-user host policy override for a capability.
pub async fn set_network_host_policy_handler(
    user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<SetNetworkHostPolicyForm>,
) -> Response {
    let policy = match crate::permissions::network_policy::HostPolicy::from_str(&form.policy) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let host = match crate::permissions::network_policy::normalize_host_entry(&form.host) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    if let Err(e) = crate::permissions::network_policy::set_user_host_override(
        &state.db,
        &user.user_id,
        &agent_id,
        &form.capability_id,
        &host,
        policy,
    )
    .await
    {
        error!("Failed to set network host override: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    state
        .capabilities
        .invalidate_network_policy_cache(&user.user_id, &agent_id, &form.capability_id)
        .await;

    let hx_redirect = format!("{}/agents/{agent_id}/network", state.base_path);
    (StatusCode::OK, [("HX-Redirect", hx_redirect)], "").into_response()
}

/// HTMX endpoint: delete a custom per-user host override.
pub async fn delete_network_host_handler(
    user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<DeleteNetworkHostForm>,
) -> Response {
    let host = match crate::permissions::network_policy::normalize_host_entry(&form.host) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    if let Err(e) = crate::permissions::network_policy::delete_user_host_override(
        &state.db,
        &user.user_id,
        &agent_id,
        &form.capability_id,
        &host,
    )
    .await
    {
        error!("Failed to delete network host override: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    state
        .capabilities
        .invalidate_network_policy_cache(&user.user_id, &agent_id, &form.capability_id)
        .await;

    let hx_redirect = format!("{}/agents/{agent_id}/network", state.base_path);
    (StatusCode::OK, [("HX-Redirect", hx_redirect)], "").into_response()
}

/// HTMX endpoint: update a single tool policy for an agent.
///
/// The `scope` form field determines where the policy is saved:
/// - `"global"` — updates the global default (admin only)
/// - `"user"` (default) — creates a personal override for the current user
///
/// Returns an updated badge fragment so the UI reflects the new state
/// without a full page reload.
pub async fn set_tool_policy_handler(
    user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<SetToolPolicyForm>,
) -> Response {
    let policy = match crate::permissions::tool_policy::ToolPolicy::from_str(&form.policy) {
        Ok(p) => p,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let result = if form.scope == "global" && user.is_admin {
        // Admin setting the global default
        crate::permissions::tool_policy::set_global_policies(
            &state.db,
            &agent_id,
            &[(form.capability_id, form.tool_name, policy)],
        )
        .await
    } else {
        // Per-user override (any user, or admin editing their own)
        crate::permissions::tool_policy::set_policies(
            &state.db,
            &user.user_id,
            &agent_id,
            &[(form.capability_id, form.tool_name, policy)],
        )
        .await
    };

    if let Err(e) = result {
        error!("Failed to set tool policy: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Return empty 200 — the radio buttons already reflect the new state client-side
    StatusCode::OK.into_response()
}

/// HTMX endpoint: reset a user's personal tool policy override back to the global default.
///
/// Deletes the per-user override from `tool_policies`, so the user falls back
/// to the global default.
pub async fn reset_tool_policy_handler(
    user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<ResetToolPolicyForm>,
) -> Response {
    if let Err(e) = crate::permissions::tool_policy::delete_user_policy(
        &state.db,
        &user.user_id,
        &agent_id,
        &form.capability_id,
        &form.tool_name,
    )
    .await
    {
        error!("Failed to reset tool policy: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let return_to = safe_return_path(
        form.return_to.as_deref(),
        &base_path,
        &format!("/agents/{agent_id}/permissions"),
    );

    // Tell HTMX to reload the page so the UI reflects the reset state
    (StatusCode::OK, [("HX-Redirect", return_to)], "").into_response()
}

/// Update an already-installed agent to a newer marketplace version
pub async fn start_agent_update(
    user: AuthUser,
    State(state): State<AppState>,
    Form(form): Form<InstallForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    let entry: MarketplaceEntry = match serde_json::from_str(&form.entry_json) {
        Ok(e) => e,
        Err(e) => {
            error!("Invalid entry JSON: {e}");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error_key": "agents.update.error.invalid_payload"
                })),
            )
                .into_response();
        }
    };

    {
        let jobs = state.agent_update_jobs.lock().await;
        if let Some((existing_job_id, _)) = jobs
            .iter()
            .find(|(_, j)| j.owner_user_id == user.user_id && j.agent_id == entry.id && !j.done)
        {
            return Json(StartAgentUpdateResponse {
                job_id: existing_job_id.clone(),
            })
            .into_response();
        }
    }

    let job_id = Uuid::new_v4().to_string();
    {
        let mut jobs = state.agent_update_jobs.lock().await;
        jobs.insert(
            job_id.clone(),
            crate::state::AgentUpdateJob {
                owner_user_id: user.user_id.clone(),
                agent_id: entry.id.clone(),
                agent_name: entry.localized_name(&user.language),
                target_version: entry.version.clone(),
                progress: 5,
                message_key: "agents.update.phase.preparing".to_string(),
                done: false,
                success: false,
                redirect_to: None,
                error_key: None,
                updated_at: std::time::Instant::now(),
            },
        );
    }

    let state_clone = state.clone();
    let job_id_clone = job_id.clone();
    tokio::spawn(async move {
        run_agent_update_job(state_clone, entry, job_id_clone).await;
    });

    Json(StartAgentUpdateResponse { job_id }).into_response()
}

async fn set_update_job_progress(state: &AppState, job_id: &str, progress: u8, message_key: &str) {
    let mut jobs = state.agent_update_jobs.lock().await;
    if let Some(job) = jobs.get_mut(job_id) {
        if job.done {
            return;
        }
        job.progress = progress;
        job.message_key = message_key.to_string();
        job.updated_at = std::time::Instant::now();
    }
}

async fn run_agent_update_job(state: AppState, entry: MarketplaceEntry, job_id: String) {
    set_update_job_progress(&state, &job_id, 10, "agents.update.phase.preparing").await;
    let docker = match bollard::Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to connect to Docker: {e}");
            let mut jobs = state.agent_update_jobs.lock().await;
            if let Some(job) = jobs.get_mut(&job_id) {
                job.progress = 100;
                job.message_key = "agents.update.error.docker".to_string();
                job.done = true;
                job.success = false;
                job.error_key = Some("agents.update.error.docker".to_string());
                job.updated_at = std::time::Instant::now();
            }
            return;
        }
    };

    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::unbounded_channel::<marketplace::PullProgress>();
    let progress_state = state.clone();
    let progress_job_id = job_id.clone();
    let progress_task = tokio::spawn(async move {
        while let Some(progress) = progress_rx.recv().await {
            let pct = (20.0 + (progress.overall_fraction.clamp(0.0, 1.0) * 70.0)).round() as u8;
            let mut jobs = progress_state.agent_update_jobs.lock().await;
            let Some(job) = jobs.get_mut(&progress_job_id) else {
                break;
            };
            if job.done {
                break;
            }
            if pct > job.progress {
                job.progress = pct.min(90);
            }
            job.message_key = "agents.update.phase.downloading".to_string();
            job.updated_at = std::time::Instant::now();
            let _ = &progress.image;
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
        Some(progress_tx),
    )
    .await;

    progress_task.abort();

    let mut jobs = state.agent_update_jobs.lock().await;
    let Some(job) = jobs.get_mut(&job_id) else {
        return;
    };

    match result {
        Ok(agent_def) => {
            let setup_required = marketplace::agent_requires_setup(
                &state.db,
                &state.credentials,
                &entry.id,
                &agent_def,
            )
            .await
            .unwrap_or(true);
            job.progress = 100;
            job.message_key = if setup_required {
                "agents.update.success.setup_required".to_string()
            } else {
                "agents.update.success.updated".to_string()
            };
            job.done = true;
            job.success = true;
            job.redirect_to = if setup_required {
                Some(format!(
                    "{}/agents/{}/setup?flow=update",
                    state.base_path, entry.id
                ))
            } else {
                None
            };
            job.updated_at = std::time::Instant::now();
        }
        Err(e) => {
            error!("Agent update failed: {e}");
            job.progress = 100;
            job.message_key = "agents.update.error.failed".to_string();
            job.done = true;
            job.success = false;
            job.error_key = Some("agents.update.error.failed".to_string());
            job.updated_at = std::time::Instant::now();
        }
    }
}

pub async fn agent_update_status(
    user: AuthUser,
    Path(job_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    let jobs = state.agent_update_jobs.lock().await;
    let Some(job) = jobs.get(&job_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if job.owner_user_id != user.user_id {
        return StatusCode::FORBIDDEN.into_response();
    }

    Json(AgentUpdateStatusResponse {
        job_id,
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
    .into_response()
}

/// Update an already-installed agent to a newer marketplace version
pub async fn update_agent(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<InstallForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    let entry: MarketplaceEntry = match serde_json::from_str(&form.entry_json) {
        Ok(e) => e,
        Err(e) => {
            error!("Invalid entry JSON: {e}");
            return Redirect::to(&format!(
                "{}/agents?error=Invalid+agent+data.+Please+try+again.",
                base_path
            ))
            .into_response();
        }
    };

    let docker = match bollard::Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to connect to Docker: {e}");
            return Redirect::to(&format!(
                "{}/agents?error=Cannot+connect+to+Docker.+Is+Docker+running%3F",
                base_path
            ))
            .into_response();
        }
    };

    match marketplace::update_agent(
        &entry,
        &state.config.installed_agents_dir,
        &state.db,
        &state.agents,
        &docker,
        &state.capabilities,
        &state.credentials,
    )
    .await
    {
        Ok(agent_def) => {
            let setup_required = marketplace::agent_requires_setup(
                &state.db,
                &state.credentials,
                &entry.id,
                &agent_def,
            )
            .await
            .unwrap_or(true);
            if setup_required {
                Redirect::to(&format!(
                    "{}/agents/{}/setup?flow=update",
                    base_path, entry.id
                ))
                .into_response()
            } else {
                let msg = urlencoding::encode("updated");
                Redirect::to(&format!("{}/agents?success={msg}", base_path)).into_response()
            }
        }
        Err(e) => {
            error!("Agent update failed: {e}");
            let text = format!("Agent update failed: {e}");
            let msg = urlencoding::encode(&text);
            Redirect::to(&format!("{}/agents?error={msg}", base_path)).into_response()
        }
    }
}

/// Toggle auto-update for an agent
pub async fn toggle_auto_update(
    user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<ToggleAutoUpdateForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    let enabled =
        form.auto_update.as_deref() == Some("on") || form.auto_update.as_deref() == Some("1");
    let value: i32 = if enabled { 1 } else { 0 };

    if let Err(e) = sqlx::query("UPDATE agents SET auto_update = ? WHERE id = ? AND is_bundled = 0")
        .bind(value)
        .bind(&agent_id)
        .execute(&state.db)
        .await
    {
        error!("Failed to toggle auto-update: {e}");
        return Redirect::to(&format!(
            "{}/agents?error=Failed+to+update+setting.+Please+try+again.",
            base_path
        ))
        .into_response();
    }

    Redirect::to(&format!(
        "{}/agents?success=Auto-update+setting+saved.",
        base_path
    ))
    .into_response()
}

/// Submit a 1-5 star rating for an installed agent.
pub async fn rate_agent(
    _user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<RateAgentForm>,
) -> Response {
    let is_installed = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM agents WHERE id = ? AND setup_complete = 1 LIMIT 1",
    )
    .bind(&agent_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .is_some();

    if !is_installed {
        let msg = urlencoding::encode("not_installed");
        return Redirect::to(&format!("{}/agents?error={msg}", base_path)).into_response();
    }

    if !(1..=5).contains(&form.rating) {
        let msg = urlencoding::encode("rating_invalid");
        return Redirect::to(&format!("{}/agents?error={msg}", base_path)).into_response();
    }

    let instance_id = match crate::persistence::db::get_instance_id(&state.db).await {
        Ok(id) => id,
        Err(e) => {
            error!("Failed to load instance ID: {e}");
            let msg = urlencoding::encode("rating_submit_failed");
            return Redirect::to(&format!("{}/agents?error={msg}", base_path)).into_response();
        }
    };

    let ratings_base = match ratings_api_base_url(&state.config.marketplace_url) {
        Some(base) => base,
        None => {
            error!("Invalid marketplace URL: {}", state.config.marketplace_url);
            let msg = urlencoding::encode("rating_unavailable");
            return Redirect::to(&format!("{}/agents?error={msg}", base_path)).into_response();
        }
    };

    let url = format!("{ratings_base}/ratings/agents/{agent_id}");
    let payload = SubmitRatingRequest {
        rating: form.rating,
    };

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to build HTTP client for rating submit: {e}");
            let msg = urlencoding::encode("rating_unavailable");
            return Redirect::to(&format!("{}/agents?error={msg}", base_path)).into_response();
        }
    };

    let response = client
        .post(&url)
        .header("x-instance-id", instance_id)
        .json(&payload)
        .send()
        .await;

    match response {
        Ok(resp) if resp.status().is_success() => {
            let msg = urlencoding::encode("rating_saved");
            Redirect::to(&format!("{}/agents?success={msg}", base_path)).into_response()
        }
        Ok(resp) => {
            error!("Rating submission failed with status {}", resp.status());
            let msg = urlencoding::encode("rating_save_failed");
            Redirect::to(&format!("{}/agents?error={msg}", base_path)).into_response()
        }
        Err(e) => {
            error!("Rating submission request failed: {e}");
            let msg = urlencoding::encode("rating_save_failed");
            Redirect::to(&format!("{}/agents?error={msg}", base_path)).into_response()
        }
    }
}

/// Uninstall an agent
pub async fn uninstall_agent(
    user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Err(e) = marketplace::uninstall_agent(
        &agent_id,
        &state.config.installed_agents_dir,
        &state.db,
        &state.agents,
    )
    .await
    {
        error!("Failed to uninstall agent: {e}");
        let text = format!("Failed to uninstall agent: {e}");
        let msg = urlencoding::encode(&text);
        return Redirect::to(&format!("{}/agents?error={msg}", base_path)).into_response();
    }

    Redirect::to(&format!(
        "{}/agents?success=Agent+uninstalled+successfully.",
        base_path
    ))
    .into_response()
}

/// Set the global default model
pub async fn set_default_model(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<SetDefaultModelForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    let temp: f32 = form.temperature.parse().unwrap_or(0.7);

    if let Err(e) =
        model::set_global_default(&state.db, &form.provider_id, &form.model_id, temp).await
    {
        error!("Failed to set default model: {e}");
        return Redirect::to(&format!(
            "{}/agents?error=Failed+to+update+global+default+model.+Please+try+again.",
            base_path
        ))
        .into_response();
    }

    Redirect::to(&format!(
        "{}/agents?success=global_default_model_updated",
        base_path
    ))
    .into_response()
}

/// Set the global default image model (optional).
pub async fn set_default_image_model(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<SetDefaultImageModelForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }

    let provider = form.provider_id.trim();
    let model_id = form.model_id.trim();

    let result = if provider.is_empty() || model_id.is_empty() {
        model::set_global_image_default(&state.db, "", "").await
    } else {
        model::set_global_image_default(&state.db, provider, model_id).await
    };

    if let Err(e) = result {
        error!("Failed to set default image model: {e}");
        return Redirect::to(&format!(
            "{}/agents?error=Failed+to+update+global+default+image+model.+Please+try+again.",
            base_path
        ))
        .into_response();
    }

    Redirect::to(&format!(
        "{}/agents?success=global_default_image_model_updated",
        base_path
    ))
    .into_response()
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn is_provider_configured_for_agent_selector(
    provider_id: &str,
    api_key_encrypted: Option<&str>,
    base_url: Option<&str>,
) -> bool {
    let has_key = api_key_encrypted.is_some_and(|k| !k.is_empty());
    let has_base_url = base_url.is_some_and(|u| !u.is_empty());

    // Most providers need a key. Bedrock and Pico can be usable without one.
    has_key || ((provider_id == "bedrock" || provider_id == "pico") && has_base_url)
}

/// Load only providers that are actually usable in the agents UI.
/// Most providers require an API key.
/// Bedrock is considered configured if it has a region set in `base_url`.
/// Pico is considered configured if it has a base URL (API key is not needed).
async fn load_provider_options(state: &AppState) -> Vec<ProviderOption> {
    sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
        "SELECT id, display_name, api_key_encrypted, base_url FROM llm_providers WHERE active = 1 ORDER BY id",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .filter(|(id, _, key, base_url)| {
        is_provider_configured_for_agent_selector(id, key.as_deref(), base_url.as_deref())
    })
    .map(|(id, display_name, _, _)| ProviderOption { id, display_name })
    .collect()
}

/// HTMX endpoint: returns `<option>` elements for the model dropdown of a given provider.
///
/// Fetches models dynamically from the provider API (if an API key is configured),
/// falling back to a static catalogue of well-known models.
pub async fn models_for_provider(
    _user: AuthUser,
    Path(provider_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    let models = crate::llm::models::list_models(&state.db, &state.credentials, &provider_id).await;

    if models.is_empty() {
        return Html(
            r#"<option value="">No models available for this provider</option>"#.to_string(),
        )
        .into_response();
    }

    let mut html = String::from(r#"<option value="">-- Select a model --</option>"#);
    for m in models {
        html.push_str(&format!(r#"<option value="{}">{}</option>"#, m.id, m.name,));
    }

    Html(html).into_response()
}

/// Look up a human-friendly display name for a model ID using the static catalogue.
fn resolve_model_display_name(provider_id: &str, model_id: &str) -> String {
    if model_id.is_empty() {
        return String::new();
    }
    // Check the static fallback list for a matching display name
    let statics = crate::llm::models::static_fallback(provider_id);
    for m in statics {
        if m.id == model_id {
            return m.name.clone();
        }
    }
    // No match — return the raw ID
    model_id.to_string()
}

fn ratings_api_base_url(marketplace_url: &str) -> Option<String> {
    let trimmed = marketplace_url.trim_end_matches('/');
    let marker = "/marketplace/agents";
    let idx = trimmed.find(marker)?;
    Some(trimmed[..idx].to_string())
}

fn safe_return_path(return_to: Option<&str>, base_path: &str, fallback_suffix: &str) -> String {
    let fallback = format!("{base_path}{fallback_suffix}");
    let Some(path) = return_to else {
        return fallback;
    };
    if path.starts_with(base_path) {
        path.to_string()
    } else {
        fallback
    }
}

fn automation_schedule_name(agent_id: &str, preset_id: &str) -> String {
    format!("auto.{}.{}", agent_id, preset_id)
}

// ── Self-improvement HTMX handlers ───────────────────────────────────────────

#[derive(Deserialize)]
pub struct InsightActionForm {
    pub insight_id: String,
}

pub async fn improvement_pause_handler(
    _user: AuthUser,
    Path(_agent_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<InsightActionForm>,
) -> Response {
    match crate::agents::improvement::update_insight_status(&state.db, &form.insight_id, "paused")
        .await
    {
        Ok(true) => Html(
            r#"<span class="text-xs text-amber-400" data-i18n="improvement.status.paused">Paused</span>"#,
        )
        .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn improvement_reject_handler(
    _user: AuthUser,
    Path(_agent_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<InsightActionForm>,
) -> Response {
    match crate::agents::improvement::update_insight_status(
        &state.db,
        &form.insight_id,
        "rejected",
    )
    .await
    {
        Ok(true) => Html(
            r#"<span class="text-xs text-red-400" data-i18n="improvement.status.rejected">Removed</span>"#,
        )
        .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn improvement_activate_handler(
    _user: AuthUser,
    Path(_agent_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<InsightActionForm>,
) -> Response {
    match crate::agents::improvement::update_insight_status(&state.db, &form.insight_id, "active")
        .await
    {
        Ok(true) => Html(
            r#"<span class="text-xs text-emerald-400" data-i18n="improvement.status.activated">Activated</span>"#,
        )
        .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn improvement_reset_handler(
    user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    match crate::agents::improvement::reset_all(&state.db, &agent_id, &user.user_id).await {
        Ok(()) => {
            Redirect::to(&format!("/agents/{}/improvement?success=reset", agent_id)).into_response()
        }
        Err(e) => {
            error!("Failed to reset improvement data: {e}");
            Redirect::to(&format!(
                "/agents/{}/improvement?error=reset_failed",
                agent_id
            ))
            .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_provider_configured_for_agent_selector;

    #[test]
    fn openai_requires_api_key() {
        assert!(!is_provider_configured_for_agent_selector(
            "openai",
            None,
            Some("https://api.openai.com")
        ));
        assert!(is_provider_configured_for_agent_selector(
            "openai",
            Some("encrypted-key"),
            None
        ));
    }

    #[test]
    fn grok_requires_api_key() {
        assert!(!is_provider_configured_for_agent_selector(
            "grok",
            None,
            Some("https://api.x.ai")
        ));
        assert!(is_provider_configured_for_agent_selector(
            "grok",
            Some("encrypted-key"),
            None
        ));
    }

    #[test]
    fn bedrock_works_with_region_even_without_key() {
        assert!(is_provider_configured_for_agent_selector(
            "bedrock",
            None,
            Some("eu-central-1")
        ));
        assert!(!is_provider_configured_for_agent_selector(
            "bedrock", None, None
        ));
    }

    #[test]
    fn pico_works_with_base_url_even_without_key() {
        assert!(is_provider_configured_for_agent_selector(
            "pico",
            None,
            Some("http://192.168.1.100:8090")
        ));
        assert!(!is_provider_configured_for_agent_selector(
            "pico", None, None
        ));
    }
}
