/// Natural language to schedule translator.
///
/// Uses an LLM provider to parse human-readable scheduling descriptions
/// into either recurring cron expressions or one-shot timestamps.
/// Handles both recurring patterns ("every weekday at 8am") and
/// one-time reminders ("next Sunday at 10am", "in 20 minutes").
use anyhow::Result;
use serde::Deserialize;
use std::sync::Arc;

use crate::llm::provider::{ChatMessage, LlmProvider, LlmResponse};

/// The type of schedule parsed from natural language.
#[derive(Debug, Clone)]
pub enum ScheduleType {
    /// A recurring schedule with a cron expression.
    Recurring {
        cron_expression: String,
        cron_description: String,
    },
    /// A one-shot reminder with a specific fire time (ISO 8601 UTC).
    OneShot {
        fire_at: String,
        description: String,
    },
}

/// Result of parsing a natural language schedule description.
#[derive(Debug, Clone)]
pub struct ParsedSchedule {
    /// Auto-generated short name for the schedule
    pub name: String,
    /// The prompt portion (what the agent should do), separated from timing
    pub prompt: String,
    /// Whether this is recurring or one-shot, with the relevant timing data
    pub schedule_type: ScheduleType,
}

/// Intermediate JSON structure returned by the LLM.
#[derive(Debug, Deserialize)]
struct LlmParseResult {
    #[serde(rename = "type")]
    schedule_type: String,
    #[serde(default)]
    cron: Option<String>,
    description: String,
    name: String,
    prompt: String,
    #[serde(default)]
    fire_at: Option<String>,
}

/// The type of timing parsed from the web UI "when" field.
#[derive(Debug, Clone)]
pub enum TimingResult {
    Recurring {
        cron_expression: String,
        description: String,
    },
    OneShot {
        fire_at: String,
        description: String,
    },
}

/// Parse a combined "/schedule add" or "/remind" input into structured schedule data.
///
/// The input is the full text after the command, which mixes the prompt
/// with timing info. The LLM separates them and determines whether this
/// is a recurring schedule or a one-shot reminder.
///
/// `now_utc` and `user_timezone` are provided so the LLM can resolve
/// relative times like "in 20 minutes" or "next Sunday".
pub async fn parse_schedule(
    input: &str,
    provider: &Arc<dyn LlmProvider>,
    now_utc: &str,
    user_timezone: &str,
) -> Result<ParsedSchedule> {
    let system_prompt = format!(
        r#"You are a scheduling assistant. Your job is to parse a user's scheduling request into structured data.

The user will provide a combined message like:
"Give me a morning summary of what's in my calendar today, emails received over night and weather at 06:45 from monday to friday"
or
"Check the weather and email about meat for grilling next Sunday morning"

You must:
1. Separate the PROMPT (what should happen) from the TIMING (when it should happen)
2. Determine if this is RECURRING or ONE-SHOT
3. Generate the appropriate timing data and a short name

Current UTC time: {now_utc}
User timezone: {user_timezone}

RECURRING schedules — use 6-field cron format: second minute hour day-of-month month day-of-week
- Seconds must always be 0
- Day-of-week: 0=Sunday, 1=Monday, 2=Tuesday, 3=Wednesday, 4=Thursday, 5=Friday, 6=Saturday
- Use * for "any"
- Examples: "every day at 8am", "weekdays at 6:45", "every hour"

ONE-SHOT reminders — for things that happen once at a specific time:
- "next Sunday at 10am", "in 20 minutes", "tomorrow at 3pm", "on March 15 at noon"
- Return an ISO 8601 datetime in UTC (convert from user's timezone)

Return ONLY a JSON object (no markdown, no backticks, no explanation):

For RECURRING:
{{
  "type": "recurring",
  "cron": "0 45 6 * * 1-5",
  "description": "Weekdays at 6:45 AM",
  "name": "morning-summary",
  "prompt": "Give me a morning summary of what's in my calendar today"
}}

For ONE-SHOT:
{{
  "type": "one_shot",
  "fire_at": "2026-03-08T09:00:00Z",
  "description": "Next Sunday at 10:00 AM (Europe/Berlin)",
  "name": "check-weather-sunday",
  "prompt": "Check the weather and email about meat for grilling"
}}

Rules for the name:
- Lowercase, kebab-case, max 30 characters
- Descriptive of the task, not the timing

Rules for the description:
- Short, human-readable timing (e.g. "Weekdays at 6:45 AM", "Next Sunday at 10:00 AM")

Rules for the prompt:
- The full task description WITHOUT any timing information
- Keep the user's original wording

Rules for determining type:
- Words like "every", "daily", "weekly", "weekdays", "on mondays" → recurring
- Words like "next", "tomorrow", "in X minutes/hours", "on [specific date]", "this Sunday" → one_shot
- If ambiguous, prefer recurring"#
    );

    let msgs = vec![
        ChatMessage::system(&system_prompt),
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

    let schedule_type = if parsed.schedule_type == "one_shot" {
        let fire_at = parsed
            .fire_at
            .ok_or_else(|| anyhow::anyhow!("LLM returned one_shot type but no fire_at"))?;
        ScheduleType::OneShot {
            fire_at,
            description: parsed.description,
        }
    } else {
        let cron = parsed
            .cron
            .ok_or_else(|| anyhow::anyhow!("LLM returned recurring type but no cron"))?;
        // Validate the cron expression
        super::validate_cron(&cron)?;
        ScheduleType::Recurring {
            cron_expression: cron,
            cron_description: parsed.description,
        }
    };

    Ok(ParsedSchedule {
        name: parsed.name,
        prompt: parsed.prompt,
        schedule_type,
    })
}

/// Parse just the timing portion from a web UI "when" field.
///
/// Returns either a recurring cron result or a one-shot timestamp.
/// `now_utc` and `user_timezone` are provided for resolving relative times.
pub async fn parse_timing(
    timing_text: &str,
    provider: &Arc<dyn LlmProvider>,
    now_utc: &str,
    user_timezone: &str,
) -> Result<TimingResult> {
    let system_prompt = format!(
        r#"You are a scheduling assistant. Convert a human-readable timing description to either a cron expression (for recurring) or an ISO 8601 UTC timestamp (for one-shot).

Current UTC time: {now_utc}
User timezone: {user_timezone}

RECURRING — use 6-field cron format: second minute hour day-of-month month day-of-week
- Seconds must always be 0
- Day-of-week: 0=Sunday, 1=Monday, 2=Tuesday, 3=Wednesday, 4=Thursday, 5=Friday, 6=Saturday
- Use * for "any"

ONE-SHOT — for things that happen once:
- Convert from user's timezone to UTC

Return ONLY a JSON object (no markdown, no backticks):

For RECURRING:
{{"type": "recurring", "cron": "0 45 6 * * 1-5", "description": "Weekdays at 6:45 AM"}}

For ONE-SHOT:
{{"type": "one_shot", "fire_at": "2026-03-08T09:00:00Z", "description": "Next Sunday at 10:00 AM"}}

Rules for determining type:
- Words like "every", "daily", "weekly", "weekdays", "on mondays" → recurring
- Words like "next", "tomorrow", "in X minutes/hours", "on [specific date]", "this Sunday" → one_shot
- If ambiguous, prefer recurring"#
    );

    let msgs = vec![
        ChatMessage::system(&system_prompt),
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
    struct RawTimingResult {
        #[serde(rename = "type")]
        timing_type: String,
        #[serde(default)]
        cron: Option<String>,
        description: String,
        #[serde(default)]
        fire_at: Option<String>,
    }

    let parsed: RawTimingResult = serde_json::from_str(json_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse timing response: {}. Raw: {}", e, text))?;

    if parsed.timing_type == "one_shot" {
        let fire_at = parsed
            .fire_at
            .ok_or_else(|| anyhow::anyhow!("LLM returned one_shot type but no fire_at"))?;
        Ok(TimingResult::OneShot {
            fire_at,
            description: parsed.description,
        })
    } else {
        let cron = parsed
            .cron
            .ok_or_else(|| anyhow::anyhow!("LLM returned recurring type but no cron"))?;
        super::validate_cron(&cron)?;
        Ok(TimingResult::Recurring {
            cron_expression: cron,
            description: parsed.description,
        })
    }
}
