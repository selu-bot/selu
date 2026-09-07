pub mod spa;

use crate::{api::auth::ApiPrincipal, state::AppState};
use axum::{
    Router,
    extract::{FromRequestParts, Path},
    http::{Uri, request::Parts},
    response::Redirect,
    routing::get,
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

/// Validate and canonicalize a reverse-proxy path prefix. Prefixes are made
/// from URL path segments only: no query/fragment delimiters, backslashes,
/// control characters, empty segments, or traversal segments are accepted.
fn normalize_base_path(raw: &str) -> Option<String> {
    if raw.is_empty() || raw == "/" {
        return Some(String::new());
    }
    if !raw.starts_with('/') || raw.ends_with("//") {
        return None;
    }

    let normalized = raw.trim_end_matches('/');
    if normalized.is_empty() {
        return Some(String::new());
    }

    for segment in normalized[1..].split('/') {
        if segment.is_empty() || matches!(segment, "." | "..") {
            return None;
        }
        let bytes = segment.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                decoded.push(byte);
                index += 1;
            } else if byte == b'%'
                && index + 2 < bytes.len()
                && bytes[index + 1].is_ascii_hexdigit()
                && bytes[index + 2].is_ascii_hexdigit()
            {
                let decoded_byte = (hex_value(bytes[index + 1]) << 4) | hex_value(bytes[index + 2]);
                if decoded_byte.is_ascii_control()
                    || matches!(
                        decoded_byte,
                        b'/' | b'\\' | b'?' | b'#' | b'<' | b'>' | b'\"' | b'\''
                    )
                {
                    return None;
                }
                decoded.push(decoded_byte);
                index += 3;
            } else {
                return None;
            }
        }
        if decoded == b"." || decoded == b".." {
            return None;
        }
    }

    Some(normalized.to_owned())
}

fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => unreachable!("hex digits are validated before decoding"),
    }
}

/// Strip a validated prefix only at a complete path-segment boundary.
fn strip_base_path<'a>(path: &'a str, base_path: &str) -> Option<&'a str> {
    if base_path.is_empty() {
        return None;
    }
    if path == base_path {
        return Some("/");
    }
    path.strip_prefix(base_path)
        .filter(|remainder| remainder.starts_with('/'))
}

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
///   2. Strips that prefix from the request URI (`/selu/app/` → `/app/`)
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
        // ── 1. Resolve and validate base path ────────────────────────────
        let configured_bp = normalize_base_path(&self.state.base_path).unwrap_or_default();
        let bp = req
            .headers()
            .get("x-forwarded-prefix")
            .and_then(|value| value.to_str().ok())
            .and_then(normalize_base_path)
            .unwrap_or(configured_bp);

        // ── 2. Strip prefix from URI ─────────────────────────────────────
        if let Some(stripped) = strip_base_path(req.uri().path(), &bp) {
            let new_uri = if let Some(query) = req.uri().query() {
                format!("{stripped}?{query}")
            } else {
                stripped.to_owned()
            };
            if let Ok(parsed) = new_uri.parse::<Uri>() {
                *req.uri_mut() = parsed;
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

impl<S: Send + Sync> FromRequestParts<S> for BasePath {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<BasePath>()
            .cloned()
            .unwrap_or(BasePath(String::new())))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for ExternalOrigin {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<ExternalOrigin>()
            .cloned()
            .unwrap_or(ExternalOrigin(String::new())))
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Build a redirect target that respects the resolved base path.
/// Example: `prefixed_redirect("/selu", "/app/")` → `Redirect::to("/selu/app/")`
pub fn prefixed_redirect(base_path: &str, path: &str) -> Redirect {
    Redirect::to(&format!("{}{}", base_path, path))
}

// ── SPA and compatibility redirects ─────────────────────────────────────────

async fn root_redirect(BasePath(base_path): BasePath) -> Redirect {
    prefixed_redirect(&base_path, "/app/")
}

async fn app_redirect(BasePath(base_path): BasePath) -> Redirect {
    prefixed_redirect(&base_path, "/app/")
}

async fn login_redirect(BasePath(base_path): BasePath) -> Redirect {
    prefixed_redirect(&base_path, "/app/login")
}

async fn setup_redirect(BasePath(base_path): BasePath) -> Redirect {
    prefixed_redirect(&base_path, "/app/setup")
}

async fn connectors_redirect(_principal: ApiPrincipal, BasePath(base_path): BasePath) -> Redirect {
    prefixed_redirect(&base_path, "/app/connectors")
}

async fn agents_redirect(_principal: ApiPrincipal, BasePath(base_path): BasePath) -> Redirect {
    prefixed_redirect(&base_path, "/app/agents")
}

async fn agent_redirect(
    _principal: ApiPrincipal,
    Path(agent_id): Path<String>,
    BasePath(base_path): BasePath,
) -> Redirect {
    prefixed_redirect(&base_path, &format!("/app/agents/{agent_id}"))
}

async fn updates_redirect(_principal: ApiPrincipal, BasePath(base_path): BasePath) -> Redirect {
    prefixed_redirect(&base_path, "/app/updates")
}

async fn settings_redirect(_principal: ApiPrincipal, BasePath(base_path): BasePath) -> Redirect {
    prefixed_redirect(&base_path, "/app/settings")
}

async fn connections_redirect(_principal: ApiPrincipal, BasePath(base_path): BasePath) -> Redirect {
    prefixed_redirect(&base_path, "/app/connections")
}

async fn automations_redirect(_principal: ApiPrincipal, BasePath(base_path): BasePath) -> Redirect {
    prefixed_redirect(&base_path, "/app/automations")
}

async fn people_redirect(_principal: ApiPrincipal, BasePath(base_path): BasePath) -> Redirect {
    prefixed_redirect(&base_path, "/app/people")
}

async fn about_redirect(_principal: ApiPrincipal, BasePath(base_path): BasePath) -> Redirect {
    prefixed_redirect(&base_path, "/app/about-you")
}

async fn feedback_redirect(_principal: ApiPrincipal, BasePath(base_path): BasePath) -> Redirect {
    prefixed_redirect(&base_path, "/app/feedback")
}

// The web layer serves only the React shell and backward-compatible GET
// redirects. Every read and mutation is owned by a versioned JSON API.
pub fn router(state: AppState) -> Router<AppState> {
    let ui_dir = spa::ui_dir();
    Router::new()
        .route("/", get(root_redirect))
        .route("/login", get(login_redirect))
        .route("/setup", get(setup_redirect))
        .route("/app", get(app_redirect))
        .merge(spa::router(ui_dir))
        .route("/pipes", get(connectors_redirect))
        .route("/pipes/new", get(connectors_redirect))
        .route("/pipes/new/{pipe_type}", get(connectors_redirect))
        .route("/pipes/webhook/new", get(connectors_redirect))
        .route("/pipes/web/new", get(connectors_redirect))
        .route("/pipes/imessage/setup", get(connectors_redirect))
        .route("/pipes/imessage/{config_id}", get(connectors_redirect))
        .route("/pipes/telegram/setup", get(connectors_redirect))
        .route("/pipes/telegram/{config_id}", get(connectors_redirect))
        .route("/pipes/whatsapp/setup", get(connectors_redirect))
        .route("/pipes/whatsapp/{config_id}", get(connectors_redirect))
        .route("/agents", get(agents_redirect))
        .route("/agents/{agent_id}", get(agent_redirect))
        .route("/agents/{agent_id}/storage", get(agent_redirect))
        .route("/agents/{agent_id}/memory", get(agent_redirect))
        .route("/agents/{agent_id}/network", get(agent_redirect))
        .route("/agents/{agent_id}/permissions", get(agent_redirect))
        .route("/agents/{agent_id}/improvement", get(agent_redirect))
        .route("/agents/{agent_id}/secrets", get(agent_redirect))
        .route("/agents/{agent_id}/setup", get(agent_redirect))
        .route("/subscriptions", get(agents_redirect))
        .route("/system-updates", get(updates_redirect))
        .route("/cache", get(settings_redirect))
        .route("/credentials", get(settings_redirect))
        .route("/mobile", get(settings_redirect))
        .route("/providers", get(connections_redirect))
        .route("/providers/new", get(connections_redirect))
        .route("/providers/new/{provider_id}", get(connections_redirect))
        .route("/schedules", get(automations_redirect))
        .route("/users", get(people_redirect))
        .route("/personality", get(about_redirect))
        .route("/personality/{id}/edit", get(about_redirect))
        .route("/personality/{id}/row", get(about_redirect))
        .route("/feedback", get(feedback_redirect))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::{normalize_base_path, strip_base_path};

    #[test]
    fn validates_and_canonicalizes_base_paths() {
        assert_eq!(normalize_base_path(""), Some(String::new()));
        assert_eq!(normalize_base_path("/"), Some(String::new()));
        assert_eq!(normalize_base_path("/selu/"), Some("/selu".to_owned()));
        assert_eq!(
            normalize_base_path("/tenant/selu"),
            Some("/tenant/selu".to_owned())
        );
        assert_eq!(
            normalize_base_path("/tenant%20one/selu"),
            Some("/tenant%20one/selu".to_owned())
        );
    }

    #[test]
    fn rejects_malicious_or_ambiguous_base_paths() {
        for prefix in [
            "selu",
            "/selu//nested",
            "/selu/../admin",
            "/selu/./app",
            "/selu?<script>",
            "/selu#fragment",
            "/selu\\app",
            "/selu/\"onload=alert(1)",
            "/selu/%xx",
            "/selu/%2e%2e/admin",
            "/selu/%2Fadmin",
            "/selu/%22onload",
        ] {
            assert_eq!(normalize_base_path(prefix), None, "{prefix}");
        }
    }

    #[test]
    fn strips_prefixes_only_at_segment_boundaries() {
        assert_eq!(strip_base_path("/selu", "/selu"), Some("/"));
        assert_eq!(
            strip_base_path("/selu/app/login", "/selu"),
            Some("/app/login")
        );
        assert_eq!(strip_base_path("/selux/app/login", "/selu"), None);
        assert_eq!(strip_base_path("/sel", "/selu"), None);
        assert_eq!(strip_base_path("/app/login", "/selu"), None);
    }
}
