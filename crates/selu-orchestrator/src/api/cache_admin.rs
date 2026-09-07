use axum::{
    Json, Router,
    extract::{FromRef, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use sqlx::SqlitePool;

use crate::{
    api::auth::{ApiPrincipal, internal_error},
    services::cache_admin,
};

/// Cache-volume routes intended to be merged under `/api/v1`.
pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    SqlitePool: FromRef<S>,
{
    Router::new()
        .route("/cache-volumes", get(list_cache_volumes))
        .route("/cache-volumes/{id}", delete(delete_cache_volume))
}

pub async fn list_cache_volumes(principal: ApiPrincipal, State(db): State<SqlitePool>) -> Response {
    match cache_admin::list_cache_volumes(&db, &principal).await {
        Ok(volumes) => Json(volumes).into_response(),
        Err(error) => {
            tracing::error!("Failed to list cache volumes: {error:#}");
            internal_error()
        }
    }
}

pub async fn delete_cache_volume(
    principal: ApiPrincipal,
    Path(id): Path<String>,
    State(db): State<SqlitePool>,
) -> Response {
    match cache_admin::delete_cache_volume(&db, &principal, &id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            tracing::error!(cache_volume_id = %id, "Failed to delete cache volume: {error:#}");
            internal_error()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::auth::{self, SESSION_COOKIE};
    use axum::body::Body;
    use http_body_util::BodyExt;
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    async fn test_db() -> SqlitePool {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        for (id, username, display_name, is_admin) in [
            ("admin", "admin", "Ada Admin", 1_i64),
            ("alice", "alice", "Alice", 0_i64),
            ("bob", "bob", "Bob", 0_i64),
        ] {
            sqlx::query(
                "INSERT INTO users (id, username, display_name, password_hash, is_admin, language) \
                 VALUES (?, ?, ?, 'test', ?, 'en')",
            )
            .bind(id)
            .bind(username)
            .bind(display_name)
            .bind(is_admin)
            .execute(&db)
            .await
            .unwrap();
        }
        for (id, user_id, capability) in [
            ("alice-cache", "alice", "cargo"),
            ("bob-cache", "bob", "npm"),
        ] {
            sqlx::query(
                "INSERT INTO cache_volumes (id, user_id, capability_id, status) \
                 VALUES (?, ?, ?, 'active')",
            )
            .bind(id)
            .bind(user_id)
            .bind(capability)
            .execute(&db)
            .await
            .unwrap();
        }
        db
    }

    async fn cookie_for(db: &SqlitePool, user_id: &str) -> String {
        let session = auth::create_session(db, user_id).await.unwrap();
        format!("{SESSION_COOKIE}={session}")
    }

    async fn json(response: Response) -> serde_json::Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn router_enforces_authentication_and_user_scoping() {
        let db = test_db().await;
        let app = router::<SqlitePool>().with_state(db.clone());

        let anonymous = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/cache-volumes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

        let alice_cookie = cookie_for(&db, "alice").await;
        let alice_response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/cache-volumes")
                    .header("cookie", &alice_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(alice_response.status(), StatusCode::OK);
        let alice_json = json(alice_response).await;
        assert_eq!(alice_json.as_array().unwrap().len(), 1);
        assert_eq!(alice_json[0]["id"], "alice-cache");
        assert!(alice_json[0].get("owner_display_name").is_none());
        assert!(alice_json[0]["size_bytes"].is_null());

        let cross_user_delete = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/cache-volumes/bob-cache")
                    .header("cookie", &alice_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cross_user_delete.status(), StatusCode::NOT_FOUND);

        let admin_cookie = cookie_for(&db, "admin").await;
        let admin_response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/cache-volumes")
                    .header("cookie", admin_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(admin_response.status(), StatusCode::OK);
        let admin_json = json(admin_response).await;
        assert_eq!(admin_json.as_array().unwrap().len(), 2);
        assert_eq!(admin_json[0]["owner_display_name"], "Alice");
        assert_eq!(admin_json[1]["owner_display_name"], "Bob");
    }
}
