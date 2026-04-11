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
