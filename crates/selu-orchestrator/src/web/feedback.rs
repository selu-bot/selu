use anyhow::{Context, Result};
use askama::Template;
use axum::{
    Form,
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::llm::provider::ToolSpec;
use crate::state::AppState;
use crate::web::BasePath;
use crate::web::auth::AuthUser;

// ── Template ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "feedback.html")]
struct FeedbackTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
}

// ── GET /feedback ───────────────────────────────────────────────────────────

pub async fn feedback_page(user: AuthUser, BasePath(base_path): BasePath) -> Response {
    let tmpl = FeedbackTemplate {
        active_nav: "feedback",
        is_admin: user.is_admin,
        base_path,
    };
    match tmpl.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── POST /feedback ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct FeedbackForm {
    pub category: String,
    pub description: String,
}

pub async fn feedback_submit(
    _user: AuthUser,
    State(state): State<AppState>,
    Form(form): Form<FeedbackForm>,
) -> Response {
    let description = form.description.trim();

    // Validate
    if !matches!(
        form.category.as_str(),
        "bug" | "idea" | "question" | "other"
    ) {
        return Html(
            r#"<div class="p-3 rounded-lg bg-rosie/10 text-rosie text-sm" data-i18n="feedback.error.category">Please select a category.</div>"#,
        )
        .into_response();
    }
    if description.len() < 10 {
        return Html(
            r#"<div class="p-3 rounded-lg bg-rosie/10 text-rosie text-sm" data-i18n="feedback.error.too_short">Please describe your feedback in at least a few words.</div>"#,
        )
        .into_response();
    }
    if description.len() > 2000 {
        return Html(
            r#"<div class="p-3 rounded-lg bg-rosie/10 text-rosie text-sm" data-i18n="feedback.error.too_long">Your message is too long. Please keep it under 2000 characters.</div>"#,
        )
        .into_response();
    }

    let gateway_base = match feedback_api_base_url(&state.config.marketplace_url) {
        Some(url) => url,
        None => {
            error!("Could not derive feedback API base URL from marketplace_url");
            return Html(
                r#"<div class="p-3 rounded-lg bg-rosie/10 text-rosie text-sm" data-i18n="feedback.error.unavailable">Feedback is currently unavailable. Please try again later.</div>"#,
            )
            .into_response();
        }
    };

    let instance_id = match crate::persistence::db::get_instance_id(&state.db).await {
        Ok(id) => id,
        Err(e) => {
            error!("Failed to get instance_id: {e}");
            return Html(
                r#"<div class="p-3 rounded-lg bg-rosie/10 text-rosie text-sm" data-i18n="feedback.error.unavailable">Feedback is currently unavailable. Please try again later.</div>"#,
            )
            .into_response();
        }
    };

    match submit_to_gateway(
        &gateway_base,
        &instance_id,
        &form.category,
        None,
        description,
    )
    .await
    {
        Ok(resp) => {
            let url = crate::web::chat::html_escape(&resp.issue_url);
            Html(format!(
                r#"<div class="p-4 rounded-lg bg-emerald-500/10 text-emerald-600 dark:text-emerald-400 text-sm space-y-2">
                    <p class="font-medium" data-i18n="feedback.success.title">Thank you for your feedback!</p>
                    <p data-i18n="feedback.success.body">Your feedback has been submitted. You can track it here:</p>
                    <a href="{url}" target="_blank" rel="noopener" class="inline-flex items-center gap-1 underline hover:no-underline">#{} ↗</a>
                </div>"#,
                resp.issue_number,
            ))
            .into_response()
        }
        Err(e) => {
            error!("Failed to submit feedback: {e}");
            Html(
                r#"<div class="p-3 rounded-lg bg-rosie/10 text-rosie text-sm" data-i18n="feedback.error.failed">Something went wrong while sending your feedback. Please try again.</div>"#,
            )
            .into_response()
        }
    }
}

// ── Gateway client ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
pub struct GatewayFeedbackResponse {
    pub issue_url: String,
    pub issue_number: u64,
}

#[derive(Debug, Serialize)]
struct GatewayFeedbackRequest<'a> {
    category: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    description: &'a str,
}

async fn submit_to_gateway(
    api_base: &str,
    instance_id: &str,
    category: &str,
    title: Option<&str>,
    description: &str,
) -> Result<GatewayFeedbackResponse> {
    let url = format!("{}/feedback", api_base);
    let payload = GatewayFeedbackRequest {
        category,
        title,
        description,
    };

    let resp = reqwest::Client::new()
        .post(&url)
        .header("x-instance-id", instance_id)
        .json(&payload)
        .send()
        .await
        .context("Failed to reach feedback gateway")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Feedback gateway returned {status}: {body}");
    }

    resp.json::<GatewayFeedbackResponse>()
        .await
        .context("Failed to parse feedback gateway response")
}

/// Derive the API base URL (e.g. `https://selu.bot/api`) from the marketplace URL.
fn feedback_api_base_url(marketplace_url: &str) -> Option<String> {
    let trimmed = marketplace_url.trim_end_matches('/');
    let marker = "/marketplace/agents";
    trimmed
        .find(marker)
        .map(|idx| trimmed[..idx].to_string())
        .filter(|s| !s.is_empty())
}

// ── Built-in tool: submit_feedback ──────────────────────────────────────────

pub fn submit_feedback_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "submit_feedback".to_string(),
        description: "Submit feedback, a bug report, feature idea, or question to the selu developers. \
             The feedback will be posted publicly as a GitHub issue. \
             IMPORTANT: Before using this tool, warn the user that their feedback will be visible publicly. \
             Never include personal data, conversation history, or private information — \
             only include what the user explicitly wants to share."
            .to_string(),
        parameters: serde_json::json!({
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
    db: &sqlx::SqlitePool,
    args: &str,
) -> Result<String> {
    let parsed: SubmitFeedbackArgs =
        serde_json::from_str(args).context("Invalid submit_feedback arguments")?;

    let api_base =
        feedback_api_base_url(marketplace_url).context("Could not derive feedback API base URL")?;

    let instance_id = crate::persistence::db::get_instance_id(db)
        .await
        .context("Failed to load instance_id")?;

    let resp = submit_to_gateway(
        &api_base,
        &instance_id,
        &parsed.category,
        Some(&parsed.title),
        &parsed.description,
    )
    .await?;

    info!(
        issue_number = resp.issue_number,
        category = %parsed.category,
        "Feedback submitted via chat tool"
    );

    Ok(format!(
        "Feedback submitted successfully! Issue #{}: {}",
        resp.issue_number, resp.issue_url
    ))
}
