/// Agent marketplace: fetch catalogue, download, verify, extract, and install agents.
///
/// The marketplace is a JSON catalogue served over HTTPS. Each entry points to a
/// GitHub release archive (tar.gz) containing the agent's files. Capability
/// Docker images are pulled from a container registry (e.g. GHCR).
use anyhow::{Context, Result};
use ring::digest;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::agents::loader::{self, AgentDefinition};

// ── Marketplace types ─────────────────────────────────────────────────────────

/// The top-level marketplace catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceCatalogue {
    pub version: u32,
    pub agents: Vec<MarketplaceEntry>,
}

/// A single agent listing in the marketplace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    #[serde(default)]
    pub author: String,
    /// URL to download the agent archive (tar.gz)
    pub archive_url: String,
    /// SHA-256 hex digest of the archive for verification
    #[serde(default)]
    pub archive_sha256: String,
    /// Docker images to pull for the agent's capabilities
    #[serde(default)]
    pub capability_images: Vec<String>,
}

// ── Catalogue fetching ────────────────────────────────────────────────────────

/// Fetch the marketplace catalogue from the configured URL.
pub async fn fetch_catalogue(marketplace_url: &str) -> Result<MarketplaceCatalogue> {
    info!(url = marketplace_url, "Fetching marketplace catalogue");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("Failed to build HTTP client")?;

    let resp = client
        .get(marketplace_url)
        .send()
        .await
        .with_context(|| format!("Failed to fetch marketplace at {}", marketplace_url))?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Marketplace returned HTTP {} for {}",
            resp.status(),
            marketplace_url
        ));
    }

    let catalogue: MarketplaceCatalogue = resp
        .json()
        .await
        .context("Failed to parse marketplace JSON")?;

    info!(
        agents = catalogue.agents.len(),
        "Marketplace catalogue loaded"
    );

    Ok(catalogue)
}

// ── Agent installation ────────────────────────────────────────────────────────

/// Install an agent from a marketplace entry.
///
/// 1. Download the archive
/// 2. Verify SHA-256 checksum (if provided)
/// 3. Extract to `installed_dir/{agent_id}/`
/// 4. Pull Docker images for capabilities
/// 5. Insert DB row
/// 6. Load the agent definition and add to the in-memory map
///
/// Returns the loaded `AgentDefinition` for the setup wizard.
pub async fn install_agent(
    entry: &MarketplaceEntry,
    installed_dir: &str,
    db: &SqlitePool,
    agents: &Arc<RwLock<HashMap<String, AgentDefinition>>>,
    docker: &bollard::Docker,
) -> Result<AgentDefinition> {
    let agent_dir = Path::new(installed_dir).join(&entry.id);

    // Check if already installed
    if agent_dir.exists() {
        return Err(anyhow::anyhow!(
            "Agent '{}' is already installed at {}",
            entry.id,
            agent_dir.display()
        ));
    }

    info!(agent = %entry.id, url = %entry.archive_url, "Installing agent");

    // 1. Download the archive
    let archive_bytes = download_archive(&entry.archive_url).await?;

    // 2. Verify checksum
    if !entry.archive_sha256.is_empty() {
        verify_sha256(&archive_bytes, &entry.archive_sha256)?;
    } else {
        warn!(agent = %entry.id, "No SHA-256 checksum provided — skipping verification");
    }

    // 3. Extract archive
    extract_archive(&archive_bytes, installed_dir, &entry.id)?;

    // Steps 4-6 can fail after files have been extracted to disk.
    // If anything goes wrong, clean up the extracted directory and any
    // partial DB row so the user can retry cleanly.
    match install_agent_inner(entry, &agent_dir, db, agents, docker).await {
        Ok(agent_def) => Ok(agent_def),
        Err(e) => {
            warn!(agent = %entry.id, "Installation failed, cleaning up: {e}");

            // Remove from in-memory map (best-effort, may not have been added)
            {
                let mut map = agents.write().await;
                map.remove(&entry.id);
            }

            // Remove partially-inserted DB row (best-effort)
            let _ = sqlx::query("DELETE FROM agents WHERE id = ? AND is_bundled = 0")
                .bind(&entry.id)
                .execute(db)
                .await;

            // Remove extracted directory (best-effort)
            if agent_dir.exists() {
                let _ = tokio::fs::remove_dir_all(&agent_dir).await;
            }

            Err(e)
        }
    }
}

/// Inner install logic (steps 4-6) that runs after the archive has been
/// extracted. Factored out so that `install_agent` can clean up on failure.
async fn install_agent_inner(
    entry: &MarketplaceEntry,
    agent_dir: &Path,
    db: &SqlitePool,
    agents: &Arc<RwLock<HashMap<String, AgentDefinition>>>,
    docker: &bollard::Docker,
) -> Result<AgentDefinition> {
    // 4. Pull Docker images
    for image in &entry.capability_images {
        pull_image(docker, image).await?;
    }

    // 5. Insert DB row (setup_complete = 0 if the agent has install steps)
    let agent_def = loader::load_one(agent_dir).await
        .with_context(|| format!("Failed to load extracted agent from {}", agent_dir.display()))?;

    let has_steps = !agent_def.install_steps.is_empty();
    let setup_complete = if has_steps { 0 } else { 1 };

    sqlx::query(
        "INSERT INTO agents (id, display_name, version, source_url, is_bundled, setup_complete) \
         VALUES (?, ?, ?, ?, 0, ?) \
         ON CONFLICT(id) DO UPDATE SET display_name = excluded.display_name, \
         version = excluded.version, source_url = excluded.source_url, \
         setup_complete = excluded.setup_complete",
    )
    .bind(&entry.id)
    .bind(&entry.name)
    .bind(&entry.version)
    .bind(&entry.archive_url)
    .bind(setup_complete)
    .execute(db)
    .await
    .context("Failed to insert agent into DB")?;

    // 6. Add to in-memory map (only if setup is complete / no steps needed)
    if !has_steps {
        let mut map = agents.write().await;
        map.insert(agent_def.id.clone(), agent_def.clone());
    }

    info!(agent = %entry.id, setup_complete, "Agent installed");
    Ok(agent_def)
}

/// Mark an agent's setup as complete and add it to the in-memory map.
pub async fn complete_setup(
    agent_id: &str,
    installed_dir: &str,
    db: &SqlitePool,
    agents: &Arc<RwLock<HashMap<String, AgentDefinition>>>,
) -> Result<()> {
    sqlx::query("UPDATE agents SET setup_complete = 1 WHERE id = ?")
        .bind(agent_id)
        .execute(db)
        .await
        .context("Failed to mark setup complete")?;

    let agent_dir = Path::new(installed_dir).join(agent_id);
    let agent_def = loader::load_one(&agent_dir).await?;

    let mut map = agents.write().await;
    map.insert(agent_def.id.clone(), agent_def);

    info!(agent = agent_id, "Agent setup completed");
    Ok(())
}

/// Uninstall an agent: remove from DB, memory, and filesystem.
pub async fn uninstall_agent(
    agent_id: &str,
    installed_dir: &str,
    db: &SqlitePool,
    agents: &Arc<RwLock<HashMap<String, AgentDefinition>>>,
) -> Result<()> {
    // Don't allow uninstalling the bundled default
    let row = sqlx::query_as::<_, (i32,)>(
        "SELECT is_bundled FROM agents WHERE id = ?",
    )
    .bind(agent_id)
    .fetch_optional(db)
    .await?;

    if let Some((1,)) = row {
        return Err(anyhow::anyhow!(
            "Cannot uninstall the bundled default agent"
        ));
    }

    // Collect capability IDs before we tear anything down. Try the
    // in-memory map first; fall back to loading from disk.
    let cap_ids: Vec<String> = {
        let map = agents.read().await;
        if let Some(def) = map.get(agent_id) {
            def.capability_manifests.keys().cloned().collect()
        } else {
            drop(map);
            let agent_dir = Path::new(installed_dir).join(agent_id);
            loader::load_one(&agent_dir)
                .await
                .map(|d| d.capability_manifests.keys().cloned().collect())
                .unwrap_or_default()
        }
    };

    // Remove from in-memory map
    {
        let mut map = agents.write().await;
        map.remove(agent_id);
    }

    // Remove tool policies for this agent (all users — per-user overrides)
    sqlx::query("DELETE FROM tool_policies WHERE agent_id = ?")
        .bind(agent_id)
        .execute(db)
        .await
        .context("Failed to delete tool policies")?;

    // Remove global tool policies for this agent
    sqlx::query("DELETE FROM global_tool_policies WHERE agent_id = ?")
        .bind(agent_id)
        .execute(db)
        .await
        .context("Failed to delete global tool policies")?;

    // Remove pending approvals for this agent
    sqlx::query("DELETE FROM pending_tool_approvals WHERE agent_id = ?")
        .bind(agent_id)
        .execute(db)
        .await
        .context("Failed to delete pending approvals")?;

    // Remove credentials and egress log entries for each capability
    for cap_id in &cap_ids {
        sqlx::query("DELETE FROM system_credentials WHERE capability_id = ?")
            .bind(cap_id)
            .execute(db)
            .await
            .context("Failed to delete system credentials")?;

        sqlx::query("DELETE FROM user_credentials WHERE capability_id = ?")
            .bind(cap_id)
            .execute(db)
            .await
            .context("Failed to delete user credentials")?;

        sqlx::query("DELETE FROM egress_log WHERE capability_id = ?")
            .bind(cap_id)
            .execute(db)
            .await
            .context("Failed to delete egress log entries")?;
    }

    // Remove agent DB row
    sqlx::query("DELETE FROM agents WHERE id = ?")
        .bind(agent_id)
        .execute(db)
        .await
        .context("Failed to delete agent from DB")?;

    // Remove filesystem
    let agent_dir = Path::new(installed_dir).join(agent_id);
    if agent_dir.exists() {
        tokio::fs::remove_dir_all(&agent_dir).await
            .with_context(|| format!("Failed to remove agent dir {}", agent_dir.display()))?;
    }

    info!(agent = agent_id, "Agent uninstalled");
    Ok(())
}

/// Check if an agent is installed.
#[allow(dead_code)]
pub async fn is_installed(db: &SqlitePool, agent_id: &str) -> Result<bool> {
    let row = sqlx::query_as::<_, (i32,)>(
        "SELECT COUNT(*) FROM agents WHERE id = ?",
    )
    .bind(agent_id)
    .fetch_one(db)
    .await?;

    Ok(row.0 > 0)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Download an archive from a URL.
async fn download_archive(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Failed to download archive from {}", url))?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Archive download returned HTTP {} for {}",
            resp.status(),
            url
        ));
    }

    let bytes = resp
        .bytes()
        .await
        .context("Failed to read archive bytes")?;

    info!(bytes = bytes.len(), "Downloaded archive");
    Ok(bytes.to_vec())
}

/// Verify SHA-256 checksum of downloaded bytes.
fn verify_sha256(data: &[u8], expected_hex: &str) -> Result<()> {
    let actual = digest::digest(&digest::SHA256, data);
    let actual_hex = hex_encode(actual.as_ref());

    if actual_hex != expected_hex.to_lowercase() {
        return Err(anyhow::anyhow!(
            "SHA-256 mismatch: expected {}, got {}",
            expected_hex,
            actual_hex
        ));
    }

    info!("Archive checksum verified");
    Ok(())
}

/// Simple hex encoding (avoids pulling in the `hex` crate).
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Extract a tar.gz archive into `installed_dir/{agent_id}/`.
///
/// The archive may contain files at the root or inside a single top-level
/// directory. We normalize to `installed_dir/{agent_id}/...`.
fn extract_archive(data: &[u8], installed_dir: &str, agent_id: &str) -> Result<()> {
    let decoder = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(decoder);

    let target_dir = Path::new(installed_dir).join(agent_id);
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("Failed to create directory {}", target_dir.display()))?;

    // First pass: detect if there's a common top-level directory
    let decoder2 = flate2::read::GzDecoder::new(data);
    let mut archive2 = tar::Archive::new(decoder2);
    let mut top_dirs = std::collections::HashSet::new();

    for entry in archive2.entries().context("Failed to read archive entries")? {
        let entry = entry.context("Failed to read archive entry")?;
        if let Ok(path) = entry.path() {
            if let Some(first) = path.components().next() {
                top_dirs.insert(first.as_os_str().to_owned());
            }
        }
    }

    // If all files share a single top-level directory, strip it
    let strip_prefix = if top_dirs.len() == 1 {
        top_dirs.into_iter().next()
    } else {
        None
    };

    for entry in archive.entries().context("Failed to read archive entries")? {
        let mut entry = entry.context("Failed to read archive entry")?;
        let path = entry.path().context("Invalid path in archive")?.into_owned();

        let relative = if let Some(ref prefix) = strip_prefix {
            match path.strip_prefix(prefix) {
                Ok(p) => p.to_path_buf(),
                Err(_) => path,
            }
        } else {
            path
        };

        // Skip empty paths (the top-level directory entry itself)
        if relative.as_os_str().is_empty() {
            continue;
        }

        let full_path = target_dir.join(&relative);

        // Security: ensure we don't escape the target directory
        if !full_path.starts_with(&target_dir) {
            warn!(
                path = %relative.display(),
                "Archive path escapes target directory — skipping"
            );
            continue;
        }

        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&full_path).ok();
        } else {
            if let Some(parent) = full_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let mut file = std::fs::File::create(&full_path)
                .with_context(|| format!("Failed to create {}", full_path.display()))?;
            std::io::copy(&mut entry, &mut file)
                .with_context(|| format!("Failed to write {}", full_path.display()))?;
        }
    }

    info!(
        dir = %target_dir.display(),
        "Extracted archive"
    );
    Ok(())
}

/// Pull a Docker image.
async fn pull_image(docker: &bollard::Docker, image: &str) -> Result<()> {
    use bollard::image::CreateImageOptions;
    use futures::StreamExt;

    info!(image, "Pulling capability Docker image");

    let opts = CreateImageOptions {
        from_image: image,
        ..Default::default()
    };

    let mut stream = docker.create_image(Some(opts), None, None);

    while let Some(result) = stream.next().await {
        match result {
            Ok(info) => {
                if let Some(status) = info.status {
                    tracing::debug!(image, status, "Docker pull progress");
                }
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Failed to pull image '{}': {}",
                    image,
                    e
                ));
            }
        }
    }

    info!(image, "Docker image pulled");
    Ok(())
}
