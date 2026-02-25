use askama::Template;
use axum::{extract::State, http::StatusCode, response::{Html, IntoResponse, Response}};
use tracing::error;

use crate::state::AppState;
use crate::web::auth::AuthUser;

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AgentView {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub model_id: String,
    pub capability_count: usize,
    pub session_trigger: String,
}

// ── Template ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "agents.html")]
struct AgentsTemplate {
    active_nav: &'static str,
    agents: Vec<AgentView>,
}

// ── Handler ───────────────────────────────────────────────────────────────────

pub async fn agents_index(_user: AuthUser, State(state): State<AppState>) -> Response {
    let agents_map = state.agents.read().await;
    let mut agents: Vec<AgentView> = agents_map
        .values()
        .map(|def| AgentView {
            id: def.id.clone(),
            name: def.name.clone(),
            provider: def.model.provider.clone(),
            model_id: def.model.model_id.clone(),
            capability_count: def.capabilities.len(),
            session_trigger: def.session.trigger.clone(),
        })
        .collect();
    drop(agents_map);
    agents.sort_by(|a, b| a.id.cmp(&b.id));

    match (AgentsTemplate { active_nav: "agents", agents }).render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
