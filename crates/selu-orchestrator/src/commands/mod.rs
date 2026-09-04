//! Slash command system.
//!
//! Messages that start with `/` are handled here instead of by the agent
//! engine, on every channel: the v1 conversation API (web and iOS), Telegram,
//! iMessage and webhook pipes such as WhatsApp. Commands are deterministic and
//! independent of agent routing and tool policies, which is what makes them
//! worth having next to the natural-language scheduling tools.
//!
//! Extensible: add a variant to [`Command`], an entry to [`catalog`], a match
//! arm in [`dispatch`], and the i18n keys. Clients build their pickers from
//! the catalog, so a new command needs no client release.
pub mod remind;
pub mod schedule;

use serde::Serialize;
use tracing::{error, warn};
use uuid::Uuid;

use crate::i18n::t;
use crate::state::AppState;

/// The result of a slash command execution.
pub struct CommandResult {
    /// Markdown response, shown as the assistant reply.
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
    Remind(String),         // everything after "/remind "
    Help,
    Unknown(String), // the full command text
}

/// One entry of the command catalog shown by chat clients.
#[derive(Debug, Clone, Serialize)]
pub struct CommandInfo {
    /// Text a client inserts into the composer, e.g. `/schedule add `.
    pub command: String,
    /// Short display form, e.g. `/schedule add`.
    pub label: String,
    /// One-line, localized description.
    pub description: String,
    /// Localized placeholder for the argument, when the command takes one.
    pub argument_hint: Option<String>,
}

/// The commands available to users, in display order.
pub fn catalog(lang: &str) -> Vec<CommandInfo> {
    let entry = |label: &str, key: &str, hint: Option<&str>| CommandInfo {
        command: if hint.is_some() {
            format!("{label} ")
        } else {
            label.to_string()
        },
        label: label.to_string(),
        description: t(lang, &format!("cmd.catalog.{key}.description")).to_string(),
        argument_hint: hint.map(|hint_key| t(lang, hint_key).to_string()),
    };
    vec![
        entry("/remind", "remind", Some("cmd.catalog.remind.hint")),
        entry(
            "/schedule add",
            "schedule_add",
            Some("cmd.catalog.schedule_add.hint"),
        ),
        entry("/schedule list", "schedule_list", None),
        entry(
            "/schedule delete",
            "schedule_delete",
            Some("cmd.catalog.schedule_delete.hint"),
        ),
        entry("/help", "help", None),
    ]
}

/// Top-level commands for channels with a single-word command menu (Telegram).
/// Returns `(command_without_slash, localized_description)`.
pub fn channel_menu(lang: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "remind",
            t(lang, "cmd.catalog.remind.description").to_string(),
        ),
        (
            "schedule",
            t(lang, "cmd.catalog.schedule.description").to_string(),
        ),
        ("help", t(lang, "cmd.catalog.help.description").to_string()),
    ]
}

/// Localized help text listing every command, built from the catalog so it
/// can never drift from what the pickers show.
pub fn help_text(lang: &str) -> String {
    let mut lines = vec![t(lang, "cmd.help.title").to_string(), String::new()];
    for info in catalog(lang) {
        let usage = match &info.argument_hint {
            Some(hint) => format!("{} <{}>", info.label, hint),
            None => info.label.clone(),
        };
        lines.push(format!("- `{}` — {}", usage, info.description));
    }
    lines.push(String::new());
    lines.push(t(lang, "cmd.help.footer").to_string());
    lines.join("\n")
}

/// Whether a message should be handled as a command rather than sent to an
/// agent: a leading slash followed by a letter. This keeps a stray path or
/// fraction such as `/tmp` or `1/2` from being swallowed only when it starts
/// with a digit or symbol, which mirrors what users expect from chat apps.
pub fn is_command(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with('/')
        && trimmed
            .chars()
            .nth(1)
            .is_some_and(|c| c.is_ascii_alphabetic())
}

/// Try to parse a message as a slash command.
///
/// Returns `None` if the message is not a command. A Telegram-style bot
/// suffix on the command word (`/schedule@selubot list`) is ignored.
pub fn parse_command(text: &str) -> Option<Command> {
    if !is_command(text) {
        return None;
    }
    let trimmed = text.trim();
    let (word, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (trimmed, ""),
    };
    let word = word.split('@').next().unwrap_or(word).to_lowercase();

    Some(match word.as_str() {
        "/help" | "/start" | "/commands" => Command::Help,
        "/remind" if rest.is_empty() => Command::Unknown(trimmed.to_string()),
        "/remind" => Command::Remind(rest.to_string()),
        "/schedules" if rest.is_empty() => Command::ScheduleList,
        "/schedule" => {
            let (sub, arg) = match rest.split_once(char::is_whitespace) {
                Some((sub, arg)) => (sub.to_lowercase(), arg.trim()),
                None => (rest.to_lowercase(), ""),
            };
            match sub.as_str() {
                "" | "help" => Command::Help,
                "add" if !arg.is_empty() => Command::ScheduleAdd(arg.to_string()),
                "list" if arg.is_empty() => Command::ScheduleList,
                "delete" | "remove" if !arg.is_empty() => Command::ScheduleDelete(arg.to_string()),
                _ => Command::Unknown(trimmed.to_string()),
            }
        }
        _ => Command::Unknown(trimmed.to_string()),
    })
}

/// Dispatch a parsed command and return the response text.
pub async fn dispatch(cmd: Command, ctx: CommandContext<'_>) -> CommandResult {
    match cmd {
        Command::ScheduleAdd(input) => schedule::handle_add(&input, &ctx).await,
        Command::ScheduleList => schedule::handle_list(&ctx).await,
        Command::ScheduleDelete(name) => schedule::handle_delete(&name, &ctx).await,
        Command::Remind(input) => remind::handle_add(&input, &ctx).await,
        Command::Help => CommandResult {
            text: help_text(ctx.language),
        },
        Command::Unknown(text) => CommandResult {
            text: format!(
                "{}\n\n{}",
                t(ctx.language, "cmd.unknown").replace("{cmd}", &text),
                help_text(ctx.language)
            ),
        },
    }
}

/// A command that was executed and recorded in a conversation thread.
pub struct CommandReply {
    /// ID of the persisted assistant message carrying `text`.
    pub reply_message_id: String,
    /// Markdown reply for the channel to deliver.
    pub text: String,
    pub created_at: String,
}

/// Handle `text` as a slash command if it is one.
///
/// This is the single entry point for every channel. It parses and runs the
/// command for the thread's owner and pipe, persists both the command and the
/// reply as ordinary messages so conversation history stays complete, and
/// returns the reply for the channel to deliver. `None` means the message is
/// not a command and should go to the agent engine.
pub async fn try_handle_message(
    state: &AppState,
    user_id: &str,
    thread_id: &str,
    text: &str,
    client_message_id: Option<&str>,
) -> Option<CommandReply> {
    let cmd = parse_command(text)?;

    let (pipe_id, session_id) = match sqlx::query_as::<_, (String, String)>(
        "SELECT pipe_id, session_id FROM threads WHERE id = ? AND user_id = ?",
    )
    .bind(thread_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            warn!(
                thread_id,
                "Slash command for unknown thread; passing to agent"
            );
            return None;
        }
        Err(e) => {
            error!(thread_id, "Could not load thread for slash command: {e}");
            return None;
        }
    };

    let language = crate::i18n::user_language(&state.db, user_id).await;
    let result = dispatch(
        cmd,
        CommandContext {
            state,
            user_id,
            pipe_id: &pipe_id,
            language: &language,
        },
    )
    .await;

    let user_message_id = client_message_id
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let reply_message_id = Uuid::new_v4().to_string();
    let command_text = text.trim().to_string();
    for (id, role, content) in [
        (&user_message_id, "user", &command_text),
        (&reply_message_id, "assistant", &result.text),
    ] {
        let created_at = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3f")
            .to_string();
        if let Err(e) = sqlx::query(
            "INSERT OR IGNORE INTO messages (id, pipe_id, session_id, thread_id, role, content, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(&pipe_id)
        .bind(&session_id)
        .bind(thread_id)
        .bind(role)
        .bind(content)
        .bind(&created_at)
        .execute(&state.db)
        .await
        {
            error!(thread_id, "Failed to persist slash command message: {e}");
        }
    }

    Some(CommandReply {
        reply_message_id,
        text: result.text,
        created_at: chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3f")
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_commands_only_with_a_letter_after_the_slash() {
        assert!(is_command("/help"));
        assert!(is_command("  /Schedule list"));
        assert!(!is_command("1/2 cup"));
        assert!(!is_command("/ what"));
        assert!(!is_command("//comment"));
        assert!(!is_command("hello /remind"));
    }

    #[test]
    fn parses_every_catalog_command() {
        assert!(matches!(parse_command("/help"), Some(Command::Help)));
        assert!(matches!(parse_command("/start"), Some(Command::Help)));
        assert!(matches!(parse_command("/schedule"), Some(Command::Help)));
        assert!(matches!(
            parse_command("/schedule list"),
            Some(Command::ScheduleList)
        ));
        assert!(matches!(
            parse_command("/schedules"),
            Some(Command::ScheduleList)
        ));
        assert!(matches!(
            parse_command("/schedule add Coffee every day at 8"),
            Some(Command::ScheduleAdd(arg)) if arg == "Coffee every day at 8"
        ));
        assert!(matches!(
            parse_command("/schedule remove Morning news"),
            Some(Command::ScheduleDelete(arg)) if arg == "Morning news"
        ));
        assert!(matches!(
            parse_command("/remind Call mom tomorrow at 9"),
            Some(Command::Remind(arg)) if arg == "Call mom tomorrow at 9"
        ));
        assert!(matches!(
            parse_command("/remind"),
            Some(Command::Unknown(_))
        ));
        assert!(matches!(
            parse_command("/schedule add"),
            Some(Command::Unknown(_))
        ));
        assert!(matches!(parse_command("/dance"), Some(Command::Unknown(_))));
        assert!(parse_command("plain text").is_none());
    }

    #[test]
    fn ignores_telegram_bot_suffix_and_case() {
        assert!(matches!(
            parse_command("/Schedule@SeluBot LIST"),
            Some(Command::ScheduleList)
        ));
        assert!(matches!(parse_command("/HELP@bot"), Some(Command::Help)));
    }

    #[test]
    fn help_text_lists_every_catalog_entry_in_both_languages() {
        for lang in ["en", "de"] {
            let help = help_text(lang);
            for info in catalog(lang) {
                assert!(help.contains(&info.label), "{lang}: missing {}", info.label);
                assert!(
                    !info.description.starts_with("cmd."),
                    "{lang}: untranslated {}",
                    info.label
                );
            }
        }
        assert!(catalog("de")[0].argument_hint.as_deref() == Some("was + wann"));
    }
}
