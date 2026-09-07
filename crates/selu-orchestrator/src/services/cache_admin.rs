use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Serialize;
use tracing::warn;

use crate::api::auth::ApiPrincipal;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CacheVolume {
    pub id: String,
    pub capability: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_display_name: Option<String>,
    pub last_used: String,
    pub size_bytes: Option<i64>,
    pub state: String,
}

#[derive(Debug)]
struct CacheVolumeRecord {
    id: String,
    user_id: String,
    capability: String,
    owner_display_name: String,
    last_used: String,
    state: String,
}

/// List cache volumes visible to the authenticated principal.
///
/// Docker usage data is advisory. If Docker is unavailable or omits a volume,
/// `size_bytes` is returned as `None` instead of failing the request.
pub async fn list_cache_volumes(
    db: &sqlx::SqlitePool,
    principal: &ApiPrincipal,
) -> Result<Vec<CacheVolume>> {
    let sizes = fetch_docker_volume_sizes().await;
    list_cache_volumes_with_sizes(db, principal, &sizes).await
}

async fn list_cache_volumes_with_sizes(
    db: &sqlx::SqlitePool,
    principal: &ApiPrincipal,
    sizes: &HashMap<String, i64>,
) -> Result<Vec<CacheVolume>> {
    let records: Vec<CacheVolumeRecord> = if principal.is_admin {
        sqlx::query!(
            r#"SELECT cv.id AS "id!",
                      cv.user_id AS "user_id!",
                      cv.capability_id AS "capability!",
                      u.display_name AS "owner_display_name!",
                      COALESCE(cv.last_active_at, cv.created_at) AS "last_used!",
                      cv.status AS "state!"
               FROM cache_volumes cv
               JOIN users u ON u.id = cv.user_id
               WHERE cv.status = 'active'
               ORDER BY COALESCE(cv.last_active_at, cv.created_at) DESC, cv.id ASC"#
        )
        .fetch_all(db)
        .await
        .context("failed to list cache volumes")?
        .into_iter()
        .map(|row| CacheVolumeRecord {
            id: row.id,
            user_id: row.user_id,
            capability: row.capability,
            owner_display_name: row.owner_display_name,
            last_used: row.last_used,
            state: row.state,
        })
        .collect()
    } else {
        sqlx::query!(
            r#"SELECT cv.id AS "id!",
                      cv.user_id AS "user_id!",
                      cv.capability_id AS "capability!",
                      u.display_name AS "owner_display_name!",
                      COALESCE(cv.last_active_at, cv.created_at) AS "last_used!",
                      cv.status AS "state!"
               FROM cache_volumes cv
               JOIN users u ON u.id = cv.user_id
               WHERE cv.status = 'active' AND cv.user_id = ?
               ORDER BY COALESCE(cv.last_active_at, cv.created_at) DESC, cv.id ASC"#,
            principal.user_id
        )
        .fetch_all(db)
        .await
        .context("failed to list cache volumes")?
        .into_iter()
        .map(|row| CacheVolumeRecord {
            id: row.id,
            user_id: row.user_id,
            capability: row.capability,
            owner_display_name: row.owner_display_name,
            last_used: row.last_used,
            state: row.state,
        })
        .collect()
    };

    Ok(records
        .into_iter()
        .map(|record| {
            let volume_name = docker_volume_name(&record.user_id, &record.capability);
            CacheVolume {
                id: record.id,
                capability: record.capability,
                owner_display_name: principal.is_admin.then_some(record.owner_display_name),
                last_used: record.last_used,
                size_bytes: sizes.get(&volume_name).copied().filter(|size| *size >= 0),
                state: record.state,
            }
        })
        .collect())
}

/// Delete a cache volume if it is visible to the authenticated principal.
///
/// The existing capability purge implementation owns Docker volume removal and
/// database state transitions. Passing the caller's user ID for non-admins
/// makes cross-user and missing IDs indistinguishable to callers.
pub async fn delete_cache_volume(
    db: &sqlx::SqlitePool,
    principal: &ApiPrincipal,
    cache_id: &str,
) -> Result<bool> {
    let restrict_user_id = (!principal.is_admin).then_some(principal.user_id.as_str());
    crate::capabilities::purge_cache_volume(db, cache_id, restrict_user_id).await
}

fn docker_volume_name(user_id: &str, capability: &str) -> String {
    format!("selu-cache-{user_id}-{capability}")
}

/// Reusable Docker service boundary for cache-volume usage metadata.
///
/// This intentionally uses Bollard's Docker API rather than shelling out. An
/// empty map represents unavailable usage data and is normalized to unknown
/// sizes by `list_cache_volumes`.
pub async fn fetch_docker_volume_sizes() -> HashMap<String, i64> {
    let docker = match bollard::Docker::connect_with_local_defaults() {
        Ok(docker) => docker,
        Err(error) => {
            warn!("Cannot connect to Docker for cache volume sizes: {error}");
            return HashMap::new();
        }
    };

    let usage = match docker
        .df(Some(bollard::query_parameters::DataUsageOptions {
            _type: Some(vec!["volume".to_string()]),
            verbose: true,
        }))
        .await
    {
        Ok(usage) => usage,
        Err(error) => {
            warn!("Docker cache volume size lookup failed: {error}");
            return HashMap::new();
        }
    };

    volume_sizes_from_usage(usage)
}

fn volume_sizes_from_usage(
    usage: bollard::models::SystemDataUsageResponse,
) -> HashMap<String, i64> {
    let mut sizes = HashMap::new();
    if let Some(volume_usage) = usage.volume_usage
        && let Some(volumes) = volume_usage.items
    {
        for value in volumes {
            if let Ok(volume) = serde_json::from_value::<bollard::models::Volume>(value)
                && let Some(usage_data) = volume.usage_data
                && usage_data.size >= 0
            {
                sizes.insert(volume.name, usage_data.size);
            }
        }
    }
    sizes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::auth::SessionUser;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_db() -> sqlx::SqlitePool {
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
                "INSERT INTO cache_volumes \
                 (id, user_id, capability_id, status, last_active_at) \
                 VALUES (?, ?, ?, 'active', '2026-01-02 03:04:05')",
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

    fn principal(user_id: &str, display_name: &str, is_admin: bool) -> ApiPrincipal {
        ApiPrincipal(SessionUser {
            user_id: user_id.to_string(),
            username: user_id.to_string(),
            display_name: display_name.to_string(),
            is_admin,
            language: "en".to_string(),
        })
    }

    #[tokio::test]
    async fn listing_is_user_scoped_and_unknown_sizes_do_not_fail() {
        let db = test_db().await;
        let mut sizes = HashMap::new();
        sizes.insert(docker_volume_name("alice", "cargo"), 4096);

        let alice = list_cache_volumes_with_sizes(&db, &principal("alice", "Alice", false), &sizes)
            .await
            .unwrap();
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].id, "alice-cache");
        assert_eq!(alice[0].size_bytes, Some(4096));
        assert_eq!(alice[0].owner_display_name, None);

        let admin =
            list_cache_volumes_with_sizes(&db, &principal("admin", "Ada Admin", true), &sizes)
                .await
                .unwrap();
        assert_eq!(admin.len(), 2);
        assert_eq!(admin[0].owner_display_name.as_deref(), Some("Alice"));
        assert_eq!(admin[1].owner_display_name.as_deref(), Some("Bob"));
        assert_eq!(admin[1].size_bytes, None);
    }

    #[tokio::test]
    async fn cross_user_delete_is_not_found_and_does_not_mutate() {
        let db = test_db().await;
        let deleted = delete_cache_volume(&db, &principal("alice", "Alice", false), "bob-cache")
            .await
            .unwrap();
        assert!(!deleted);

        let state = sqlx::query_scalar::<_, String>(
            "SELECT status FROM cache_volumes WHERE id = 'bob-cache'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(state, "active");
    }
}
