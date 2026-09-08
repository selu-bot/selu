use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct DownloadQuery {
    pub exp: i64,
    pub sig: String,
}

/// GET /api/artifacts/{artifact_id}/download?exp=...&sig=...
pub async fn download_artifact(
    Path(artifact_id): Path<String>,
    Query(q): Query<DownloadQuery>,
    State(state): State<AppState>,
) -> Response {
    let artifact = if let Some(in_memory) =
        crate::agents::artifacts::get_by_id(&state.artifacts, &artifact_id).await
    {
        in_memory
    } else {
        match crate::agents::artifacts::get_persisted_by_id(&state.db, &artifact_id).await {
            Ok(Some(persisted)) => crate::agents::artifacts::StoredArtifact {
                user_id: persisted.user_id,
                session_id: String::new(),
                thread_id: None,
                filename: persisted.filename,
                mime_type: persisted.mime_type,
                data: persisted.data,
                created_at: std::time::Instant::now(),
            },
            Ok(None) => return StatusCode::NOT_FOUND.into_response(),
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    };

    let valid = crate::agents::artifacts::verify_download_token(
        &state.config.encryption_key,
        &artifact_id,
        &artifact.user_id,
        q.exp,
        &q.sig,
    );
    if !valid {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let mut resp = Response::new(Body::from(artifact.data));
    let headers = resp.headers_mut();
    let content_type = HeaderValue::from_str(&artifact.mime_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    headers.insert(header::CONTENT_TYPE, content_type);
    let disposition_type = if artifact.mime_type.starts_with("image/") {
        "inline"
    } else {
        "attachment"
    };
    let disposition = format!(
        "{}; filename=\"{}\"",
        disposition_type,
        artifact.filename.replace('"', "_")
    );
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=300"),
    );
    resp
}

/// Cookie-authenticated download for native clients. Ownership is checked on
/// both the in-memory and persisted paths; IDs never grant access by themselves.
pub async fn download_authenticated(
    principal: crate::api::auth::ApiPrincipal,
    Path(artifact_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    let artifact = match owned_artifact(
        &state.artifacts,
        &state.db,
        &artifact_id,
        &principal.user_id,
    )
    .await
    {
        Ok(value) => value,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let Some((_filename, mime_type, data)) = artifact else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = Response::new(Body::from(data));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn owned_artifact(
    store: &crate::agents::artifacts::ArtifactStore,
    db: &sqlx::SqlitePool,
    artifact_id: &str,
    user_id: &str,
) -> anyhow::Result<Option<(String, String, Vec<u8>)>> {
    if let Some(value) = crate::agents::artifacts::get_by_id(store, artifact_id).await {
        return Ok((value.user_id == user_id).then_some((
            value.filename,
            value.mime_type,
            value.data,
        )));
    }
    // Check ownership before reading the persisted file into memory.
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM thread_artifacts WHERE id = ? AND user_id = ?)",
    )
    .bind(artifact_id)
    .bind(user_id)
    .fetch_one(db)
    .await?;
    if !owned {
        return Ok(None);
    }
    Ok(
        crate::agents::artifacts::get_persisted_by_id(db, artifact_id)
            .await?
            .filter(|value| value.user_id == user_id)
            .map(|value| (value.filename, value.mime_type, value.data)),
    )
}

#[cfg(test)]
mod authenticated_photo_tests {
    use super::*;

    #[tokio::test]
    async fn memory_photo_requires_its_owner() {
        let store = crate::agents::artifacts::new_store();
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let photo = crate::agents::artifacts::store_inbound_attachment_scoped(
            &store,
            "owner",
            "session",
            Some("conversation"),
            "photo.jpg",
            "image/jpeg",
            vec![1, 2, 3],
        )
        .await
        .unwrap();
        assert!(
            owned_artifact(&store, &db, &photo.artifact_id, "other")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            owned_artifact(&store, &db, &photo.artifact_id, "owner")
                .await
                .unwrap()
                .unwrap()
                .2,
            vec![1, 2, 3]
        );
    }

    #[tokio::test]
    async fn persisted_foreign_photo_is_rejected_before_disk_read() {
        let store = crate::agents::artifacts::new_store();
        let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE thread_artifacts (id TEXT, user_id TEXT)")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO thread_artifacts VALUES ('photo', 'owner')")
            .execute(&db)
            .await
            .unwrap();
        // No file_path column: reaching disk lookup would fail this test.
        assert!(
            owned_artifact(&store, &db, "photo", "other")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            owned_artifact(&store, &db, "missing", "owner")
                .await
                .unwrap()
                .is_none()
        );
    }
}
