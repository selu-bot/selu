use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;
use tracing::error;

use crate::agents::{
    loader::StepType,
    marketplace::{self, MarketplaceEntry},
    model,
};
use crate::state::AppState;
use crate::web::auth::AuthUser;

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct InstalledAgentView {
    pub id: String,
    pub name: String,
    pub provider_id: String,
    pub model_id: String,
    pub model_display_name: String,
    pub capability_count: usize,
    pub is_bundled: bool,
    pub setup_complete: bool,
}

#[derive(Debug, Clone)]
pub struct MarketplaceAgentView {
    pub name: String,
    pub description: String,
    pub version: String,
    pub author: String,
    pub is_installed: bool,
    /// JSON-encoded MarketplaceEntry for the install form
    pub entry_json: String,
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

// ── Templates ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "agents.html")]
struct AgentsTemplate {
    active_nav: &'static str,
    agents: Vec<InstalledAgentView>,
    marketplace_agents: Vec<MarketplaceAgentView>,
    providers: Vec<ProviderOption>,
    global_provider: String,
    global_model: String,
    global_model_display_name: String,
    global_temperature: String,
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
    agent_id: String,
    agent_name: String,
    steps: Vec<SetupStepView>,
    tool_policies: Vec<ToolPolicyView>,
    providers: Vec<ProviderOption>,
}

// ── Agent detail page ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CapabilityView {
    pub id: String,
    pub network_mode: String,
    pub allowed_hosts: Vec<String>,
    pub filesystem: String,
    pub max_memory_mb: u32,
    pub max_cpu_fraction: String,
    pub pids_limit: u32,
    pub tools: Vec<ToolView>,
}

#[derive(Debug, Clone)]
pub struct ToolView {
    pub name: String,
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

/// A built-in tool (delegate_to_agent, emit_event) with its current policy.
#[derive(Debug, Clone)]
pub struct BuiltinPolicyView {
    pub capability_id: String,
    pub tool_name: String,
    pub display_name: String,
    pub description: String,
    /// The effective policy for the current viewer.
    pub policy: String,
    /// The global default policy.
    pub global_default: String,
    /// Whether the current user has a personal override.
    pub has_override: bool,
}

#[derive(Template)]
#[template(path = "agents_detail.html")]
struct AgentDetailTemplate {
    active_nav: &'static str,
    agent_id: String,
    agent_name: String,
    is_admin: bool,
    capabilities: Vec<CapabilityView>,
    builtin_policies: Vec<BuiltinPolicyView>,
    egress_entries: Vec<EgressEntryView>,
}

// ── Form structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SetAgentModelForm {
    pub provider_id: String,
    pub model_id: String,
    #[serde(default = "default_temp_str")]
    pub temperature: String,
}
fn default_temp_str() -> String { "0.7".into() }

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
fn default_scope_user() -> String { "user".into() }

#[derive(Debug, Deserialize)]
pub struct ResetToolPolicyForm {
    pub capability_id: String,
    pub tool_name: String,
}

#[derive(Debug, Deserialize)]
pub struct SetDefaultModelForm {
    pub provider_id: String,
    pub model_id: String,
    #[serde(default = "default_temp_str")]
    pub temperature: String,
}

#[derive(Debug, Deserialize)]
pub struct InstallForm {
    /// JSON-encoded MarketplaceEntry (passed as hidden field)
    pub entry_json: String,
}

#[derive(Debug, Deserialize)]
pub struct SetupSubmitForm {
    /// Step values as step_<id>=<value> pairs
    #[serde(flatten)]
    pub values: std::collections::HashMap<String, String>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// Main agents page: installed agents + marketplace catalogue
pub async fn agents_index(_user: AuthUser, Query(q): Query<AgentsQuery>, State(state): State<AppState>) -> Response {
    // Installed agents from in-memory map + DB metadata
    let agents_map = state.agents.read().await;
    let mut agents: Vec<InstalledAgentView> = Vec::new();

    for def in agents_map.values() {
        let db_row = sqlx::query_as::<_, (Option<String>, Option<String>, f64, i32, i32)>(
            "SELECT provider_id, model_id, temperature, is_bundled, setup_complete \
             FROM agents WHERE id = ?",
        )
        .bind(&def.id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();

        let (provider_id, model_id, _temp, is_bundled, setup_complete) =
            db_row.unwrap_or((None, None, 0.7, 0, 1));

        let prov_id = provider_id.unwrap_or_default();
        let mod_id = model_id.unwrap_or_default();
        let model_display_name = resolve_model_display_name(&prov_id, &mod_id);

        agents.push(InstalledAgentView {
            id: def.id.clone(),
            name: def.name.clone(),
            provider_id: prov_id,
            model_id: mod_id,
            model_display_name,
            capability_count: def.capability_manifests.len(),
            is_bundled: is_bundled != 0,
            setup_complete: setup_complete != 0,
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
            agents.push(InstalledAgentView {
                id,
                name,
                provider_id: String::new(),
                model_id: String::new(),
                model_display_name: String::new(),
                capability_count: 0,
                is_bundled: false,
                setup_complete: false,
            });
        }
    }

    // Marketplace catalogue
    let marketplace_url = state.config.marketplace_url.clone();
    let (marketplace_agents, marketplace_error) = match marketplace::fetch_catalogue(&marketplace_url).await {
        Ok(catalogue) => {
            let installed_ids: Vec<String> = agents.iter().map(|a| a.id.clone()).collect();
            let views: Vec<MarketplaceAgentView> = catalogue
                .agents
                .iter()
                .map(|entry| {
                    let entry_json = serde_json::to_string(entry).unwrap_or_default();
                    MarketplaceAgentView {
                        name: entry.name.clone(),
                        description: entry.description.clone(),
                        version: entry.version.clone(),
                        author: entry.author.clone(),
                        is_installed: installed_ids.contains(&entry.id),
                        entry_json,
                    }
                })
                .collect();
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

    match (AgentsTemplate {
        active_nav: "agents",
        agents,
        marketplace_agents,
        providers,
        global_provider,
        global_model,
        global_model_display_name,
        global_temperature,
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
    _user: AuthUser,
    State(state): State<AppState>,
    Form(form): Form<InstallForm>,
) -> Response {
    let entry: MarketplaceEntry = match serde_json::from_str(&form.entry_json) {
        Ok(e) => e,
        Err(e) => {
            error!("Invalid entry JSON: {e}");
            return Redirect::to("/agents?error=Invalid+agent+data.+Please+try+again.").into_response();
        }
    };

    let docker = match bollard::Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to connect to Docker: {e}");
            return Redirect::to("/agents?error=Cannot+connect+to+Docker.+Is+Docker+running%3F").into_response();
        }
    };

    match marketplace::install_agent(
        &entry,
        &state.config.installed_agents_dir,
        &state.db,
        &state.agents,
        &docker,
    )
    .await
    {
        Ok(agent_def) => {
            // Always redirect to setup if there are install steps or tools that need policies
            let has_tools = agent_def.capability_manifests.values()
                .any(|m| !m.tools.is_empty());
            if agent_def.install_steps.is_empty() && !has_tools {
                Redirect::to("/agents").into_response()
            } else {
                Redirect::to(&format!("/agents/{}/setup", entry.id)).into_response()
            }
        }
        Err(e) => {
            error!("Agent installation failed: {e}");
            let text = format!("Agent installation failed: {e}");
            let msg = urlencoding::encode(&text);
            Redirect::to(&format!("/agents?error={msg}")).into_response()
        }
    }
}

/// Show the setup wizard for an agent
pub async fn setup_wizard(
    _user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
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
            label: s.label.clone(),
            description: s.description.clone(),
            default_value: s.default.clone().unwrap_or_default(),
            validation: s.validation.clone().unwrap_or_default(),
        })
        .collect();

    let providers = load_provider_options(&state).await;

    // Build tool policy views from capability manifests
    let mut tool_policies: Vec<ToolPolicyView> = Vec::new();
    for (cap_id, manifest) in &agent_def.capability_manifests {
        for tool in &manifest.tools {
            let key = format!("{}_{}", cap_id, tool.name).replace('-', "_");
            tool_policies.push(ToolPolicyView {
                key,
                capability_id: cap_id.clone(),
                tool_name: tool.name.clone(),
                display_name: tool.name.replace('_', " "),
                description: tool.description.clone(),
                recommended: tool.effective_recommended_policy().to_string(),
            });
        }
    }
    // Add built-in tools
    tool_policies.push(ToolPolicyView {
        key: "__builtin___emit_event".to_string(),
        capability_id: "__builtin__".to_string(),
        tool_name: "emit_event".to_string(),
        display_name: "Emit Event".to_string(),
        description: "Allows the agent to emit events that can trigger other agents or notifications.".to_string(),
        recommended: "ask".to_string(),
    });
    tool_policies.push(ToolPolicyView {
        key: "__builtin___delegate_to_agent".to_string(),
        capability_id: "__builtin__".to_string(),
        tool_name: "delegate_to_agent".to_string(),
        display_name: "Delegate to Agent".to_string(),
        description: "Allows the agent to hand off tasks to other specialist agents.".to_string(),
        recommended: "ask".to_string(),
    });

    let name = sqlx::query_as::<_, (String,)>(
        "SELECT display_name FROM agents WHERE id = ?",
    )
    .bind(&agent_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .map(|(n,)| n)
    .unwrap_or_else(|| agent_def.name.clone());

    match (AgentSetupTemplate {
        active_nav: "agents",
        agent_id,
        agent_name: name,
        steps,
        tool_policies,
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
    _user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<SetupSubmitForm>,
) -> Response {
    let installed_dir = &state.config.installed_agents_dir;
    let agent_dir = std::path::Path::new(installed_dir).join(&agent_id);

    let agent_def = match crate::agents::loader::load_one(&agent_dir).await {
        Ok(a) => a,
        Err(e) => {
            error!("Failed to load agent for setup: {e}");
            let text = format!("Failed to load agent: {e}");
            let msg = urlencoding::encode(&text);
            return Redirect::to(&format!("/agents?error={msg}")).into_response();
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
                    let msg = urlencoding::encode("Failed to encrypt credential. Please try again.");
                    return Redirect::to(&format!("/agents/{agent_id}/setup?error={msg}")).into_response();
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
                    let msg = urlencoding::encode("Failed to store credential. Please try again.");
                    return Redirect::to(&format!("/agents/{agent_id}/setup?error={msg}")).into_response();
                }
            }
        }
    }

    // Process tool policies: extract policy_cap_*, policy_tool_*, policy_val_* fields
    // These are saved as GLOBAL defaults (not per-user) — they apply to all users.
    {
        use crate::permissions::tool_policy::{self, ToolPolicy};

        let mut policies: Vec<(String, String, ToolPolicy)> = Vec::new();

        // Collect all unique keys from form values that match the pattern
        let keys: Vec<String> = form.values.keys()
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
            if let Err(e) = tool_policy::set_global_policies(
                &state.db,
                &agent_id,
                &policies,
            ).await {
                error!("Failed to save global tool policies: {e}");
                let msg = urlencoding::encode("Failed to save tool permissions. Please try again.");
                return Redirect::to(&format!("/agents/{agent_id}/setup?error={msg}")).into_response();
            }
        }
    }

    // Mark setup complete + load agent into memory
    if let Err(e) = marketplace::complete_setup(
        &agent_id,
        installed_dir,
        &state.db,
        &state.agents,
    )
    .await
    {
        error!("Failed to complete setup: {e}");
        let text = format!("Failed to complete setup: {e}");
        let msg = urlencoding::encode(&text);
        return Redirect::to(&format!("/agents?error={msg}")).into_response();
    }

    let msg = urlencoding::encode("Agent setup completed successfully.");
    Redirect::to(&format!("/agents?success={msg}")).into_response()
}

/// Execute a test step during setup (HTMX endpoint)
pub async fn setup_test(
    _user: AuthUser,
    Path((agent_id, step_id)): Path<(String, String)>,
    State(state): State<AppState>,
    Form(form): Form<SetupSubmitForm>,
) -> Response {
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
    State(state): State<AppState>,
) -> Response {
    // Load agent definition
    let agents_map = state.agents.read().await;
    let agent = match agents_map.get(&agent_id) {
        Some(a) => a.clone(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    drop(agents_map);

    let agent_name = sqlx::query("SELECT display_name FROM agents WHERE id = ?")
        .bind(&agent_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .and_then(|r| {
            use sqlx::Row;
            r.try_get::<String, _>("display_name").ok()
        })
        .unwrap_or_else(|| agent.name.clone());

    // Build capability views
    let mut capabilities: Vec<CapabilityView> = Vec::new();

    // Get global default policies for this agent
    let global_policies = crate::permissions::tool_policy::get_global_policies_for_agent(
        &state.db,
        &agent_id,
    )
    .await
    .unwrap_or_default();

    let global_policy_map: std::collections::HashMap<(String, String), String> = global_policies
        .into_iter()
        .map(|p| ((p.capability_id, p.tool_name), p.policy.as_str().to_string()))
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
        .map(|p| ((p.capability_id, p.tool_name), p.policy.as_str().to_string()))
        .collect();

    for (cap_id, manifest) in &agent.capability_manifests {
        let network_mode = match manifest.network.mode {
            crate::capabilities::manifest::NetworkMode::None => "none",
            crate::capabilities::manifest::NetworkMode::Allowlist => "allowlist",
            crate::capabilities::manifest::NetworkMode::Any => "any",
        };

        let filesystem = match manifest.filesystem {
            crate::capabilities::manifest::FilesystemPolicy::None => "none",
            crate::capabilities::manifest::FilesystemPolicy::Temp => "temp",
            crate::capabilities::manifest::FilesystemPolicy::Workspace => "workspace",
        };

        let tools: Vec<ToolView> = manifest.tools.iter().map(|t| {
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
                format!("{}...", &t.description[..117])
            } else {
                t.description.clone()
            };
            ToolView {
                name: t.name.clone(),
                description: desc,
                policy,
                capability_id: cap_id.clone(),
                global_default,
                has_override,
            }
        }).collect();

        capabilities.push(CapabilityView {
            id: cap_id.clone(),
            network_mode: network_mode.to_string(),
            allowed_hosts: manifest.network.hosts.clone(),
            filesystem: filesystem.to_string(),
            max_memory_mb: manifest.resources.max_memory_mb,
            max_cpu_fraction: format!("{:.0}%", manifest.resources.max_cpu_fraction * 100.0),
            pids_limit: manifest.resources.pids_limit,
            tools,
        });
    }

    // Fetch egress log entries for this agent's capabilities
    let cap_ids: Vec<String> = agent.capability_manifests.keys().cloned().collect();
    let mut egress_entries: Vec<EgressEntryView> = Vec::new();

    // Query egress log for all capabilities of this agent (last 100)
    for cap_id in &cap_ids {
        let rows = sqlx::query(
            "SELECT capability_id, method, host, port, allowed, created_at
             FROM egress_log
             WHERE capability_id = ?
             ORDER BY created_at DESC
             LIMIT 100"
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

    // Build built-in tool policy views
    use crate::permissions::tool_policy::{BUILTIN_CAPABILITY_ID, BUILTIN_DELEGATE, BUILTIN_EMIT_EVENT};
    let builtin_tools = [
        (BUILTIN_DELEGATE, "Delegate to Agent", "Allows the agent to hand off tasks to other specialist agents."),
        (BUILTIN_EMIT_EVENT, "Emit Event", "Allows the agent to emit events that can trigger other agents or notifications."),
    ];
    let builtin_policies: Vec<BuiltinPolicyView> = builtin_tools
        .iter()
        .map(|(tool_name, display, desc)| {
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
                description: desc.to_string(),
                policy,
                global_default,
                has_override,
            }
        })
        .collect();

    match (AgentDetailTemplate {
        active_nav: "agents",
        agent_id,
        agent_name,
        is_admin: user.is_admin,
        capabilities,
        builtin_policies,
        egress_entries,
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

/// Set model for a specific agent
pub async fn set_agent_model_handler(
    _user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<SetAgentModelForm>,
) -> Response {
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
        return Redirect::to("/agents?error=Failed+to+update+model+assignment.+Please+try+again.").into_response();
    }

    Redirect::to("/agents?success=Model+updated+successfully.").into_response()
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

    // Tell HTMX to reload the page so the UI reflects the reset state
    (
        StatusCode::OK,
        [("HX-Redirect", format!("/agents/{agent_id}"))],
        "",
    )
        .into_response()
}

/// Uninstall an agent
pub async fn uninstall_agent(
    _user: AuthUser,
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
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
        return Redirect::to(&format!("/agents?error={msg}")).into_response();
    }

    Redirect::to("/agents?success=Agent+uninstalled+successfully.").into_response()
}

/// Set the global default model
pub async fn set_default_model(
    _user: AuthUser,
    State(state): State<AppState>,
    Form(form): Form<SetDefaultModelForm>,
) -> Response {
    let temp: f32 = form.temperature.parse().unwrap_or(0.7);

    if let Err(e) = model::set_global_default(&state.db, &form.provider_id, &form.model_id, temp)
        .await
    {
        error!("Failed to set default model: {e}");
        return Redirect::to("/agents?error=Failed+to+update+global+default+model.+Please+try+again.").into_response();
    }

    Redirect::to("/agents?success=Global+default+model+updated.").into_response()
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Load only providers that have an API key configured (i.e. are actually usable).
/// Bedrock is special: it uses IAM auth, so it's considered configured if it has a region.
async fn load_provider_options(state: &AppState) -> Vec<ProviderOption> {
    sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
        "SELECT id, display_name, api_key_encrypted, base_url FROM llm_providers WHERE active = 1 ORDER BY id",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .filter(|(id, _, key, base_url)| {
        let has_key = key.as_ref().map_or(false, |k| !k.is_empty());
        let has_region = base_url.as_ref().map_or(false, |u| !u.is_empty());
        // Bedrock uses IAM credentials (not an API key in our DB), so treat
        // it as configured if it has a region set.
        has_key || (id == "bedrock" && has_region)
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
    let models = crate::llm::models::list_models(
        &state.db,
        &state.credentials,
        &provider_id,
    )
    .await;

    if models.is_empty() {
        return Html(r#"<option value="">No models available for this provider</option>"#.to_string())
            .into_response();
    }

    let mut html = String::from(r#"<option value="">-- Select a model --</option>"#);
    for m in models {
        html.push_str(&format!(
            r#"<option value="{}">{}</option>"#,
            m.id, m.name,
        ));
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
