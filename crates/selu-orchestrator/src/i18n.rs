/// Minimal server-side i18n for non-web channels (iMessage, webhooks).
///
/// The web UI has its own client-side i18n in layout.html. This module
/// provides translations for messages sent through channels where
/// JavaScript is not available (e.g. tool approval prompts in iMessage).
///
/// Usage:
///   use crate::i18n::t;
///   let msg = t("de", "approval.reply_to_approve");
use std::collections::HashMap;
use std::sync::LazyLock;

/// All server-side translations. Add new keys here.
///
/// Convention: same dot-separated keys as the web UI where possible,
/// prefixed with the section (e.g. `approval.`, `chat.`).
static TRANSLATIONS: LazyLock<HashMap<&'static str, HashMap<&'static str, &'static str>>> =
    LazyLock::new(|| {
        let mut m: HashMap<&str, HashMap<&str, &str>> = HashMap::new();

        // ── English ──────────────────────────────────────────────────────
        let mut en = HashMap::new();
        en.insert(
            "approval.would_like_to_use",
            "I'd like to use the tool \"{tool}\"",
        );
        en.insert("approval.with_args", " with:");
        en.insert(
            "approval.reply_to_approve",
            "Reply to this message to approve.",
        );
        en.insert("approval.approved_processing", "Approved. Processing...");
        en.insert(
            "error.agent_turn_failed",
            "Something went wrong — please try again.",
        );

        // ── Schedule commands ────────────────────────────────────────
        en.insert(
            "cmd.unknown",
            "Unknown command: `{cmd}`\n\nAvailable commands:\n- `/schedule add <what to do + when>` — Create a recurring schedule\n- `/schedule list` — List your schedules and reminders\n- `/schedule delete <name>` — Delete a schedule or reminder\n- `/remind <what to do + when>` — Set a one-time reminder",
        );
        en.insert(
            "cmd.schedule.created",
            "Schedule created!\n\n**{name}** — {timing}\n> {prompt}\n\nTimezone: {timezone}",
        );
        en.insert(
            "cmd.schedule.create_error",
            "Something went wrong while creating the schedule. Please try again.",
        );
        en.insert(
            "cmd.schedule.parse_error",
            "Couldn't understand that schedule. Please try something like:\n`/schedule add Check my emails every day at 8am`\n`/schedule add Give me a weather report at 7:00 on weekdays`",
        );
        en.insert(
            "cmd.schedule.no_provider",
            "No LLM provider configured. Please set up a default model in Providers first.",
        );
        en.insert(
            "cmd.schedule.provider_error",
            "Couldn't load the LLM provider. Is the API key configured?",
        );
        en.insert("cmd.schedule.list_title", "Your Schedules:");
        en.insert(
            "cmd.schedule.list_empty",
            "You don't have any schedules yet.\n\nCreate one with:\n`/schedule add Give me a morning summary at 8am on weekdays`",
        );
        en.insert(
            "cmd.schedule.list_error",
            "Something went wrong while listing schedules.",
        );
        en.insert("cmd.schedule.status_on", "On");
        en.insert("cmd.schedule.status_off", "Off");
        en.insert("cmd.schedule.no_pipes", "no pipes");
        en.insert("cmd.schedule.next_run", "Next");
        en.insert("cmd.schedule.deleted", "Schedule **{name}** deleted.");
        en.insert(
            "cmd.schedule.not_found",
            "No schedule found matching \"**{name}**\". Use `/schedule list` to see your schedules.",
        );
        en.insert(
            "cmd.schedule.not_found_hint",
            "Couldn't find that schedule. Use `/schedule list` to see your schedules.",
        );
        en.insert(
            "cmd.schedule.delete_error",
            "Something went wrong while deleting the schedule.",
        );
        en.insert("schedule.run_label", "Scheduled run");
        en.insert("schedule.reminder_label", "Reminder");

        // ── Remind commands ──────────────────────────────────────
        en.insert(
            "cmd.remind.created",
            "Reminder set!\n\n**{name}** — {timing}\n> {prompt}",
        );
        en.insert(
            "cmd.remind.create_error",
            "Something went wrong while creating the reminder. Please try again.",
        );
        en.insert(
            "cmd.remind.parse_error",
            "Couldn't understand that reminder. Please try something like:\n`/remind Check the weather next Sunday at 10am`\n`/remind Call the dentist tomorrow at 3pm`",
        );
        en.insert(
            "cmd.remind.use_schedule",
            "That sounds like a recurring task. Try `/schedule add {input}` instead.",
        );

        // ── Schedule list extras ─────────────────────────────────
        en.insert("cmd.schedule.status_completed", "Completed");
        en.insert("cmd.schedule.kind_reminder", "Reminder");
        en.insert("cmd.schedule.kind_schedule", "Schedule");

        m.insert("en", en);

        // ── German ───────────────────────────────────────────────────────
        let mut de = HashMap::new();
        de.insert(
            "approval.would_like_to_use",
            "Ich moechte das Tool \"{tool}\" verwenden",
        );
        de.insert("approval.with_args", " mit:");
        de.insert(
            "approval.reply_to_approve",
            "Antworte auf diese Nachricht, um zu bestaetigen.",
        );
        de.insert(
            "approval.approved_processing",
            "Bestaetigt. Wird verarbeitet...",
        );
        de.insert(
            "error.agent_turn_failed",
            "Da ist leider etwas schiefgelaufen — versuch es bitte nochmal.",
        );

        // ── Schedule commands ────────────────────────────────────────
        de.insert(
            "cmd.unknown",
            "Unbekannter Befehl: `{cmd}`\n\nVerf\u{00fc}gbare Befehle:\n- `/schedule add <was + wann>` — Wiederkehrenden Zeitplan erstellen\n- `/schedule list` — Zeitpl\u{00e4}ne und Erinnerungen anzeigen\n- `/schedule delete <Name>` — Zeitplan oder Erinnerung l\u{00f6}schen\n- `/remind <was + wann>` — Einmalige Erinnerung setzen",
        );
        de.insert(
            "cmd.schedule.created",
            "Zeitplan erstellt!\n\n**{name}** — {timing}\n> {prompt}\n\nZeitzone: {timezone}",
        );
        de.insert(
            "cmd.schedule.create_error",
            "Beim Erstellen des Zeitplans ist etwas schiefgelaufen. Bitte versuche es erneut.",
        );
        de.insert(
            "cmd.schedule.parse_error",
            "Konnte den Zeitplan nicht verstehen. Versuch es zum Beispiel so:\n`/schedule add Pr\u{00fc}fe meine E-Mails jeden Tag um 8 Uhr`\n`/schedule add Gib mir einen Wetterbericht um 7:00 an Werktagen`",
        );
        de.insert(
            "cmd.schedule.no_provider",
            "Kein LLM-Anbieter konfiguriert. Bitte richte zuerst ein Standardmodell unter Anbieter ein.",
        );
        de.insert(
            "cmd.schedule.provider_error",
            "LLM-Anbieter konnte nicht geladen werden. Ist der API-Schl\u{00fc}ssel eingerichtet?",
        );
        de.insert("cmd.schedule.list_title", "Deine Zeitpl\u{00e4}ne:");
        de.insert(
            "cmd.schedule.list_empty",
            "Du hast noch keine Zeitpl\u{00e4}ne.\n\nErstelle einen mit:\n`/schedule add Gib mir eine Morgenzusammenfassung um 8 Uhr an Werktagen`",
        );
        de.insert(
            "cmd.schedule.list_error",
            "Beim Laden der Zeitpl\u{00e4}ne ist etwas schiefgelaufen.",
        );
        de.insert("cmd.schedule.status_on", "An");
        de.insert("cmd.schedule.status_off", "Aus");
        de.insert("cmd.schedule.no_pipes", "keine Pipes");
        de.insert("cmd.schedule.next_run", "N\u{00e4}chster Lauf");
        de.insert(
            "cmd.schedule.deleted",
            "Zeitplan **{name}** gel\u{00f6}scht.",
        );
        de.insert(
            "cmd.schedule.not_found",
            "Kein Zeitplan gefunden f\u{00fc}r \"**{name}**\". Nutze `/schedule list` um deine Zeitpl\u{00e4}ne zu sehen.",
        );
        de.insert(
            "cmd.schedule.not_found_hint",
            "Zeitplan nicht gefunden. Nutze `/schedule list` um deine Zeitpl\u{00e4}ne zu sehen.",
        );
        de.insert(
            "cmd.schedule.delete_error",
            "Beim L\u{00f6}schen des Zeitplans ist etwas schiefgelaufen.",
        );
        de.insert("schedule.run_label", "Geplanter Lauf");
        de.insert("schedule.reminder_label", "Erinnerung");

        // ── Remind commands ──────────────────────────────────────
        de.insert(
            "cmd.remind.created",
            "Erinnerung gesetzt!\n\n**{name}** — {timing}\n> {prompt}",
        );
        de.insert(
            "cmd.remind.create_error",
            "Beim Erstellen der Erinnerung ist etwas schiefgelaufen. Bitte versuche es erneut.",
        );
        de.insert(
            "cmd.remind.parse_error",
            "Konnte die Erinnerung nicht verstehen. Versuch es zum Beispiel so:\n`/remind Wetter pr\u{00fc}fen n\u{00e4}chsten Sonntag um 10 Uhr`\n`/remind Zahnarzt anrufen morgen um 15 Uhr`",
        );
        de.insert(
            "cmd.remind.use_schedule",
            "Das klingt nach einer wiederkehrenden Aufgabe. Versuch `/schedule add {input}` stattdessen.",
        );

        // ── Schedule list extras ─────────────────────────────────
        de.insert("cmd.schedule.status_completed", "Abgeschlossen");
        de.insert("cmd.schedule.kind_reminder", "Erinnerung");
        de.insert("cmd.schedule.kind_schedule", "Zeitplan");

        m.insert("de", de);

        m
    });

/// Look up a translation key for the given language.
/// Falls back to English if the key is missing in the requested language.
/// Returns the key itself if not found in any language.
pub fn t<'a>(lang: &str, key: &'a str) -> &'a str {
    // Try the requested language first, then English
    if let Some(val) = TRANSLATIONS.get(lang).and_then(|m| m.get(key).copied()) {
        return val;
    }
    if let Some(val) = TRANSLATIONS.get("en").and_then(|m| m.get(key).copied()) {
        return val;
    }
    key
}

/// Look up a translation and replace `{tool}` with the provided tool name.
/// For templates that need simple variable substitution.
pub fn t_with_tool(lang: &str, key: &str, tool_name: &str) -> String {
    let template = t(lang, key);
    template.replace("{tool}", tool_name)
}

/// Look up a user's language preference from the database.
/// Falls back to "de" if not found.
pub async fn user_language(db: &sqlx::SqlitePool, user_id: &str) -> String {
    let result: Option<String> = sqlx::query_scalar("SELECT language FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();

    result.unwrap_or_else(|| "de".to_string())
}
