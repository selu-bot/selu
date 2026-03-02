/// Integrated Telegram Bot adapter.
///
/// Uses the Telegram Bot API webhook mechanism:
///   1. Exposes a webhook endpoint (`POST /api/telegram/webhook/{config_id}`)
///   2. Registers the webhook URL with Telegram via `setWebhook`
///   3. Dispatches inbound messages through the standard pipe inbound flow
///   4. Sends replies via the Telegram `sendMessage` API
///   5. Supports streaming via `sendMessageDraft` (Bot API 9.3+)
///
/// The Telegram adapter follows the same patterns as BlueBubbles:
///   - Module-level `AdapterRegistry` keyed by config_id
///   - `ChannelSender` implementation (`TelegramSender`) registered per-pipe
///   - `init_all()` at startup, `start_one()` for hot-start after setup
use anyhow::{Context as _, Result};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Router,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::agents::{
    engine::{noop_sender, run_turn, ChannelKind, TurnParams},
    router as agent_router,
    thread as thread_mgr,
};
use crate::permissions::approval_queue;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Telegram Bot API types
// ---------------------------------------------------------------------------

/// Outer wrapper for a Telegram Bot API webhook update.
#[derive(Debug, Deserialize)]
struct TgUpdate {
    update_id: i64,
    message: Option<TgMessage>,
    edited_message: Option<TgMessage>,
}

#[derive(Debug, Deserialize)]
struct TgMessage {
    message_id: i64,
    from: Option<TgUser>,
    chat: TgChat,
    text: Option<String>,
    #[allow(dead_code)]
    date: i64,
    /// If the message is a reply, the original message
    reply_to_message: Option<Box<TgMessage>>,
}

#[derive(Debug, Deserialize)]
struct TgUser {
    id: i64,
    first_name: String,
    last_name: Option<String>,
    #[allow(dead_code)]
    username: Option<String>,
    #[allow(dead_code)]
    language_code: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TgChat {
    id: i64,
    #[serde(rename = "type")]
    chat_type: String,
    title: Option<String>,
    #[allow(dead_code)]
    username: Option<String>,
}

/// Telegram `sendMessage` request body.
#[derive(Debug, Serialize)]
struct TgSendMessage {
    chat_id: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to_message_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<String>,
}

/// Telegram `sendChatAction` request body.
#[derive(Debug, Serialize)]
struct TgChatAction {
    chat_id: String,
    action: String,
}

/// Telegram `sendMessageDraft` request body (streaming support, Bot API 9.3+).
#[derive(Debug, Serialize)]
#[allow(dead_code)]
struct TgSendMessageDraft {
    /// The business connection id — not needed for regular bots.
    #[serde(skip_serializing_if = "Option::is_none")]
    business_connection_id: Option<String>,
    chat_id: String,
    text: String,
    /// Draft message id; used to continue streaming into the same draft.
    #[serde(skip_serializing_if = "Option::is_none")]
    draft_message_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to_message_id: Option<i64>,
}

/// Response from Telegram `sendMessage`.
#[derive(Debug, Deserialize)]
struct TgApiResponse {
    ok: bool,
    #[allow(dead_code)]
    description: Option<String>,
    result: Option<TgApiMessageResult>,
}

#[derive(Debug, Deserialize)]
struct TgApiMessageResult {
    message_id: i64,
}

/// Generic Telegram API response (e.g. for `setWebhook`).
#[derive(Debug, Deserialize)]
struct TgGenericResponse {
    ok: bool,
    description: Option<String>,
}

// ---------------------------------------------------------------------------
// Per-adapter state
// ---------------------------------------------------------------------------

type AdapterRegistry = Arc<RwLock<HashMap<String, AdapterState>>>;

struct AdapterState {
    bot_token: String,
    chat_id: String,
    pipe_id: String,
}

static ADAPTER_REGISTRY: std::sync::OnceLock<AdapterRegistry> = std::sync::OnceLock::new();

fn registry() -> &'static AdapterRegistry {
    ADAPTER_REGISTRY.get_or_init(|| Arc::new(RwLock::new(HashMap::new())))
}

// ---------------------------------------------------------------------------
// Internal config struct
// ---------------------------------------------------------------------------

struct TgConfig {
    id: String,
    name: String,
    bot_token: String,
    chat_id: String,
    pipe_id: String,
    inbound_token: String,
}

// ---------------------------------------------------------------------------
// Public: Axum router
// ---------------------------------------------------------------------------

/// Returns the Axum router for the Telegram webhook endpoint.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/telegram/webhook/{config_id}", post(webhook_handler))
}

// ---------------------------------------------------------------------------
// Public: startup
// ---------------------------------------------------------------------------

/// Called from main.rs after AppState is built.
///
/// For each active Telegram config:
///   1. Registers its `TelegramSender` on the `ChannelRegistry`
///   2. Sets the webhook URL with Telegram via `setWebhook`
pub async fn init_all(state: AppState) {
    let configs = match load_active_configs(&state).await {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to load Telegram configs: {e}");
            return;
        }
    };

    if configs.is_empty() {
        info!("No active Telegram adapters configured");
        return;
    }

    info!("Initialising {} Telegram adapter(s)", configs.len());
    let base_url = state.config.external_base_url();

    for cfg in configs {
        register_adapter(&state, &cfg, &base_url).await;
    }
}

async fn register_adapter(state: &AppState, cfg: &TgConfig, base_url: &str) {
    // Store in module-level registry
    {
        let mut reg = registry().write().await;
        reg.insert(cfg.id.clone(), AdapterState {
            bot_token: cfg.bot_token.clone(),
            chat_id: cfg.chat_id.clone(),
            pipe_id: cfg.pipe_id.clone(),
        });
    }

    // Register ChannelSender
    let sender = Arc::new(TelegramSender {
        http: Client::new(),
        bot_token: cfg.bot_token.clone(),
        chat_id: cfg.chat_id.clone(),
    });
    state.channel_registry.register(&cfg.pipe_id, sender).await;

    // Set webhook with Telegram
    let webhook_url = format!(
        "{}/api/telegram/webhook/{}?token={}",
        base_url, cfg.id, cfg.inbound_token
    );

    info!(
        config_id = %cfg.id,
        name = %cfg.name,
        webhook_url = %webhook_url,
        "Registering webhook with Telegram"
    );

    match set_webhook(&cfg.bot_token, &webhook_url).await {
        Ok(()) => {
            info!(config_id = %cfg.id, "Webhook registered with Telegram");
        }
        Err(e) => {
            warn!(config_id = %cfg.id, "Failed to register webhook with Telegram: {e}. Will retry on next startup.");
        }
    }
}

/// Register a single adapter by config ID.
/// Called after creating a new telegram_configs row so the adapter
/// is ready immediately without restart.
pub async fn start_one(state: AppState, config_id: &str) -> Result<()> {
    let cfg = load_config_by_id(&state, config_id).await?;
    let base_url = state.config.external_base_url();
    register_adapter(&state, &cfg, &base_url).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Webhook handler
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WebhookQuery {
    token: Option<String>,
}

/// Axum handler: receives webhook POSTs from Telegram.
async fn webhook_handler(
    State(state): State<AppState>,
    Path(config_id): Path<String>,
    Query(query): Query<WebhookQuery>,
    axum::Json(update): axum::Json<TgUpdate>,
) -> impl IntoResponse {
    debug!(
        config_id = %config_id,
        update_id = update.update_id,
        "Telegram webhook received"
    );

    // 1. Authenticate via token
    let provided_token = match query.token {
        Some(t) => t,
        None => {
            warn!(config_id = %config_id, "Telegram webhook REJECTED: no token provided");
            return StatusCode::UNAUTHORIZED;
        }
    };

    let pipe_token = sqlx::query!(
        "SELECT p.inbound_token
         FROM telegram_configs tc
         JOIN pipes p ON p.id = tc.pipe_id
         WHERE tc.id = ? AND tc.active = 1",
        config_id
    )
    .fetch_optional(&state.db)
    .await;

    let expected_token = match pipe_token {
        Ok(Some(row)) => row.inbound_token,
        Ok(None) => {
            warn!(config_id = %config_id, "Telegram webhook REJECTED: config not found or inactive");
            return StatusCode::NOT_FOUND;
        }
        Err(e) => {
            error!(config_id = %config_id, error = %e, "Telegram webhook REJECTED: DB error");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    };

    if provided_token != expected_token {
        warn!(config_id = %config_id, "Telegram webhook REJECTED: token mismatch");
        return StatusCode::UNAUTHORIZED;
    }

    // 2. Extract the message (new or edited)
    let msg = match update.message.or(update.edited_message) {
        Some(m) => m,
        None => {
            debug!(config_id = %config_id, "Telegram webhook SKIPPED: no message in update");
            return StatusCode::OK;
        }
    };

    // 3. Look up adapter state
    let reg = registry().read().await;
    let adapter = match reg.get(&config_id) {
        Some(a) => a,
        None => {
            warn!(config_id = %config_id, "Telegram webhook REJECTED: config not in adapter registry");
            return StatusCode::NOT_FOUND;
        }
    };

    // 4. Filter by chat_id — only process messages from the configured chat
    let msg_chat_id = msg.chat.id.to_string();
    if msg_chat_id != adapter.chat_id {
        debug!(
            config_id = %config_id,
            expected = %adapter.chat_id,
            got = %msg_chat_id,
            "Telegram webhook SKIPPED: wrong chat"
        );
        return StatusCode::OK;
    }

    // 5. Skip messages from bots (loop prevention)
    let from_user = match &msg.from {
        Some(u) => u,
        None => {
            debug!(config_id = %config_id, "Telegram webhook SKIPPED: no sender info");
            return StatusCode::OK;
        }
    };

    // 6. Skip empty messages
    let text = match &msg.text {
        Some(t) if !t.is_empty() => t.clone(),
        _ => {
            debug!(config_id = %config_id, "Telegram webhook SKIPPED: empty or non-text message");
            return StatusCode::OK;
        }
    };

    // Telegram user ID as sender_ref
    let sender_ref = from_user.id.to_string();
    let message_id = msg.message_id;
    let reply_to_message_id = msg.reply_to_message.as_ref().map(|r| r.message_id);

    debug!(
        config_id = %config_id,
        message_id = message_id,
        sender = %sender_ref,
        text_len = text.len(),
        "Telegram webhook inbound message"
    );

    // 7. Resolve sender
    let pipe_id = adapter.pipe_id.clone();
    let resolved_user_id = match resolve_sender(&state, &pipe_id, &sender_ref).await {
        Some(uid) => uid,
        None => {
            debug!(
                config_id = %config_id,
                sender = %sender_ref,
                "Telegram webhook SKIPPED: unknown sender"
            );
            return StatusCode::OK;
        }
    };

    info!(
        sender = %sender_ref,
        user_id = %resolved_user_id,
        pipe_id = %pipe_id,
        "Processing Telegram webhook message"
    );

    // 8. Dispatch in background (return 200 immediately)
    let bot_token = adapter.bot_token.clone();
    let chat_id = adapter.chat_id.clone();
    let state_clone = state.clone();

    tokio::spawn(async move {
        dispatch_message(
            state_clone,
            pipe_id,
            resolved_user_id,
            sender_ref,
            text,
            message_id,
            reply_to_message_id,
            bot_token,
            chat_id,
            Client::new(),
        ).await;
    });

    StatusCode::OK
}

// ---------------------------------------------------------------------------
// Telegram API helpers
// ---------------------------------------------------------------------------

/// Set the webhook URL for the bot via Telegram Bot API.
async fn set_webhook(bot_token: &str, webhook_url: &str) -> Result<()> {
    let http = Client::new();
    let token = bot_token.trim();
    let url = format!(
        "https://api.telegram.org/bot{}/setWebhook",
        token
    );

    let body = serde_json::json!({
        "url": webhook_url,
        "allowed_updates": ["message", "edited_message"],
    });

    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("Failed to reach Telegram API for setWebhook")?;

    let resp_body = resp.text().await.unwrap_or_default();
    debug!(response = %resp_body, "Telegram setWebhook response");

    let tg_resp: TgGenericResponse = serde_json::from_str(&resp_body)
        .context("Failed to parse Telegram setWebhook response")?;

    if !tg_resp.ok {
        anyhow::bail!(
            "Telegram rejected setWebhook: {}",
            tg_resp.description.unwrap_or_default()
        );
    }

    Ok(())
}

/// Remove the webhook for the bot.
pub async fn delete_webhook(bot_token: &str) -> Result<()> {
    let http = Client::new();
    let token = bot_token.trim();
    let url = format!(
        "https://api.telegram.org/bot{}/deleteWebhook",
        token
    );

    let resp = http
        .post(&url)
        .json(&serde_json::json!({ "drop_pending_updates": false }))
        .send()
        .await
        .context("Failed to reach Telegram API for deleteWebhook")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        warn!("Telegram deleteWebhook returned {status}: {body}");
    } else {
        info!("Webhook deleted from Telegram");
    }

    Ok(())
}

/// Send a text message via the Telegram Bot API.
/// Returns the sent message_id on success.
async fn send_telegram_message(
    http: &Client,
    bot_token: &str,
    chat_id: &str,
    text: &str,
    reply_to_message_id: Option<i64>,
) -> Option<i64> {
    let url = format!(
        "https://api.telegram.org/bot{}/sendMessage",
        bot_token.trim()
    );

    let body = TgSendMessage {
        chat_id: chat_id.to_string(),
        text: text.to_string(),
        reply_to_message_id,
        parse_mode: Some("Markdown".to_string()),
    };

    let resp = http.post(&url).json(&body).send().await;

    match resp {
        Ok(r) if r.status().is_success() => {
            if let Ok(tg_resp) = r.json::<TgApiResponse>().await {
                if tg_resp.ok {
                    let msg_id = tg_resp.result.map(|r| r.message_id);
                    if let Some(id) = msg_id {
                        info!(message_id = id, "Sent via Telegram");
                    }
                    return msg_id;
                }
            }
            None
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            // If Markdown parse fails, retry without parse_mode
            if body.contains("can't parse") {
                debug!("Markdown parse failed, retrying as plain text");
                return send_telegram_plain(http, bot_token, chat_id, text, reply_to_message_id).await;
            }
            error!(%status, %body, "Telegram rejected sendMessage");
            None
        }
        Err(e) => {
            error!("Failed to reach Telegram API: {e}");
            None
        }
    }
}

/// Fallback: send as plain text when Markdown parsing fails.
async fn send_telegram_plain(
    http: &Client,
    bot_token: &str,
    chat_id: &str,
    text: &str,
    reply_to_message_id: Option<i64>,
) -> Option<i64> {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token.trim());

    let body = TgSendMessage {
        chat_id: chat_id.to_string(),
        text: text.to_string(),
        reply_to_message_id,
        parse_mode: None,
    };

    match http.post(&url).json(&body).send().await {
        Ok(r) if r.status().is_success() => {
            r.json::<TgApiResponse>().await.ok()
                .and_then(|resp| resp.result.map(|r| r.message_id))
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            error!(%status, %body, "Telegram rejected sendMessage (plain fallback)");
            None
        }
        Err(e) => {
            error!("Failed to reach Telegram API (plain fallback): {e}");
            None
        }
    }
}

/// Send a streaming draft message via Telegram Bot API `sendMessageDraft`.
/// This shows a "typing draft" preview to the user while the agent is
/// generating its response.
///
/// - First call: `draft_message_id = None` → Telegram returns a draft ID.
/// - Subsequent calls: pass the returned draft ID to update the same draft.
///
/// Returns the draft_message_id for continuation.
#[allow(dead_code)]
async fn send_message_draft(
    http: &Client,
    bot_token: &str,
    chat_id: &str,
    text: &str,
    draft_message_id: Option<i64>,
    reply_to_message_id: Option<i64>,
) -> Option<i64> {
    let url = format!(
        "https://api.telegram.org/bot{}/sendMessageDraft",
        bot_token.trim()
    );

    let body = TgSendMessageDraft {
        business_connection_id: None,
        chat_id: chat_id.to_string(),
        text: text.to_string(),
        draft_message_id,
        reply_to_message_id,
    };

    match http.post(&url).json(&body).send().await {
        Ok(r) if r.status().is_success() => {
            r.json::<TgApiResponse>().await.ok()
                .and_then(|resp| resp.result.map(|r| r.message_id))
        }
        Ok(r) => {
            let status = r.status();
            let body_text = r.text().await.unwrap_or_default();
            debug!(%status, %body_text, "sendMessageDraft not available (Bot API 9.3+ required)");
            None
        }
        Err(e) => {
            debug!("sendMessageDraft request failed: {e}");
            None
        }
    }
}

/// Send the "typing" action to let the user know the bot is processing.
async fn send_typing(http: &Client, bot_token: &str, chat_id: &str) {
    let url = format!(
        "https://api.telegram.org/bot{}/sendChatAction",
        bot_token.trim()
    );

    let body = TgChatAction {
        chat_id: chat_id.to_string(),
        action: "typing".to_string(),
    };

    match http.post(&url).json(&body).send().await {
        Ok(r) if r.status().is_success() => {
            debug!("Typing indicator sent");
        }
        Ok(r) => {
            debug!(status = %r.status(), "Failed to send typing indicator");
        }
        Err(e) => {
            debug!("Typing indicator request failed: {e}");
        }
    }
}

/// Verify a bot token with Telegram by calling `getMe`.
/// Returns the bot's username on success, or an error message.
pub async fn verify_bot_token(bot_token: &str) -> Result<String> {
    let http = Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    let token = bot_token.trim();
    if token.is_empty() {
        anyhow::bail!("Bot token is empty.");
    }

    let url = format!("https://api.telegram.org/bot{}/getMe", token);

    let resp = http.get(&url).send().await
        .context("Failed to reach Telegram API")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.as_u16() == 401 || status.as_u16() == 404 {
            anyhow::bail!("Invalid bot token. Double-check the token you got from @BotFather — it should look like 123456:ABC-DEF1234ghIkl-zyx57W2v.");
        }
        anyhow::bail!("Telegram API error {status}: {body}");
    }

    #[derive(Deserialize)]
    struct GetMeResponse {
        ok: bool,
        result: Option<GetMeResult>,
    }
    #[derive(Deserialize)]
    struct GetMeResult {
        username: Option<String>,
        first_name: String,
    }

    let body = resp.json::<GetMeResponse>().await
        .context("Failed to parse Telegram getMe response")?;

    if !body.ok {
        anyhow::bail!("Telegram returned an error for getMe");
    }

    let result = body.result.context("No result in getMe response")?;
    Ok(result.username.unwrap_or(result.first_name))
}

/// Fetch recent updates to discover the chat_id.
/// Called from the setup proxy to let the user pick a chat.
pub async fn get_recent_chats(bot_token: &str) -> Result<Vec<TgRecentChat>> {
    let http = Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    let token = bot_token.trim();
    let url = format!("https://api.telegram.org/bot{}/getUpdates", token);

    let body = serde_json::json!({
        "limit": 100,
        "timeout": 0,
    });

    let resp = http.post(&url).json(&body).send().await
        .context("Failed to reach Telegram API for getUpdates")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Telegram getUpdates returned {status}: {body}");
    }

    #[derive(Deserialize)]
    struct UpdatesResponse {
        ok: bool,
        result: Option<Vec<TgUpdate>>,
    }

    let data = resp.json::<UpdatesResponse>().await
        .context("Failed to parse Telegram getUpdates response")?;

    if !data.ok {
        anyhow::bail!("Telegram returned an error for getUpdates");
    }

    let updates = data.result.unwrap_or_default();

    // Extract unique chats from updates
    let mut seen = std::collections::HashSet::new();
    let mut chats = Vec::new();

    for update in updates {
        let msg = update.message.or(update.edited_message);
        if let Some(m) = msg {
            let chat_id = m.chat.id.to_string();
            if seen.insert(chat_id.clone()) {
                let display_name = m.chat.title.clone().unwrap_or_else(|| {
                    if let Some(ref user) = m.from {
                        let mut name = user.first_name.clone();
                        if let Some(ref last) = user.last_name {
                            name.push(' ');
                            name.push_str(last);
                        }
                        name
                    } else {
                        "Unknown".to_string()
                    }
                });

                let last_message = m.text.unwrap_or_default();
                let last_message = if last_message.len() > 80 {
                    format!("{}...", &last_message[..77])
                } else {
                    last_message
                };

                chats.push(TgRecentChat {
                    chat_id,
                    display_name,
                    chat_type: m.chat.chat_type,
                    last_message,
                });
            }
        }
    }

    Ok(chats)
}

/// A recent chat discovered via getUpdates.
#[derive(Debug, Serialize)]
pub struct TgRecentChat {
    pub chat_id: String,
    pub display_name: String,
    pub chat_type: String,
    pub last_message: String,
}

// ---------------------------------------------------------------------------
// Sender resolution
// ---------------------------------------------------------------------------

async fn resolve_sender(state: &AppState, pipe_id: &str, sender_ref: &str) -> Option<String> {
    // Check specific mapping
    let mapped = sqlx::query!(
        "SELECT user_id FROM user_sender_refs WHERE pipe_id = ? AND sender_ref = ?",
        pipe_id,
        sender_ref,
    )
    .fetch_optional(&state.db)
    .await
    .ok()?;

    if let Some(row) = mapped {
        return Some(row.user_id);
    }

    // Check if there are any mappings for this pipe
    let has_mappings = sqlx::query!(
        "SELECT COUNT(*) as cnt FROM user_sender_refs WHERE pipe_id = ?",
        pipe_id
    )
    .fetch_one(&state.db)
    .await
    .ok()
    .map(|r| r.cnt > 0)
    .unwrap_or(false);

    if has_mappings {
        None
    } else {
        // No mappings → fall back to pipe owner
        sqlx::query!("SELECT user_id FROM pipes WHERE id = ?", pipe_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .map(|r| r.user_id)
    }
}

// ---------------------------------------------------------------------------
// Message dispatch
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn dispatch_message(
    state: AppState,
    pipe_id: String,
    user_id: String,
    _sender_ref: String,
    text: String,
    message_id: i64,
    reply_to_message_id: Option<i64>,
    bot_token: String,
    chat_id: String,
    http: Client,
) {
    // Route to agent
    let agents_snapshot = state.agents.load();
    let default_agent_id = sqlx::query!(
        "SELECT default_agent_id FROM pipes WHERE id = ?", pipe_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .and_then(|r| r.default_agent_id);

    let (agent_id, effective_text) = agent_router::route(
        &text,
        default_agent_id.as_deref(),
        &agents_snapshot,
    );

    // Thread resolution
    let origin_ref = Some(message_id.to_string());
    let reply_ref = reply_to_message_id.map(|id| id.to_string());

    let thread = match thread_mgr::find_or_create_thread(
        &state.db,
        &pipe_id,
        &user_id,
        &agent_id,
        origin_ref.as_deref(),
        reply_ref.as_deref(),
    ).await {
        Ok(t) => t,
        Err(e) => {
            error!("Failed to find/create thread: {e}");
            return;
        }
    };

    let thread_id = thread.id.to_string();

    // Approval interception
    if approval_queue::try_resolve_pending(&state, &thread_id).await {
        info!(thread_id = %thread_id, "Message consumed as tool approval response (Telegram)");
        let lang = crate::i18n::user_language(&state.db, &user_id).await;
        let ack_text = crate::i18n::t(&lang, "approval.approved_processing");
        let _ = send_telegram_message(
            &http, &bot_token, &chat_id, ack_text,
            Some(message_id),
        ).await;
        return;
    }

    // Show typing indicator
    send_typing(&http, &bot_token, &chat_id).await;

    let params = TurnParams {
        pipe_id: pipe_id.clone(),
        user_id: user_id.clone(),
        agent_id: Some(agent_id),
        message: effective_text,
        thread_id: Some(thread_id.clone()),
        chain_depth: 0,
        channel_kind: ChannelKind::ThreadedNonInteractive {
            pipe_id: pipe_id.clone(),
            thread_id: thread_id.clone(),
        },
    };

    let reply = run_turn(&state, params, noop_sender()).await;

    let reply_text = match reply {
        Ok(t) if !t.is_empty() => t,
        Ok(_) => return,
        Err(e) => {
            error!("Agent turn failed: {e}");
            let _ = thread_mgr::fail_thread(&state.db, &thread_id).await;
            let lang = crate::i18n::user_language(&state.db, &user_id).await;
            let error_text = crate::i18n::t(&lang, "error.agent_turn_failed");
            let _ = send_telegram_message(
                &http, &bot_token, &chat_id, error_text,
                Some(message_id),
            ).await;
            return;
        }
    };

    // Send reply, replying to the original message
    let sent_id = send_telegram_message(
        &http,
        &bot_token,
        &chat_id,
        &reply_text,
        Some(message_id),
    ).await;

    // Store the sent message ID for thread matching
    if let Some(id) = sent_id {
        let guid = id.to_string();
        let _ = thread_mgr::update_reply_guid(&state.db, &thread_id, &pipe_id, &guid).await;
    }
}

// ---------------------------------------------------------------------------
// ChannelSender implementation
// ---------------------------------------------------------------------------

pub struct TelegramSender {
    http: Client,
    bot_token: String,
    chat_id: String,
}

#[async_trait::async_trait]
impl crate::channels::ChannelSender for TelegramSender {
    async fn send_message(
        &self,
        _thread_id: &str,
        text: &str,
        reply_to_guid: Option<&str>,
    ) -> Result<Option<String>> {
        let reply_to_id = reply_to_guid
            .and_then(|s| s.parse::<i64>().ok());

        let msg_id = send_telegram_message(
            &self.http,
            &self.bot_token,
            &self.chat_id,
            text,
            reply_to_id,
        ).await;

        Ok(msg_id.map(|id| id.to_string()))
    }
}

// ---------------------------------------------------------------------------
// DB helpers
// ---------------------------------------------------------------------------

async fn load_active_configs(state: &AppState) -> Result<Vec<TgConfig>> {
    let rows = sqlx::query!(
        "SELECT tc.id, tc.name, tc.bot_token, tc.chat_id,
                tc.pipe_id, p.inbound_token
         FROM telegram_configs tc
         JOIN pipes p ON p.id = tc.pipe_id
         WHERE tc.active = 1"
    )
    .fetch_all(&state.db)
    .await
    .context("Failed to load telegram_configs")?;

    Ok(rows.into_iter().map(|r| TgConfig {
        id: r.id.unwrap_or_default(),
        name: r.name,
        bot_token: r.bot_token.trim().to_string(),
        chat_id: r.chat_id.trim().to_string(),
        pipe_id: r.pipe_id,
        inbound_token: r.inbound_token,
    }).collect())
}

async fn load_config_by_id(state: &AppState, config_id: &str) -> Result<TgConfig> {
    let row = sqlx::query!(
        "SELECT tc.id, tc.name, tc.bot_token, tc.chat_id,
                tc.pipe_id, p.inbound_token
         FROM telegram_configs tc
         JOIN pipes p ON p.id = tc.pipe_id
         WHERE tc.id = ? AND tc.active = 1",
        config_id
    )
    .fetch_optional(&state.db)
    .await
    .context("Failed to load Telegram config")?;

    let row = row.ok_or_else(|| anyhow::anyhow!("Telegram config not found or inactive: {config_id}"))?;

    Ok(TgConfig {
        id: row.id.unwrap_or_default(),
        name: row.name,
        bot_token: row.bot_token.trim().to_string(),
        chat_id: row.chat_id.trim().to_string(),
        pipe_id: row.pipe_id,
        inbound_token: row.inbound_token,
    })
}
