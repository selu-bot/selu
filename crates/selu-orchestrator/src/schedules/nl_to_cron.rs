/// Natural language to cron expression translator.
///
/// Uses an LLM provider to parse human-readable scheduling descriptions
/// (e.g. "at 06:45 from monday to friday") into cron expressions.
/// Also extracts a short schedule name from the full prompt.
use anyhow::Result;
use serde::Deserialize;
use std::sync::Arc;

use crate::llm::provider::{ChatMessage, LlmProvider, LlmResponse};

/// Result of parsing a natural language schedule description.
#[derive(Debug, Clone)]
pub struct ParsedSchedule {
    /// Standard 6-field cron expression (sec min hour dom month dow)
    pub cron_expression: String,
    /// Human-readable description of the timing (e.g. "Weekdays at 6:45 AM")
    pub cron_description: String,
    /// Auto-generated short name for the schedule
    pub name: String,
    /// The prompt portion (what the agent should do), separated from timing
    pub prompt: String,
}

/// Intermediate JSON structure returned by the LLM.
#[derive(Debug, Deserialize)]
struct LlmParseResult {
    cron: String,
    description: String,
    name: String,
    prompt: String,
}

/// Parse a combined "/schedule add" input into structured schedule data.
///
/// The input is the full text after "/schedule add", which mixes the prompt
/// with timing info. The LLM separates them and generates the cron expression.
///
/// Uses the standard cron 6-field format: sec min hour dom month dow
pub async fn parse_schedule(
    input: &str,
    provider: &Arc<dyn LlmProvider>,
) -> Result<ParsedSchedule> {
    let system_prompt = r#"You are a scheduling assistant. Your job is to parse a user's scheduling request into structured data.

The user will provide a combined message like:
"Give me a morning summary of what's in my calendar today, emails received over night and weather at 06:45 from monday to friday"

You must separate the PROMPT (what should happen) from the TIMING (when it should happen), generate a cron expression, and create a short name.

CRITICAL: Use 6-field cron format: second minute hour day-of-month month day-of-week
- Seconds must always be 0
- Day-of-week: 0=Sunday, 1=Monday, 2=Tuesday, 3=Wednesday, 4=Thursday, 5=Friday, 6=Saturday
- Use * for "any"

Examples:
- "at 06:45 from monday to friday" → "0 45 6 * * 1-5"
- "every day at 8am" → "0 0 8 * * *"
- "every hour" → "0 0 * * * *"
- "every 30 minutes" → "0 */30 * * * *"
- "at 9pm on sundays" → "0 0 21 * * 0"
- "twice a day at 8am and 6pm" → "0 0 8,18 * * *"

Return ONLY a JSON object with these fields (no markdown, no backticks, no explanation):
{
  "cron": "0 45 6 * * 1-5",
  "description": "Weekdays at 6:45 AM",
  "name": "morning-summary",
  "prompt": "Give me a morning summary of what's in my calendar today, emails received over night and weather"
}

Rules for the name:
- Lowercase, kebab-case, max 30 characters
- Descriptive of the task, not the timing

Rules for the description:
- Short, human-readable timing in English (e.g. "Weekdays at 6:45 AM", "Every day at 8:00 AM")

Rules for the prompt:
- The full task description WITHOUT any timing information
- Keep the user's original wording"#;

    let msgs = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(format!("Parse this scheduling request: {}", input)),
    ];

    let response = provider.chat(&msgs, &[], 0.0).await?;

    let text = match response {
        LlmResponse::Text(t) => t,
        LlmResponse::ToolCalls(_) => {
            return Err(anyhow::anyhow!("LLM returned tool calls instead of text"));
        }
    };

    let text = text.trim();
    // Strip markdown code fences if the LLM wrapped it
    let json_str = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .map(|s: &str| s.strip_suffix("```").unwrap_or(s))
        .unwrap_or(text)
        .trim();

    let parsed: LlmParseResult = serde_json::from_str(json_str).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse LLM schedule response: {}. Raw: {}",
            e,
            text
        )
    })?;

    // Validate the cron expression
    super::validate_cron(&parsed.cron)?;

    Ok(ParsedSchedule {
        cron_expression: parsed.cron,
        cron_description: parsed.description,
        name: parsed.name,
        prompt: parsed.prompt,
    })
}

/// Parse just the timing portion from a web UI "when" field.
/// Returns (cron_expression, human_description).
pub async fn parse_timing(
    timing_text: &str,
    provider: &Arc<dyn LlmProvider>,
) -> Result<(String, String)> {
    let system_prompt = r#"You are a scheduling assistant. Convert a human-readable timing description to a cron expression.

CRITICAL: Use 6-field cron format: second minute hour day-of-month month day-of-week
- Seconds must always be 0
- Day-of-week: 0=Sunday, 1=Monday, 2=Tuesday, 3=Wednesday, 4=Thursday, 5=Friday, 6=Saturday
- Use * for "any"

Examples:
- "at 06:45 from monday to friday" → cron: "0 45 6 * * 1-5", description: "Weekdays at 6:45 AM"
- "every day at 8am" → cron: "0 0 8 * * *", description: "Every day at 8:00 AM"
- "every hour" → cron: "0 0 * * * *", description: "Every hour"
- "every 30 minutes" → cron: "0 */30 * * * *", description: "Every 30 minutes"

Return ONLY a JSON object (no markdown, no backticks):
{"cron": "0 45 6 * * 1-5", "description": "Weekdays at 6:45 AM"}"#;

    let msgs = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(format!("Convert this timing: {}", timing_text)),
    ];

    let response = provider.chat(&msgs, &[], 0.0).await?;

    let text = match response {
        LlmResponse::Text(t) => t,
        LlmResponse::ToolCalls(_) => {
            return Err(anyhow::anyhow!("LLM returned tool calls instead of text"));
        }
    };

    let text = text.trim();
    let json_str = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .map(|s: &str| s.strip_suffix("```").unwrap_or(s))
        .unwrap_or(text)
        .trim();

    #[derive(Deserialize)]
    struct TimingResult {
        cron: String,
        description: String,
    }

    let parsed: TimingResult = serde_json::from_str(json_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse timing response: {}. Raw: {}", e, text))?;

    super::validate_cron(&parsed.cron)?;

    Ok((parsed.cron, parsed.description))
}
