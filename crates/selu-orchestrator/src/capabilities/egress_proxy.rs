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
use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};

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

// ── Proxy server ──────────────────────────────────────────────────────────────

/// Start the egress proxy server.
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
            if let Err(e) = handle_connection(stream, peer_addr, registry, log_tx).await {
                debug!(peer = %peer_addr, "Proxy connection error: {e}");
            }
        });
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    _peer_addr: SocketAddr,
    registry: EgressRegistry,
    log_tx: EgressLogSender,
) -> Result<()> {
    // Read the initial request
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }

    let request_str = String::from_utf8_lossy(&buf[..n]);
    let first_line = request_str.lines().next().unwrap_or("");

    // Parse method, target, version from "METHOD target HTTP/x.x"
    let parts: Vec<&str> = first_line.splitn(3, ' ').collect();
    if parts.len() < 3 {
        stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await?;
        return Ok(());
    }

    let method = parts[0];
    let target = parts[1];

    // Extract the auth token from the Proxy-Authorization header.
    // Format: "Proxy-Authorization: Basic <base64(selu:token)>"
    let token = extract_proxy_auth_token(&request_str);

    // Look up policy by token
    let policy = match &token {
        Some(t) => {
            let reg = registry.read().await;
            reg.get(t).cloned()
        }
        None => None,
    };

    let ident = token.as_deref().unwrap_or("no-token");

    match method {
        "CONNECT" => handle_connect(stream, target, ident, policy, &log_tx).await,
        _ => handle_http(stream, method, target, &buf[..n], ident, policy, &log_tx).await,
    }
}

/// Extract the token from a `Proxy-Authorization: Basic <base64>` header.
/// We expect the credentials to be `selu:<token>`, so we decode and extract the token part.
fn extract_proxy_auth_token(request: &str) -> Option<String> {
    for line in request.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with("proxy-authorization:") {
            let value = line.splitn(2, ':').nth(1)?.trim();
            if let Some(encoded) = value.strip_prefix("Basic ").or_else(|| value.strip_prefix("basic ")) {
                let decoded = String::from_utf8(
                    base64_decode(encoded.trim())?
                ).ok()?;
                // Format is "selu:<token>" — extract the token after the colon
                let token = decoded.splitn(2, ':').nth(1)?;
                return Some(token.to_string());
            }
        }
    }
    None
}

/// Simple base64 decode (standard alphabet, no padding required)
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .ok()
}

/// Handle HTTPS CONNECT tunnelling
async fn handle_connect(
    mut client: TcpStream,
    target: &str, // "hostname:port"
    ident: &str,
    policy: Option<ContainerEgressPolicy>,
    log_tx: &EgressLogSender,
) -> Result<()> {
    let (host, port) = parse_host_port(target, 443);
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
        client.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await?;
        return Ok(());
    }

    debug!(ident = %ident, target = %target, "EGRESS ALLOWED (CONNECT)");

    // Connect to the real destination
    let upstream = TcpStream::connect(format!("{}:{}", host, port)).await?;

    // Tell the client the tunnel is ready
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;

    // Bidirectional byte tunnel
    let (mut cr, mut cw) = client.into_split();
    let (mut ur, mut uw) = upstream.into_split();

    let c_to_u = tokio::spawn(async move { io::copy(&mut cr, &mut uw).await });
    let u_to_c = tokio::spawn(async move { io::copy(&mut ur, &mut cw).await });

    let _ = tokio::join!(c_to_u, u_to_c);
    Ok(())
}

/// Handle plain HTTP requests (forwarded to origin)
async fn handle_http(
    mut client: TcpStream,
    method: &str,
    target: &str,
    raw_request: &[u8],
    ident: &str,
    policy: Option<ContainerEgressPolicy>,
    log_tx: &EgressLogSender,
) -> Result<()> {
    // Extract host from either the target URL or the Host header
    let host_str = if target.starts_with("http://") {
        // Absolute URL: "http://hostname[:port]/path"
        target
            .strip_prefix("http://")
            .and_then(|s| s.split('/').next())
            .unwrap_or(target)
            .to_string()
    } else {
        // Look for Host: header
        String::from_utf8_lossy(raw_request)
            .lines()
            .find(|l| l.to_lowercase().starts_with("host:"))
            .and_then(|l| l.splitn(2, ':').nth(1))
            .map(|h| h.trim().to_string())
            .unwrap_or_default()
    };

    let (hostname, port) = parse_host_port(&host_str, 80);
    let allowed = is_allowed(&hostname, port, ident, &policy);
    let cap_id = policy.as_ref()
        .map(|p| p.capability_id.clone())
        .unwrap_or_else(|| "unknown".to_string());

    // Log the request
    let _ = log_tx.try_send(EgressLogEntry {
        capability_id: cap_id.clone(),
        method: method.to_string(),
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
        client.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n").await?;
        return Ok(());
    }

    debug!(ident = %ident, host = %host_str, method = %method, "EGRESS ALLOWED (HTTP)");

    // Forward to upstream
    let mut upstream = TcpStream::connect(format!("{}:{}", hostname, port)).await?;
    upstream.write_all(raw_request).await?;

    let (mut cr, mut cw) = client.into_split();
    let (mut ur, mut uw) = upstream.into_split();

    let _ = tokio::join!(
        tokio::spawn(async move { io::copy(&mut ur, &mut cw).await }),
        tokio::spawn(async move { io::copy(&mut cr, &mut uw).await }),
    );
    Ok(())
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
