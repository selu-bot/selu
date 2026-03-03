/// /schedule command handlers.
///
/// Handles add, list, and delete subcommands for the schedule system.
/// All user-facing strings go through the server-side i18n system.
use tracing::error;

use super::{CommandContext, CommandResult};
use crate::i18n::t;
use crate::schedules;
use crate::schedules::nl_to_cron;

/// Handle `/schedule add <prompt + timing>`.
///
/// Uses the LLM to parse the natural language input into a cron expression,
/// then creates the schedule associated with the current pipe.
pub async fn handle_add(input: &str, ctx: &CommandContext<'_>) -> CommandResult {
    let lang = ctx.language;

    // Resolve the LLM provider for parsing
    let provider = match resolve_provider(ctx).await {
        Ok(p) => p,
        Err(msg) => return CommandResult { text: msg },
    };

    // Parse the natural language schedule
    let parsed = match nl_to_cron::parse_schedule(input, &provider).await {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to parse schedule: {e}");
            return CommandResult {
                text: t(lang, "cmd.schedule.parse_error").replace("{error}", &e.to_string()),
            };
        }
    };

    // Look up user's timezone
    let timezone = sqlx::query_scalar!("SELECT timezone FROM users WHERE id = ?", ctx.user_id)
        .fetch_optional(&ctx.state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "UTC".to_string());

    // Create the schedule with the current pipe
    match schedules::create_schedule(
        &ctx.state.db,
        ctx.user_id,
        &parsed.name,
        &parsed.prompt,
        &parsed.cron_expression,
        &parsed.cron_description,
        &timezone,
        &[ctx.pipe_id.to_string()],
    )
    .await
    {
        Ok(_id) => CommandResult {
            text: t(lang, "cmd.schedule.created")
                .replace("{name}", &parsed.name)
                .replace("{timing}", &parsed.cron_description)
                .replace("{prompt}", &parsed.prompt)
                .replace("{timezone}", &timezone)
                .replace("{cron}", &parsed.cron_expression),
        },
        Err(e) => {
            error!("Failed to create schedule: {e}");
            CommandResult {
                text: t(lang, "cmd.schedule.create_error").to_string(),
            }
        }
    }
}

/// Handle `/schedule list`.
pub async fn handle_list(ctx: &CommandContext<'_>) -> CommandResult {
    let lang = ctx.language;

    match schedules::list_schedules(&ctx.state.db, ctx.user_id).await {
        Ok(schedules) => {
            if schedules.is_empty() {
                return CommandResult {
                    text: t(lang, "cmd.schedule.list_empty").to_string(),
                };
            }

            let mut lines = vec![format!("**{}**\n", t(lang, "cmd.schedule.list_title"))];
            for s in &schedules {
                let status = if s.active {
                    t(lang, "cmd.schedule.status_on")
                } else {
                    t(lang, "cmd.schedule.status_off")
                };
                let pipes = if s.pipe_names.is_empty() {
                    t(lang, "cmd.schedule.no_pipes").to_string()
                } else {
                    s.pipe_names.join(", ")
                };
                lines.push(format!(
                    "- **{}** — {} [{}]\n  > {}\n  Pipes: {} | {}: {}",
                    s.name,
                    s.cron_description,
                    status,
                    truncate(&s.prompt, 80),
                    pipes,
                    t(lang, "cmd.schedule.next_run"),
                    s.next_run_at,
                ));
            }

            CommandResult {
                text: lines.join("\n"),
            }
        }
        Err(e) => {
            error!("Failed to list schedules: {e}");
            CommandResult {
                text: t(lang, "cmd.schedule.list_error").to_string(),
            }
        }
    }
}

/// Handle `/schedule delete <name>`.
pub async fn handle_delete(name_query: &str, ctx: &CommandContext<'_>) -> CommandResult {
    let lang = ctx.language;

    match schedules::find_by_name(&ctx.state.db, ctx.user_id, name_query).await {
        Ok(Some(schedule)) => {
            match schedules::delete_schedule(&ctx.state.db, &schedule.id, ctx.user_id).await {
                Ok(true) => CommandResult {
                    text: t(lang, "cmd.schedule.deleted").replace("{name}", &schedule.name),
                },
                Ok(false) => CommandResult {
                    text: t(lang, "cmd.schedule.not_found_hint").to_string(),
                },
                Err(e) => {
                    error!("Failed to delete schedule: {e}");
                    CommandResult {
                        text: t(lang, "cmd.schedule.delete_error").to_string(),
                    }
                }
            }
        }
        Ok(None) => CommandResult {
            text: t(lang, "cmd.schedule.not_found").replace("{name}", name_query),
        },
        Err(e) => {
            error!("Failed to find schedule: {e}");
            CommandResult {
                text: t(lang, "cmd.schedule.delete_error").to_string(),
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

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}
