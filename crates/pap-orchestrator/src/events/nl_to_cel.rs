/// Natural language to CEL expression translator.
///
/// Uses an LLM provider to translate a human-readable filter description
/// into a valid CEL expression for event subscription filtering.
use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tracing::debug;

/// Translate a natural language description into a CEL expression.
///
/// The LLM is prompted with available variables (`event_type: string`, `event: map`)
/// and asked to produce a single valid CEL expression.
///
/// Requires an LLM API key (uses the same provider as chat).
pub async fn translate(
    description: &str,
    api_key: &str,
    base_url: &str,
    model: &str,
) -> Result<String> {
    if description.trim().is_empty() {
        return Ok(String::new());
    }

    let client = Client::new();

    let system_prompt = r#"You are a CEL (Common Expression Language) expert. Your job is to translate natural language filter descriptions into valid CEL expressions.

Available variables:
- `event_type`: string — the event type, e.g. "calendar.reminder", "home.alert"
- `event`: map — the event payload (arbitrary JSON object)

Rules:
1. Return ONLY the CEL expression, nothing else
2. No markdown, no backticks, no explanation
3. The expression must evaluate to a boolean
4. Use proper CEL syntax (==, !=, &&, ||, !, .startsWith(), .contains(), etc.)

Examples:
- "only calendar events" → event_type.startsWith("calendar.")
- "high severity alerts" → event_type == "home.alert" && event.severity == "high"
- "temperature above 25 degrees" → event.temperature > 25
- "any home automation event" → event_type.startsWith("home.")"#;

    let body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": format!("Translate this to a CEL expression: {}", description)}
        ],
        "temperature": 0.0,
        "max_tokens": 256,
    });

    // Try Anthropic-style first, then OpenAI-style
    let is_anthropic = base_url.contains("anthropic.com");

    let resp = if is_anthropic {
        let anthropic_body = json!({
            "model": model,
            "system": system_prompt,
            "messages": [
                {"role": "user", "content": format!("Translate this to a CEL expression: {}", description)}
            ],
            "temperature": 0.0,
            "max_tokens": 256,
        });
        client
            .post(format!("{}/v1/messages", base_url))
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&anthropic_body)
            .send()
            .await?
    } else {
        client
            .post(format!("{}/v1/chat/completions", base_url))
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await?
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("NL-to-CEL API error {}: {}", status, text));
    }

    let cel_expression = if is_anthropic {
        #[derive(Deserialize)]
        struct Block { text: Option<String> }
        #[derive(Deserialize)]
        struct Resp { content: Vec<Block> }
        let parsed: Resp = resp.json().await?;
        parsed.content.into_iter()
            .filter_map(|b| b.text)
            .collect::<Vec<_>>()
            .join("")
    } else {
        #[derive(Deserialize)]
        struct Msg { content: Option<String> }
        #[derive(Deserialize)]
        struct Choice { message: Msg }
        #[derive(Deserialize)]
        struct Resp { choices: Vec<Choice> }
        let parsed: Resp = resp.json().await?;
        parsed.choices.into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default()
    };

    let cel = cel_expression.trim().to_string();
    debug!(description = %description, cel = %cel, "Translated NL to CEL");
    Ok(cel)
}
