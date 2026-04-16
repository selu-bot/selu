use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::hmac;
use selu_core::types::OutboundAttachment;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

const ARTIFACT_TTL: Duration = Duration::from_secs(60 * 60 * 6);
const DOWNLOAD_URL_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
pub struct StoredArtifact {
    pub user_id: String,
    pub session_id: String,
    /// Thread that owns this artifact. When set, the artifact is only visible
    /// to turns running in the same thread — preventing cross-thread leaks.
    pub thread_id: Option<String>,
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
    pub created_at: Instant,
}

pub type ArtifactStore = Arc<RwLock<HashMap<String, StoredArtifact>>>;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArtifactRef {
    pub artifact_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct PersistedArtifact {
    pub user_id: String,
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ThreadArtifactFile {
    pub artifact_id: String,
    pub file_path: String,
}

pub fn new_store() -> ArtifactStore {
    Arc::new(RwLock::new(HashMap::new()))
}

pub async fn list_session_artifacts(
    store: &ArtifactStore,
    user_id: &str,
    session_id: &str,
    limit: usize,
) -> Vec<ArtifactRef> {
    list_session_artifacts_scoped(store, user_id, session_id, None, limit).await
}

/// List artifacts scoped to a specific thread.
///
/// When `thread_id` is `Some`, only artifacts that belong to that thread (or
/// have no thread affinity — e.g. delegation-produced artifacts) are returned.
/// This prevents artifacts uploaded in Thread A from leaking into Thread B's
/// context.
pub async fn list_session_artifacts_scoped(
    store: &ArtifactStore,
    user_id: &str,
    session_id: &str,
    thread_id: Option<&str>,
    limit: usize,
) -> Vec<ArtifactRef> {
    let lock = store.read().await;
    let mut out: Vec<(String, StoredArtifact)> = lock
        .iter()
        .filter(|(_, a)| {
            if a.user_id != user_id || a.session_id != session_id {
                return false;
            }
            // Thread isolation: when a thread_id filter is provided, only
            // include artifacts that either belong to that thread or have no
            // thread affinity (thread_id == None, e.g. delegation results).
            if let Some(tid) = thread_id {
                match &a.thread_id {
                    Some(artifact_tid) => artifact_tid == tid,
                    None => true, // thread-agnostic artifact (e.g. from delegation)
                }
            } else {
                true
            }
        })
        .map(|(id, a)| (id.clone(), a.clone()))
        .collect();
    out.sort_by_key(|(_, a)| std::cmp::Reverse(a.created_at));
    out.into_iter()
        .take(limit)
        .map(|(artifact_id, a)| ArtifactRef {
            artifact_id,
            filename: a.filename,
            mime_type: a.mime_type,
            size_bytes: a.data.len(),
        })
        .collect()
}

pub async fn get_for_user(
    store: &ArtifactStore,
    artifact_id: &str,
    user_id: &str,
) -> Option<StoredArtifact> {
    let lock = store.read().await;
    let artifact = lock.get(artifact_id)?;
    if artifact.user_id != user_id {
        return None;
    }
    Some(artifact.clone())
}

pub async fn get_by_id(store: &ArtifactStore, artifact_id: &str) -> Option<StoredArtifact> {
    let lock = store.read().await;
    lock.get(artifact_id).cloned()
}

pub async fn remove_ids(store: &ArtifactStore, artifact_ids: &[String]) {
    if artifact_ids.is_empty() {
        return;
    }
    let mut lock = store.write().await;
    for id in artifact_ids {
        lock.remove(id);
    }
}

pub async fn get_persisted_by_id(
    db: &sqlx::SqlitePool,
    artifact_id: &str,
) -> Result<Option<PersistedArtifact>> {
    let row = sqlx::query!(
        "SELECT id, user_id, filename, mime_type, file_path
         FROM thread_artifacts
         WHERE id = ?
         LIMIT 1",
        artifact_id
    )
    .fetch_optional(db)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let data = match tokio::fs::read(&row.file_path).await {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let artifact_id = row.id.unwrap_or_default();
    if artifact_id.is_empty() {
        return Ok(None);
    }

    Ok(Some(PersistedArtifact {
        user_id: row.user_id,
        filename: row.filename,
        mime_type: row.mime_type,
        data,
    }))
}

pub async fn persisted_exists_for_user(
    db: &sqlx::SqlitePool,
    artifact_id: &str,
    user_id: &str,
) -> bool {
    let Ok(row) = sqlx::query!(
        "SELECT file_path
         FROM thread_artifacts
         WHERE id = ? AND user_id = ?
         LIMIT 1",
        artifact_id,
        user_id
    )
    .fetch_optional(db)
    .await
    else {
        return false;
    };

    let Some(row) = row else {
        return false;
    };
    tokio::fs::metadata(&row.file_path).await.is_ok()
}

pub async fn persist_refs_for_thread(
    db: &sqlx::SqlitePool,
    store: &ArtifactStore,
    database_url: &str,
    thread_id: Option<&str>,
    user_id: &str,
    refs: &[ArtifactRef],
) {
    for r in refs {
        let Some(artifact) = get_for_user(store, &r.artifact_id, user_id).await else {
            continue;
        };
        if let Err(e) =
            persist_one_for_thread(db, database_url, thread_id, &r.artifact_id, &artifact).await
        {
            warn!(
                thread_id = ?thread_id,
                artifact_id = %r.artifact_id,
                "Failed to persist artifact to storage: {e}"
            );
        }
    }
}

async fn persist_one_for_thread(
    db: &sqlx::SqlitePool,
    database_url: &str,
    thread_id: Option<&str>,
    artifact_id: &str,
    artifact: &StoredArtifact,
) -> Result<()> {
    let root = artifacts_root_from_db_url(database_url);
    tokio::fs::create_dir_all(&root).await?;

    let path = root.join(safe_storage_name(artifact_id, &artifact.filename));
    tokio::fs::write(&path, &artifact.data).await?;

    let file_path = path.to_string_lossy().to_string();
    let size_bytes = artifact.data.len() as i64;
    sqlx::query!(
        "INSERT INTO thread_artifacts (id, thread_id, user_id, filename, mime_type, size_bytes, file_path)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
            thread_id = excluded.thread_id,
            user_id = excluded.user_id,
            filename = excluded.filename,
            mime_type = excluded.mime_type,
            size_bytes = excluded.size_bytes,
            file_path = excluded.file_path",
        artifact_id,
        thread_id,
        artifact.user_id,
        artifact.filename,
        artifact.mime_type,
        size_bytes,
        file_path
    )
    .execute(db)
    .await?;

    Ok(())
}

pub async fn list_thread_artifact_files(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    thread_id: &str,
) -> Result<Vec<ThreadArtifactFile>> {
    let rows = sqlx::query!(
        "SELECT id, file_path FROM thread_artifacts WHERE thread_id = ?",
        thread_id
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            r.id.map(|artifact_id| ThreadArtifactFile {
                artifact_id,
                file_path: r.file_path,
            })
        })
        .collect())
}

pub async fn delete_thread_artifacts_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    thread_id: &str,
) -> Result<()> {
    sqlx::query!(
        "DELETE FROM thread_artifacts WHERE thread_id = ?",
        thread_id
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn delete_artifact_files(files: &[ThreadArtifactFile]) {
    for file in files {
        match tokio::fs::remove_file(&file.file_path).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(
                artifact_id = %file.artifact_id,
                file_path = %file.file_path,
                "Failed to delete persisted artifact file: {e}"
            ),
        }
    }
}

fn artifacts_root_from_db_url(database_url: &str) -> PathBuf {
    let path_without_query = database_url.split('?').next().unwrap_or(database_url);
    let db_path = if let Some(rest) = path_without_query.strip_prefix("sqlite:///") {
        PathBuf::from(format!("/{}", rest.trim_start_matches('/')))
    } else if let Some(rest) = path_without_query.strip_prefix("sqlite://") {
        PathBuf::from(rest)
    } else {
        PathBuf::from("selu.db")
    };

    if db_path == Path::new(":memory:") {
        return PathBuf::from("./artifacts");
    }
    db_path
        .parent()
        .map(|p| p.join("artifacts"))
        .unwrap_or_else(|| PathBuf::from("./artifacts"))
}

fn safe_storage_name(artifact_id: &str, filename: &str) -> String {
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e)
        .filter(|e| !e.is_empty() && e.len() <= 12 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(|e| format!(".{}", e.to_ascii_lowercase()))
        .unwrap_or_default();
    format!("{artifact_id}{ext}")
}

pub async fn to_outbound_attachments(
    store: &ArtifactStore,
    refs: &[ArtifactRef],
    user_id: &str,
    external_base_url: &str,
    signing_secret: &str,
    include_inline_data: bool,
) -> Vec<OutboundAttachment> {
    let mut out = Vec::new();
    for r in refs {
        let Some(artifact) = get_for_user(store, &r.artifact_id, user_id).await else {
            continue;
        };
        let download_url =
            generate_download_url(external_base_url, signing_secret, &r.artifact_id, user_id);
        let data_base64 = if include_inline_data {
            Some(STANDARD.encode(&artifact.data))
        } else {
            None
        };
        out.push(OutboundAttachment {
            artifact_id: r.artifact_id.clone(),
            filename: r.filename.clone(),
            mime_type: r.mime_type.clone(),
            size_bytes: r.size_bytes,
            download_url,
            data_base64,
        });
    }
    out
}

pub fn generate_download_url(
    base_url: &str,
    signing_secret: &str,
    artifact_id: &str,
    user_id: &str,
) -> Option<String> {
    if signing_secret.is_empty() {
        return None;
    }
    let expires_at =
        (chrono::Utc::now() + chrono::Duration::from_std(DOWNLOAD_URL_TTL).ok()?).timestamp();
    let sig = sign_download_token(signing_secret, artifact_id, user_id, expires_at);
    // base_url can be a full origin ("https://example.com"), a sub-path ("/selu"),
    // or empty — the latter two produce browser-relative URLs.
    let prefix = base_url.trim_end_matches('/');
    Some(format!(
        "{}/api/artifacts/{}/download?exp={}&sig={}",
        prefix,
        urlencoding::encode(artifact_id),
        expires_at,
        urlencoding::encode(&sig)
    ))
}

pub fn verify_download_token(
    signing_secret: &str,
    artifact_id: &str,
    user_id: &str,
    exp: i64,
    sig: &str,
) -> bool {
    if signing_secret.is_empty() || sig.trim().is_empty() {
        return false;
    }
    let now = chrono::Utc::now().timestamp();
    if exp < now {
        return false;
    }
    let msg = format!("{}:{}:{}", artifact_id, user_id, exp);
    let Ok(sig_bytes) = URL_SAFE_NO_PAD.decode(sig) else {
        return false;
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, signing_secret.as_bytes());
    hmac::verify(&key, msg.as_bytes(), &sig_bytes).is_ok()
}

fn sign_download_token(signing_secret: &str, artifact_id: &str, user_id: &str, exp: i64) -> String {
    let msg = format!("{}:{}:{}", artifact_id, user_id, exp);
    let key = hmac::Key::new(hmac::HMAC_SHA256, signing_secret.as_bytes());
    let tag = hmac::sign(&key, msg.as_bytes());
    URL_SAFE_NO_PAD.encode(tag.as_ref())
}

pub async fn prepare_attachment_artifacts_for_capability<F, Fut>(
    store: &ArtifactStore,
    user_id: &str,
    session_id: &str,
    args: &Value,
    mut upload_to_capability: F,
) -> Result<Value>
where
    F: FnMut(&StoredArtifact) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let mut value = args.clone();
    let Some(obj) = value.as_object_mut() else {
        return Ok(value);
    };

    let Some(attachments_value) = obj.get_mut("attachments") else {
        return Ok(value);
    };
    let Some(attachments) = attachments_value.as_array_mut() else {
        return Ok(value);
    };

    cleanup_expired_force(store).await;
    debug!(
        user_id = %user_id,
        session_id = %session_id,
        attachments_count = attachments.len(),
        "Preparing attachment artifact references for capability invocation"
    );

    for (idx, attachment) in attachments.iter_mut().enumerate() {
        let Some(att_obj) = attachment.as_object_mut() else {
            continue;
        };
        if att_obj.contains_key("data_base64") {
            return Err(anyhow!(
                "attachments[{}] contains inline data_base64, which is no longer supported",
                idx
            ));
        }
        if att_obj.contains_key("capability_artifact_id") {
            continue;
        }

        let Some(artifact_id) = att_obj.get("artifact_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let artifact = get_authorized(store, artifact_id, user_id, session_id).await?;
        debug!(
            user_id = %user_id,
            session_id = %session_id,
            attachment_index = idx,
            artifact_id = %artifact_id,
            filename = %artifact.filename,
            mime_type = %artifact.mime_type,
            size_bytes = artifact.data.len(),
            "Resolved orchestrator artifact for attachment handoff"
        );
        let capability_artifact_id = upload_to_capability(&artifact).await?;
        info!(
            user_id = %user_id,
            session_id = %session_id,
            attachment_index = idx,
            artifact_id = %artifact_id,
            capability_artifact_id = %capability_artifact_id,
            "Attachment artifact handoff completed"
        );
        debug!(
            user_id = %user_id,
            session_id = %session_id,
            attachment_index = idx,
            artifact_id = %artifact_id,
            capability_artifact_id = %capability_artifact_id,
            "Uploaded attachment artifact into capability runtime"
        );
        att_obj.remove("artifact_id");
        att_obj.insert(
            "filename".to_string(),
            Value::String(artifact.filename.clone()),
        );
        att_obj.insert(
            "mime_type".to_string(),
            Value::String(artifact.mime_type.clone()),
        );
        att_obj.insert(
            "size_bytes".to_string(),
            Value::Number((artifact.data.len() as u64).into()),
        );
        att_obj.insert(
            "capability_artifact_id".to_string(),
            Value::String(capability_artifact_id),
        );
    }

    Ok(value)
}

pub async fn capture_artifacts_from_result<F, Fut>(
    store: &ArtifactStore,
    user_id: &str,
    session_id: &str,
    result: &str,
    mut fetch_from_capability: F,
) -> Result<String>
where
    F: FnMut(&str) -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    let Ok(mut json) = serde_json::from_str::<Value>(result) else {
        return Ok(result.to_string());
    };
    let Some(obj) = json.as_object_mut() else {
        return Ok(result.to_string());
    };

    cleanup_expired_force(store).await;

    let mut changed = false;

    if let Some(artifact_value) = obj.get_mut("artifact") {
        if let Some(summary) = capture_one(
            store,
            user_id,
            session_id,
            artifact_value,
            &mut fetch_from_capability,
        )
        .await?
        {
            *artifact_value = summary;
            changed = true;
        }
    }

    if let Some(artifacts_value) = obj.get_mut("artifacts") {
        if let Some(artifacts) = artifacts_value.as_array_mut() {
            let mut any_replaced = false;
            for item in artifacts {
                if let Some(summary) =
                    capture_one(store, user_id, session_id, item, &mut fetch_from_capability)
                        .await?
                {
                    *item = summary;
                    any_replaced = true;
                }
            }
            if any_replaced {
                changed = true;
            }
        }
    }

    if !changed {
        return Ok(result.to_string());
    }

    Ok(serde_json::to_string(&json)?)
}

pub async fn store_inbound_attachment(
    store: &ArtifactStore,
    user_id: &str,
    session_id: &str,
    filename: &str,
    mime_type: &str,
    data: Vec<u8>,
) -> Result<ArtifactRef> {
    store_inbound_attachment_scoped(store, user_id, session_id, None, filename, mime_type, data)
        .await
}

/// Store an inbound attachment with thread affinity.
///
/// When `thread_id` is `Some`, the artifact is tagged with that thread so it
/// will only be visible in that thread's context — preventing cross-thread
/// leaks.
pub async fn store_inbound_attachment_scoped(
    store: &ArtifactStore,
    user_id: &str,
    session_id: &str,
    thread_id: Option<&str>,
    filename: &str,
    mime_type: &str,
    data: Vec<u8>,
) -> Result<ArtifactRef> {
    if data.is_empty() {
        return Err(anyhow!("Attachment payload is empty"));
    }

    cleanup_expired_force(store).await;
    let artifact_id = Uuid::new_v4().to_string();
    let size_bytes = data.len();
    store.write().await.insert(
        artifact_id.clone(),
        StoredArtifact {
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            thread_id: thread_id.map(|s| s.to_string()),
            filename: filename.to_string(),
            mime_type: mime_type.to_string(),
            data,
            created_at: Instant::now(),
        },
    );

    Ok(ArtifactRef {
        artifact_id,
        filename: filename.to_string(),
        mime_type: mime_type.to_string(),
        size_bytes,
    })
}

async fn capture_one<F, Fut>(
    store: &ArtifactStore,
    user_id: &str,
    session_id: &str,
    artifact_value: &Value,
    fetch_from_capability: &mut F,
) -> Result<Option<Value>>
where
    F: FnMut(&str) -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    let Some(obj) = artifact_value.as_object() else {
        return Ok(None);
    };
    if obj.contains_key("data_base64") {
        return Err(anyhow!(
            "Capability returned inline artifact data_base64, which is no longer supported"
        ));
    }
    let Some(capability_artifact_id) = obj.get("capability_artifact_id").and_then(|v| v.as_str())
    else {
        return Ok(None);
    };

    let filename = obj
        .get("filename")
        .and_then(|v| v.as_str())
        .unwrap_or("document.bin");
    let mime_type = obj
        .get("mime_type")
        .and_then(|v| v.as_str())
        .unwrap_or("application/octet-stream");

    let data = fetch_from_capability(capability_artifact_id).await?;
    info!(
        user_id = %user_id,
        session_id = %session_id,
        capability_artifact_id = %capability_artifact_id,
        downloaded_bytes = data.len(),
        "Capability output artifact downloaded"
    );
    debug!(
        user_id = %user_id,
        session_id = %session_id,
        capability_artifact_id = %capability_artifact_id,
        filename = %filename,
        mime_type = %mime_type,
        downloaded_bytes = data.len(),
        "Downloaded output artifact from capability"
    );
    if data.is_empty() {
        return Err(anyhow!("Artifact payload is empty"));
    }

    let artifact_id = Uuid::new_v4().to_string();
    store.write().await.insert(
        artifact_id.clone(),
        StoredArtifact {
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            thread_id: None, // capability-produced artifacts are thread-agnostic
            filename: filename.to_string(),
            mime_type: mime_type.to_string(),
            data: data.clone(),
            created_at: Instant::now(),
        },
    );
    info!(
        user_id = %user_id,
        session_id = %session_id,
        artifact_id = %artifact_id,
        size_bytes = data.len(),
        "Session artifact stored"
    );
    debug!(
        user_id = %user_id,
        session_id = %session_id,
        artifact_id = %artifact_id,
        filename = %filename,
        mime_type = %mime_type,
        size_bytes = data.len(),
        "Stored session artifact in orchestrator store"
    );

    Ok(Some(serde_json::json!({
        "artifact_id": artifact_id,
        "filename": filename,
        "mime_type": mime_type,
        "size_bytes": data.len()
    })))
}

async fn get_authorized(
    store: &ArtifactStore,
    artifact_id: &str,
    user_id: &str,
    _session_id: &str,
) -> Result<StoredArtifact> {
    let lock = store.read().await;
    let artifact = lock
        .get(artifact_id)
        .ok_or_else(|| anyhow!("Attachment artifact not found or expired: {}", artifact_id))?;
    if artifact.user_id != user_id {
        return Err(anyhow!(
            "Attachment artifact not found or expired: {}",
            artifact_id
        ));
    }
    Ok(artifact.clone())
}

async fn cleanup_expired_force(store: &ArtifactStore) {
    let mut lock = store.write().await;
    lock.retain(|_, v| v.created_at.elapsed() <= ARTIFACT_TTL);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_and_prepares_attachment_artifact() {
        let store = new_store();
        let raw = serde_json::json!({
            "ok": true,
            "artifact": {
                "filename": "test.pdf",
                "mime_type": "application/pdf",
                "capability_artifact_id": "cap-a1"
            }
        })
        .to_string();

        let captured = capture_artifacts_from_result(&store, "u1", "s1", &raw, |cap_id| {
            let cap_id = cap_id.to_string();
            async move {
                assert_eq!(cap_id, "cap-a1");
                Ok(b"hello".to_vec())
            }
        })
        .await
        .unwrap();
        let captured_json: Value = serde_json::from_str(&captured).unwrap();
        let artifact_id = captured_json["artifact"]["artifact_id"].as_str().unwrap();

        let args = serde_json::json!({
            "to": "x@example.com",
            "subject": "x",
            "body": "x",
            "attachments": [{"artifact_id": artifact_id}]
        });
        let prepared =
            prepare_attachment_artifacts_for_capability(&store, "u1", "s1", &args, |_a| async {
                Ok("cap-in-1".to_string())
            })
            .await
            .unwrap();
        assert_eq!(
            prepared["attachments"][0]["capability_artifact_id"]
                .as_str()
                .unwrap(),
            "cap-in-1"
        );
        assert!(prepared["attachments"][0]["artifact_id"].is_null());
    }

    #[test]
    fn verifies_download_token() {
        let sig = sign_download_token("secret", "a1", "u1", 4_000_000_000);
        assert!(verify_download_token(
            "secret",
            "a1",
            "u1",
            4_000_000_000,
            &sig
        ));
        assert!(!verify_download_token(
            "secret",
            "a2",
            "u1",
            4_000_000_000,
            &sig
        ));
    }
}
