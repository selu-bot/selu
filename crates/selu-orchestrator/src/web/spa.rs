//! Authenticated entry point for the standalone conversation SPA.

use axum::{
    http::{HeaderName, HeaderValue, header},
    response::{IntoResponse, Response},
};
use std::path::Path;

use crate::web::{BasePath, auth::AuthUser};

/// Docker receives `/app/ui` from the UI build stage. An IDE run uses the
/// checked-out `ui/dist` directory so there is no machine-specific setting.
pub fn ui_dir() -> String {
    if let Ok(path) = std::env::var("SELU__UI_DIR") {
        return path;
    }
    if Path::new("/app/ui/index.html").exists() {
        "/app/ui".to_owned()
    } else {
        format!("{}/../../ui/dist", env!("CARGO_MANIFEST_DIR"))
    }
}

/// The HTML shell is deliberately served through an authenticated handler.
/// JavaScript/CSS assets contain no user data and are served separately.
pub async fn app_index(_user: AuthUser, BasePath(base_path): BasePath) -> Response {
    let ui_dir = ui_dir();
    let path = format!("{ui_dir}/index.html");
    match tokio::fs::read_to_string(path).await {
        Ok(html) => {
            let headers = [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/html; charset=utf-8"),
                ),
                (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
                (
                    HeaderName::from_static("content-security-policy"),
                    // Safari still falls back to `style-src` for some style
                    // applications instead of honoring the more granular
                    // directives. Allow inline CSS at the parent level for that
                    // fallback. Inline CSS is allowed, but stylesheets otherwise
                    // remain same-origin and scripts remain strict.
                    HeaderValue::from_static(
                        "default-src 'self'; connect-src 'self'; img-src 'self' data:; script-src 'self'; script-src-attr 'none'; style-src 'self' 'unsafe-inline'; style-src-elem 'self' 'unsafe-inline'; style-src-attr 'unsafe-inline'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'; form-action 'self'",
                    ),
                ),
                (
                    HeaderName::from_static("referrer-policy"),
                    HeaderValue::from_static("same-origin"),
                ),
                (
                    HeaderName::from_static("x-content-type-options"),
                    HeaderValue::from_static("nosniff"),
                ),
                (
                    HeaderName::from_static("x-frame-options"),
                    HeaderValue::from_static("DENY"),
                ),
            ];
            (headers, html.replace("__SELU_BASE_PATH__", &base_path)).into_response()
        }
        Err(error) => {
            tracing::error!(%error, "Conversation SPA assets are unavailable");
            axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}
