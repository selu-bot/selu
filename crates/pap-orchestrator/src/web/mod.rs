pub mod agents;
pub mod auth;
pub mod bluebubbles;
pub mod chat;
pub mod credentials;
pub mod integrations;
pub mod pipes;
pub mod providers;
pub mod subscriptions;
pub mod users;

use crate::state::AppState;
use axum::{
    routing::{delete, get, post},
    Router,
};

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        // Public routes (no auth required)
        .route("/login", get(auth::login_page).post(auth::login_submit))
        .route("/logout", post(auth::logout))
        .route("/setup", get(auth::setup_page).post(auth::setup_submit))
        // Root redirect
        .route("/", get(|| async { axum::response::Redirect::to("/chat") }))
        // All routes below require AuthUser extractor (session cookie)
        // Chat
        .route("/chat", get(chat::chat_index))
        .route("/chat/{pipe_id}", get(chat::chat_pipe))
        .route("/chat/{pipe_id}/stream/{stream_id}", get(chat::chat_stream))
        .route("/chat/{pipe_id}/send", post(chat::chat_send))
        .route("/chat/confirm/{confirmation_id}", post(chat::chat_confirm))
        // Integrations (user-friendly flow)
        .route("/integrations", get(integrations::integrations_index))
        .route("/integrations/imessage/setup", get(integrations::imessage_setup_page).post(integrations::imessage_setup_submit))
        .route("/integrations/imessage/proxy/chats", post(integrations::bb_proxy_chats))
        .route("/integrations/imessage/{config_id}", get(integrations::imessage_detail).delete(integrations::imessage_delete))
        .route("/integrations/imessage/{config_id}/people", post(integrations::imessage_add_person))
        .route("/integrations/imessage/{config_id}/people/{ref_id}", delete(integrations::imessage_remove_person))
        // Agents (marketplace, install, setup, model assignment)
        .route("/agents", get(agents::agents_index))
        .route("/agents/install", post(agents::install_agent))
        .route("/agents/default-model", post(agents::set_default_model))
        .route("/agents/{agent_id}/setup", get(agents::setup_wizard).post(agents::setup_submit))
        .route("/agents/{agent_id}/setup/test/{step_id}", post(agents::setup_test))
        .route("/agents/{agent_id}/model", post(agents::set_agent_model_handler))
        .route("/agents/{agent_id}/uninstall", post(agents::uninstall_agent))
        .route("/agents/models/{provider_id}", get(agents::models_for_provider))
        // Pipes (advanced)
        .route("/pipes", get(pipes::pipes_index).post(pipes::pipes_create))
        .route("/pipes/{pipe_id}", delete(pipes::pipes_delete))
        // Credentials (advanced)
        .route(
            "/credentials",
            get(credentials::credentials_index),
        )
        .route(
            "/credentials/system",
            post(credentials::credentials_set_system),
        )
        .route(
            "/credentials/system/{cap_id}/{name}",
            delete(credentials::credentials_delete_system),
        )
        .route(
            "/credentials/user",
            post(credentials::credentials_set_user),
        )
        .route(
            "/credentials/user/{user_id}/{cap_id}/{name}",
            delete(credentials::credentials_delete_user),
        )
        // Providers
        .route("/providers", get(providers::providers_index))
        .route("/providers/{provider_id}/key", post(providers::providers_set_key).delete(providers::providers_delete_key))
        .route("/providers/{provider_id}/region", post(providers::providers_set_region))
        // Subscriptions (advanced)
        .route(
            "/subscriptions",
            get(subscriptions::subscriptions_index).post(subscriptions::subscriptions_create),
        )
        .route(
            "/subscriptions/{id}",
            delete(subscriptions::subscriptions_delete),
        )
        // Users
        .route("/users", get(users::users_index).post(users::users_create))
        .route("/users/{id}", delete(users::users_delete))
        // BlueBubbles (legacy, kept for direct access)
        .route("/bluebubbles", get(bluebubbles::bluebubbles_index))
        .route("/bluebubbles/configs", post(bluebubbles::bb_config_create))
        .route("/bluebubbles/configs/{id}", delete(bluebubbles::bb_config_delete))
        .route("/bluebubbles/sender-refs", post(bluebubbles::sender_ref_create))
        .route("/bluebubbles/sender-refs/{id}", delete(bluebubbles::sender_ref_delete))
        .with_state(state)
}
