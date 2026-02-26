use crate::state::AppState;
use axum::{
    Router,
    routing::{delete, get, post, put},
};

pub mod credentials;
pub mod pipes;
pub mod providers;
pub mod subscriptions;

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/api/health", get(health))
        // Pipe management
        .route("/api/pipes", get(pipes::list_pipes))
        .route("/api/pipes", post(pipes::create_pipe))
        .route("/api/pipes/{pipe_id}", delete(pipes::delete_pipe))
        // LLM provider management
        .route("/api/providers", get(providers::list_providers))
        .route("/api/providers/{id}/key", put(providers::set_api_key))
        .route("/api/providers/{id}/region", put(providers::set_region))
        // System credential management
        .route("/api/credentials/system/{capability_id}", get(credentials::list_system))
        .route("/api/credentials/system/{capability_id}/{name}", put(credentials::set_system))
        .route("/api/credentials/system/{capability_id}/{name}", delete(credentials::delete_system))
        // User credential management
        .route("/api/credentials/user/{user_id}/{capability_id}", get(credentials::list_user))
        .route("/api/credentials/user/{user_id}/{capability_id}/{name}", put(credentials::set_user))
        .route("/api/credentials/user/{user_id}/{capability_id}/{name}", delete(credentials::delete_user))
        // Event subscriptions
        .route("/api/subscriptions", get(subscriptions::list_subscriptions))
        .route("/api/subscriptions", post(subscriptions::create_subscription))
        .route("/api/subscriptions/{id}", delete(subscriptions::delete_subscription))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}
