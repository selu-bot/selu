/// Integrated BlueBubbles adapter.
///
/// Instead of running as a separate sidecar binary, this module runs
/// inside the orchestrator process. It:
///   1. Loads BlueBubbles configs from the DB (one per BB server/chat)
///   2. Spawns a polling task per active config
///   3. Dispatches inbound messages through the standard pipe inbound flow
///      (with sender_ref resolution and thread creation)
///   4. Handles outbound replies with reply-to-message support
///
/// The adapter is started from main.rs after the DB is ready and the
/// AppState is built.
use anyhow::{Context as _, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{Duration, interval};
use tracing::{debug, error, info, warn};

use crate::agents::{
    engine::{noop_sender, run_turn, TurnParams},
    router as agent_router,
    thread as thread_mgr,
};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// BlueBubbles API types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct BbQueryResponse {
    #[allow(dead_code)]
    status: u16,
    data: Vec<BbMessage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BbMessage {
    text: Option<String>,
    #[allow(dead_code)]
    is_from_me: bool,
    handle: Option<BbHandle>,
    guid: Option<String>,
    date_created: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BbHandle {
    address: String,
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

// ---------------------------------------------------------------------------
// Per-adapter state
// ---------------------------------------------------------------------------

/// Tracks sent message GUIDs for loop prevention (per adapter instance).
type SentGuids = Arc<RwLock<HashSet<String>>>;

struct AdapterInstance {
    config_id: String,
    server_url: String,
    server_password: String,
    chat_guid: String,
    pipe_id: String,
    poll_interval_ms: u64,
    http: Client,
    sent_guids: SentGuids,
}

// ---------------------------------------------------------------------------
// Public: start all configured adapters
// ---------------------------------------------------------------------------

/// Loads all active BlueBubbles configs from the DB and spawns a polling
/// task for each one. Call this from main.rs after building AppState.
pub async fn start_all(state: AppState) {
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

    info!("Starting {} BlueBubbles adapter(s)", configs.len());

    for cfg in configs {
        let instance = AdapterInstance {
            config_id: cfg.id.clone(),
            server_url: cfg.server_url.clone(),
            server_password: cfg.server_password.clone(),
            chat_guid: cfg.chat_guid.clone(),
            pipe_id: cfg.pipe_id.clone(),
            poll_interval_ms: cfg.poll_interval_ms as u64,
            http: Client::new(),
            sent_guids: Arc::new(RwLock::new(HashSet::new())),
        };

        info!(
            config_id = %cfg.id,
            name = %cfg.name,
            chat = %cfg.chat_guid,
            pipe_id = %cfg.pipe_id,
            poll_ms = cfg.poll_interval_ms,
            "Starting BlueBubbles adapter"
        );

        let state_clone = state.clone();
        tokio::spawn(async move {
            poll_loop(instance, state_clone).await;
        });
    }
}

struct BbConfig {
    id: String,
    name: String,
    server_url: String,
    server_password: String,
    chat_guid: String,
    pipe_id: String,
    poll_interval_ms: i64,
}

async fn load_active_configs(state: &AppState) -> Result<Vec<BbConfig>> {
    let rows = sqlx::query!(
        "SELECT id, name, server_url, server_password, chat_guid, pipe_id, poll_interval_ms
         FROM bluebubbles_configs WHERE active = 1"
    )
    .fetch_all(&state.db)
    .await
    .context("Failed to load bluebubbles_configs")?;

    Ok(rows.into_iter().map(|r| BbConfig {
        id: r.id.unwrap_or_default(),
        name: r.name,
        server_url: r.server_url,
        server_password: r.server_password,
        chat_guid: r.chat_guid,
        pipe_id: r.pipe_id,
        poll_interval_ms: r.poll_interval_ms,
    }).collect())
}

// ---------------------------------------------------------------------------
// Polling loop
// ---------------------------------------------------------------------------

async fn poll_loop(instance: AdapterInstance, state: AppState) {
    let mut ticker = interval(Duration::from_millis(instance.poll_interval_ms));
    let mut last_ts: i64 = current_epoch_ms();

    info!(
        config_id = %instance.config_id,
        chat = %instance.chat_guid,
        "Poller started"
    );

    loop {
        ticker.tick().await;

        match poll_once(&instance, &state, last_ts).await {
            Ok(new_ts) => {
                if new_ts > last_ts {
                    last_ts = new_ts;
                }
            }
            Err(e) => {
                warn!(config_id = %instance.config_id, "Poll error: {e}");
            }
        }
    }
}

async fn poll_once(instance: &AdapterInstance, state: &AppState, after_ts: i64) -> Result<i64> {
    let url = format!(
        "{}/api/v1/message/query?password={}",
        instance.server_url, instance.server_password
    );

    let body = serde_json::json!({
        "chatGuid": instance.chat_guid,
        "limit": 20,
        "sort": "ASC",
        "after": after_ts,
        "with": ["handle"],
    });

    let resp = instance.http.post(&url).json(&body).send().await?;
    let bb_resp: BbQueryResponse = resp.json().await?;

    let mut max_ts = after_ts;

    for msg in &bb_resp.data {
        let ts = msg.date_created.unwrap_or(0);
        if ts >= max_ts {
            max_ts = ts + 1;
        }

        // Skip messages we sent ourselves (loop prevention)
        if let Some(guid) = &msg.guid {
            let sent = instance.sent_guids.read().await;
            if sent.contains(guid) {
                debug!(guid = %guid, "Skipping adapter-sent message");
                continue;
            }
        }

        let text = match &msg.text {
            Some(t) if !t.is_empty() => t.clone(),
            _ => continue,
        };

        let sender_ref = msg
            .handle
            .as_ref()
            .map(|h| h.address.clone())
            .unwrap_or_else(|| "self".into());

        let message_guid = msg.guid.clone();

        // ── Sender resolution ─────────────────────────────────────────────
        let resolved_user_id = match resolve_sender(state, &instance.pipe_id, &sender_ref).await {
            Some(uid) => uid,
            None => {
                debug!(
                    sender = %sender_ref,
                    pipe_id = %instance.pipe_id,
                    "Ignoring message from unknown sender"
                );
                continue;
            }
        };

        info!(
            sender = %sender_ref,
            user_id = %resolved_user_id,
            pipe_id = %instance.pipe_id,
            "Processing BlueBubbles message"
        );

        // ── Route + dispatch ──────────────────────────────────────────────
        let state_clone = state.clone();
        let pipe_id = instance.pipe_id.clone();
        let chat_guid = instance.chat_guid.clone();
        let server_url = instance.server_url.clone();
        let server_password = instance.server_password.clone();
        let sent_guids = instance.sent_guids.clone();
        let http = instance.http.clone();

        tokio::spawn(async move {
            dispatch_message(
                state_clone,
                pipe_id,
                resolved_user_id,
                sender_ref,
                text,
                message_guid,
                chat_guid,
                server_url,
                server_password,
                sent_guids,
                http,
            ).await;
        });
    }

    Ok(max_ts)
}

/// Resolve sender_ref to user_id. Returns None if sender is unknown.
async fn resolve_sender(state: &AppState, pipe_id: &str, sender_ref: &str) -> Option<String> {
    // First check if there's a specific mapping
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

    // Check if there are any mappings for this pipe at all
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
        // Mappings exist but this sender isn't in them → reject
        None
    } else {
        // No mappings → fall back to pipe owner (backward compat)
        sqlx::query!("SELECT user_id FROM pipes WHERE id = ?", pipe_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .map(|r| r.user_id)
    }
}

/// Process a single inbound message: create thread, run agent, send reply.
#[allow(clippy::too_many_arguments)]
async fn dispatch_message(
    state: AppState,
    pipe_id: String,
    user_id: String,
    _sender_ref: String,
    text: String,
    message_guid: Option<String>,
    chat_guid: String,
    server_url: String,
    server_password: String,
    sent_guids: SentGuids,
    http: Client,
) {
    // Route to agent
    let agents_snapshot = state.agents.read().await.clone();
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

    // Create thread
    let thread = match thread_mgr::create_thread(
        &state.db,
        &pipe_id,
        &user_id,
        &agent_id,
        message_guid.as_deref(),
    ).await {
        Ok(t) => t,
        Err(e) => {
            error!("Failed to create thread: {e}");
            return;
        }
    };

    let thread_id = thread.id.to_string();

    let params = TurnParams {
        pipe_id,
        user_id,
        agent_id: Some(agent_id),
        message: effective_text,
        thread_id: Some(thread_id.clone()),
        chain_depth: 0,
    };

    let reply = run_turn(&state, params, noop_sender()).await;

    let reply_text = match reply {
        Ok(t) if !t.is_empty() => t,
        Ok(_) => {
            let _ = thread_mgr::complete_thread(&state.db, &thread_id).await;
            return;
        }
        Err(e) => {
            error!("Agent turn failed: {e}");
            let _ = thread_mgr::fail_thread(&state.db, &thread_id).await;
            return;
        }
    };

    // Send reply via BlueBubbles
    let clean_text = strip_markdown(&reply_text);
    send_bb_reply(
        &http,
        &server_url,
        &server_password,
        &chat_guid,
        &clean_text,
        message_guid.as_deref(), // reply to the original message
        &sent_guids,
    ).await;

    let _ = thread_mgr::complete_thread(&state.db, &thread_id).await;
}

// ---------------------------------------------------------------------------
// BlueBubbles send (outbound)
// ---------------------------------------------------------------------------

/// Send a message via the BlueBubbles API, optionally as a reply to a specific message.
async fn send_bb_reply(
    http: &Client,
    server_url: &str,
    server_password: &str,
    chat_guid: &str,
    text: &str,
    reply_to_guid: Option<&str>,
    sent_guids: &SentGuids,
) {
    let url = format!(
        "{}/api/v1/message/text?password={}",
        server_url, server_password
    );

    let temp_guid = format!("pap-{}", current_epoch_ms());

    let body = BbSendTextRequest {
        chat_guid: chat_guid.to_string(),
        message: text.to_string(),
        method: "apple-script".into(),
        temp_guid,
        selected_message_guid: reply_to_guid.map(|s| s.to_string()),
    };

    let resp = http.post(&url).json(&body).send().await;

    match resp {
        Ok(r) if r.status().is_success() => {
            if let Ok(bb_resp) = r.json::<BbSendResponse>().await {
                if let Some(guid) = bb_resp.data.and_then(|d| d.guid) {
                    let mut sent = sent_guids.write().await;
                    sent.insert(guid.clone());
                    info!(guid = %guid, "Sent via BlueBubbles");

                    // Prune if set gets too large
                    if sent.len() > 500 {
                        sent.clear();
                        sent.insert(guid);
                    }
                }
            }
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            error!(%status, %body, "BlueBubbles rejected send");
        }
        Err(e) => {
            error!("Failed to reach BlueBubbles: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Public: handle outbound from pipe (for pipes that have BB adapter attached)
// ---------------------------------------------------------------------------

/// Send an outbound envelope via BlueBubbles.
/// Called when a pipe's outbound should go through a BB adapter.
/// This is used by the pipe outbound flow when the adapter is integrated.
pub async fn send_outbound(
    state: &AppState,
    pipe_id: &str,
    envelope: &pap_core::types::OutboundEnvelope,
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

    send_bb_reply(
        &http,
        &cfg.server_url,
        &cfg.server_password,
        &cfg.chat_guid,
        &clean_text,
        envelope.reply_to_message_ref.as_deref(),
        &sent_guids,
    ).await;

    Ok(())
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
        if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
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
