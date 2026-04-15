/// User fact extraction: auto-extracts facts about users from conversations
/// and stores them in the `user_profile` table.
///
/// Profile facts are always injected into agent context unconditionally,
/// ensuring the agent always knows who the user is.
use anyhow::Result;
use serde::Deserialize;
use sqlx::SqlitePool;
use tracing::{debug, warn};

use crate::agents::profile;
use crate::llm::provider::{ChatMessage, LlmResponse};
use crate::llm::registry::load_provider;
use crate::permissions::store::CredentialStore;

// ── Auto-extraction ───────────────────────────────────────────────────────────

/// Extract personality facts from a conversation exchange.
///
/// Called as a background task after each agent turn. Uses the same LLM
/// provider as the agent to analyse the conversation and extract new facts.
///
/// This is best-effort: failures are logged but never block the agent turn.
pub async fn extract_from_conversation(
    db: &SqlitePool,
    creds: &CredentialStore,
    user_id: &str,
    agent_id: &str,
    user_message: &str,
    assistant_reply: &str,
    user_language: &str,
) -> Result<()> {
    // Skip very short exchanges (unlikely to contain personality info)
    if user_message.len() < 10 {
        return Ok(());
    }

    // Load existing profile facts to avoid duplicates
    let existing = profile::list_facts(db, user_id, 100)
        .await
        .unwrap_or_default();
    let existing_summary: String = existing
        .iter()
        .map(|f| format!("- [{}] {}", f.category, f.fact))
        .collect::<Vec<_>>()
        .join("\n");

    // Resolve the model (same as the agent uses)
    let resolved = crate::agents::model::resolve_model(db, agent_id).await?;
    let provider = load_provider(db, &resolved.provider_id, &resolved.model_id, creds).await?;

    let lang_name = match user_language {
        "de" => "German",
        "en" => "English",
        _ => "German",
    };

    let system_prompt = format!(
        r#"You extract facts about a user from conversations. Facts are persistent things about the user such as their name, where they live, what they like, their job, hobbies, family, preferences, etc.

Analyse the following conversation and identify NEW facts about the user. Only include facts that are:
- Genuinely about the user as a person (not about the current task)
- Persistent (not temporary states like "I'm tired right now")
- Not already known (see existing facts below)

Existing facts:
{existing}

Respond with a JSON array. Each element must have:
- "category": one of "personal", "preferences", "location", "work", "other"
- "fact": a short, clear statement in {lang} (e.g. "Name is Jan", "Lives in Berlin", "Prefers dark mode")

IMPORTANT: Always write the facts in {lang}. This is the user's preferred language.

If there are NO new facts to extract, respond with an empty array: []

Respond ONLY with the JSON array, nothing else."#,
        existing = if existing_summary.is_empty() {
            "(none yet)".to_string()
        } else {
            existing_summary
        },
        lang = lang_name
    );

    let messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(format!(
            "User: {}\n\nAssistant: {}",
            user_message, assistant_reply
        )),
    ];

    let response = provider.chat(&messages, &[], 0.1).await?;

    let text = match response {
        LlmResponse::Text(t) => t,
        _ => return Ok(()), // Unexpected tool call response
    };

    // Parse the JSON response
    let text = text.trim();

    // Handle markdown code fences
    let json_str = if text.starts_with("```") {
        text.lines()
            .skip(1)
            .take_while(|l| !l.starts_with("```"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text.to_string()
    };

    #[derive(Deserialize)]
    struct ExtractedFact {
        category: String,
        fact: String,
    }

    let facts: Vec<ExtractedFact> = match serde_json::from_str(&json_str) {
        Ok(f) => f,
        Err(e) => {
            debug!("Failed to parse personality extraction response: {e}");
            return Ok(());
        }
    };

    if facts.is_empty() {
        return Ok(());
    }

    // Validate categories and store into unified memory.
    let valid_categories = ["personal", "preferences", "location", "work", "other"];
    let mut stored = 0;
    for fact in facts {
        let cat = if valid_categories.contains(&fact.category.as_str()) {
            &fact.category
        } else {
            "other"
        };

        if fact.fact.trim().is_empty() {
            continue;
        }

        if let Err(e) =
            profile::add_fact(db, user_id, fact.fact.trim(), cat, "auto", agent_id).await
        {
            warn!("Failed to store extracted fact: {e}");
        } else {
            stored += 1;
        }
    }

    if stored > 0 {
        debug!(user_id = %user_id, count = stored, "Extracted user facts from conversation");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_extraction_response() {
        let json = r#"[{"category": "personal", "fact": "Name is Jan"}, {"category": "location", "fact": "Lives in Berlin"}]"#;

        #[derive(Deserialize)]
        struct ExtractedFact {
            category: String,
            fact: String,
        }

        let facts: Vec<ExtractedFact> = serde_json::from_str(json).unwrap();
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].category, "personal");
        assert_eq!(facts[0].fact, "Name is Jan");
        assert_eq!(facts[1].category, "location");
        assert_eq!(facts[1].fact, "Lives in Berlin");
    }

    #[test]
    fn test_parse_empty_response() {
        let json = "[]";

        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct ExtractedFact {
            category: String,
            fact: String,
        }

        let facts: Vec<ExtractedFact> = serde_json::from_str(json).unwrap();
        assert!(facts.is_empty());
    }

    #[test]
    fn test_parse_code_fenced_response() {
        let text = "```json\n[{\"category\": \"personal\", \"fact\": \"Name is Jan\"}]\n```";

        let json_str: String = text
            .lines()
            .skip(1)
            .take_while(|l| !l.starts_with("```"))
            .collect::<Vec<_>>()
            .join("\n");

        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct ExtractedFact {
            category: String,
            fact: String,
        }

        let facts: Vec<ExtractedFact> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].fact, "Name is Jan");
    }
}
