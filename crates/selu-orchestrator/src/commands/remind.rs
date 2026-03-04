/// /remind command handler.
///
/// Handles creating one-shot reminders from natural language input.
/// All user-facing strings go through the server-side i18n system.
use chrono::Utc;
use tracing::error;

use super::{CommandContext, CommandResult};
use crate::i18n::t;
use crate::schedules;
use crate::schedules::nl_to_cron;

/// Handle `/remind <prompt + timing>`.
///
/// Uses the LLM to parse the natural language input, then creates
/// a one-shot reminder. If the LLM detects recurring timing, it
/// suggests using /schedule add instead.
pub async fn handle_add(input: &str, ctx: &CommandContext<'_>) -> CommandResult {
    let lang = ctx.language;

    // Resolve the LLM provider for parsing
    let provider = match resolve_provider(ctx).await {
        Ok(p) => p,
        Err(msg) => return CommandResult { text: msg },
    };

    // Get user's timezone
    let timezone = sqlx::query_scalar!("SELECT timezone FROM users WHERE id = ?", ctx.user_id)
        .fetch_optional(&ctx.state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "UTC".to_string());

    let now_utc = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    // Parse the natural language input
    let parsed = match nl_to_cron::parse_schedule(input, &provider, &now_utc, &timezone).await {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to parse reminder: {e}");
            return CommandResult {
                text: t(lang, "cmd.remind.parse_error").replace("{error}", &e.to_string()),
            };
        }
    };

    match parsed.schedule_type {
        nl_to_cron::ScheduleType::OneShot {
            fire_at,
            description,
        } => {
            // Parse the fire_at timestamp
            let fire_at_dt = match chrono::DateTime::parse_from_rfc3339(&fire_at)
                .or_else(|_| {
                    chrono::NaiveDateTime::parse_from_str(&fire_at, "%Y-%m-%dT%H:%M:%S")
                        .map(|dt| dt.and_utc().fixed_offset())
                })
                .map(|dt| dt.with_timezone(&Utc))
            {
                Ok(dt) => dt,
                Err(e) => {
                    error!("Failed to parse fire_at '{}': {e}", fire_at);
                    return CommandResult {
                        text: t(lang, "cmd.remind.parse_error").replace("{error}", &e.to_string()),
                    };
                }
            };

            // Create the one-shot reminder
            match schedules::create_reminder(
                &ctx.state.db,
                ctx.user_id,
                &parsed.name,
                &parsed.prompt,
                fire_at_dt,
                &description,
                &[ctx.pipe_id.to_string()],
            )
            .await
            {
                Ok(_id) => CommandResult {
                    text: t(lang, "cmd.remind.created")
                        .replace("{name}", &parsed.name)
                        .replace("{timing}", &description)
                        .replace("{prompt}", &parsed.prompt),
                },
                Err(e) => {
                    error!("Failed to create reminder: {e}");
                    CommandResult {
                        text: t(lang, "cmd.remind.create_error").to_string(),
                    }
                }
            }
        }
        nl_to_cron::ScheduleType::Recurring { .. } => {
            // The user asked for something recurring via /remind — suggest /schedule
            CommandResult {
                text: t(lang, "cmd.remind.use_schedule").replace("{input}", input),
            }
        }
    }
}

/// Resolve the default LLM provider for command parsing.
async fn resolve_provider(
    ctx: &CommandContext<'_>,
) -> Result<std::sync::Arc<dyn crate::llm::provider::LlmProvider>, String> {
    let lang = ctx.language;

    let resolved = crate::agents::model::resolve_model(&ctx.state.db, "default")
        .await
        .map_err(|_| t(lang, "cmd.schedule.no_provider").to_string())?;

    ctx.state
        .provider_cache
        .get_or_load(
            &ctx.state.db,
            &resolved.provider_id,
            &resolved.model_id,
            &ctx.state.credentials,
        )
        .await
        .map_err(|_| t(lang, "cmd.schedule.provider_error").to_string())
}
