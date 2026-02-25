use anyhow::{Context, Result};
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::post,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{Duration, interval};
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Config {
    /// Adapter listen address (e.g. "0.0.0.0:3100")
    listen_addr: String,
    /// BlueBubbles server base URL (e.g. "http://localhost:1234")
    bb_url: String,
    /// BlueBubbles server password
    bb_password: String,
    /// PAP inbound endpoint (e.g. "http://localhost:3000/api/pipes/{pipe_id}/inbound")
    pap_inbound_url: String,
    /// PAP pipe inbound Bearer token
    pap_inbound_token: String,
    /// Only process messages from this BlueBubbles chat GUID.
    bb_chat_guid: String,
    /// Poll interval in milliseconds.
    poll_interval_ms: u64,
}

impl Config {
    fn from_env() -> Result<Self> {
        Ok(Self {
            listen_addr: std::env::var("ADAPTER_LISTEN_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:3100".into()),
            bb_url: std::env::var("BB_URL")
                .unwrap_or_else(|_| "http://localhost:1234".into()),
            bb_password: std::env::var("BB_PASSWORD")
                .context("BB_PASSWORD must be set")?,
            pap_inbound_url: std::env::var("PAP_INBOUND_URL")
                .context("PAP_INBOUND_URL must be set (e.g. http://localhost:3000/api/pipes/<pipe_id>/inbound)")?,
            pap_inbound_token: std::env::var("PAP_INBOUND_TOKEN")
                .context("PAP_INBOUND_TOKEN must be set")?,
            bb_chat_guid: std::env::var("BB_CHAT_GUID")
                .context("BB_CHAT_GUID must be set (the chat to listen to and reply in)")?,
            poll_interval_ms: std::env::var("ADAPTER_POLL_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2000),
        })
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Tracks message GUIDs that the adapter itself sent (loop prevention).
type SentGuids = Arc<RwLock<HashSet<String>>>;

#[derive(Clone)]
struct AppState {
    config: Config,
    http: Client,
    sent_guids: SentGuids,
}

// ---------------------------------------------------------------------------
// BlueBubbles types
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
    /// Epoch timestamp in milliseconds.
    date_created: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BbHandle {
    address: String,
}

// ---------------------------------------------------------------------------
// PAP envelope types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct PapInboundEnvelope {
    sender_ref: String,
    text: String,
    metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct PapOutboundEnvelope {
    #[allow(dead_code)]
    recipient_ref: String,
    text: String,
    #[allow(dead_code)]
    metadata: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// BlueBubbles send-message API
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BbSendTextRequest {
    chat_guid: String,
    message: String,
    method: String,
    temp_guid: String,
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
// Poller: fetches new messages from BlueBubbles on a timer
// ---------------------------------------------------------------------------

async fn poll_loop(state: AppState) {
    let mut ticker = interval(Duration::from_millis(state.config.poll_interval_ms));
    // Track the latest timestamp we've seen so we only forward new messages.
    let mut last_ts: i64 = current_epoch_ms();

    info!(
        chat = %state.config.bb_chat_guid,
        poll_ms = state.config.poll_interval_ms,
        "starting poller"
    );

    loop {
        ticker.tick().await;

        match poll_once(&state, last_ts).await {
            Ok(new_ts) => {
                if new_ts > last_ts {
                    last_ts = new_ts;
                }
            }
            Err(e) => {
                warn!("poll error: {e}");
            }
        }
    }
}

async fn poll_once(state: &AppState, after_ts: i64) -> Result<i64> {
    let url = format!(
        "{}/api/v1/message/query",
        state.config.bb_url
    );

    let body = serde_json::json!({
        "chatGuid": state.config.bb_chat_guid,
        "limit": 20,
        "sort": "ASC",
        "after": after_ts,
        "with": ["handle"],
    });

    let resp = state.http.post(&url)
        .query(&[("password", state.config.bb_password.as_str())])
        .json(&body)
        .send()
        .await?;
    let bb_resp: BbQueryResponse = resp.json().await?;

    let mut max_ts = after_ts;

    for msg in &bb_resp.data {
        let ts = msg.date_created.unwrap_or(0);
        if ts >= max_ts {
            // +1 so the inclusive "after" filter skips this message next poll.
            max_ts = ts + 1;
        }

        // Skip messages we sent ourselves via the adapter (loop prevention).
        if let Some(guid) = &msg.guid {
            let sent = state.sent_guids.read().await;
            if sent.contains(guid) {
                info!(guid = %guid, "skipping adapter-sent message");
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

        let metadata = serde_json::json!({
            "source": "bluebubbles",
            "message_guid": msg.guid,
            "chat_guid": state.config.bb_chat_guid,
        });

        let envelope = PapInboundEnvelope {
            sender_ref,
            text,
            metadata: Some(metadata),
        };

        info!(envelope = ?envelope, "forwarding to pap");

        let resp = state
            .http
            .post(&state.config.pap_inbound_url)
            .bearer_auth(&state.config.pap_inbound_token)
            .json(&envelope)
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                info!("forwarded to pap successfully");
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                error!(%status, %body, "pap rejected inbound message");
            }
            Err(e) => {
                error!("failed to reach pap: {e}");
            }
        }
    }

    Ok(max_ts)
}

fn current_epoch_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ---------------------------------------------------------------------------
// Markdown → plain-text conversion
// ---------------------------------------------------------------------------

/// Strip common Markdown formatting so messages look clean in iMessage.
fn strip_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());

    for line in s.lines() {
        let trimmed = line.trim_start();

        // Heading lines: "## Foo" → "Foo"
        if trimmed.starts_with('#') {
            let content = trimmed.trim_start_matches('#').trim_start();
            out.push_str(content);
            out.push('\n');
            continue;
        }

        // Bullet lists: "- Foo" / "* Foo" → "• Foo"
        if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            out.push_str("• ");
            out.push_str(rest);
            out.push('\n');
            continue;
        }

        out.push_str(line);
        out.push('\n');
    }

    // Inline formatting: bold, italic, code, links
    let out = out.trim_end_matches('\n').to_string();

    // Bold: **text** or __text__
    let out = strip_delimiters(&out, "**");
    let out = strip_delimiters(&out, "__");
    // Italic: *text* or _text_ (only single)
    let out = strip_single_delimiter(&out, '*');
    let out = strip_single_delimiter(&out, '_');
    // Inline code: `text`
    let out = strip_delimiters(&out, "`");
    // Links: [text](url) → text
    let out = strip_links(&out);

    out
}

/// Remove paired delimiters like ** or ` from text.
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
            // No closing delimiter — keep as-is.
            result.push_str(delim);
            rest = after_open;
        }
    }
    result.push_str(rest);
    result
}

/// Remove single-char emphasis (* or _), being careful not to touch ** or __.
fn strip_single_delimiter(s: &str, ch: char) -> String {
    let mut result = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;
    while i < len {
        if chars[i] == ch {
            // Skip if this is part of a double delimiter (already handled).
            let next_is_same = i + 1 < len && chars[i + 1] == ch;
            let prev_is_same = i > 0 && chars[i - 1] == ch;
            if next_is_same || prev_is_same {
                result.push(chars[i]);
                i += 1;
                continue;
            }
            // Single delimiter — look for closing.
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == ch) {
                let end_idx = i + 1 + end;
                // Skip if the close is also doubled.
                if end_idx + 1 < len && chars[end_idx + 1] == ch {
                    result.push(chars[i]);
                    i += 1;
                    continue;
                }
                // Strip the delimiters, keep content.
                for &c in &chars[i + 1..end_idx] {
                    result.push(c);
                }
                i = end_idx + 1;
                continue;
            }
            // No closing — keep as-is.
            result.push(chars[i]);
        } else {
            result.push(chars[i]);
        }
        i += 1;
    }
    result
}

/// Convert [text](url) links to just text.
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
            // Not a link — keep the bracket.
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

// ---------------------------------------------------------------------------
// Outbound handler: pap -> BlueBubbles
// ---------------------------------------------------------------------------

/// POST /pap/outbound  — receives pap OutboundEnvelope, sends via BlueBubbles.
async fn handle_pap_outbound(
    State(state): State<AppState>,
    axum::Json(envelope): axum::Json<PapOutboundEnvelope>,
) -> impl IntoResponse {
    info!(
        recipient = %envelope.recipient_ref,
        text_len = envelope.text.len(),
        "received outbound from pap, sending to BlueBubbles"
    );

    let bb_url = format!(
        "{}/api/v1/message/text",
        state.config.bb_url
    );

    let clean_text = strip_markdown(&envelope.text);
    let temp_guid = format!("pap-{}", current_epoch_ms());

    let body = BbSendTextRequest {
        chat_guid: state.config.bb_chat_guid.clone(),
        message: clean_text,
        method: "apple-script".into(),
        temp_guid,
    };

    let resp = state.http.post(&bb_url)
        .query(&[("password", state.config.bb_password.as_str())])
        .json(&body)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            if let Ok(bb_resp) = r.json::<BbSendResponse>().await {
                if let Some(guid) = bb_resp.data.and_then(|d| d.guid) {
                    let mut sent = state.sent_guids.write().await;
                    sent.insert(guid.clone());
                    info!(guid = %guid, "sent message via BlueBubbles, tracking for loop prevention");

                    // Prune if the set gets too large.
                    if sent.len() > 500 {
                        sent.clear();
                        sent.insert(guid);
                    }
                }
            } else {
                info!("sent message via BlueBubbles (could not parse response for GUID tracking)");
            }
            StatusCode::OK
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            error!(%status, %body, "BlueBubbles rejected send request");
            StatusCode::BAD_GATEWAY
        }
        Err(e) => {
            error!("failed to reach BlueBubbles: {e}");
            StatusCode::BAD_GATEWAY
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::from_filename("../../.env");
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pap_bluebubbles=info".into()),
        )
        .init();

    let config = Config::from_env()?;
    info!(
        listen = %config.listen_addr,
        bb = %config.bb_url,
        chat = %config.bb_chat_guid,
        poll_ms = config.poll_interval_ms,
        "starting BlueBubbles adapter"
    );

    let state = AppState {
        config: config.clone(),
        http: Client::new(),
        sent_guids: Arc::new(RwLock::new(HashSet::new())),
    };

    // Spawn the poller in the background.
    let poller_state = state.clone();
    tokio::spawn(async move {
        poll_loop(poller_state).await;
    });

    // Keep the HTTP server for pap's outbound replies.
    let app = Router::new()
        .route("/pap/outbound", post(handle_pap_outbound))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    info!("listening on {}", config.listen_addr);
    axum::serve(listener, app).await?;

    Ok(())
}
