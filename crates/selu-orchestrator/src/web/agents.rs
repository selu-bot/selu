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
    providers: Vec<ProviderOption>,
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
            if agent_def.install_steps.is_empty() {
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
