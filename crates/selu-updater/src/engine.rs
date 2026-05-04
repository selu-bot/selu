use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::path::Path;
use tokio::process::Command;
use tracing::warn;

use crate::state::AppState;
use crate::types::{WhatsappBridgeChat, WhatsappBridgeChatsResponse, WhatsappBridgeStatusResponse};

#[derive(Debug, Clone, Default)]
pub struct InstalledImageState {
    pub tag: String,
    pub digest: String,
}

pub async fn run_apply(
    state: AppState,
    job_id: String,
    channel: String,
    target_tag: String,
    target_digest: String,
    target_version: String,
    target_build: String,
) -> Result<()> {
    let previous_tag = get_env_var(&state.config.compose_env_file, "SELU_IMAGE_TAG")
        .await
        .unwrap_or_default();
    let previous_digest = get_env_var(&state.config.compose_env_file, "SELU_IMAGE_DIGEST")
        .await
        .unwrap_or_default();
    let previous_version = get_env_var(&state.config.compose_env_file, "SELU_IMAGE_VERSION")
        .await
        .unwrap_or_default();
    let previous_build = get_env_var(&state.config.compose_env_file, "SELU_IMAGE_BUILD")
        .await
        .unwrap_or_default();

    if target_digest.trim().is_empty() {
        return Err(anyhow!("Target digest is required for updates"));
    }
    if !previous_digest.trim().is_empty() && previous_digest.trim() == target_digest.trim() {
        set_runtime(
            &state,
            None,
            "idle",
            "updates.progress.already_current",
            "You are already on the latest version.",
        )
        .await;
        set_runtime_versions(
            &state,
            &target_tag,
            &target_digest,
            &target_version,
            &target_build,
            &previous_tag,
            &previous_digest,
            &previous_version,
            &previous_build,
        )
        .await;
        return Ok(());
    }

    set_runtime(
        &state,
        Some(job_id.clone()),
        "updating",
        "updates.progress.preparing",
        "Preparing update",
    )
    .await;

    upsert_env_var(
        &state.config.compose_env_file,
        "SELU_IMAGE_TAG",
        &target_tag,
    )
    .await?;
    upsert_env_var(
        &state.config.compose_env_file,
        "SELU_IMAGE_DIGEST",
        &target_digest,
    )
    .await?;
    upsert_env_var(
        &state.config.compose_env_file,
        "SELU_IMAGE_VERSION",
        &target_version,
    )
    .await?;
    upsert_env_var(
        &state.config.compose_env_file,
        "SELU_IMAGE_BUILD",
        &target_build,
    )
    .await?;

    set_runtime(
        &state,
        Some(job_id.clone()),
        "updating",
        "updates.progress.pulling",
        "Downloading image",
    )
    .await;
    if let Err(e) = run_compose_cmd(&state, &["pull", &state.config.compose_service]).await {
        restore_previous_image(
            &state,
            &previous_tag,
            &previous_digest,
            &previous_version,
            &previous_build,
        )
        .await;
        return Err(e);
    }
    if let Err(e) = verify_local_digest(
        &state,
        &format!("{}:{}", state.config.image_repo, target_tag),
        &target_digest,
    )
    .await
    {
        restore_previous_image(
            &state,
            &previous_tag,
            &previous_digest,
            &previous_version,
            &previous_build,
        )
        .await;
        return Err(e);
    }

    set_runtime(
        &state,
        Some(job_id.clone()),
        "updating",
        "updates.progress.restarting",
        "Restarting Selu",
    )
    .await;
    if let Err(e) = run_compose_cmd(&state, &["up", "-d", &state.config.compose_service]).await {
        restore_previous_image(
            &state,
            &previous_tag,
            &previous_digest,
            &previous_version,
            &previous_build,
        )
        .await;
        return Err(e);
    }

    set_runtime(
        &state,
        Some(job_id.clone()),
        "updating",
        "updates.progress.health_check",
        "Running health checks",
    )
    .await;
    if let Err(e) = wait_for_health(&state).await {
        warn!("Health check failed, attempting rollback: {e}");
        if !previous_tag.trim().is_empty() {
            upsert_env_var(
                &state.config.compose_env_file,
                "SELU_IMAGE_TAG",
                &previous_tag,
            )
            .await
            .ok();
            upsert_env_var(
                &state.config.compose_env_file,
                "SELU_IMAGE_DIGEST",
                &previous_digest,
            )
            .await
            .ok();
            upsert_env_var(
                &state.config.compose_env_file,
                "SELU_IMAGE_VERSION",
                &previous_version,
            )
            .await
            .ok();
            upsert_env_var(
                &state.config.compose_env_file,
                "SELU_IMAGE_BUILD",
                &previous_build,
            )
            .await
            .ok();
            let _ = run_compose_cmd(&state, &["up", "-d", &state.config.compose_service]).await;
            set_runtime(
                &state,
                None,
                "rollback_available",
                "updates.progress.rollback_ready",
                "Update failed health check. Previous version restored.",
            )
            .await;
            set_runtime_versions(
                &state,
                &previous_tag,
                &previous_digest,
                &previous_version,
                &previous_build,
                &target_tag,
                &target_digest,
                &target_version,
                &target_build,
            )
            .await;
            return Ok(());
        }

        set_runtime(
            &state,
            None,
            "failed",
            "updates.progress.apply_failed",
            "Update failed and no rollback version was available.",
        )
        .await;
        return Err(anyhow!("Update failed and rollback was unavailable"));
    }

    set_runtime(
        &state,
        None,
        "idle",
        "updates.progress.apply_done",
        "Update completed",
    )
    .await;
    set_runtime_versions(
        &state,
        &target_tag,
        &target_digest,
        &target_version,
        &target_build,
        &previous_tag,
        &previous_digest,
        &previous_version,
        &previous_build,
    )
    .await;
    maybe_refresh_updater_sidecar(&state, &channel).await;
    Ok(())
}

pub async fn run_rollback(
    state: AppState,
    job_id: String,
    _channel: String,
    rollback_tag: String,
    rollback_digest: String,
    rollback_version: String,
    rollback_build: String,
) -> Result<()> {
    if rollback_tag.trim().is_empty() {
        return Err(anyhow!("Rollback tag is required"));
    }
    if rollback_digest.trim().is_empty() {
        return Err(anyhow!("Rollback digest is required"));
    }

    let previous_tag = get_env_var(&state.config.compose_env_file, "SELU_IMAGE_TAG")
        .await
        .unwrap_or_default();
    let previous_digest = get_env_var(&state.config.compose_env_file, "SELU_IMAGE_DIGEST")
        .await
        .unwrap_or_default();
    let previous_version = get_env_var(&state.config.compose_env_file, "SELU_IMAGE_VERSION")
        .await
        .unwrap_or_default();
    let previous_build = get_env_var(&state.config.compose_env_file, "SELU_IMAGE_BUILD")
        .await
        .unwrap_or_default();

    set_runtime(
        &state,
        Some(job_id.clone()),
        "updating",
        "updates.progress.rollback_start",
        "Starting rollback",
    )
    .await;

    upsert_env_var(
        &state.config.compose_env_file,
        "SELU_IMAGE_TAG",
        &rollback_tag,
    )
    .await?;
    upsert_env_var(
        &state.config.compose_env_file,
        "SELU_IMAGE_DIGEST",
        &rollback_digest,
    )
    .await?;
    upsert_env_var(
        &state.config.compose_env_file,
        "SELU_IMAGE_VERSION",
        &rollback_version,
    )
    .await?;
    upsert_env_var(
        &state.config.compose_env_file,
        "SELU_IMAGE_BUILD",
        &rollback_build,
    )
    .await?;

    set_runtime(
        &state,
        Some(job_id.clone()),
        "updating",
        "updates.progress.rollback_restarting",
        "Restarting previous version",
    )
    .await;
    if let Err(e) = run_compose_cmd(&state, &["pull", &state.config.compose_service]).await {
        restore_previous_image(
            &state,
            &previous_tag,
            &previous_digest,
            &previous_version,
            &previous_build,
        )
        .await;
        return Err(e);
    }
    if let Err(e) = verify_local_digest(
        &state,
        &format!("{}:{}", state.config.image_repo, rollback_tag),
        &rollback_digest,
    )
    .await
    {
        restore_previous_image(
            &state,
            &previous_tag,
            &previous_digest,
            &previous_version,
            &previous_build,
        )
        .await;
        return Err(e);
    }
    if let Err(e) = run_compose_cmd(&state, &["up", "-d", &state.config.compose_service]).await {
        restore_previous_image(
            &state,
            &previous_tag,
            &previous_digest,
            &previous_version,
            &previous_build,
        )
        .await;
        return Err(e);
    }

    set_runtime(
        &state,
        Some(job_id.clone()),
        "updating",
        "updates.progress.health_check",
        "Running health checks",
    )
    .await;
    if let Err(e) = wait_for_health(&state).await {
        restore_previous_image(
            &state,
            &previous_tag,
            &previous_digest,
            &previous_version,
            &previous_build,
        )
        .await;
        return Err(e);
    }

    set_runtime(
        &state,
        None,
        "idle",
        "updates.progress.rollback_done",
        "Rollback completed",
    )
    .await;
    set_runtime_versions(
        &state,
        &rollback_tag,
        &rollback_digest,
        &rollback_version,
        &rollback_build,
        &previous_tag,
        &previous_digest,
        &previous_version,
        &previous_build,
    )
    .await;
    Ok(())
}

pub async fn upsert_env_var(path: &str, key: &str, value: &str) -> Result<()> {
    let existing = tokio::fs::read_to_string(path).await.unwrap_or_default();
    let mut found = false;
    let mut lines: Vec<String> = Vec::new();

    for line in existing.lines() {
        if line.trim_start().starts_with(&(key.to_string() + "=")) {
            lines.push(format!("{}={}", key, value));
            found = true;
        } else {
            lines.push(line.to_string());
        }
    }

    if !found {
        lines.push(format!("{}={}", key, value));
    }

    let content = if lines.is_empty() {
        format!("{}={}\n", key, value)
    } else {
        format!("{}\n", lines.join("\n"))
    };

    tokio::fs::write(path, content)
        .await
        .with_context(|| format!("Failed to write env file '{}'", path))?;
    Ok(())
}

pub async fn get_env_var(path: &str, key: &str) -> Result<String> {
    let existing = tokio::fs::read_to_string(path).await.unwrap_or_default();
    for line in existing.lines() {
        if line.trim_start().starts_with(&(key.to_string() + "=")) {
            if let Some((_, value)) = line.split_once('=') {
                return Ok(value.trim().to_string());
            }
        }
    }
    Ok(String::new())
}

async fn run_compose_cmd(state: &AppState, args: &[&str]) -> Result<()> {
    let mut cmd_args: Vec<String> = vec![
        "compose".to_string(),
        "-f".to_string(),
        state.config.compose_file.clone(),
        "--env-file".to_string(),
        state.config.compose_env_file.clone(),
    ];
    if !state.config.compose_project_dir.trim().is_empty() {
        cmd_args.push("--project-directory".to_string());
        cmd_args.push(state.config.compose_project_dir.clone());
    }
    cmd_args.extend(args.iter().map(|s| s.to_string()));

    let output = Command::new(&state.config.docker_bin)
        .args(&cmd_args)
        .output()
        .await
        .context("Failed to launch docker compose command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("docker compose failed: {}", stderr.trim()));
    }

    Ok(())
}

async fn run_compose_cmd_output(state: &AppState, args: &[&str]) -> Result<String> {
    let mut cmd_args: Vec<String> = vec![
        "compose".to_string(),
        "-f".to_string(),
        state.config.compose_file.clone(),
        "--env-file".to_string(),
        state.config.compose_env_file.clone(),
    ];
    if !state.config.compose_project_dir.trim().is_empty() {
        cmd_args.push("--project-directory".to_string());
        cmd_args.push(state.config.compose_project_dir.clone());
    }
    cmd_args.extend(args.iter().map(|s| s.to_string()));

    let output = Command::new(&state.config.docker_bin)
        .args(&cmd_args)
        .output()
        .await
        .context("Failed to launch docker compose command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("docker compose failed: {}", stderr.trim()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn run_docker_cmd(state: &AppState, args: &[&str]) -> Result<String> {
    let output = Command::new(&state.config.docker_bin)
        .args(args)
        .output()
        .await
        .context("Failed to launch docker command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("docker command failed: {}", stderr.trim()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub async fn get_installed_image_state(state: &AppState) -> Result<InstalledImageState> {
    let env_tag = get_env_var(&state.config.compose_env_file, "SELU_IMAGE_TAG")
        .await
        .unwrap_or_default();
    let env_digest = get_env_var(&state.config.compose_env_file, "SELU_IMAGE_DIGEST")
        .await
        .unwrap_or_default();

    let mut installed = InstalledImageState {
        tag: env_tag,
        digest: env_digest,
    };

    let container_id = run_compose_cmd_output(state, &["ps", "-q", &state.config.compose_service])
        .await?
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();

    if container_id.is_empty() {
        return Ok(installed);
    }

    let configured_image = run_docker_cmd(
        state,
        &["inspect", "--format", "{{.Config.Image}}", &container_id],
    )
    .await?
    .trim()
    .to_string();
    if let Some(tag) = extract_tag_from_image_ref(&configured_image, &state.config.image_repo) {
        if !tag.trim().is_empty() {
            installed.tag = tag;
        }
    }

    let image_id = run_docker_cmd(state, &["inspect", "--format", "{{.Image}}", &container_id])
        .await?
        .trim()
        .to_string();
    if !image_id.is_empty() {
        let repo_digests = run_docker_cmd(
            state,
            &[
                "image",
                "inspect",
                "--format",
                "{{range .RepoDigests}}{{println .}}{{end}}",
                &image_id,
            ],
        )
        .await?;

        if let Some(digest) = repo_digests
            .lines()
            .find_map(|line| extract_digest_from_repo_digest_line(line.trim()))
        {
            installed.digest = digest;
        }
    }

    Ok(installed)
}

fn extract_digest_from_repo_digest_line(line: &str) -> Option<String> {
    let (_, digest) = line.rsplit_once('@')?;
    let digest = digest.trim();
    if digest.is_empty() {
        None
    } else {
        Some(digest.to_string())
    }
}

fn extract_tag_from_image_ref(image_ref: &str, image_repo: &str) -> Option<String> {
    let without_digest = image_ref
        .trim()
        .split('@')
        .next()
        .unwrap_or_default()
        .trim();
    if without_digest.is_empty() {
        return None;
    }

    let repo = image_repo.trim();
    if !repo.is_empty() {
        let prefix = format!("{repo}:");
        if let Some(tag) = without_digest.strip_prefix(&prefix) {
            let tag = tag.trim();
            if !tag.is_empty() {
                return Some(tag.to_string());
            }
        }
    }

    let last_slash = without_digest.rfind('/').unwrap_or(0);
    let Some(colon_idx) = without_digest.rfind(':') else {
        return None;
    };
    if colon_idx <= last_slash {
        return None;
    }

    let tag = without_digest[colon_idx + 1..].trim();
    if tag.is_empty() {
        None
    } else {
        Some(tag.to_string())
    }
}

async fn verify_local_digest(
    state: &AppState,
    image_ref: &str,
    expected_digest: &str,
) -> Result<()> {
    let out = run_docker_cmd(
        state,
        &[
            "image",
            "inspect",
            "--format",
            "{{range .RepoDigests}}{{println .}}{{end}}",
            image_ref,
        ],
    )
    .await?;

    let expected_suffix = format!("@{}", expected_digest.trim());
    if out
        .lines()
        .any(|line| line.trim_end().ends_with(&expected_suffix))
    {
        return Ok(());
    }

    Err(anyhow!(
        "Pulled image digest does not match expected release digest"
    ))
}

async fn restore_previous_image(
    state: &AppState,
    previous_tag: &str,
    previous_digest: &str,
    previous_version: &str,
    previous_build: &str,
) {
    if previous_tag.trim().is_empty() {
        return;
    }

    let _ = upsert_env_var(
        &state.config.compose_env_file,
        "SELU_IMAGE_TAG",
        previous_tag,
    )
    .await;
    let _ = upsert_env_var(
        &state.config.compose_env_file,
        "SELU_IMAGE_DIGEST",
        previous_digest,
    )
    .await;
    let _ = upsert_env_var(
        &state.config.compose_env_file,
        "SELU_IMAGE_VERSION",
        previous_version,
    )
    .await;
    let _ = upsert_env_var(
        &state.config.compose_env_file,
        "SELU_IMAGE_BUILD",
        previous_build,
    )
    .await;
}

async fn wait_for_health(state: &AppState) -> Result<()> {
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(state.config.health_timeout_secs);
    let interval = std::time::Duration::from_secs(state.config.health_interval_secs);

    loop {
        if start.elapsed() > timeout {
            return Err(anyhow!("Health check timed out"));
        }

        if let Ok(resp) = reqwest::Client::new()
            .get(&state.config.health_url)
            .send()
            .await
        {
            if resp.status().is_success() {
                return Ok(());
            }
        }

        tokio::time::sleep(interval).await;
    }
}

pub async fn set_runtime(
    state: &AppState,
    active_job_id: Option<String>,
    status: &str,
    progress_key: &str,
    message: &str,
) {
    let mut runtime = state.runtime.lock().await;
    runtime.active_job_id = active_job_id;
    runtime.status = status.to_string();
    runtime.progress_key = progress_key.to_string();
    runtime.message = message.to_string();
}

pub async fn set_runtime_versions(
    state: &AppState,
    installed_tag: &str,
    installed_digest: &str,
    installed_version: &str,
    installed_build: &str,
    previous_tag: &str,
    previous_digest: &str,
    previous_version: &str,
    previous_build: &str,
) {
    let mut runtime = state.runtime.lock().await;
    runtime.installed_tag = installed_tag.to_string();
    runtime.installed_digest = installed_digest.to_string();
    runtime.installed_version = installed_version.to_string();
    runtime.installed_build = installed_build.to_string();
    runtime.previous_tag = previous_tag.to_string();
    runtime.previous_digest = previous_digest.to_string();
    runtime.previous_version = previous_version.to_string();
    runtime.previous_build = previous_build.to_string();
}

async fn maybe_refresh_updater_sidecar(state: &AppState, channel: &str) {
    let desired_channel = channel.trim();
    if desired_channel.is_empty() {
        return;
    }

    if state.config.self_update_on_apply {
        if let Err(e) = upsert_env_var(
            &state.config.compose_env_file,
            &state.config.updater_tag_env_key,
            desired_channel,
        )
        .await
        {
            warn!("Failed to set updater channel tag in env: {e}");
            return;
        }

        if let Err(e) = run_compose_cmd(state, &["pull", &state.config.updater_service]).await {
            warn!("Failed to pull updater sidecar image: {e}");
            return;
        }

        if let Err(e) = restart_self_via_helper(state).await {
            warn!("Failed to refresh updater sidecar container: {e}");
        }
        return;
    }

    let current_tag = get_env_var(
        &state.config.compose_env_file,
        &state.config.updater_tag_env_key,
    )
    .await
    .unwrap_or_default();
    let pending_tag = get_env_var(
        &state.config.compose_env_file,
        &state.config.updater_pending_tag_env_key,
    )
    .await
    .unwrap_or_default();

    let mut effective_tag = current_tag.clone();
    if !pending_tag.trim().is_empty() && pending_tag.trim() != current_tag.trim() {
        if let Err(e) = upsert_env_var(
            &state.config.compose_env_file,
            &state.config.updater_tag_env_key,
            pending_tag.trim(),
        )
        .await
        {
            warn!("Failed to set deferred updater tag: {e}");
            return;
        }
        if let Err(e) = run_compose_cmd(state, &["pull", &state.config.updater_service]).await {
            warn!("Failed to pull deferred updater image: {e}");
            return;
        }
        if let Err(e) = restart_self_via_helper(state).await {
            warn!("Failed to apply deferred updater refresh: {e}");
            return;
        }
        effective_tag = pending_tag.trim().to_string();
    }

    if desired_channel != effective_tag {
        if let Err(e) = upsert_env_var(
            &state.config.compose_env_file,
            &state.config.updater_pending_tag_env_key,
            desired_channel,
        )
        .await
        {
            warn!("Failed to set deferred updater target channel: {e}");
        }
    } else if !pending_tag.trim().is_empty() {
        let _ = upsert_env_var(
            &state.config.compose_env_file,
            &state.config.updater_pending_tag_env_key,
            "",
        )
        .await;
    }
}

pub async fn ensure_whatsapp_bridge(
    state: &AppState,
    channel: &str,
    inbound_url: &str,
    inbound_token: &str,
    outbound_auth: &str,
) -> Result<()> {
    if !state.config.whatsapp_bridge_enabled {
        return Ok(());
    }

    let desired_channel = channel.trim();
    if desired_channel.is_empty() {
        return Err(anyhow!("Channel is required"));
    }
    if inbound_url.trim().is_empty() || inbound_token.trim().is_empty() {
        return Err(anyhow!("Inbound URL and token are required"));
    }

    let image = resolve_whatsapp_bridge_image_ref(
        &state.config.whatsapp_bridge_image_repo,
        desired_channel,
    )?;
    let container = state.config.whatsapp_bridge_container_name.trim();
    let volume = state.config.whatsapp_bridge_data_volume.trim();
    if container.is_empty() || volume.is_empty() {
        return Err(anyhow!(
            "WhatsApp bridge container name and data volume must be configured"
        ));
    }

    let _ = run_docker_cmd(state, &["rm", "-f", container]).await;

    if should_pull_whatsapp_bridge_image(&image) {
        run_docker_cmd(state, &["pull", &image]).await?;
    }

    let mut args: Vec<String> = vec![
        "run".to_string(),
        "-d".to_string(),
        "--restart".to_string(),
        "unless-stopped".to_string(),
        "--name".to_string(),
        container.to_string(),
        "-v".to_string(),
        format!("{}:/data", volume),
        "-e".to_string(),
        format!("SELU_INBOUND_URL={}", inbound_url.trim()),
        "-e".to_string(),
        format!("SELU_INBOUND_TOKEN={}", inbound_token.trim()),
    ];

    if !outbound_auth.trim().is_empty() {
        args.push("-e".to_string());
        args.push(format!("BRIDGE_EXPECT_AUTH={}", outbound_auth.trim()));
    }

    let configured_network = state.config.whatsapp_bridge_network.trim();
    let network = if configured_network.is_empty() {
        detect_compose_network(state).await.unwrap_or_default()
    } else {
        configured_network.to_string()
    };

    if network.is_empty() {
        args.push("-p".to_string());
        args.push(format!(
            "127.0.0.1:{}:3200",
            state.config.whatsapp_bridge_port
        ));
        args.push("--add-host".to_string());
        args.push("host.docker.internal:host-gateway".to_string());
    } else {
        args.push("--network".to_string());
        args.push(network);
        args.push("--network-alias".to_string());
        args.push(container.to_string());
    }

    args.push(image);

    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_docker_cmd(state, &args_ref).await?;

    Ok(())
}

fn resolve_whatsapp_bridge_image_ref(repo: &str, channel: &str) -> Result<String> {
    let repo = repo.trim();
    if repo.is_empty() {
        return Err(anyhow!(
            "UPDATER__WHATSAPP_BRIDGE_IMAGE_REPO must not be empty"
        ));
    }

    if !channel
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(anyhow!(
            "Invalid WhatsApp bridge channel '{}'. Use only letters, numbers, '.', '_' or '-'.",
            channel
        ));
    }

    // If repo is already pinned by digest or has an explicit tag, use it as-is.
    if repo.contains('@') || image_repo_has_tag(repo) {
        return Ok(repo.to_string());
    }

    Ok(format!("{repo}:{channel}"))
}

fn image_repo_has_tag(repo: &str) -> bool {
    let last_slash = repo.rfind('/').unwrap_or(0);
    match repo.rfind(':') {
        Some(colon) => colon > last_slash,
        None => false,
    }
}

fn should_pull_whatsapp_bridge_image(image: &str) -> bool {
    let image = image.trim();
    if image.is_empty() {
        return false;
    }

    if image.contains('@') {
        return true;
    }

    let first_segment = image.split('/').next().unwrap_or_default();
    let has_path = image.contains('/');
    let looks_like_registry = first_segment.contains('.')
        || first_segment.eq_ignore_ascii_case("localhost")
        || (has_path && first_segment.contains(':'));

    looks_like_registry
}

pub async fn stop_whatsapp_bridge(state: &AppState, channel: &str) -> Result<()> {
    if !state.config.whatsapp_bridge_enabled {
        return Ok(());
    }

    let container = state.config.whatsapp_bridge_container_name.trim();
    let volume = state.config.whatsapp_bridge_data_volume.trim();
    if container.is_empty() {
        return Err(anyhow!("WhatsApp bridge container name must be configured"));
    }
    if volume.is_empty() {
        return Err(anyhow!("WhatsApp bridge data volume must be configured"));
    }

    match run_docker_cmd(state, &["rm", "-f", container]).await {
        Ok(_) => clear_whatsapp_bridge_auth(state, volume, channel).await,
        Err(e) if e.to_string().contains("No such container") => {
            clear_whatsapp_bridge_auth(state, volume, channel).await
        }
        Err(e) => Err(e),
    }
}

async fn clear_whatsapp_bridge_auth(state: &AppState, volume: &str, channel: &str) -> Result<()> {
    if is_bind_mount_source(volume) {
        clear_whatsapp_bridge_bind_mount(state, volume, channel).await?;
        return Ok(());
    }

    match run_docker_cmd(state, &["volume", "rm", "-f", volume]).await {
        Ok(_) => {}
        Err(e) if e.to_string().contains("No such volume") => {}
        Err(e) => return Err(e).context("remove WhatsApp bridge data volume"),
    }

    run_docker_cmd(state, &["volume", "create", volume])
        .await
        .context("recreate WhatsApp bridge data volume")?;

    Ok(())
}

async fn clear_whatsapp_bridge_bind_mount(
    state: &AppState,
    mount_path: &str,
    channel: &str,
) -> Result<()> {
    let channel = if channel.trim().is_empty() {
        "stable"
    } else {
        channel.trim()
    };
    let image =
        resolve_whatsapp_bridge_image_ref(&state.config.whatsapp_bridge_image_repo, channel)?;

    let helper_name = format!(
        "selu-whatsapp-auth-cleanup-{}",
        uuid::Uuid::new_v4().simple()
    );

    let _ = run_docker_cmd(state, &["rm", "-f", &helper_name]).await;

    let result = run_docker_cmd(
        state,
        &[
            "run",
            "--rm",
            "--name",
            &helper_name,
            "--entrypoint",
            "sh",
            "-v",
            &format!("{mount_path}:/data"),
            &image,
            "-c",
            "rm -rf /data/auth && mkdir -p /data/auth",
        ],
    )
    .await;

    let _ = run_docker_cmd(state, &["rm", "-f", &helper_name]).await;

    result
        .map(|_| ())
        .context("clear WhatsApp bridge auth bind mount")
}

fn is_bind_mount_source(source: &str) -> bool {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return false;
    }

    Path::new(trimmed).is_absolute()
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
        || trimmed.contains(std::path::MAIN_SEPARATOR)
}

pub async fn whatsapp_bridge_status(state: &AppState) -> Result<WhatsappBridgeStatusResponse> {
    if !state.config.whatsapp_bridge_enabled {
        return Ok(WhatsappBridgeStatusResponse {
            running: false,
            connection_state: None,
            requires_qr: false,
            qr_data_url: None,
            jid: None,
            last_error: None,
            message: Some("WhatsApp bridge is disabled".to_string()),
        });
    }

    let container = state.config.whatsapp_bridge_container_name.trim();
    if container.is_empty() {
        return Err(anyhow!("WhatsApp bridge container name must be configured"));
    }

    let running_out = run_docker_cmd(
        state,
        &["inspect", "--format", "{{.State.Running}}", container],
    )
    .await;
    let is_running = match running_out {
        Ok(v) => v.trim() == "true",
        Err(e) if e.to_string().contains("No such object") => false,
        Err(e) if e.to_string().contains("No such container") => false,
        Err(e) => return Err(e),
    };

    if !is_running {
        return Ok(WhatsappBridgeStatusResponse {
            running: false,
            connection_state: None,
            requires_qr: false,
            qr_data_url: None,
            jid: None,
            last_error: None,
            message: Some("WhatsApp bridge is not running".to_string()),
        });
    }

    for base in bridge_base_urls(state) {
        if let Ok(session) = fetch_bridge_session(&base).await {
            let mut qr_data_url = None;
            if session.requires_qr && session.qr_available {
                qr_data_url = fetch_bridge_qr(&base).await.ok();
            }

            return Ok(WhatsappBridgeStatusResponse {
                running: true,
                connection_state: Some(session.connection_state),
                requires_qr: session.requires_qr,
                qr_data_url,
                jid: session.jid,
                last_error: session.last_error,
                message: Some("WhatsApp bridge is running".to_string()),
            });
        }
    }

    Ok(WhatsappBridgeStatusResponse {
        running: true,
        connection_state: None,
        requires_qr: false,
        qr_data_url: None,
        jid: None,
        last_error: None,
        message: Some("WhatsApp bridge is running, but session status is unavailable".to_string()),
    })
}

pub async fn whatsapp_bridge_chats(
    state: &AppState,
    query: &str,
) -> Result<WhatsappBridgeChatsResponse> {
    if !state.config.whatsapp_bridge_enabled {
        return Ok(WhatsappBridgeChatsResponse {
            running: false,
            connection_state: None,
            chats: Vec::new(),
            message: Some("WhatsApp bridge is disabled".to_string()),
        });
    }

    let container = state.config.whatsapp_bridge_container_name.trim();
    if container.is_empty() {
        return Err(anyhow!("WhatsApp bridge container name must be configured"));
    }

    let running_out = run_docker_cmd(
        state,
        &["inspect", "--format", "{{.State.Running}}", container],
    )
    .await;
    let is_running = match running_out {
        Ok(v) => v.trim() == "true",
        Err(e) if e.to_string().contains("No such object") => false,
        Err(e) if e.to_string().contains("No such container") => false,
        Err(e) => return Err(e),
    };
    if !is_running {
        return Ok(WhatsappBridgeChatsResponse {
            running: false,
            connection_state: None,
            chats: Vec::new(),
            message: Some("WhatsApp bridge is not running".to_string()),
        });
    }

    for base in bridge_base_urls(state) {
        if let Ok(resp) = fetch_bridge_chats(&base, query).await {
            return Ok(WhatsappBridgeChatsResponse {
                running: true,
                connection_state: Some(resp.connection_state),
                chats: resp.chats,
                message: Some("WhatsApp chats loaded".to_string()),
            });
        }
    }

    Ok(WhatsappBridgeChatsResponse {
        running: true,
        connection_state: None,
        chats: Vec::new(),
        message: Some("WhatsApp bridge is running, but chats are unavailable".to_string()),
    })
}

#[derive(Debug, Deserialize)]
struct BridgeSessionStatus {
    connection_state: String,
    requires_qr: bool,
    qr_available: bool,
    jid: Option<String>,
    last_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BridgeQrResponse {
    qr_data_url: String,
}

#[derive(Debug, Deserialize)]
struct BridgeChatsResponse {
    chats: Vec<WhatsappBridgeChat>,
    connection_state: String,
}

async fn fetch_bridge_session(base_url: &str) -> Result<BridgeSessionStatus> {
    reqwest::Client::new()
        .get(format!("{}/session/status", base_url))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .with_context(|| format!("Failed to query bridge session status at {}", base_url))?
        .error_for_status()
        .context("Bridge session status returned non-success")?
        .json::<BridgeSessionStatus>()
        .await
        .context("Invalid bridge session status payload")
}

async fn fetch_bridge_qr(base_url: &str) -> Result<String> {
    Ok(reqwest::Client::new()
        .get(format!("{}/session/qr", base_url))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .with_context(|| format!("Failed to query bridge QR at {}", base_url))?
        .error_for_status()
        .context("Bridge QR endpoint returned non-success")?
        .json::<BridgeQrResponse>()
        .await
        .context("Invalid bridge QR payload")?
        .qr_data_url)
}

async fn fetch_bridge_chats(base_url: &str, query: &str) -> Result<BridgeChatsResponse> {
    reqwest::Client::new()
        .get(format!("{}/chats", base_url))
        .query(&[("q", query)])
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .with_context(|| format!("Failed to query bridge chats at {}", base_url))?
        .error_for_status()
        .context("Bridge chats endpoint returned non-success")?
        .json::<BridgeChatsResponse>()
        .await
        .context("Invalid bridge chats payload")
}

fn bridge_base_urls(state: &AppState) -> [String; 2] {
    let container = state.config.whatsapp_bridge_container_name.trim();
    [
        format!("http://{}:3200", container),
        format!("http://127.0.0.1:{}", state.config.whatsapp_bridge_port),
    ]
}

async fn detect_compose_network(state: &AppState) -> Option<String> {
    let self_container_id = tokio::fs::read_to_string("/etc/hostname")
        .await
        .ok()?
        .trim()
        .to_string();
    if self_container_id.is_empty() {
        return None;
    }

    let out = run_docker_cmd(
        state,
        &[
            "inspect",
            "--format",
            "{{range $k,$v := .NetworkSettings.Networks}}{{println $k}}{{end}}",
            &self_container_id,
        ],
    )
    .await
    .ok()?;

    out.lines()
        .map(str::trim)
        .find(|n| !n.is_empty() && *n != "bridge" && *n != "host" && *n != "none")
        .map(|s| s.to_string())
}

/// Restart the updater by spawning a detached helper container.
///
/// Running `docker compose up -d selu-updater` directly would kill the CLI
/// process mid-execution (exit 137) because it recreates the container we are
/// running in.  Instead we launch a short-lived helper container that inherits
/// our volume mounts (compose file, env file, docker socket) via
/// `--volumes-from` and performs the restart from *outside* this container.
async fn restart_self_via_helper(state: &AppState) -> Result<()> {
    let container_id = tokio::fs::read_to_string("/etc/hostname")
        .await
        .context("Failed to read own container ID from /etc/hostname")?
        .trim()
        .to_string();

    if container_id.is_empty() {
        return Err(anyhow!("Own container ID is empty"));
    }

    let image = resolve_updater_service_image(state).await?;

    let helper_name = format!(
        "selu-updater-restart-{}",
        &container_id[..12.min(container_id.len())]
    );

    // Remove any leftover helper container from a previous run.
    let _ = run_docker_cmd(state, &["rm", "-f", &helper_name]).await;

    run_docker_cmd(
        state,
        &[
            "run",
            "--rm",
            "-d",
            "--name",
            &helper_name,
            "--volumes-from",
            &container_id,
            "--entrypoint",
            "sh",
            &image,
            "-c",
            &build_self_restart_helper_script(state, &container_id),
        ],
    )
    .await?;

    Ok(())
}

fn build_self_restart_helper_script(state: &AppState, current_container_id: &str) -> String {
    let compose_args = build_compose_shell_args(state);
    let current_container_id = shell_quote(current_container_id);
    let updater_service = shell_quote(&state.config.updater_service);

    format!(
        concat!(
            "sleep 2; ",
            "docker stop {current_container_id} >/dev/null 2>&1 || true; ",
            "docker rm -f {current_container_id} >/dev/null 2>&1 || true; ",
            "docker compose {compose_args} rm -f -s {updater_service} >/dev/null 2>&1 || true; ",
            "docker compose {compose_args} up -d --no-deps --force-recreate {updater_service}"
        ),
        current_container_id = current_container_id,
        compose_args = compose_args,
        updater_service = updater_service,
    )
}

fn build_compose_shell_args(state: &AppState) -> String {
    let mut args = vec![
        "-f".to_string(),
        shell_quote(&state.config.compose_file),
        "--env-file".to_string(),
        shell_quote(&state.config.compose_env_file),
    ];
    if !state.config.compose_project_dir.trim().is_empty() {
        args.push("--project-directory".to_string());
        args.push(shell_quote(&state.config.compose_project_dir));
    }
    args.join(" ")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

async fn resolve_updater_service_image(state: &AppState) -> Result<String> {
    let image = run_docker_cmd(
        state,
        &[
            "compose",
            "-f",
            &state.config.compose_file,
            "--env-file",
            &state.config.compose_env_file,
            "config",
            "--images",
            &state.config.updater_service,
        ],
    )
    .await
    .map(|s| s.trim().to_string())
    .unwrap_or_default();

    if image.is_empty() {
        return Err(anyhow!(
            "Could not resolve updater service image from compose config"
        ));
    }

    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::{
        build_compose_shell_args, build_self_restart_helper_script,
        extract_digest_from_repo_digest_line, extract_tag_from_image_ref,
        resolve_whatsapp_bridge_image_ref, should_pull_whatsapp_bridge_image,
    };
    use crate::config::{ServerConfig, UpdaterConfig};
    use crate::state::AppState;

    #[test]
    fn resolves_whatsapp_bridge_image_with_channel_tag() {
        let image =
            resolve_whatsapp_bridge_image_ref("ghcr.io/selu-bot/selu-whatsapp-bridge", "stable")
                .expect("image should resolve");
        assert_eq!(image, "ghcr.io/selu-bot/selu-whatsapp-bridge:stable");
    }

    #[test]
    fn keeps_existing_whatsapp_bridge_image_tag() {
        let image =
            resolve_whatsapp_bridge_image_ref("ghcr.io/selu-bot/selu-whatsapp-bridge:1.2.3", "x")
                .expect("image should resolve");
        assert_eq!(image, "ghcr.io/selu-bot/selu-whatsapp-bridge:1.2.3");
    }

    #[test]
    fn pulls_registry_backed_whatsapp_bridge_images() {
        assert!(should_pull_whatsapp_bridge_image(
            "ghcr.io/selu-bot/selu-whatsapp-bridge:stable"
        ));
        assert!(should_pull_whatsapp_bridge_image(
            "ghcr.io/selu-bot/selu-whatsapp-bridge@sha256:abc"
        ));
        assert!(!should_pull_whatsapp_bridge_image(
            "selu-whatsapp-bridge:stable"
        ));
    }

    #[test]
    fn helper_script_stops_old_updater_before_recreate() {
        let state = AppState::new(test_config());

        let script = build_self_restart_helper_script(&state, "abc123");

        assert!(script.contains("docker stop 'abc123'"));
        assert!(script.contains("docker rm -f 'abc123'"));
        assert!(script.contains(
            "docker compose -f './docker-compose.yml' --env-file './.env' rm -f -s 'selu-updater'"
        ));
        assert!(script.contains(
            "docker compose -f './docker-compose.yml' --env-file './.env' up -d --no-deps --force-recreate 'selu-updater'"
        ));
    }

    #[test]
    fn compose_shell_args_quote_project_directory() {
        let mut cfg = test_config();
        cfg.compose_file = "/tmp/dir with spaces/docker-compose.yml".to_string();
        cfg.compose_env_file = "/tmp/dir with spaces/.env".to_string();
        cfg.compose_project_dir = "/tmp/dir with spaces".to_string();
        let state = AppState::new(cfg);

        let args = build_compose_shell_args(&state);

        assert_eq!(
            args,
            "-f '/tmp/dir with spaces/docker-compose.yml' --env-file '/tmp/dir with spaces/.env' --project-directory '/tmp/dir with spaces'"
        );
    }

    #[test]
    fn extracts_digest_from_repo_digest_line() {
        let line = "ghcr.io/selu-bot/selu@sha256:abc123";
        assert_eq!(
            extract_digest_from_repo_digest_line(line),
            Some("sha256:abc123".to_string())
        );
    }

    #[test]
    fn extracts_tag_from_repo_image_ref() {
        let tag =
            extract_tag_from_image_ref("ghcr.io/selu-bot/selu:unstable", "ghcr.io/selu-bot/selu");
        assert_eq!(tag, Some("unstable".to_string()));
    }

    #[test]
    fn extracts_tag_with_registry_port() {
        let tag =
            extract_tag_from_image_ref("localhost:5000/selu-bot/selu:dev", "ghcr.io/selu-bot/selu");
        assert_eq!(tag, Some("dev".to_string()));
    }

    fn test_config() -> UpdaterConfig {
        UpdaterConfig {
            server: ServerConfig {
                host: "0.0.0.0".to_string(),
                port: 8090,
            },
            shared_secret: String::new(),
            compose_file: "./docker-compose.yml".to_string(),
            compose_project_dir: String::new(),
            compose_service: "selu".to_string(),
            updater_service: "selu-updater".to_string(),
            compose_env_file: "./.env".to_string(),
            health_url: "http://127.0.0.1:3000/api/health".to_string(),
            health_timeout_secs: 90,
            health_interval_secs: 3,
            docker_bin: "docker".to_string(),
            image_repo: "ghcr.io/selu-bot/selu".to_string(),
            updater_tag_env_key: "SELU_UPDATER_IMAGE_TAG".to_string(),
            updater_pending_tag_env_key: "SELU_UPDATER_PENDING_TAG".to_string(),
            self_update_on_apply: false,
            whatsapp_bridge_enabled: true,
            whatsapp_bridge_image_repo: "ghcr.io/selu-bot/selu-whatsapp-bridge".to_string(),
            whatsapp_bridge_container_name: "selu-whatsapp-bridge".to_string(),
            whatsapp_bridge_data_volume: "selu-whatsapp-bridge-data".to_string(),
            whatsapp_bridge_network: String::new(),
            whatsapp_bridge_port: 3200,
        }
    }
}
