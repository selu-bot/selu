use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use tracing::info;

use crate::{
    llm::provider::ToolSpec,
    services::accounts::{self, GeneralFeedback},
};

pub fn submit_feedback_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "submit_feedback".to_string(),
        description: "Submit feedback, a bug report, feature idea, or question to the Selu developers. \
             The feedback will be posted publicly as a GitHub issue. \
             IMPORTANT: Before using this tool, warn the user that their feedback will be visible publicly. \
             Never include personal data, conversation history, or private information — \
             only include what the user explicitly wants to share."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "category": {
                    "type": "string",
                    "enum": ["bug", "idea", "question", "other"],
                    "description": "The type of feedback: bug (something broken), idea (feature request), question (need help), or other"
                },
                "title": {
                    "type": "string",
                    "description": "A short summary of the feedback (max 100 characters)"
                },
                "description": {
                    "type": "string",
                    "description": "Detailed description of the feedback (10-2000 characters). Write this from the user's perspective."
                }
            },
            "required": ["category", "title", "description"]
        }),
    }
}

#[derive(Debug, Deserialize)]
struct SubmitFeedbackArgs {
    category: String,
    title: String,
    description: String,
}

pub async fn dispatch_submit_feedback(
    marketplace_url: &str,
    db: &SqlitePool,
    args: &str,
) -> Result<String> {
    let parsed: SubmitFeedbackArgs =
        serde_json::from_str(args).context("invalid submit_feedback arguments")?;
    let instance_id = crate::persistence::db::get_instance_id(db)
        .await
        .context("failed to load instance ID")?;
    let client = reqwest::Client::builder()
        .user_agent(format!("selu/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to create feedback client")?;
    let receipt = accounts::submit_general_feedback(
        &client,
        marketplace_url,
        &instance_id,
        &GeneralFeedback {
            category: parsed.category.clone(),
            title: Some(parsed.title),
            description: parsed.description,
        },
    )
    .await
    .context("failed to submit feedback")?;

    info!(
        issue_number = receipt.issue_number,
        category = %parsed.category,
        "Feedback submitted via chat tool"
    );

    Ok(format!(
        "Feedback submitted successfully! Issue #{}: {}",
        receipt.issue_number, receipt.issue_url
    ))
}
