pub mod agents;
pub mod auth;
pub mod chat;
pub mod credentials;
pub mod feedback;
pub mod integrations;
pub mod mobile;
pub mod personality;
pub mod pipes;
pub mod providers;
pub mod schedules;
pub mod system_updates;
pub mod telegram;
pub mod users;
pub mod whatsapp;

use crate::state::AppState;
use axum::{
    Router,
    extract::{DefaultBodyLimit, FromRequestParts},
    http::{Uri, request::Parts},
    response::Redirect,
    routing::{delete, get, post},
};
use std::convert::Infallible;
use std::task::{Context, Poll};
use tower::{Layer, Service};

// ── BasePath extractor ───────────────────────────────────────────────────────

/// Per-request URL path prefix resolved from the `X-Forwarded-Prefix` header,
/// falling back to the static `SELU__BASE_PATH` config, then to `""`.
///
/// The [`StripPrefixService`] Tower service inserts this into request extensions
/// before Axum's router performs route matching.
/// Handlers extract it as `BasePath(base_path): BasePath`.
#[derive(Debug, Clone)]
pub struct BasePath(pub String);

/// The full external origin URL (scheme + host + base_path) as seen by the
/// user's browser.
///
/// Resolution order:
///   1. Admin-configured public web address (highest priority)
///   2. Auto-detected from request headers: `X-Forwarded-Proto` + `Host`
///      (or `X-Forwarded-Host`) + base_path
///   3. Fallback: `http://localhost:{port}` (from config)
///
/// Handlers extract it as `ExternalOrigin(origin): ExternalOrigin`.
#[derive(Debug, Clone)]
pub struct ExternalOrigin(pub String);

// ── Tower service: prefix stripping + extension injection ────────────────────
//
// This runs BEFORE Axum's router performs route matching, which is critical
// because `Router::layer()` only wraps matched handlers — it cannot rewrite
// the URI before route matching.  A Tower service wrapping the entire Router
// from the outside does run first.

/// Tower [`Layer`] that wraps a service with [`StripPrefixService`].
#[derive(Clone)]
pub struct StripPrefixLayer {
    pub state: AppState,
}

impl<S> Layer<S> for StripPrefixLayer {
    type Service = StripPrefixService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        StripPrefixService {
            inner,
            state: self.state.clone(),
        }
    }
}

/// Tower service that intercepts every inbound request and:
///
///   1. Reads the `X-Forwarded-Prefix` header (e.g. `/selu`)
///   2. Strips that prefix from the request URI (`/selu/chat` → `/chat`)
///      so Axum's router can match the bare path
///   3. Injects [`BasePath`] and [`ExternalOrigin`] into request extensions
///      for handlers / templates / redirects
///
/// Because this is a Tower service wrapping the entire Axum router, the URI
/// rewrite happens **before** route matching.
#[derive(Clone)]
pub struct StripPrefixService<S> {
    inner: S,
    state: AppState,
}

impl<S> Service<axum::extract::Request> for StripPrefixService<S>
where
    S: Service<axum::extract::Request, Response = axum::response::Response, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = axum::response::Response;
    type Error = Infallible;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: axum::extract::Request) -> Self::Future {
        // ── 1. Resolve base path ─────────────────────────────────────────
        let bp = req
            .headers()
            .get("x-forwarded-prefix")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| self.state.base_path.clone());

        // ── 2. Strip prefix from URI ─────────────────────────────────────
        if !bp.is_empty() {
            let path = req.uri().path().to_string();
            if let Some(stripped) = path.strip_prefix(bp.as_str()) {
                let new_path = if stripped.is_empty() || !stripped.starts_with('/') {
                    format!("/{}", stripped)
                } else {
                    stripped.to_string()
                };
                let new_uri = if let Some(q) = req.uri().query() {
                    format!("{}?{}", new_path, q)
                } else {
                    new_path
                };
                if let Ok(parsed) = new_uri.parse::<Uri>() {
                    *req.uri_mut() = parsed;
                }
            }
        }

        // ── 3. Resolve external origin ───────────────────────────────────
        let origin = if let Some(configured) = self.state.public_origin_override.load_full() {
            configured.trim_end_matches('/').to_string()
        } else if let Some(ref configured) = self.state.config.external_url {
            configured.trim_end_matches('/').to_string()
        } else {
            let proto = req
                .headers()
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("http");
            let host = req
                .headers()
                .get("x-forwarded-host")
                .or_else(|| req.headers().get("host"))
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if host.is_empty() {
                format!("http://localhost:{}", self.state.config.server.port)
            } else {
                format!("{}://{}", proto, host)
            }
        };

        let full_origin = if bp.is_empty() {
            origin
        } else {
            format!("{}{}", origin, bp)
        };

        // ── 4. Inject extensions ─────────────────────────────────────────
        req.extensions_mut().insert(BasePath(bp));
        req.extensions_mut().insert(ExternalOrigin(full_origin));

        // ── 5. Forward to inner service (Axum Router) ────────────────────
        self.inner.call(req)
    }
}

// ── Extractors ───────────────────────────────────────────────────────────────

impl FromRequestParts<AppState> for BasePath {
    type Rejection = Infallible;

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

impl FromRequestParts<AppState> for ExternalOrigin {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<ExternalOrigin>()
            .cloned()
            .unwrap_or(ExternalOrigin(String::new())))
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
        .route(
            "/chat/{pipe_id}/t/{thread_id}/delete",
            post(chat::chat_delete_thread),
        )
        .route(
            "/chat/{pipe_id}/t/{thread_id}/send",
            post(chat::chat_send).layer(DefaultBodyLimit::max(6 * 1024 * 1024)),
        )
        .route("/chat/{pipe_id}/stream/{stream_id}", get(chat::chat_stream))
        .route("/chat/confirm/{confirmation_id}", post(chat::chat_confirm))
        .route(
            "/chat/{pipe_id}/t/{thread_id}/feedback",
            post(chat::chat_feedback),
        )
        // Pipes (unified: all pipe types including iMessage, webhook, web, etc.)
        .route("/pipes", get(pipes::pipes_index))
        .route("/pipes/new", get(pipes::pipes_new))
        .route("/pipes/new/{pipe_type}", get(pipes::pipes_new_redirect))
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
            "/pipes/telegram/{config_id}/webhook-check",
            get(telegram::telegram_check_webhook),
        )
        .route(
            "/pipes/telegram/{config_id}/webhook-reregister",
            post(telegram::telegram_reregister_webhook),
        )
        .route(
            "/pipes/telegram/{config_id}/people",
            post(telegram::telegram_add_person),
        )
        .route(
            "/pipes/telegram/{config_id}/people/{ref_id}",
            delete(telegram::telegram_remove_person),
        )
        // Pipes: WhatsApp setup & management
        .route(
            "/pipes/whatsapp/setup",
            get(whatsapp::whatsapp_setup_page).post(whatsapp::whatsapp_setup_submit),
        )
        .route(
            "/pipes/whatsapp/chats/search",
            get(whatsapp::whatsapp_search_chats),
        )
        .route(
            "/pipes/whatsapp/{config_id}",
            get(whatsapp::whatsapp_detail),
        )
        .route(
            "/pipes/whatsapp/{config_id}/delete",
            post(whatsapp::whatsapp_delete),
        )
        .route(
            "/pipes/whatsapp/{config_id}/people",
            post(whatsapp::whatsapp_add_person),
        )
        .route(
            "/pipes/whatsapp/{config_id}/people/{ref_id}",
            delete(whatsapp::whatsapp_remove_person),
        )
        // Agents (marketplace, install, setup, model assignment)
        .route("/agents", get(agents::agents_index))
        .route("/agents/install", post(agents::install_agent))
        .route("/agents/update/wizard", post(agents::update_wizard))
        .route("/agents/update", post(agents::update_agent))
        .route("/agents/update/start", post(agents::start_agent_update))
        .route(
            "/agents/update/status/{job_id}",
            get(agents::agent_update_status),
        )
        .route("/agents/default-model", post(agents::set_default_model))
        .route(
            "/agents/default-image-model",
            post(agents::set_default_image_model),
        )
        .route("/agents/{agent_id}", get(agents::agent_detail))
        .route(
            "/agents/{agent_id}/storage",
            get(agents::agent_detail_storage),
        )
        .route(
            "/agents/{agent_id}/memory",
            get(agents::agent_detail_memory),
        )
        .route(
            "/agents/{agent_id}/network",
            get(agents::agent_detail_network),
        )
        .route(
            "/agents/{agent_id}/network/access",
            post(agents::set_network_access_handler),
        )
        .route(
            "/agents/{agent_id}/network/host",
            post(agents::set_network_host_policy_handler),
        )
        .route(
            "/agents/{agent_id}/network/host/delete",
            post(agents::delete_network_host_handler),
        )
        .route(
            "/agents/{agent_id}/permissions",
            get(agents::agent_detail_permissions),
        )
        .route(
            "/agents/{agent_id}/improvement",
            get(agents::agent_detail_improvement),
        )
        .route(
            "/agents/{agent_id}/improvement/pause",
            post(agents::improvement_pause_handler),
        )
        .route(
            "/agents/{agent_id}/improvement/reject",
            post(agents::improvement_reject_handler),
        )
        .route(
            "/agents/{agent_id}/improvement/activate",
            post(agents::improvement_activate_handler),
        )
        .route(
            "/agents/{agent_id}/improvement/reset",
            post(agents::improvement_reset_handler),
        )
        .route(
            "/agents/{agent_id}/secrets",
            get(agents::agent_detail_secrets),
        )
        .route(
            "/agents/{agent_id}/tool-loop-limit",
            post(agents::set_runtime_settings_handler),
        )
        .route(
            "/agents/{agent_id}/runtime-settings",
            post(agents::set_runtime_settings_handler),
        )
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
            "/agents/{agent_id}/image-model",
            post(agents::set_agent_image_model_handler),
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
            "/agents/{agent_id}/credential",
            post(agents::agent_credential_set),
        )
        .route(
            "/agents/{agent_id}/credential/{scope}/{cap_id}/{name}",
            delete(agents::agent_credential_delete),
        )
        .route(
            "/agents/{agent_id}/storage/delete",
            post(agents::agent_storage_delete),
        )
        .route(
            "/agents/{agent_id}/memory/delete",
            post(agents::agent_memory_delete),
        )
        .route(
            "/agents/{agent_id}/automation",
            post(agents::agent_automation_toggle),
        )
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
        .route("/providers/new", get(providers::providers_new))
        .route(
            "/providers/new/{provider_id}",
            get(providers::providers_setup_page).post(providers::providers_setup_submit),
        )
        .route(
            "/providers/{provider_id}/disconnect",
            post(providers::providers_disconnect),
        )
        // System updates
        .route("/system-updates", get(system_updates::updates_index))
        .route(
            "/system-updates/channel",
            post(system_updates::updates_set_channel),
        )
        .route(
            "/system-updates/public-origin",
            post(system_updates::updates_set_public_origin),
        )
        .route(
            "/system-updates/public-origin/apply-current",
            post(system_updates::updates_apply_current_host),
        )
        .route(
            "/system-updates/check",
            post(system_updates::updates_check_now),
        )
        .route(
            "/system-updates/job-status",
            get(system_updates::updates_job_status),
        )
        .route(
            "/system-updates/apply",
            post(system_updates::updates_apply_now),
        )
        .route(
            "/system-updates/apply/start",
            post(system_updates::updates_apply_start),
        )
        .route(
            "/system-updates/rollback",
            post(system_updates::updates_rollback_now),
        )
        .route(
            "/system-updates/rollback/start",
            post(system_updates::updates_rollback_start),
        )
        .route(
            "/system-updates/auto-update",
            post(system_updates::updates_toggle_auto_update),
        )
        .route(
            "/system-updates/installation-telemetry",
            post(system_updates::updates_toggle_installation_telemetry),
        )
        .route(
            "/system-updates/push-notifications",
            post(system_updates::updates_toggle_push_notifications),
        )
        // Schedules
        .route(
            "/schedules",
            get(schedules::schedules_index).post(schedules::schedules_create),
        )
        .route("/schedules/{id}", delete(schedules::schedules_delete))
        .route("/schedules/{id}/toggle", post(schedules::schedules_toggle))
        .route(
            "/schedules/{id}/pipes",
            post(schedules::schedules_update_pipes),
        )
        .route("/user/timezone", post(schedules::user_set_timezone))
        // Users
        .route("/users", get(users::users_index).post(users::users_create))
        .route("/users/change-password", post(users::users_change_password))
        .route("/users/{id}", delete(users::users_delete))
        .route("/users/{id}/toggle-admin", post(users::users_toggle_admin))
        .route("/users/{id}/language", post(users::users_set_language))
        .route("/users/{id}/agents", post(users::users_set_agents))
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
        // Feedback
        .route(
            "/feedback",
            get(feedback::feedback_page).post(feedback::feedback_submit),
        )
        // Mobile app setup
        .route("/mobile", get(mobile::mobile_page))
        .route("/mobile/setup-token", post(mobile::create_setup_token))
        .with_state(state)
}
