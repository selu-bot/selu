/// Built-in image generation and editing tools.
///
/// Exposed as native tools in the LLM tool loop when the agent has an
/// image model configured. This lets the LLM decide when to generate or
/// edit images naturally, supports multi-step workflows (e.g. "generate
/// an image and email it"), and integrates with tool policies and the
/// approval flow like every other built-in tool.
use anyhow::Result;
use tracing::warn;

use crate::agents::artifacts::{self, ArtifactRef, ArtifactStore};
use crate::llm::image_provider::ImageInput;
use crate::llm::provider::ToolSpec;
use crate::llm::registry::load_image_provider;
use crate::permissions::CredentialStore;

// ── Tool specs ──────────────────────────────────────────────────────────────

/// Tool spec for `image_generate` — generate an image from a text prompt.
pub fn generate_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "image_generate".to_string(),
        description: "Generate an image from a text description. \
             Use this tool when the user asks you to create, draw, render, \
             or generate an image, illustration, logo, poster, or any visual. \
             Returns an artifact reference to the generated image."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Detailed description of the image to generate. \
                         Be specific about style, composition, colors, and subject matter."
                }
            },
            "required": ["prompt"]
        }),
    }
}

/// Tool spec for `image_edit` — edit an existing image.
pub fn edit_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "image_edit".to_string(),
        description: "Edit or modify an existing image. Use this tool when the user \
             asks you to change, retouch, remove background, add elements, crop, \
             or otherwise modify an image they provided. Requires at least one \
             artifact_id referencing an image attachment."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Description of the edits to apply to the image."
                },
                "artifact_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Artifact IDs of the input images to edit. \
                         These come from inbound attachments visible in the conversation."
                }
            },
            "required": ["prompt", "artifact_ids"]
        }),
    }
}

// ── Dispatch handlers ───────────────────────────────────────────────────────

/// Handle an `image_generate` tool call.
pub async fn dispatch_generate(
    db: &sqlx::SqlitePool,
    credentials: &CredentialStore,
    artifact_store: &ArtifactStore,
    agent_id: &str,
    user_id: &str,
    session_id: &str,
    args: &serde_json::Value,
) -> Result<String> {
    let prompt = args["prompt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("image_generate: missing 'prompt' string"))?;

    let image_model = crate::agents::model::resolve_image_model(db, agent_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("No image model configured for this agent"))?;

    let provider = load_image_provider(
        db,
        &image_model.provider_id,
        &image_model.model_id,
        credentials,
    )
    .await?;

    let generated = provider.generate(prompt).await?;

    let stored = artifacts::store_inbound_attachment(
        artifact_store,
        user_id,
        session_id,
        &generated.filename,
        &generated.mime_type,
        generated.data,
    )
    .await?;

    Ok(artifact_result_json(&stored))
}

/// Handle an `image_edit` tool call.
pub async fn dispatch_edit(
    db: &sqlx::SqlitePool,
    credentials: &CredentialStore,
    artifact_store: &ArtifactStore,
    agent_id: &str,
    user_id: &str,
    session_id: &str,
    args: &serde_json::Value,
) -> Result<String> {
    let prompt = args["prompt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("image_edit: missing 'prompt' string"))?;

    let artifact_ids: Vec<&str> = args["artifact_ids"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("image_edit: missing 'artifact_ids' array"))?
        .iter()
        .filter_map(|v| v.as_str())
        .collect();

    if artifact_ids.is_empty() {
        return Ok(serde_json::json!({
            "error": "No artifact IDs provided. Please reference the image attachments from the conversation."
        }).to_string());
    }

    let mut edit_inputs = Vec::new();
    for artifact_id in &artifact_ids {
        let Some(stored) = artifacts::get_for_user(artifact_store, artifact_id, user_id).await
        else {
            warn!(artifact_id = %artifact_id, "image_edit: artifact not found");
            continue;
        };
        if !stored.mime_type.starts_with("image/") {
            warn!(artifact_id = %artifact_id, mime = %stored.mime_type, "image_edit: skipping non-image artifact");
            continue;
        }
        edit_inputs.push(ImageInput {
            filename: stored.filename,
            mime_type: stored.mime_type,
            data: stored.data,
        });
    }

    if edit_inputs.is_empty() {
        return Ok(serde_json::json!({
            "error": "None of the provided artifact IDs reference valid image attachments."
        })
        .to_string());
    }

    let image_model = crate::agents::model::resolve_image_model(db, agent_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("No image model configured for this agent"))?;

    let provider = load_image_provider(
        db,
        &image_model.provider_id,
        &image_model.model_id,
        credentials,
    )
    .await?;

    let generated = provider.edit(prompt, &edit_inputs).await?;

    let stored = artifacts::store_inbound_attachment(
        artifact_store,
        user_id,
        session_id,
        &generated.filename,
        &generated.mime_type,
        generated.data,
    )
    .await?;

    Ok(artifact_result_json(&stored))
}

fn artifact_result_json(stored: &ArtifactRef) -> String {
    serde_json::json!({
        "ok": true,
        "artifact_id": stored.artifact_id,
        "filename": stored.filename,
        "mime_type": stored.mime_type,
        "size_bytes": stored.size_bytes,
    })
    .to_string()
}
