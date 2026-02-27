/// User personality: persistent per-user facts extracted from conversations
/// or added manually. Global across all agents.
///
/// Facts are injected into the LLM context window so every agent knows about
/// the user from the first message.
use anyhow::{Context, Result};
use serde::Deserialize;
use sqlx::SqlitePool;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::llm::provider::{ChatMessage, LlmResponse};
use crate::llm::registry::load_provider;
use crate::permissions::store::CredentialStore;

// ── Types ─────────────────────────────────────────────────────────────────────

/// A single personality fact about a user.
#[derive(Debug, Clone)]
pub struct PersonalityFact {
    pub id: String,
    pub user_id: String,
    pub category: String,
    pub fact: String,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

// ── CRUD ──────────────────────────────────────────────────────────────────────

/// Load all personality facts for a user, ordered by category then creation time.
pub async fn get_facts(db: &SqlitePool, user_id: &str) -> Result<Vec<PersonalityFact>> {
    let rows = sqlx::query_as::<_, (String, String, String, String, String, String, String)>(
        "SELECT id, user_id, category, fact, source, created_at, updated_at \
         FROM user_personality \
         WHERE user_id = ? \
         ORDER BY category, created_at",
    )
    .bind(user_id)
    .fetch_all(db)
    .await
    .context("Failed to load personality facts")?;

    Ok(rows
        .into_iter()
        .map(|(id, user_id, category, fact, source, created_at, updated_at)| PersonalityFact {
            id,
            user_id,
            category,
            fact,
            source,
            created_at,
            updated_at,
        })
        .collect())
}

/// Add a single personality fact.
pub async fn add_fact(
    db: &SqlitePool,
    user_id: &str,
    category: &str,
    fact: &str,
    source: &str,
) -> Result<String> {
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO user_personality (id, user_id, category, fact, source) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(user_id)
    .bind(category)
    .bind(fact)
    .bind(source)
    .execute(db)
    .await
    .context("Failed to add personality fact")?;

    debug!(user_id = %user_id, category = %category, "Added personality fact");
    Ok(id)
}

/// Update an existing personality fact.
pub async fn update_fact(
    db: &SqlitePool,
    fact_id: &str,
    category: &str,
    fact: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE user_personality SET category = ?, fact = ?, updated_at = datetime('now') WHERE id = ?",
    )
    .bind(category)
    .bind(fact)
    .bind(fact_id)
    .execute(db)
    .await
    .context("Failed to update personality fact")?;

    Ok(())
}

/// Delete a personality fact.
pub async fn delete_fact(db: &SqlitePool, fact_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM user_personality WHERE id = ?")
        .bind(fact_id)
        .execute(db)
        .await
        .context("Failed to delete personality fact")?;

    Ok(())
}

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
) -> Result<()> {
    // Skip very short exchanges (unlikely to contain personality info)
    if user_message.len() < 10 {
        return Ok(());
    }

    // Load existing facts to avoid duplicates
    let existing = get_facts(db, user_id).await.unwrap_or_default();
    let existing_summary: String = existing
        .iter()
        .map(|f| format!("- [{}] {}", f.category, f.fact))
        .collect::<Vec<_>>()
        .join("\n");

    // Resolve the model (same as the agent uses)
    let resolved = crate::agents::model::resolve_model(db, agent_id).await?;
    let provider = load_provider(db, &resolved.provider_id, &resolved.model_id, creds).await?;

    let system_prompt = format!(
        r#"You extract personality facts about a user from conversations. Personality facts are persistent things about the user such as their name, where they live, what they like, their job, hobbies, family, preferences, etc.

Analyse the following conversation and identify NEW facts about the user. Only include facts that are:
- Genuinely about the user as a person (not about the current task)
- Persistent (not temporary states like "I'm tired right now")
- Not already known (see existing facts below)

Existing facts:
{existing}

Respond with a JSON array. Each element must have:
- "category": one of "personal", "preferences", "location", "work", "other"
- "fact": a short, clear statement (e.g. "Name is Jan", "Lives in Berlin", "Prefers dark mode")

If there are NO new facts to extract, respond with an empty array: []

Respond ONLY with the JSON array, nothing else."#,
        existing = if existing_summary.is_empty() {
            "(none yet)".to_string()
        } else {
            existing_summary
        }
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

    // Validate categories and store
    let valid_categories = ["personal", "preferences", "location", "work", "other"];
    let mut stored = 0;
    for fact in facts {
        let cat = if valid_categories.contains(&fact.category.as_str()) {
            fact.category
        } else {
            "other".to_string()
        };

        if fact.fact.trim().is_empty() {
            continue;
        }

        if let Err(e) = add_fact(db, user_id, &cat, fact.fact.trim(), "auto").await {
            warn!("Failed to store extracted personality fact: {e}");
        } else {
            stored += 1;
        }
    }

    if stored > 0 {
        debug!(user_id = %user_id, count = stored, "Extracted personality facts from conversation");
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
        struct ExtractedFact {
            category: String,
            fact: String,
        }

        let facts: Vec<ExtractedFact> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].fact, "Name is Jan");
    }
}
