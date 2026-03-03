pub mod agents;
pub mod auth;
pub mod chat;
pub mod credentials;
pub mod integrations;
pub mod personality;
pub mod pipes;
pub mod providers;
pub mod subscriptions;
pub mod telegram;
pub mod users;

use crate::state::AppState;
use axum::{
    Router,
    extract::{FromRequestParts, State},
    http::{request::Parts, HeaderMap},
    middleware::{self, Next},
    response::{IntoResponse, Redirect},
    routing::{delete, get, post},
};

// ── BasePath extractor ───────────────────────────────────────────────────────

/// Per-request URL path prefix resolved from the `X-Forwarded-Prefix` header,
/// falling back to the static `SELU__BASE_PATH` config, then to `""`.
///
/// Middleware inserts this into request extensions before any handler runs.
/// Handlers extract it as `BasePath(base_path): BasePath`.
#[derive(Debug, Clone)]
pub struct BasePath(pub String);

/// Middleware that resolves the base path from the `X-Forwarded-Prefix` header
/// (set by the reverse proxy) and falls back to the static `state.base_path`.
pub async fn resolve_base_path(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut req: axum::extract::Request,
    next: Next,
) -> impl IntoResponse {
    let bp = headers
        .get("x-forwarded-prefix")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| state.base_path.clone());
    req.extensions_mut().insert(BasePath(bp));
    next.run(req).await
}

impl FromRequestParts<AppState> for BasePath {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<BasePath>()
            .cloned()
            .unwrap_or(BasePath(String::new())))
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Build a redirect target that respects the resolved base path.
/// Example: `prefixed_redirect("/selu", "/chat")` → `Redirect::to("/selu/chat")`
pub fn prefixed_redirect(base_path: &str, path: &str) -> Redirect {
    Redirect::to(&format!("{}{}", base_path, path))
}

/// Build a URL string that respects the resolved base path.
/// Useful for `format!`-based redirect targets and `HX-Redirect` headers.
pub fn prefixed(base_path: &str, path: &str) -> String {
    format!("{}{}", base_path, path)
}

// ── Root redirect handler ────────────────────────────────────────────────────

async fn root_redirect(BasePath(base_path): BasePath) -> Redirect {
    Redirect::to(&format!("{}/chat", base_path))
}

// ── Router ───────────────────────────────────────────────────────────────────

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        // Public routes (no auth required)
        .route("/login", get(auth::login_page).post(auth::login_submit))
        .route("/logout", post(auth::logout))
        .route("/setup", get(auth::setup_page).post(auth::setup_submit))
        // Root redirect
        .route("/", get(root_redirect))
        // All routes below require AuthUser extractor (session cookie)
        // Chat
        .route("/chat", get(chat::chat_index))
        .route("/chat/{pipe_id}", get(chat::chat_pipe))
        .route("/chat/{pipe_id}/t/new", post(chat::chat_new_thread))
        .route("/chat/{pipe_id}/t/{thread_id}", get(chat::chat_thread))
        .route("/chat/{pipe_id}/t/{thread_id}/send", post(chat::chat_send))
        .route("/chat/{pipe_id}/stream/{stream_id}", get(chat::chat_stream))
        .route("/chat/confirm/{confirmation_id}", post(chat::chat_confirm))
        // Pipes (unified: all pipe types including iMessage, webhook, web, etc.)
        .route("/pipes", get(pipes::pipes_index))
        .route("/pipes/webhook/new", get(pipes::pipes_webhook_new))
        .route("/pipes/webhook", post(pipes::pipes_webhook_create))
        .route("/pipes/web/new", get(pipes::pipes_web_new))
        .route("/pipes/web", post(pipes::pipes_web_create))
        .route("/pipes/{pipe_id}", delete(pipes::pipes_delete))
        // Pipes: iMessage setup & management
        .route(
            "/pipes/imessage/setup",
            get(integrations::imessage_setup_page).post(integrations::imessage_setup_submit),
        )
        .route(
            "/pipes/imessage/proxy/chats",
            post(integrations::bb_proxy_chats),
        )
        .route(
            "/pipes/imessage/{config_id}",
            get(integrations::imessage_detail).delete(integrations::imessage_delete),
        )
        .route(
            "/pipes/imessage/{config_id}/people",
            post(integrations::imessage_add_person),
        )
        .route(
            "/pipes/imessage/{config_id}/people/{ref_id}",
            delete(integrations::imessage_remove_person),
        )
        // Pipes: Telegram setup & management
        .route(
            "/pipes/telegram/setup",
            get(telegram::telegram_setup_page).post(telegram::telegram_setup_submit),
        )
        .route(
            "/pipes/telegram/proxy/chats",
            post(telegram::tg_proxy_chats),
        )
        .route(
            "/pipes/telegram/{config_id}",
            get(telegram::telegram_detail),
        )
        .route(
            "/pipes/telegram/{config_id}/delete",
            post(telegram::telegram_delete),
        )
        .route(
            "/pipes/telegram/{config_id}/people",
            post(telegram::telegram_add_person),
        )
        .route(
            "/pipes/telegram/{config_id}/people/{ref_id}",
            delete(telegram::telegram_remove_person),
        )
        // Agents (marketplace, install, setup, model assignment)
        .route("/agents", get(agents::agents_index))
        .route("/agents/install", post(agents::install_agent))
        .route("/agents/update", post(agents::update_agent))
        .route("/agents/default-model", post(agents::set_default_model))
        .route("/agents/{agent_id}", get(agents::agent_detail))
        .route(
            "/agents/{agent_id}/setup",
            get(agents::setup_wizard).post(agents::setup_submit),
        )
        .route(
            "/agents/{agent_id}/setup/test/{step_id}",
            post(agents::setup_test),
        )
        .route(
            "/agents/{agent_id}/model",
            post(agents::set_agent_model_handler),
        )
        .route(
            "/agents/{agent_id}/policy",
            post(agents::set_tool_policy_handler),
        )
        .route(
            "/agents/{agent_id}/policy/reset",
            post(agents::reset_tool_policy_handler),
        )
        .route(
            "/agents/{agent_id}/uninstall",
            post(agents::uninstall_agent),
        )
        .route(
            "/agents/{agent_id}/auto-update",
            post(agents::toggle_auto_update),
        )
        .route("/agents/{agent_id}/rate", post(agents::rate_agent))
        .route(
            "/agents/models/{provider_id}",
            get(agents::models_for_provider),
        )
        // Credentials
        .route("/credentials", get(credentials::credentials_index))
        .route(
            "/credentials/system",
            post(credentials::credentials_set_system),
        )
        .route(
            "/credentials/system/{cap_id}/{name}",
            delete(credentials::credentials_delete_system),
        )
        .route("/credentials/user", post(credentials::credentials_set_user))
        .route(
            "/credentials/user/{user_id}/{cap_id}/{name}",
            delete(credentials::credentials_delete_user),
        )
        // Providers
        .route("/providers", get(providers::providers_index))
        .route(
            "/providers/{provider_id}/key",
            post(providers::providers_set_key).delete(providers::providers_delete_key),
        )
        .route(
            "/providers/{provider_id}/region",
            post(providers::providers_set_region),
        )
        // Subscriptions
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
        .route("/users/{id}/toggle-admin", post(users::users_toggle_admin))
        .route("/users/{id}/language", post(users::users_set_language))
        // Personality
        .route(
            "/personality",
            get(personality::personality_index).post(personality::personality_add),
        )
        .route(
            "/personality/{id}",
            delete(personality::personality_delete).put(personality::personality_update),
        )
        .route(
            "/personality/{id}/edit",
            get(personality::personality_edit_form),
        )
        .route("/personality/{id}/row", get(personality::personality_row))
        // Apply base path resolution middleware to ALL routes
        .layer(middleware::from_fn_with_state(state.clone(), resolve_base_path))
        .with_state(state)
}
