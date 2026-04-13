/// Integrated BlueBubbles adapter.
///
/// Instead of polling, this module receives inbound messages via webhooks
/// that BlueBubbles POSTs to a Selu endpoint. It:
///   1. Exposes a webhook endpoint (`POST /api/bb/webhook/{config_id}`)
///   2. Registers the webhook URL with BlueBubbles on setup
///   3. Dispatches inbound messages through the standard pipe inbound flow
///      (with sender_ref resolution and thread creation)
///   4. Handles outbound replies with reply-to-message support
///
/// On startup, existing configs that lack a `bb_webhook_id` are automatically
/// upgraded by registering the webhook with BlueBubbles (seamless migration
/// from the old polling approach).
use anyhow::{Context as _, Result, anyhow};
use axum::{
    Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::agents::{
    engine::{ChannelKind, TurnParams, noop_sender, run_turn},
    router as agent_router, thread as thread_mgr,
};
use crate::permissions::approval_queue;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// BlueBubbles API types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BbMessage {
    text: Option<String>,
    is_from_me: bool,
    handle: Option<BbHandle>,
    guid: Option<String>,
    date_created: Option<i64>,
    /// The GUID of the first message in this thread (the "thread originator").
    /// Set when the message is part of an iMessage reply chain.
    /// Note: BB API field is "threadOriginatorGuid" (not "threadOriginGuid").
    thread_originator_guid: Option<String>,
    /// The GUID of the specific message this is a direct reply to.
    /// Different from thread_originator_guid: the originator points to the
    /// *first* message in the thread, while reply_to_guid points to the
    /// *specific* message being replied to.
    reply_to_guid: Option<String>,
    /// Associated message GUID (for tapbacks/reactions, not regular replies).
    #[allow(dead_code)]
    associated_message_guid: Option<String>,
    /// The type of association (e.g. tapback type).
    #[allow(dead_code)]
    associated_message_type: Option<String>,
    /// Chat(s) this message belongs to. Used to route webhook messages to
    /// the correct adapter config (BB sends all events to all webhooks).
    #[serde(default)]
    chats: Vec<BbChat>,
    #[serde(default)]
    attachments: Vec<BbAttachment>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BbHandle {
    address: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BbChat {
    guid: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct BbAttachment {
    #[serde(default)]
    guid: Option<String>,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    transfer_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    download_url: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    data_base64: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BbSendTextRequest {
    chat_guid: String,
    message: String,
    method: String,
    temp_guid: String,
    /// If set, BlueBubbles will send this as a reply to the specified message.
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_message_guid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BbSendResponse {
    #[allow(dead_code)]
    status: u16,
    data: Option<BbSendResponseData>,
}

#[derive(Debug, Deserialize)]
struct BbSendResponseData {
    guid: Option<String>,
}

/// BlueBubbles webhook registration response.
#[derive(Debug, Deserialize)]
struct BbWebhookResponse {
    #[allow(dead_code)]
    status: u16,
    data: Option<BbWebhookData>,
}

#[derive(Debug, Deserialize)]
struct BbWebhookData {
    id: Option<i64>,
}

/// Payload that BlueBubbles POSTs to our webhook endpoint.
/// The outer envelope wraps the event type and the message data.
#[derive(Debug, Deserialize)]
struct BbWebhookPayload {
    #[serde(rename = "type")]
    event_type: String,
    data: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Per-adapter state (shared across webhook calls)
// ---------------------------------------------------------------------------

/// Tracks sent message GUIDs for loop prevention (per adapter instance).
type SentGuids = Arc<RwLock<HashSet<String>>>;

/// Shared registry of per-config state, keyed by config_id.
/// Lives in AppState-adjacent module-level storage so the webhook handler
/// can look up sent_guids and config details without hitting the DB every time.
type AdapterRegistry = Arc<RwLock<HashMap<String, AdapterState>>>;

struct AdapterState {
    server_url: String,
    server_password: String,
    chat_guid: String,
    pipe_id: String,
    sent_guids: SentGuids,
}

/// Module-level registry. Initialized once at startup.
static ADAPTER_REGISTRY: std::sync::OnceLock<AdapterRegistry> = std::sync::OnceLock::new();

fn registry() -> &'static AdapterRegistry {
    ADAPTER_REGISTRY.get_or_init(|| Arc::new(RwLock::new(HashMap::new())))
}

// ---------------------------------------------------------------------------
// Internal config struct (loaded from DB)
// ---------------------------------------------------------------------------

struct BbConfig {
    id: String,
    name: String,
    server_url: String,
    server_password: String,
    chat_guid: String,
    pipe_id: String,
    bb_webhook_id: Option<String>,
    inbound_token: String,
}

// ---------------------------------------------------------------------------
// Public: Axum router for the webhook endpoint
// ---------------------------------------------------------------------------

/// Returns the Axum router that serves the BB webhook inbound endpoint.
/// Merged into the main app in main.rs.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/bb/webhook/{config_id}", post(webhook_handler))
}

// ---------------------------------------------------------------------------
// Public: startup — register senders + auto-upgrade existing configs
// ---------------------------------------------------------------------------

/// Called from main.rs after AppState is built.
///
/// For each active BlueBubbles config:
///   1. Registers its `BlueBubblesSender` on the `ChannelRegistry`
///   2. If `bb_webhook_id` is NULL (legacy polling config or failed registration),
///      automatically registers the webhook with BlueBubbles
pub async fn init_all(state: AppState) {
    let configs = match load_active_configs(&state).await {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to load BlueBubbles configs: {e}");
            return;
        }
    };

    if configs.is_empty() {
        info!("No active BlueBubbles adapters configured");
        return;
    }

    info!("Initialising {} BlueBubbles adapter(s)", configs.len());
    let base_url = state.public_base_url();

    for cfg in configs {
        register_adapter(&state, &cfg, &base_url).await;
    }
}

/// Register a single adapter: set up channel sender + ensure webhook is
/// registered with BlueBubbles.
async fn register_adapter(state: &AppState, cfg: &BbConfig, base_url: &str) {
    let sent_guids: SentGuids = Arc::new(RwLock::new(HashSet::new()));

    // Store in module-level registry for the webhook handler
    {
        let mut reg = registry().write().await;
        reg.insert(
            cfg.id.clone(),
            AdapterState {
                server_url: cfg.server_url.clone(),
                server_password: cfg.server_password.clone(),
                chat_guid: cfg.chat_guid.clone(),
                pipe_id: cfg.pipe_id.clone(),
                sent_guids: sent_guids.clone(),
            },
        );
    }

    // Register ChannelSender for approval queue / outbound
    let sender = Arc::new(BlueBubblesSender {
        http: Client::new(),
        server_url: cfg.server_url.clone(),
        server_password: cfg.server_password.clone(),
        chat_guid: cfg.chat_guid.clone(),
        sent_guids: sent_guids.clone(),
    });

    let pipe_id = cfg.pipe_id.clone();
    state.channel_registry.register(&pipe_id, sender).await;

    // Ensure webhook is registered with BlueBubbles.
    // If we already have a webhook ID, delete and re-register to make sure
    // the event subscription is correct (handles upgrades from older formats).
    let callback_url = format!(
        "{}/api/bb/webhook/{}?token={}",
        base_url, cfg.id, cfg.inbound_token
    );

    // Clean up any stale webhook first
    if let Some(ref old_id) = cfg.bb_webhook_id {
        debug!(config_id = %cfg.id, old_webhook_id = %old_id, "Deregistering old webhook before re-registration");
        match deregister_webhook_from_bb(&cfg.server_url, &cfg.server_password, old_id).await {
            Ok(()) => {
                debug!(config_id = %cfg.id, old_webhook_id = %old_id, "Old webhook deregistered")
            }
            Err(e) => {
                warn!(config_id = %cfg.id, old_webhook_id = %old_id, error = %e, "Failed to deregister old webhook (continuing anyway)")
            }
        }
    } else {
        debug!(config_id = %cfg.id, "No previous webhook ID stored — registering fresh");
    }

    info!(
        config_id = %cfg.id,
        name = %cfg.name,
        callback_url = %callback_url,
        "Registering webhook with BlueBubbles"
    );
    match register_webhook_with_bb(&cfg.server_url, &cfg.server_password, &callback_url).await {
        Ok(webhook_id) => {
            let wh_id_str = webhook_id.to_string();
            if let Err(e) = sqlx::query!(
                "UPDATE bluebubbles_configs SET bb_webhook_id = ? WHERE id = ?",
                wh_id_str,
                cfg.id,
            )
            .execute(&state.db)
            .await
            {
                error!(config_id = %cfg.id, "Failed to store bb_webhook_id: {e}");
            }
            info!(config_id = %cfg.id, webhook_id = webhook_id, "Webhook registered with BlueBubbles");
        }
        Err(e) => {
            warn!(config_id = %cfg.id, "Failed to register webhook with BlueBubbles: {e}. Will retry on next startup.");
        }
    }
}

/// Register a single adapter by config ID.
///
/// Called after inserting a new `bluebubbles_configs` row so the adapter
/// is ready immediately — no restart required.
pub async fn start_one(state: AppState, config_id: &str) -> Result<()> {
    let cfg = load_config_by_id(&state, config_id).await?;
    let base_url = state.public_base_url();
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

/// Axum handler: receives webhook POSTs from BlueBubbles.
async fn webhook_handler(
    State(state): State<AppState>,
    Path(config_id): Path<String>,
    Query(query): Query<WebhookQuery>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let body_str = String::from_utf8_lossy(&body);
    debug!(
        config_id = %config_id,
        body_len = body.len(),
        "BB webhook received"
    );

    // 1. Authenticate via token
    let provided_token = match query.token {
        Some(t) => t,
        None => {
            warn!(config_id = %config_id, "BB webhook REJECTED: no token provided");
            return StatusCode::UNAUTHORIZED;
        }
    };

    // Look up the pipe's inbound_token for this config
    let pipe_token = sqlx::query!(
        "SELECT p.inbound_token
         FROM bluebubbles_configs bc
         JOIN pipes p ON p.id = bc.pipe_id
         WHERE bc.id = ? AND bc.active = 1",
        config_id
    )
    .fetch_optional(&state.db)
    .await;

    let expected_token = match pipe_token {
        Ok(Some(row)) => row.inbound_token,
        Ok(None) => {
            warn!(config_id = %config_id, "BB webhook REJECTED: config not found or inactive");
            return StatusCode::NOT_FOUND;
        }
        Err(e) => {
            error!(config_id = %config_id, error = %e, "BB webhook REJECTED: DB error");
            return StatusCode::NOT_FOUND;
        }
    };

    if provided_token != expected_token {
        warn!(config_id = %config_id, "BB webhook REJECTED: token mismatch");
        return StatusCode::UNAUTHORIZED;
    }

    // 2. Parse the webhook envelope from raw body
    let payload: BbWebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                config_id = %config_id,
                error = %e,
                body = %body_str,
                "BB webhook REJECTED: failed to parse payload"
            );
            return StatusCode::OK;
        }
    };

    // 3. Only process new-message events
    if payload.event_type != "new-message" {
        debug!(
            config_id = %config_id,
            event_type = %payload.event_type,
            "BB webhook SKIPPED: not a new-message event"
        );
        return StatusCode::OK;
    }

    // 4. Parse the message from the payload data
    let msg: BbMessage = match serde_json::from_value(payload.data) {
        Ok(m) => m,
        Err(e) => {
            warn!(
                config_id = %config_id,
                error = %e,
                "BB webhook REJECTED: failed to parse message data"
            );
            return StatusCode::OK;
        }
    };

    let msg_guid_display = msg.guid.clone().unwrap_or_else(|| "<no-guid>".into());

    // 5. Skip messages we sent ourselves (is_from_me).
    if msg.is_from_me {
        debug!(
            config_id = %config_id,
            guid = %msg_guid_display,
            "Skipping is_from_me message"
        );
        return StatusCode::OK;
    }

    // 6. Skip old messages. BB fires webhooks for messages it "catches up" on
    //    at startup or reconnect (e.g. old unread messages).
    if let Some(date_created) = msg.date_created {
        let now_ms = current_epoch_ms();
        let age_ms = now_ms - date_created;
        debug!(
            config_id = %config_id,
            guid = %msg_guid_display,
            age_ms = age_ms,
            "BB webhook: message age check"
        );
        if age_ms > 60_000 {
            debug!(
                config_id = %config_id,
                guid = %msg_guid_display,
                age_secs = age_ms / 1000,
                "BB webhook SKIPPED: message too old (>60s)"
            );
            return StatusCode::OK;
        }
    } else {
        warn!(
            config_id = %config_id,
            guid = %msg_guid_display,
            "BB webhook: no dateCreated on message"
        );
    }

    // 7. Look up adapter state from registry
    let reg = registry().read().await;
    let adapter = match reg.get(&config_id) {
        Some(a) => a,
        None => {
            warn!(config_id = %config_id, "BB webhook REJECTED: config not in adapter registry");
            return StatusCode::NOT_FOUND;
        }
    };

    // 8. BB sends all events to all registered webhooks. Filter by chat_guid.
    let msg_chat_guids: Vec<String> = msg.chats.iter().filter_map(|c| c.guid.clone()).collect();
    let msg_belongs_to_chat = msg_chat_guids.iter().any(|g| g == &adapter.chat_guid);

    debug!(
        config_id = %config_id,
        guid = %msg_guid_display,
        expected_chat = %adapter.chat_guid,
        belongs = msg_belongs_to_chat,
        "BB webhook: chat filter check"
    );

    if !msg_belongs_to_chat && !msg.chats.is_empty() {
        debug!(
            config_id = %config_id,
            expected_chat = %adapter.chat_guid,
            "Skipping message for different chat"
        );
        return StatusCode::OK;
    }

    // 9. Skip messages we sent ourselves (GUID-based loop prevention)
    if let Some(ref guid) = msg.guid {
        let sent = adapter.sent_guids.read().await;
        if sent.contains(guid) {
            debug!(
                config_id = %config_id,
                guid = %guid,
                "BB webhook SKIPPED: GUID in sent_guids (loop prevention)"
            );
            return StatusCode::OK;
        }
    }

    // 10. Extract text + inbound image attachments
    let text = msg.text.clone().unwrap_or_default();
    let inbound_attachments =
        collect_bb_inbound_attachments(&msg, &adapter.server_url, &adapter.server_password);
    if text.trim().is_empty() && inbound_attachments.is_empty() {
        debug!(
            config_id = %config_id,
            guid = %msg_guid_display,
            "BB webhook SKIPPED: empty message (no text and no image attachments)"
        );
        return StatusCode::OK;
    }

    let sender_ref = msg
        .handle
        .as_ref()
        .map(|h| h.address.clone())
        .unwrap_or_else(|| "self".into());

    let message_guid = msg.guid.clone();
    let thread_originator_guid = msg.thread_originator_guid.clone();
    let reply_to_guid = msg.reply_to_guid.clone();

    debug!(
        config_id = %config_id,
        guid = %msg_guid_display,
        sender = %sender_ref,
        thread_originator_guid = ?thread_originator_guid,
        reply_to_guid = ?reply_to_guid,
        text_len = text.len(),
        text_preview = %if text.len() > 80 { &text[..80] } else { &text },
        "BB webhook inbound message"
    );

    // 11. Resolve sender
    let pipe_id = adapter.pipe_id.clone();
    let resolved_user_id = match resolve_sender(&state, &pipe_id, &sender_ref).await {
        Some(uid) => uid,
        None => {
            debug!(
                config_id = %config_id,
                sender = %sender_ref,
                pipe_id = %pipe_id,
                "BB webhook SKIPPED: unknown sender"
            );
            return StatusCode::OK;
        }
    };

    info!(
        sender = %sender_ref,
        user_id = %resolved_user_id,
        pipe_id = %pipe_id,
        "Processing BlueBubbles webhook message"
    );

    // 12. Dispatch in background (return 200 immediately)
    let chat_guid = adapter.chat_guid.clone();
    let server_url = adapter.server_url.clone();
    let server_password = adapter.server_password.clone();
    let sent_guids = adapter.sent_guids.clone();
    let state_clone = state.clone();

    tokio::spawn(async move {
        dispatch_message(
            state_clone,
            pipe_id,
            resolved_user_id,
            sender_ref,
            text,
            inbound_attachments,
            message_guid,
            reply_to_guid,
            thread_originator_guid,
            chat_guid,
            server_url,
            server_password,
            sent_guids,
            Client::new(),
        )
        .await;
    });

    StatusCode::OK
}

// ---------------------------------------------------------------------------
// Webhook registration / deregistration with BlueBubbles
// ---------------------------------------------------------------------------

/// Register a webhook URL with the BlueBubbles server.
/// Returns the BB-assigned webhook ID on success.
pub async fn register_webhook_with_bb(
    bb_server_url: &str,
    bb_password: &str,
    callback_url: &str,
) -> Result<i64> {
    let http = Client::new();
    let url = format!("{}/api/v1/webhook", bb_server_url);

    let body = serde_json::json!({
        "url": callback_url,
        "events": ["new-message"],
    });

    let resp = http
        .post(&url)
        .query(&[("password", bb_password)])
        .json(&body)
        .send()
        .await
        .context("Failed to reach BlueBubbles for webhook registration")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("BlueBubbles rejected webhook registration: {status} — {body}");
    }

    let resp_text = resp.text().await.unwrap_or_default();
    debug!(response = %resp_text, "BB webhook registration response");

    let bb_resp: BbWebhookResponse =
        serde_json::from_str(&resp_text).context("Failed to parse BlueBubbles webhook response")?;

    let webhook_id = bb_resp
        .data
        .and_then(|d| d.id)
        .context("BlueBubbles returned no webhook ID")?;

    // Verify the webhook was actually persisted by fetching it back
    let verify_url = format!("{}/api/v1/webhook", bb_server_url);
    let verify_resp = http
        .get(&verify_url)
        .query(&[("password", bb_password)])
        .send()
        .await;
    match verify_resp {
        Ok(r) if r.status().is_success() => {
            let body = r.text().await.unwrap_or_default();
            debug!(webhooks = %body, "BB registered webhooks (verification)");
        }
        Ok(r) => warn!(status = %r.status(), "Could not verify webhooks with BB"),
        Err(e) => warn!("Webhook verification request failed: {e}"),
    }

    Ok(webhook_id)
}

/// Deregister a webhook from BlueBubbles by its ID.
pub async fn deregister_webhook_from_bb(
    bb_server_url: &str,
    bb_password: &str,
    webhook_id: &str,
) -> Result<()> {
    let http = Client::new();
    let url = format!("{}/api/v1/webhook/{}", bb_server_url, webhook_id);

    let resp = http
        .delete(&url)
        .query(&[("password", bb_password)])
        .send()
        .await
        .context("Failed to reach BlueBubbles for webhook deregistration")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        warn!("BlueBubbles webhook deregistration returned {status}: {body}");
    } else {
        info!(webhook_id = %webhook_id, "Webhook deregistered from BlueBubbles");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Sender resolution
// ---------------------------------------------------------------------------

fn collect_bb_inbound_attachments(
    msg: &BbMessage,
    server_url: &str,
    server_password: &str,
) -> Vec<selu_core::types::InboundAttachment> {
    let mut out = Vec::new();
    for (idx, attachment) in msg.attachments.iter().enumerate() {
        let mime = attachment
            .mime_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_string());
        if !mime.starts_with("image/") {
            continue;
        }
        let resolved_download_url = attachment
            .download_url
            .clone()
            .or_else(|| attachment.url.clone())
            .or_else(|| {
                attachment
                    .guid
                    .as_deref()
                    .map(|guid| build_bb_attachment_download_url(server_url, server_password, guid))
            });

        out.push(selu_core::types::InboundAttachment {
            filename: attachment
                .filename
                .clone()
                .or_else(|| attachment.transfer_name.clone())
                .unwrap_or_else(|| format!("bb-image-{}.bin", idx + 1)),
            mime_type: mime,
            size_bytes: None,
            download_url: resolved_download_url,
            data_base64: attachment.data_base64.clone(),
        });
    }
    out
}

fn build_bb_attachment_download_url(server_url: &str, server_password: &str, guid: &str) -> String {
    let base = server_url.trim_end_matches('/');
    let encoded_password = urlencoding::encode(server_password);
    let encoded_guid = urlencoding::encode(guid);
    format!("{base}/api/v1/attachment/{encoded_guid}/download?password={encoded_password}")
}

fn origin_ref_for_thread_resolution<'a>(
    message_guid: Option<&'a str>,
    _reply_to_ref: Option<&str>,
) -> Option<&'a str> {
    // Always pass origin_message_ref so each message can seed a new thread.
    // Without this, messages without reply-to metadata had no origin ref,
    // causing the fallback to route ALL messages to the same thread — mixing
    // unrelated conversations.
    message_guid
}

/// Resolve sender_ref to user_id. Returns None if sender is unknown.
async fn resolve_sender(state: &AppState, pipe_id: &str, sender_ref: &str) -> Option<String> {
    sqlx::query!(
        "SELECT user_id FROM user_sender_refs WHERE pipe_id = ? AND sender_ref = ?",
        pipe_id,
        sender_ref,
    )
    .fetch_optional(&state.db)
    .await
    .ok()?
    .map(|r| r.user_id)
}

// ---------------------------------------------------------------------------
// Message dispatch (shared by webhook handler)
// ---------------------------------------------------------------------------

/// Process a single inbound message: find/create thread, run agent, send reply.
#[allow(clippy::too_many_arguments)]
async fn dispatch_message(
    state: AppState,
    pipe_id: String,
    user_id: String,
    sender_ref: String,
    text: String,
    inbound_attachments: Vec<selu_core::types::InboundAttachment>,
    message_guid: Option<String>,
    reply_to_guid: Option<String>,
    thread_originator_guid: Option<String>,
    chat_guid: String,
    server_url: String,
    server_password: String,
    sent_guids: SentGuids,
    http: Client,
) {
    let inbound_envelope = selu_core::types::InboundEnvelope {
        sender_ref,
        text,
        attachments: if inbound_attachments.is_empty() {
            None
        } else {
            Some(inbound_attachments)
        },
        metadata: Some(serde_json::json!({
            "message_guid": message_guid.clone(),
            "reply_to_ref": reply_to_guid.clone().or(thread_originator_guid.clone()),
            "chat_ref": chat_guid.clone(),
        })),
    };
    let conversation_ref = crate::pipes::ingest::conversation_ref(&inbound_envelope);
    let Some(inbound_envelope) = crate::pipes::ingest::aggregate_inbound_envelope(
        &pipe_id,
        &conversation_ref,
        inbound_envelope,
    )
    .await
    else {
        return;
    };
    let inbound_attachment_inputs =
        crate::pipes::ingest::load_inbound_attachment_inputs(&inbound_envelope, &http).await;
    let message_guid = inbound_envelope
        .metadata
        .as_ref()
        .and_then(|m| m.get("message_guid"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(message_guid);
    let reply_to_guid = inbound_envelope
        .metadata
        .as_ref()
        .and_then(|m| m.get("reply_to_ref"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(reply_to_guid.clone());

    // Route to agent
    let agents_snapshot = state.agents.load();
    let user_agents =
        crate::agents::access::visible_agents(&state.db, &user_id, &agents_snapshot).await;
    let default_agent_id = sqlx::query!("SELECT default_agent_id FROM pipes WHERE id = ?", pipe_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.default_agent_id);

    let (agent_id, effective_text) = agent_router::route(
        &inbound_envelope.text,
        default_agent_id.as_deref(),
        &user_agents,
    );
    let force_new_session = user_agents
        .get(&agent_id)
        .map(|a| a.session.requires_thread_isolation())
        .unwrap_or(false);

    // ── Thread resolution ─────────────────────────────────────────────────
    // For matching, we try multiple GUIDs in priority order:
    //   1. reply_to_guid — the specific message being replied to (most precise)
    //   2. thread_originator_guid — the first message in the thread
    // Either can match an origin_message_ref, last_reply_guid, or an entry
    // in the thread_reply_guids table.
    let reply_to_ref = reply_to_guid
        .as_deref()
        .or(thread_originator_guid.as_deref());

    debug!(
        pipe_id = %pipe_id,
        message_guid = ?message_guid,
        reply_to_guid = ?reply_to_guid,
        thread_originator_guid = ?thread_originator_guid,
        effective_reply_to_ref = ?reply_to_ref,
        "Thread resolution: looking for existing thread"
    );
    let thread = match thread_mgr::find_or_create_thread(
        &state.db,
        &pipe_id,
        &user_id,
        &agent_id,
        force_new_session,
        origin_ref_for_thread_resolution(message_guid.as_deref(), reply_to_ref),
        reply_to_ref,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            error!("Failed to find/create thread: {e}");
            return;
        }
    };

    let thread_id = thread.id.to_string();

    debug!(
        thread_id = %thread_id,
        thread_origin_message_ref = ?thread.origin_message_ref,
        thread_last_reply_guid = ?thread.last_reply_guid,
        thread_created_at = %thread.created_at,
        "Thread resolved"
    );

    // ── Approval interception ─────────────────────────────────────────────
    // If this thread has a pending tool approval, the user's reply (inline
    // reply to the approval prompt) resolves it instead of starting a new
    // agent turn.
    if approval_queue::try_resolve_pending(&state, &thread_id).await
        || approval_queue::try_resolve_pending_for_user_pipe(&state, &user_id, &pipe_id).await
    {
        info!(thread_id = %thread_id, "Message consumed as tool approval response");
        // Send a brief acknowledgment in the user's language
        let lang = crate::i18n::user_language(&state.db, &user_id).await;
        let ack_text = crate::i18n::t(&lang, "approval.approved_processing");
        if let Err(e) = send_bb_reply(
            &http,
            &server_url,
            &server_password,
            &chat_guid,
            ack_text,
            message_guid.as_deref(),
            &sent_guids,
        )
        .await
        {
            error!("Failed to send BlueBubbles approval ack: {e}");
        }
        return;
    }

    // Show typing indicator while the agent is working.
    // Stops automatically when we send the reply message.
    // Also mark the conversation as read so it doesn't stay "unread" in
    // the Messages app while we're already processing the message.
    start_typing(&http, &server_url, &server_password, &chat_guid).await;
    mark_as_read(&http, &server_url, &server_password, &chat_guid).await;

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
        skip_user_persist: false,
        enable_streaming: true,
        inbound_attachments: inbound_attachment_inputs,
        delegation_trace: Vec::new(),
        location_context: None,
    };

    let reply = run_turn(&state, params, noop_sender()).await;

    let output = match reply {
        Ok(t) if !t.reply_text.is_empty() || !t.attachments.is_empty() => t,
        Ok(_) => {
            warn!("Agent turn returned empty reply");
            let lang = crate::i18n::user_language(&state.db, &user_id).await;
            let error_text = crate::i18n::t(&lang, "error.agent_turn_failed");
            if let Err(e) = send_bb_reply(
                &http,
                &server_url,
                &server_password,
                &chat_guid,
                error_text,
                message_guid.as_deref(),
                &sent_guids,
            )
            .await
            {
                error!("Failed to send BlueBubbles empty-turn error reply: {e}");
            }
            return;
        }
        Err(e) => {
            error!("Agent turn failed: {e}");
            let _ = thread_mgr::fail_thread(&state.db, &thread_id).await;
            // Send a user-friendly error message so the user isn't left
            // staring at silence.
            let lang = crate::i18n::user_language(&state.db, &user_id).await;
            let error_text = crate::i18n::t(&lang, "error.agent_turn_failed");
            if let Err(e) = send_bb_reply(
                &http,
                &server_url,
                &server_password,
                &chat_guid,
                error_text,
                message_guid.as_deref(),
                &sent_guids,
            )
            .await
            {
                error!("Failed to send BlueBubbles agent-turn error reply: {e}");
            }
            return;
        }
    };

    // Send reply via BlueBubbles
    let attachments = crate::agents::artifacts::to_outbound_attachments(
        &state.artifacts,
        &output.attachments,
        &user_id,
        &state.public_base_url(),
        &state.config.encryption_key,
        true,
    )
    .await;
    debug!(
        thread_id = %thread_id,
        reply_to_guid = ?message_guid,
        "Sending BB reply"
    );
    let clean_text = strip_markdown(&output.reply_text);
    let sent_guid = match send_bb_message_with_attachments(
        &http,
        &server_url,
        &server_password,
        &chat_guid,
        &clean_text,
        &attachments,
        message_guid.as_deref(), // reply to the original message
        &sent_guids,
    )
    .await
    {
        Ok(guid) => guid,
        Err(e) => {
            error!(thread_id = %thread_id, "Failed to send BlueBubbles reply: {e}");
            None
        }
    };

    // Store Selu's reply GUID so future incoming replies can be matched
    // back to this thread.
    if let Some(ref guid) = sent_guid {
        debug!(
            thread_id = %thread_id,
            sent_guid = %guid,
            "Storing outbound GUID for thread reply matching"
        );
        let _ = thread_mgr::update_reply_guid(&state.db, &thread_id, &pipe_id, guid).await;
    } else {
        warn!(
            thread_id = %thread_id,
            "No GUID returned from BB send — future replies won't match this thread"
        );
    }
}

// ---------------------------------------------------------------------------
// BlueBubbles send (outbound)
// ---------------------------------------------------------------------------

/// Send a message via the BlueBubbles API, optionally as a reply to a specific message.
/// Returns the sent message GUID if available (for thread reply-to tracking).
async fn send_bb_reply(
    http: &Client,
    server_url: &str,
    server_password: &str,
    chat_guid: &str,
    text: &str,
    reply_to_guid: Option<&str>,
    sent_guids: &SentGuids,
) -> Result<Option<String>> {
    let url = format!("{}/api/v1/message/text", server_url);
    let methods = ["private-api", "apple-script"];
    let mut last_error: Option<String> = None;

    for method in methods {
        let temp_guid = format!("selu-{}", current_epoch_ms());
        let body = BbSendTextRequest {
            chat_guid: chat_guid.to_string(),
            message: text.to_string(),
            method: method.to_string(),
            temp_guid,
            selected_message_guid: reply_to_guid.map(|s| s.to_string()),
        };

        let resp = http
            .post(&url)
            .query(&[("password", server_password)])
            .json(&body)
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                if let Ok(bb_resp) = r.json::<BbSendResponse>().await {
                    if let Some(guid) = bb_resp.data.and_then(|d| d.guid) {
                        track_sent_guid(sent_guids, &guid).await;
                        info!(method = method, guid = %guid, "Sent via BlueBubbles");

                        return Ok(Some(guid));
                    }
                }
                info!(method = method, "Sent via BlueBubbles (no GUID returned)");
                return Ok(None);
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                warn!(method = method, %status, %body, "BlueBubbles rejected send, trying fallback if available");
                last_error = Some(format!("method={method} status={status} body={body}"));
            }
            Err(e) => {
                warn!(method = method, error = %e, "Failed to reach BlueBubbles, trying fallback if available");
                last_error = Some(format!("method={method} error={e}"));
            }
        }
    }

    Err(anyhow!(
        "BlueBubbles send failed after all methods: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    ))
}

async fn send_bb_attachment(
    http: &Client,
    server_url: &str,
    server_password: &str,
    chat_guid: &str,
    attachment: &selu_core::types::OutboundAttachment,
    reply_to_guid: Option<&str>,
    sent_guids: &SentGuids,
) -> Result<Option<String>> {
    let data = attachment_bytes(http, attachment).await?;
    let url = format!("{}/api/v1/message/attachment", server_url);
    let temp_guid = format!("selu-{}", current_epoch_ms());
    let file_part = reqwest::multipart::Part::bytes(data)
        .file_name(attachment.filename.clone())
        .mime_str(&attachment.mime_type)
        .context("invalid BlueBubbles attachment mime type")?;
    let mut form = reqwest::multipart::Form::new()
        .text("chatGuid", chat_guid.to_string())
        .text("tempGuid", temp_guid)
        .text("name", attachment.filename.clone())
        .part("attachment", file_part);
    if let Some(guid) = reply_to_guid {
        form = form.text("selectedMessageGuid", guid.to_string());
    }

    let resp = http
        .post(&url)
        .query(&[("password", server_password)])
        .multipart(form)
        .send()
        .await
        .context("failed to reach BlueBubbles attachment endpoint")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "BlueBubbles attachment send failed: status={} body={}",
            status,
            body
        ));
    }

    if let Ok(bb_resp) = resp.json::<BbSendResponse>().await {
        if let Some(guid) = bb_resp.data.and_then(|d| d.guid) {
            track_sent_guid(sent_guids, &guid).await;
            return Ok(Some(guid));
        }
    }

    Ok(None)
}

async fn send_bb_message_with_attachments(
    http: &Client,
    server_url: &str,
    server_password: &str,
    chat_guid: &str,
    text: &str,
    attachments: &[selu_core::types::OutboundAttachment],
    reply_to_guid: Option<&str>,
    sent_guids: &SentGuids,
) -> Result<Option<String>> {
    let mut primary_guid = if text.trim().is_empty() {
        None
    } else {
        send_bb_reply(
            http,
            server_url,
            server_password,
            chat_guid,
            text,
            reply_to_guid,
            sent_guids,
        )
        .await?
    };

    let mut failed_links = Vec::new();
    for attachment in attachments {
        match send_bb_attachment(
            http,
            server_url,
            server_password,
            chat_guid,
            attachment,
            reply_to_guid,
            sent_guids,
        )
        .await
        {
            Ok(guid) => {
                if primary_guid.is_none() {
                    primary_guid = guid;
                }
            }
            Err(e) => {
                warn!(
                    filename = %attachment.filename,
                    mime_type = %attachment.mime_type,
                    error = %e,
                    "BlueBubbles attachment send failed; falling back to download link"
                );
                if let Some(url) = attachment.download_url.as_ref() {
                    failed_links.push(url.clone());
                }
            }
        }
    }

    if !failed_links.is_empty() {
        let mut fallback_text = String::new();
        for url in failed_links {
            if !fallback_text.is_empty() {
                fallback_text.push('\n');
                fallback_text.push('\n');
            }
            fallback_text.push_str(&url);
        }
        let clean_text = strip_markdown(&fallback_text);
        let _ = send_bb_reply(
            http,
            server_url,
            server_password,
            chat_guid,
            &clean_text,
            reply_to_guid,
            sent_guids,
        )
        .await;
    }

    Ok(primary_guid)
}

async fn attachment_bytes(
    http: &Client,
    attachment: &selu_core::types::OutboundAttachment,
) -> Result<Vec<u8>> {
    if let Some(data_base64) = attachment.data_base64.as_ref() {
        return STANDARD
            .decode(data_base64)
            .context("failed to decode attachment data");
    }

    if let Some(url) = attachment.download_url.as_ref() {
        let resp = http
            .get(url)
            .send()
            .await
            .context("failed to download attachment payload")?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "attachment download failed with status {}",
                resp.status()
            ));
        }
        return resp
            .bytes()
            .await
            .map(|b| b.to_vec())
            .context("failed to read attachment payload");
    }

    Err(anyhow!(
        "attachment has neither inline data nor download URL"
    ))
}

async fn track_sent_guid(sent_guids: &SentGuids, guid: &str) {
    let mut sent = sent_guids.write().await;
    sent.insert(guid.to_string());
    if sent.len() > 500 {
        sent.clear();
        sent.insert(guid.to_string());
    }
}

// ---------------------------------------------------------------------------
// Typing indicator
// ---------------------------------------------------------------------------

/// Start the typing indicator for a chat.
///
/// Requires the Private API to be enabled in BlueBubbles. If it's not
/// available the request will simply fail silently — no harm done.
/// The indicator stops automatically when a message is sent to the chat.
async fn start_typing(http: &Client, server_url: &str, server_password: &str, chat_guid: &str) {
    let url = format!("{}/api/v1/chat/{}/typing", server_url, chat_guid);
    let resp = http
        .post(&url)
        .query(&[("password", server_password)])
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            debug!("Typing indicator started");
        }
        Ok(r) => {
            debug!(status = %r.status(), "Typing indicator not available (Private API may be disabled)");
        }
        Err(e) => {
            debug!("Typing indicator request failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Mark chat as read
// ---------------------------------------------------------------------------

/// Mark a chat as read in BlueBubbles.
///
/// Like the typing indicator this requires the Private API. If it's not
/// available the request fails silently.
async fn mark_as_read(http: &Client, server_url: &str, server_password: &str, chat_guid: &str) {
    let url = format!("{}/api/v1/chat/{}/read", server_url, chat_guid);
    let resp = http
        .post(&url)
        .query(&[("password", server_password)])
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            debug!("Chat marked as read");
        }
        Ok(r) => {
            debug!(status = %r.status(), "Mark-as-read not available (Private API may be disabled)");
        }
        Err(e) => {
            debug!("Mark-as-read request failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// ChannelSender implementation for BlueBubbles
// ---------------------------------------------------------------------------

/// A `ChannelSender` that sends messages via the BlueBubbles API.
///
/// Registered on the `ChannelRegistry` for each pipe backed by a BB adapter.
/// The approval queue uses this to send approval prompts and the interception
/// handler uses it for acknowledgments.
pub struct BlueBubblesSender {
    http: Client,
    server_url: String,
    server_password: String,
    chat_guid: String,
    sent_guids: SentGuids,
}

#[async_trait::async_trait]
impl crate::channels::ChannelSender for BlueBubblesSender {
    async fn send_message(
        &self,
        _thread_id: &str,
        message: &crate::channels::ChannelMessage,
        reply_to_guid: Option<&str>,
    ) -> Result<Option<String>> {
        let clean_text = strip_markdown(&message.text);
        send_bb_message_with_attachments(
            &self.http,
            &self.server_url,
            &self.server_password,
            &self.chat_guid,
            &clean_text,
            &message.attachments,
            reply_to_guid,
            &self.sent_guids,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Public: handle outbound from pipe (for pipes that have BB adapter attached)
// ---------------------------------------------------------------------------

/// Send an outbound envelope via BlueBubbles.
/// Called when a pipe's outbound should go through a BB adapter.
/// This is used by the pipe outbound flow when the adapter is integrated.
#[allow(dead_code)]
pub async fn send_outbound(
    state: &AppState,
    pipe_id: &str,
    envelope: &selu_core::types::OutboundEnvelope,
) -> Result<()> {
    // Look up which BB config is attached to this pipe
    let cfg = sqlx::query!(
        "SELECT server_url, server_password, chat_guid
         FROM bluebubbles_configs
         WHERE pipe_id = ? AND active = 1
         LIMIT 1",
        pipe_id
    )
    .fetch_optional(&state.db)
    .await
    .context("DB error loading BB config for outbound")?;

    let cfg = match cfg {
        Some(c) => c,
        None => {
            // No BB config for this pipe — not an error, just not a BB pipe
            return Ok(());
        }
    };

    let http = Client::new();
    let sent_guids: SentGuids = Arc::new(RwLock::new(HashSet::new()));
    let clean_text = strip_markdown(&envelope.text);

    send_bb_message_with_attachments(
        &http,
        &cfg.server_url,
        &cfg.server_password,
        &cfg.chat_guid,
        &clean_text,
        envelope.attachments.as_deref().unwrap_or(&[]),
        envelope.reply_to_message_ref.as_deref(),
        &sent_guids,
    )
    .await
    .context("BlueBubbles outbound send failed")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// DB helpers
// ---------------------------------------------------------------------------

async fn load_active_configs(state: &AppState) -> Result<Vec<BbConfig>> {
    let rows = sqlx::query!(
        "SELECT bc.id, bc.name, bc.server_url, bc.server_password, bc.chat_guid,
                bc.pipe_id, bc.bb_webhook_id, p.inbound_token
         FROM bluebubbles_configs bc
         JOIN pipes p ON p.id = bc.pipe_id
         WHERE bc.active = 1"
    )
    .fetch_all(&state.db)
    .await
    .context("Failed to load bluebubbles_configs")?;

    Ok(rows
        .into_iter()
        .map(|r| BbConfig {
            id: r.id.unwrap_or_default(),
            name: r.name,
            server_url: r.server_url,
            server_password: r.server_password,
            chat_guid: r.chat_guid,
            pipe_id: r.pipe_id,
            bb_webhook_id: r.bb_webhook_id,
            inbound_token: r.inbound_token,
        })
        .collect())
}

async fn load_config_by_id(state: &AppState, config_id: &str) -> Result<BbConfig> {
    let row = sqlx::query!(
        "SELECT bc.id, bc.name, bc.server_url, bc.server_password, bc.chat_guid,
                bc.pipe_id, bc.bb_webhook_id, p.inbound_token
         FROM bluebubbles_configs bc
         JOIN pipes p ON p.id = bc.pipe_id
         WHERE bc.id = ? AND bc.active = 1",
        config_id
    )
    .fetch_optional(&state.db)
    .await
    .context("Failed to load BlueBubbles config")?;

    let row = row
        .ok_or_else(|| anyhow::anyhow!("BlueBubbles config not found or inactive: {config_id}"))?;

    Ok(BbConfig {
        id: row.id.unwrap_or_default(),
        name: row.name,
        server_url: row.server_url,
        server_password: row.server_password,
        chat_guid: row.chat_guid,
        pipe_id: row.pipe_id,
        bb_webhook_id: row.bb_webhook_id,
        inbound_token: row.inbound_token,
    })
}

// ---------------------------------------------------------------------------
// Markdown → plain-text
// ---------------------------------------------------------------------------

/// Strip common Markdown formatting so messages look clean in iMessage.
fn strip_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());

    for line in s.lines() {
        let trimmed = line.trim_start();

        // Heading lines
        if trimmed.starts_with('#') {
            let content = trimmed.trim_start_matches('#').trim_start();
            out.push_str(content);
            out.push('\n');
            continue;
        }

        // Bullet lists
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            out.push_str("• ");
            out.push_str(rest);
            out.push('\n');
            continue;
        }

        out.push_str(line);
        out.push('\n');
    }

    let out = out.trim_end_matches('\n').to_string();

    // Inline formatting
    let out = strip_delimiters(&out, "**");
    let out = strip_delimiters(&out, "__");
    let out = strip_single_delimiter(&out, '*');
    let out = strip_single_delimiter(&out, '_');
    let out = strip_delimiters(&out, "`");
    let out = strip_links(&out);

    out
}

fn strip_delimiters(s: &str, delim: &str) -> String {
    if delim.is_empty() {
        return s.to_string();
    }
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(delim) {
        result.push_str(&rest[..start]);
        let after_open = &rest[start + delim.len()..];
        if let Some(end) = after_open.find(delim) {
            result.push_str(&after_open[..end]);
            rest = &after_open[end + delim.len()..];
        } else {
            result.push_str(delim);
            rest = after_open;
        }
    }
    result.push_str(rest);
    result
}

fn strip_single_delimiter(s: &str, ch: char) -> String {
    let mut result = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;
    while i < len {
        if chars[i] == ch {
            let next_is_same = i + 1 < len && chars[i + 1] == ch;
            let prev_is_same = i > 0 && chars[i - 1] == ch;
            if next_is_same || prev_is_same {
                result.push(chars[i]);
                i += 1;
                continue;
            }
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == ch) {
                let end_idx = i + 1 + end;
                if end_idx + 1 < len && chars[end_idx + 1] == ch {
                    result.push(chars[i]);
                    i += 1;
                    continue;
                }
                for &c in &chars[i + 1..end_idx] {
                    result.push(c);
                }
                i = end_idx + 1;
                continue;
            }
            result.push(chars[i]);
        } else {
            result.push(chars[i]);
        }
        i += 1;
    }
    result
}

fn strip_links(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(bracket_open) = rest.find('[') {
        result.push_str(&rest[..bracket_open]);
        let after_open = &rest[bracket_open + 1..];
        if let Some(bracket_close) = after_open.find(']') {
            let link_text = &after_open[..bracket_close];
            let after_close = &after_open[bracket_close + 1..];
            if after_close.starts_with('(') {
                if let Some(paren_close) = after_close.find(')') {
                    result.push_str(link_text);
                    rest = &after_close[paren_close + 1..];
                    continue;
                }
            }
            result.push('[');
            rest = after_open;
        } else {
            result.push('[');
            rest = after_open;
        }
    }
    result.push_str(rest);
    result
}

fn current_epoch_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::{build_bb_attachment_download_url, origin_ref_for_thread_resolution};

    #[test]
    fn build_attachment_download_url_trims_trailing_slash() {
        let url = build_bb_attachment_download_url("https://bb.example.com/", "p@ss word", "A/B C");
        assert_eq!(
            url,
            "https://bb.example.com/api/v1/attachment/A%2FB%20C/download?password=p%40ss%20word"
        );
    }

    #[test]
    fn origin_ref_always_passed_through() {
        // With reply_to_ref
        let origin = origin_ref_for_thread_resolution(Some("m-123"), Some("r-456"));
        assert_eq!(origin, Some("m-123"));

        // Without reply_to_ref — still passed through (fixes thread mixing)
        let origin = origin_ref_for_thread_resolution(Some("m-123"), None);
        assert_eq!(origin, Some("m-123"));

        // None message_guid
        let origin = origin_ref_for_thread_resolution(None, Some("r-456"));
        assert_eq!(origin, None);
    }
}
