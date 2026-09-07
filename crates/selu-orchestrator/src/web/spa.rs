//! Public shell and static assets for the standalone conversation SPA.

use axum::{
    Router,
    extract::Request,
    http::{HeaderName, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use std::path::Path;
use tower_http::services::ServeDir;

use crate::web::BasePath;

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

/// Routes owned by the SPA. Assets are nested separately so a missing asset is
/// always a 404 and can never be mistaken for a client-side route.
pub fn router<S>(ui_dir: String) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let assets = Router::new()
        .fallback_service(ServeDir::new(format!("{ui_dir}/assets")))
        .layer(middleware::from_fn(asset_cache_headers));

    let shell = move |BasePath(base_path): BasePath| {
        let ui_dir = ui_dir.clone();
        async move { serve_app_index(&ui_dir, &base_path).await }
    };

    let root_shell = shell.clone();
    let app = Router::new().nest("/assets", assets).fallback(get(shell));

    Router::new()
        .route("/app/", get(root_shell))
        .nest("/app", app)
}

async fn serve_app_index(ui_dir: &str, base_path: &str) -> Response {
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
            let base_path = escape_html_attribute(base_path);
            (headers, html.replace("__SELU_BASE_PATH__", &base_path)).into_response()
        }
        Err(error) => {
            tracing::error!(%error, "Conversation SPA assets are unavailable");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

async fn asset_cache_headers(req: Request, next: Next) -> Response {
    let immutable = is_hashed_asset(req.uri().path());
    let mut response = next.run(req).await;
    if immutable && response.status().is_success() {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
    }
    response
}

fn is_hashed_asset(path: &str) -> bool {
    let Some(file_name) = path.rsplit('/').next() else {
        return false;
    };
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(stem, _)| stem);
    let Some((_, hash)) = stem.rsplit_once('-') else {
        return false;
    };

    hash.len() >= 8
        && hash.bytes().all(|byte| byte.is_ascii_alphanumeric())
        && hash
            .bytes()
            .any(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase())
}

fn escape_html_attribute(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#x27;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Extension,
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };
    use tower::ServiceExt;

    static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = std::env::var_os("KIROCREW_SCRATCH")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join(format!(
                    "selu-spa-test-{}-{}",
                    std::process::id(),
                    FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(root.join("assets")).unwrap();
            fs::write(
                root.join("index.html"),
                "<meta name=\"selu-base-path\" content=\"__SELU_BASE_PATH__\"><base href=\"__SELU_BASE_PATH__/app/\"><main>shell</main>",
            )
            .unwrap();
            fs::write(root.join("assets/app-1234AbCd.js"), "asset").unwrap();
            Self(root)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    async fn response_body(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn test_router(fixture: &Fixture, base_path: &str) -> Router {
        router::<()>(fixture.0.to_string_lossy().into_owned())
            .layer(Extension(BasePath(base_path.to_owned())))
    }

    #[tokio::test]
    async fn serves_shell_for_known_deep_links_and_future_client_routes() {
        let fixture = Fixture::new();
        for path in [
            "/app/",
            "/app/login",
            "/app/setup",
            "/app/conversations",
            "/app/conversations/conversation-id",
            "/app/a/future/client/route",
        ] {
            let response = test_router(&fixture, "/selu")
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert!(response.headers().contains_key("content-security-policy"));
            assert!(response.headers().contains_key("x-content-type-options"));
            let body = response_body(response).await;
            assert!(body.contains("content=\"/selu\""), "{path}: {body}");
            assert!(
                body.contains("<base href=\"/selu/app/\">"),
                "{path}: {body}"
            );
        }
    }

    #[tokio::test]
    async fn assets_are_public_immutable_and_never_fall_back_to_shell() {
        let fixture = Fixture::new();
        let response = test_router(&fixture, "")
            .oneshot(
                Request::get("/app/assets/app-1234AbCd.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(response_body(response).await, "asset");

        let response = test_router(&fixture, "")
            .oneshot(
                Request::get("/app/assets/missing-1234AbCd.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!response_body(response).await.contains("<main>shell</main>"));
    }

    #[tokio::test]
    async fn fallback_is_scoped_to_app_routes() {
        let fixture = Fixture::new();
        for path in ["/api/health", "/webhooks/inbound", "/app-assets/nope.js"] {
            let response = test_router(&fixture, "")
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn escapes_base_path_before_injecting_it_into_html() {
        let fixture = Fixture::new();
        let response = test_router(&fixture, "\"><script>alert('x')</script>")
            .oneshot(Request::get("/app/login").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = response_body(response).await;
        assert!(!body.contains("<script>"));
        assert!(body.contains("&quot;&gt;&lt;script&gt;alert(&#x27;x&#x27;)&lt;/script&gt;"));
    }

    #[test]
    fn identifies_only_content_hashed_asset_names() {
        assert!(is_hashed_asset("/app-9ddEq3Ic.css"));
        assert!(is_hashed_asset("/chunk-12345678.js"));
        assert!(!is_hashed_asset("/app.js"));
        assert!(!is_hashed_asset("/long-production-name.js"));
    }
}
