/// Slash command system.
///
/// Intercepts messages starting with `/` and routes them to command handlers
/// instead of the agent engine. Extensible — new commands just need a new
/// module and a match arm in `dispatch`.
pub mod schedule;

use crate::i18n::t;
use crate::state::AppState;

/// The result of a slash command execution.
pub struct CommandResult {
    /// Plain-text response (will be displayed as the assistant reply)
    pub text: String,
}

/// Context for command execution.
pub struct CommandContext<'a> {
    pub state: &'a AppState,
    pub user_id: &'a str,
    pub pipe_id: &'a str,
    pub language: &'a str,
}

/// Parsed command variants.
pub enum Command {
    ScheduleAdd(String), // everything after "/schedule add "
    ScheduleList,
    ScheduleDelete(String), // everything after "/schedule delete "
    Unknown(String),        // the full command text
}

/// Try to parse a message as a slash command.
///
/// Returns `None` if the message doesn't start with `/`.
pub fn parse_command(text: &str) -> Option<Command> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let lower = trimmed.to_lowercase();

    if lower.starts_with("/schedule add ") {
        let rest = trimmed["/schedule add ".len()..].trim().to_string();
        if rest.is_empty() {
            return Some(Command::Unknown(trimmed.to_string()));
        }
        return Some(Command::ScheduleAdd(rest));
    }

    if lower == "/schedule list" || lower == "/schedules" {
        return Some(Command::ScheduleList);
    }

    if lower.starts_with("/schedule delete ") || lower.starts_with("/schedule remove ") {
        let prefix_len = if lower.starts_with("/schedule delete ") {
            "/schedule delete ".len()
        } else {
            "/schedule remove ".len()
        };
        let rest = trimmed[prefix_len..].trim().to_string();
        if rest.is_empty() {
            return Some(Command::Unknown(trimmed.to_string()));
        }
        return Some(Command::ScheduleDelete(rest));
    }

    Some(Command::Unknown(trimmed.to_string()))
}

/// Dispatch a parsed command and return the response text.
pub async fn dispatch(cmd: Command, ctx: CommandContext<'_>) -> CommandResult {
    match cmd {
        Command::ScheduleAdd(input) => schedule::handle_add(&input, &ctx).await,
        Command::ScheduleList => schedule::handle_list(&ctx).await,
        Command::ScheduleDelete(name) => schedule::handle_delete(&name, &ctx).await,
        Command::Unknown(text) => CommandResult {
            text: t(ctx.language, "cmd.unknown").replace("{cmd}", &text),
        },
    }
}
