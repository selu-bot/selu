use std::collections::HashMap;

use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use sqlx::Row;
use tracing::{error, warn};

use crate::state::AppState;
use crate::web::BasePath;
use crate::web::auth::AuthUser;

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CacheVolumeRow {
    pub id: String,
    pub username: String,
    pub capability_id: String,
    pub created_at: String,
    pub last_active_at: String,
    /// Human-readable size (e.g. "1.2 GB"), or "–" if unavailable.
    pub size: String,
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
    // Fetch volume sizes from Docker in one call.
    let volume_sizes = fetch_volume_sizes().await;

    let rows = if user.is_admin {
        sqlx::query(
            r#"SELECT cv.id, u.username, cv.user_id, cv.capability_id,
                      cv.created_at, COALESCE(cv.last_active_at, cv.created_at) AS last_active_at
               FROM cache_volumes cv
               JOIN users u ON cv.user_id = u.id
               WHERE cv.status = 'active'
               ORDER BY cv.last_active_at DESC"#,
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
    } else {
        sqlx::query(
            r#"SELECT cv.id, u.username, cv.user_id, cv.capability_id,
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
    };

    let volumes: Vec<CacheVolumeRow> = rows
        .into_iter()
        .map(|r| {
            let user_id: String = r.get("user_id");
            let capability_id: String = r.get("capability_id");
            let vol_name = format!("selu-cache-{}-{}", user_id, capability_id);
            let size = volume_sizes
                .get(&vol_name)
                .map(|&bytes| format_bytes(bytes))
                .unwrap_or_else(|| "\u{2013}".to_string());
            CacheVolumeRow {
                id: r.get("id"),
                username: r.get("username"),
                capability_id,
                created_at: r.get("created_at"),
                last_active_at: r.get("last_active_at"),
                size,
            }
        })
        .collect();

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

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Fetch Docker volume sizes via the system df endpoint.
/// Returns a map of volume name → size in bytes.
async fn fetch_volume_sizes() -> HashMap<String, i64> {
    let docker = match bollard::Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(e) => {
            warn!("Cannot connect to Docker for volume sizes: {e}");
            return HashMap::new();
        }
    };

    let df = match docker.df().await {
        Ok(df) => df,
        Err(e) => {
            warn!("Docker df failed: {e}");
            return HashMap::new();
        }
    };

    let mut sizes = HashMap::new();
    if let Some(volumes) = df.volumes {
        for v in volumes {
            if let Some(usage) = v.usage_data {
                sizes.insert(v.name, usage.size);
            }
        }
    }
    sizes
}

fn format_bytes(bytes: i64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} KB", b / KB)
    } else if b < GB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.2} GB", b / GB)
    }
}
