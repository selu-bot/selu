use crate::state::AppState;
use axum::{
    Router,
    routing::{delete, get, post, put},
};

pub mod accounts;
pub mod agents;
pub mod artifacts;
pub mod auth;
pub mod automations;
pub mod cache_admin;
pub mod connectors;
pub mod conversations;
pub mod credentials;
pub mod jobs;
pub mod mobile;
pub mod pipes;
pub mod provider_admin;
pub mod providers;
pub mod system_updates;
pub mod tool_policies;

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/v1/auth/state", get(auth::state))
        .route("/api/v1/auth/login", post(auth::login))
        .route("/api/v1/auth/setup", post(auth::setup))
        .route("/api/v1/auth/logout", post(auth::logout))
        .merge(conversations::router())
        .merge(jobs::router())
        .merge(agents::router())
        .merge(connectors::router())
        .merge(system_updates::router())
        .nest("/api/v1", accounts::router())
        .merge(automations::router())
        .nest("/api/v1", cache_admin::router())
        .merge(provider_admin::router())
        .route(
            "/api/artifacts/{artifact_id}/download",
            get(artifacts::download_artifact),
        )
        // Pipe management
        .route("/api/pipes", get(pipes::list_pipes))
        .route("/api/pipes", post(pipes::create_pipe))
        .route("/api/pipes/{pipe_id}", delete(pipes::delete_pipe))
        // LLM provider management
        .route("/api/providers", get(providers::list_providers))
        .route("/api/providers/{id}/key", put(providers::set_api_key))
        .route("/api/providers/{id}/region", put(providers::set_region))
        // System credential management
        .route(
            "/api/credentials/system/{capability_id}",
            get(credentials::list_system),
        )
        .route(
            "/api/credentials/system/{capability_id}/{name}",
            put(credentials::set_system),
        )
        .route(
            "/api/credentials/system/{capability_id}/{name}",
            delete(credentials::delete_system),
        )
        // User credential management
        .route(
            "/api/credentials/user/{user_id}/{capability_id}",
            get(credentials::list_user),
        )
        .route(
            "/api/credentials/user/{user_id}/{capability_id}/{name}",
            put(credentials::set_user),
        )
        .route(
            "/api/credentials/user/{user_id}/{capability_id}/{name}",
            delete(credentials::delete_user),
        )
        // Tool policies
        .route(
            "/api/tool-policies",
            get(tool_policies::list_policies)
                .put(tool_policies::bulk_set_policies)
                .delete(tool_policies::delete_user_policy),
        )
        // Pending tool approvals
        .route("/api/approvals", get(tool_policies::list_approvals))
        .route("/api/approvals/{id}", post(tool_policies::resolve_approval))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}
