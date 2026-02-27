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
static TRANSLATIONS: LazyLock<HashMap<&'static str, HashMap<&'static str, &'static str>>> = LazyLock::new(|| {
    let mut m: HashMap<&str, HashMap<&str, &str>> = HashMap::new();

    // ── English ──────────────────────────────────────────────────────
    let mut en = HashMap::new();
    en.insert("approval.would_like_to_use", "I'd like to use the tool \"{tool}\"");
    en.insert("approval.with_args", " with:");
    en.insert("approval.reply_to_approve", "Reply to this message to approve.");
    en.insert("approval.approved_processing", "Approved. Processing...");
    m.insert("en", en);

    // ── German ───────────────────────────────────────────────────────
    let mut de = HashMap::new();
    de.insert("approval.would_like_to_use", "Ich moechte das Tool \"{tool}\" verwenden");
    de.insert("approval.with_args", " mit:");
    de.insert("approval.reply_to_approve", "Antworte auf diese Nachricht, um zu bestaetigen.");
    de.insert("approval.approved_processing", "Bestaetigt. Wird verarbeitet...");
    m.insert("de", de);

    m
});

/// Look up a translation key for the given language.
/// Falls back to English if the key is missing in the requested language.
/// Returns the key itself if not found in any language.
pub fn t<'a>(lang: &str, key: &'a str) -> &'a str {
    // Try the requested language first, then English
    if let Some(val) = TRANSLATIONS
        .get(lang)
        .and_then(|m| m.get(key).copied())
    {
        return val;
    }
    if let Some(val) = TRANSLATIONS
        .get("en")
        .and_then(|m| m.get(key).copied())
    {
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
    let result: Option<String> = sqlx::query_scalar(
        "SELECT language FROM users WHERE id = ?"
    )
    .bind(user_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();

    result.unwrap_or_else(|| "de".to_string())
}
