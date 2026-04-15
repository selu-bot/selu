use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use sqlx::Row;
use tracing::error;

use crate::state::AppState;
use crate::web::auth::AuthUser;
use crate::web::BasePath;

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CacheVolumeRow {
    pub id: String,
    pub username: String,
    pub capability_id: String,
    pub created_at: String,
    pub last_active_at: String,
}

// ── Template ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "cache.html")]
struct CacheTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    volumes: Vec<CacheVolumeRow>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn cache_index(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    let volumes: Vec<CacheVolumeRow> = if user.is_admin {
        sqlx::query(
            r#"SELECT cv.id, u.username, cv.capability_id,
                      cv.created_at, COALESCE(cv.last_active_at, cv.created_at) AS last_active_at
               FROM cache_volumes cv
               JOIN users u ON cv.user_id = u.id
               WHERE cv.status = 'active'
               ORDER BY cv.last_active_at DESC"#,
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| CacheVolumeRow {
            id: r.get("id"),
            username: r.get("username"),
            capability_id: r.get("capability_id"),
            created_at: r.get("created_at"),
            last_active_at: r.get("last_active_at"),
        })
        .collect()
    } else {
        sqlx::query(
            r#"SELECT cv.id, u.username, cv.capability_id,
                      cv.created_at, COALESCE(cv.last_active_at, cv.created_at) AS last_active_at
               FROM cache_volumes cv
               JOIN users u ON cv.user_id = u.id
               WHERE cv.status = 'active' AND cv.user_id = ?
               ORDER BY cv.last_active_at DESC"#,
        )
        .bind(&user.user_id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| CacheVolumeRow {
            id: r.get("id"),
            username: r.get("username"),
            capability_id: r.get("capability_id"),
            created_at: r.get("created_at"),
            last_active_at: r.get("last_active_at"),
        })
        .collect()
    };

    match (CacheTemplate {
        active_nav: "cache",
        is_admin: user.is_admin,
        base_path,
        volumes,
    })
    .render()
    {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn cache_delete(
    user: AuthUser,
    Path(cache_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    // Admins can delete any cache; non-admins only their own.
    let restrict_user = if user.is_admin {
        None
    } else {
        Some(user.user_id.as_str())
    };

    match crate::capabilities::purge_cache_volume(&state.db, &cache_id, restrict_user).await {
        Ok(true) => Html("").into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Failed to purge cache volume {cache_id}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
