/// Built-in HTTP/HTTPS egress proxy for capability containers.
///
/// Each capability container is given HTTP_PROXY / HTTPS_PROXY pointing here,
/// with a unique per-container token embedded as basic-auth credentials:
///   HTTP_PROXY=http://selu:<token>@host.docker.internal:8888
///
/// The proxy:
///   - Extracts the Proxy-Authorization header to identify the container
///   - Looks up the capability's declared network allow-list by token
///   - Allows or denies each connection
///   - Logs every request (allowed + denied) via an mpsc channel
///   - For HTTPS (CONNECT): tunnels bytes after checking hostname
///   - For HTTP: forwards the request after checking Host header
///
/// Implementation uses hyper's HTTP/1 server for correct HTTP parsing and
/// upgrade handling (CONNECT tunnels via the HTTP upgrade mechanism).
use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{self, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};

use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use http_body_util::{BodyExt, Full};
use bytes::Bytes;

use crate::capabilities::manifest::NetworkMode;

/// An allow-list entry for one running capability container
#[derive(Debug, Clone)]
pub struct ContainerEgressPolicy {
    pub capability_id: String,
    pub mode: NetworkMode,
    pub allowed_hosts: Vec<String>, // "hostname:port" or "hostname" (any port)
}

impl ContainerEgressPolicy {
    pub fn allows(&self, host: &str, port: u16) -> bool {
        match self.mode {
            NetworkMode::None => false,
            NetworkMode::Any => true,
            NetworkMode::Allowlist => {
                let target_with_port = format!("{}:{}", host, port);
                self.allowed_hosts.iter().any(|entry| {
                    if entry.contains(':') {
                        // Entry specifies a port — must match exactly
                        entry == &target_with_port
                    } else {
                        // Entry is bare hostname — allows any port
                        entry == host
                    }
                })
            }
        }
    }
}

/// Registry mapping proxy auth token → egress policy.
/// Each container gets a unique random token injected as proxy credentials.
pub type EgressRegistry = Arc<RwLock<HashMap<String, ContainerEgressPolicy>>>;

pub fn new_registry() -> EgressRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Register a container's egress policy when it starts.
/// `token` is the unique per-container auth token.
pub async fn register(registry: &EgressRegistry, token: String, policy: ContainerEgressPolicy) {
    let cap_id = policy.capability_id.clone();
    registry.write().await.insert(token.clone(), policy);
    debug!(capability = %cap_id, "Registered egress policy");
}

/// Remove a container's egress policy when it stops.
pub async fn deregister(registry: &EgressRegistry, token: &str) {
    registry.write().await.remove(token);
    debug!("Removed egress policy");
}

// ── Egress log ────────────────────────────────────────────────────────────────

/// A single egress request log entry, emitted by the proxy and persisted
/// to the `egress_log` table by a background drain task.
#[derive(Debug, Clone)]
pub struct EgressLogEntry {
    pub capability_id: String,
    pub method: String,
    pub host: String,
    pub port: u16,
    pub allowed: bool,
}

/// Sender half — passed into the proxy. Receiver half is drained by
/// `drain_egress_log` in a background task.
pub type EgressLogSender = mpsc::Sender<EgressLogEntry>;
pub type EgressLogReceiver = mpsc::Receiver<EgressLogEntry>;

/// Create a new egress log channel.
pub fn new_log_channel(capacity: usize) -> (EgressLogSender, EgressLogReceiver) {
    mpsc::channel(capacity)
}

/// Background task: drains the egress log channel into the database.
///
/// Batches writes for efficiency — accumulates up to 50 entries or 1 second,
/// whichever comes first.
pub async fn drain_egress_log(db: sqlx::SqlitePool, mut rx: EgressLogReceiver) {
    let mut batch: Vec<EgressLogEntry> = Vec::with_capacity(50);

    loop {
        // Wait for the first entry or channel close
        match rx.recv().await {
            Some(entry) => batch.push(entry),
            None => {
                // Channel closed — flush remaining and exit
                if !batch.is_empty() {
                    flush_batch(&db, &batch).await;
                }
                return;
            }
        }

        // Drain any additional entries that are immediately available
        while batch.len() < 50 {
            match rx.try_recv() {
                Ok(entry) => batch.push(entry),
                Err(_) => break,
            }
        }

        flush_batch(&db, &batch).await;
        batch.clear();
    }
}

async fn flush_batch(db: &sqlx::SqlitePool, batch: &[EgressLogEntry]) {
    for entry in batch {
        let id = uuid::Uuid::new_v4().to_string();
        let allowed: i32 = if entry.allowed { 1 } else { 0 };
        let port = entry.port as i32;
        let _ = sqlx::query(
            "INSERT INTO egress_log (id, capability_id, method, host, port, allowed) VALUES (?, ?, ?, ?, ?, ?)"
        )
        .bind(&id)
        .bind(&entry.capability_id)
        .bind(&entry.method)
        .bind(&entry.host)
        .bind(port)
        .bind(allowed)
        .execute(db)
        .await;
    }
}

// ── Proxy server (hyper-based) ───────────────────────────────────────────────

/// Start the egress proxy server.
///
/// Uses hyper's HTTP/1 server for proper request parsing and CONNECT
/// upgrade handling.  Auth, policy, and logging are layered on top.
pub async fn run_proxy(
    listen_addr: SocketAddr,
    registry: EgressRegistry,
    log_tx: EgressLogSender,
) -> Result<()> {
    let listener = TcpListener::bind(listen_addr).await?;
    info!(addr = %listen_addr, "Egress proxy listening");

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let registry = registry.clone();
        let log_tx = log_tx.clone();

        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let registry = registry.clone();
            let log_tx = log_tx.clone();

            let service = service_fn(move |req| {
                let registry = registry.clone();
                let log_tx = log_tx.clone();
                async move { proxy_request(req, registry, log_tx).await }
            });

            if let Err(e) = http1::Builder::new()
                .preserve_header_case(true)
                .title_case_headers(false)
                .serve_connection(io, service)
                .with_upgrades()
                .await
            {
                debug!(peer = %peer_addr, "Proxy connection error: {e}");
            }
        });
    }
}

/// Handle a single proxy request (both CONNECT and plain HTTP).
///
/// For CONNECT requests:
///   1. Extract credentials from Proxy-Authorization header
///   2. If missing, return 407 to trigger the browser's challenge-response
///   3. If present, check policy and either deny (403) or establish tunnel
///   4. On success, return 200 and use hyper::upgrade to get raw IO
///   5. Bidirectional copy between client and upstream
///
/// hyper handles all HTTP parsing, buffering, and upgrade mechanics.
async fn proxy_request(
    req: Request<Incoming>,
    registry: EgressRegistry,
    log_tx: EgressLogSender,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    // Extract the auth token from Proxy-Authorization header
    let token = extract_proxy_auth_token_from_headers(req.headers());

    // If no credentials, challenge with 407 (browser will retry with auth)
    if token.is_none() {
        let target = req.uri().to_string();
        debug!(target = %target, "No proxy credentials, sending 407 challenge");
        let resp = Response::builder()
            .status(StatusCode::PROXY_AUTHENTICATION_REQUIRED)
            .header("Proxy-Authenticate", "Basic realm=\"selu-egress\"")
            .header("Content-Length", "0")
            .body(Full::new(Bytes::new()))
            .unwrap();
        return Ok(resp);
    }

    let token = token.unwrap();

    // Look up policy by token
    let policy = {
        let reg = registry.read().await;
        reg.get(&token).cloned()
    };

    let ident = token.clone();

    if req.method() == Method::CONNECT {
        handle_connect_hyper(req, &ident, policy, &log_tx).await
    } else {
        handle_http_hyper(req, &ident, policy, &log_tx).await
    }
}

/// Handle CONNECT requests using hyper's upgrade mechanism.
///
/// This is the correct way to implement CONNECT proxying: hyper handles
/// the HTTP layer, we return a 200 response, and then use the upgrade
/// API to get raw bidirectional IO for the tunnel.
async fn handle_connect_hyper(
    req: Request<Incoming>,
    ident: &str,
    policy: Option<ContainerEgressPolicy>,
    log_tx: &EgressLogSender,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let target = req.uri().authority().map(|a| a.to_string())
        .unwrap_or_else(|| req.uri().to_string());
    let (host, port) = parse_host_port(&target, 443);

    let allowed = is_allowed(&host, port, ident, &policy);
    let cap_id = policy.as_ref()
        .map(|p| p.capability_id.clone())
        .unwrap_or_else(|| "unknown".to_string());

    // Log the request
    let _ = log_tx.try_send(EgressLogEntry {
        capability_id: cap_id.clone(),
        method: "CONNECT".to_string(),
        host: host.clone(),
        port,
        allowed,
    });

    if !allowed {
        warn!(
            ident = %ident,
            target = %target,
            capability = %cap_id,
            "EGRESS DENIED"
        );
        let resp = Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Full::new(Bytes::new()))
            .unwrap();
        return Ok(resp);
    }

    debug!(ident = %ident, target = %target, "EGRESS ALLOWED (CONNECT)");

    // Spawn the tunnel task.  We need to do the upstream connect and
    // bidirectional copy after hyper has sent the 200 and completed the
    // upgrade, so we spawn a task that awaits the upgrade future.
    let target_addr = format!("{}:{}", host, port);
    let ident_owned = ident.to_string();

    tokio::spawn(async move {
        // Wait for hyper to finish sending the 200 and give us raw IO
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let mut client = TokioIo::new(upgraded);

                // Connect to the real destination
                match TcpStream::connect(&target_addr).await {
                    Ok(mut upstream) => {
                        // Bidirectional byte tunnel
                        let result = io::copy_bidirectional(&mut client, &mut upstream).await;
                        if let Err(e) = result {
                            debug!(
                                ident = %ident_owned,
                                target = %target_addr,
                                "Tunnel closed: {e}"
                            );
                        }
                    }
                    Err(e) => {
                        warn!(
                            ident = %ident_owned,
                            target = %target_addr,
                            "Failed to connect upstream: {e}"
                        );
                        // Try to send an error back (best effort — client
                        // already received the 200, so this may fail)
                        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    }
                }
            }
            Err(e) => {
                debug!(
                    ident = %ident_owned,
                    target = %target_addr,
                    "Upgrade failed: {e}"
                );
            }
        }
    });

    // Return 200 to the client — hyper will send this and then hand over
    // the raw connection to the upgrade future above.
    let resp = Response::builder()
        .status(StatusCode::OK)
        .body(Full::new(Bytes::new()))
        .unwrap();
    Ok(resp)
}

/// Handle plain HTTP proxy requests.
///
/// Reads the full request body, connects to upstream, forwards the
/// request, and streams the response back.
async fn handle_http_hyper(
    req: Request<Incoming>,
    ident: &str,
    policy: Option<ContainerEgressPolicy>,
    log_tx: &EgressLogSender,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    // Extract host from the request URI or Host header
    let host_str = req.uri().host().map(|h| {
        if let Some(port) = req.uri().port_u16() {
            format!("{}:{}", h, port)
        } else {
            h.to_string()
        }
    }).or_else(|| {
        req.headers().get("host").and_then(|v| v.to_str().ok()).map(|s| s.to_string())
    }).unwrap_or_default();

    let method = req.method().to_string();
    let (hostname, port) = parse_host_port(&host_str, 80);
    let allowed = is_allowed(&hostname, port, ident, &policy);
    let cap_id = policy.as_ref()
        .map(|p| p.capability_id.clone())
        .unwrap_or_else(|| "unknown".to_string());

    // Log the request
    let _ = log_tx.try_send(EgressLogEntry {
        capability_id: cap_id.clone(),
        method: method.clone(),
        host: hostname.clone(),
        port,
        allowed,
    });

    if !allowed {
        warn!(
            ident = %ident,
            host = %host_str,
            method = %method,
            capability = %cap_id,
            "EGRESS DENIED"
        );
        let resp = Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header("Content-Length", "0")
            .body(Full::new(Bytes::new()))
            .unwrap();
        return Ok(resp);
    }

    debug!(ident = %ident, host = %host_str, method = %method, "EGRESS ALLOWED (HTTP)");

    // Reconstruct the request to forward to upstream.
    // For proxy requests the URI is absolute (http://host/path), so we
    // need to extract just the path+query for the origin request.
    let path = req.uri().path_and_query()
        .map(|pq| pq.to_string())
        .unwrap_or_else(|| "/".to_string());

    // Collect the request body
    let body_bytes = match req.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            warn!("Failed to read request body: {e}");
            let resp = Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Full::new(Bytes::new()))
                .unwrap();
            return Ok(resp);
        }
    };

    // Connect to upstream and forward the request using raw TCP.
    // We build the HTTP/1.1 request manually to preserve proxy semantics.
    match TcpStream::connect(format!("{}:{}", hostname, port)).await {
        Ok(mut upstream) => {
            // Build HTTP request
            let mut raw_req = format!("{} {} HTTP/1.1\r\nHost: {}\r\n", method, path, host_str);
            // Note: we don't forward Proxy-Authorization to the origin
            // We would forward other headers here in a more complete impl.
            raw_req.push_str(&format!("Content-Length: {}\r\n", body_bytes.len()));
            raw_req.push_str("\r\n");

            let _ = upstream.write_all(raw_req.as_bytes()).await;
            if !body_bytes.is_empty() {
                let _ = upstream.write_all(&body_bytes).await;
            }

            // Read the entire response from upstream
            let mut response_buf = Vec::new();
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut upstream, &mut response_buf).await;

            let resp = Response::builder()
                .status(StatusCode::OK)
                .body(Full::new(Bytes::from(response_buf)))
                .unwrap();
            Ok(resp)
        }
        Err(e) => {
            warn!(host = %host_str, "Failed to connect upstream: {e}");
            let resp = Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Full::new(Bytes::new()))
                .unwrap();
            Ok(resp)
        }
    }
}

/// Extract the token from a `Proxy-Authorization: Basic <base64>` header.
/// We expect the credentials to be `selu:<token>`, so we decode and extract the token part.
fn extract_proxy_auth_token_from_headers(headers: &hyper::HeaderMap) -> Option<String> {
    let value = headers.get("proxy-authorization")?.to_str().ok()?;
    let encoded = value.strip_prefix("Basic ").or_else(|| value.strip_prefix("basic "))?;
    let decoded = String::from_utf8(base64_decode(encoded.trim())?).ok()?;
    // Format is "selu:<token>" — extract the token after the colon
    let token = decoded.splitn(2, ':').nth(1)?;
    Some(token.to_string())
}

/// Simple base64 decode (standard alphabet, no padding required)
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .ok()
}

fn is_allowed(host: &str, port: u16, ident: &str, policy: &Option<ContainerEgressPolicy>) -> bool {
    match policy {
        None => {
            // No registered policy = unknown container; deny by default
            warn!(ident = %ident, host = %host, "Unknown container — egress denied");
            false
        }
        Some(p) => p.allows(host, port),
    }
}

fn parse_host_port(host_port: &str, default_port: u16) -> (String, u16) {
    // Handle IPv6 "[::1]:port"
    if host_port.starts_with('[') {
        if let Some(close) = host_port.find(']') {
            let host = host_port[1..close].to_string();
            let port = host_port[close + 1..]
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(default_port);
            return (host, port);
        }
    }
    match host_port.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(default_port)),
        None => (host_port.to_string(), default_port),
    }
}

// ── Legacy helper (kept for test compatibility) ──────────────────────────────

/// Extract the token from a raw request string (used only in tests).
#[cfg(test)]
fn extract_proxy_auth_token(request: &str) -> Option<String> {
    for line in request.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with("proxy-authorization:") {
            let value = line.splitn(2, ':').nth(1)?.trim();
            if let Some(encoded) = value.strip_prefix("Basic ").or_else(|| value.strip_prefix("basic ")) {
                let decoded = String::from_utf8(
                    base64_decode(encoded.trim())?
                ).ok()?;
                let token = decoded.splitn(2, ':').nth(1)?;
                return Some(token.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::manifest::NetworkMode;

    fn policy(mode: NetworkMode, hosts: &[&str]) -> ContainerEgressPolicy {
        ContainerEgressPolicy {
            capability_id: "test".into(),
            mode,
            allowed_hosts: hosts.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn test_allowlist() {
        let p = policy(NetworkMode::Allowlist, &["api.openweathermap.org:443"]);
        assert!(p.allows("api.openweathermap.org", 443));
        assert!(!p.allows("evil.com", 443));
        assert!(!p.allows("api.openweathermap.org", 80));
    }

    #[test]
    fn test_none_blocks_all() {
        let p = policy(NetworkMode::None, &[]);
        assert!(!p.allows("anything.com", 443));
    }

    #[test]
    fn test_any_allows_all() {
        let p = policy(NetworkMode::Any, &[]);
        assert!(p.allows("evil.com", 443));
    }

    #[test]
    fn test_host_without_port_matches_any_port() {
        let p = policy(NetworkMode::Allowlist, &["example.com"]);
        assert!(p.allows("example.com", 443));
        assert!(p.allows("example.com", 80));
        assert!(!p.allows("other.com", 443));
    }

    #[test]
    fn test_parse_host_port() {
        assert_eq!(parse_host_port("example.com:443", 80), ("example.com".into(), 443));
        assert_eq!(parse_host_port("example.com", 80), ("example.com".into(), 80));
        assert_eq!(parse_host_port("[::1]:8080", 80), ("::1".into(), 8080));
    }

    #[test]
    fn test_extract_proxy_auth_token() {
        // "selu:my-secret-token" in base64 is "c2VsdTpteS1zZWNyZXQtdG9rZW4="
        let request = "CONNECT example.com:443 HTTP/1.1\r\nProxy-Authorization: Basic c2VsdTpteS1zZWNyZXQtdG9rZW4=\r\n\r\n";
        assert_eq!(extract_proxy_auth_token(request), Some("my-secret-token".into()));

        // No header
        let request = "CONNECT example.com:443 HTTP/1.1\r\n\r\n";
        assert_eq!(extract_proxy_auth_token(request), None);
    }
}
