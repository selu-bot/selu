/// User profile: persistent facts about the user (name, location, preferences,
/// hobbies, family, etc.) that are **always** injected into agent context.
///
/// Unlike agent memory (BM25/search-gated), profile facts are unconditionally
/// available to every agent on every turn — the agent always knows who the user is.
use anyhow::{Context, Result};
use serde::Deserialize;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::llm::provider::{ChatMessage, LlmResponse};
use crate::llm::registry::load_provider;
use crate::permissions::store::CredentialStore;

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ProfileFact {
    pub id: String,
    pub user_id: String,
    pub fact: String,
    pub category: String,
    pub source: String,
    pub agent_id: String,
    pub created_at: String,
    pub updated_at: String,
}

// ── CRUD ─────────────────────────────────────────────────────────────────────

pub async fn list_facts(db: &SqlitePool, user_id: &str, limit: i64) -> Result<Vec<ProfileFact>> {
    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
        ),
    >(
        r#"SELECT id, user_id, fact_text, category, source, agent_id, created_at, updated_at
           FROM user_profile
           WHERE user_id = ?
           ORDER BY updated_at DESC, created_at DESC
           LIMIT ?"#,
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(db)
    .await
    .context("Failed to list profile facts")?;

    Ok(rows
        .into_iter()
        .map(
            |(id, user_id, fact, category, source, agent_id, created_at, updated_at)| ProfileFact {
                id,
                user_id,
                fact,
                category,
                source,
                agent_id,
                created_at,
                updated_at,
            },
        )
        .collect())
}

pub async fn add_fact(
    db: &SqlitePool,
    user_id: &str,
    fact: &str,
    category: &str,
    source: &str,
    agent_id: &str,
) -> Result<String> {
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO user_profile (id, user_id, fact_text, category, source, agent_id) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(user_id)
    .bind(fact)
    .bind(category)
    .bind(source)
    .bind(agent_id)
    .execute(db)
    .await
    .context("Failed to add profile fact")?;

    debug!(user_id = %user_id, category = %category, "Added profile fact");
    Ok(id)
}

pub async fn update_fact(db: &SqlitePool, fact_id: &str, fact: &str, category: &str) -> Result<()> {
    sqlx::query(
        "UPDATE user_profile SET fact_text = ?, category = ?, updated_at = datetime('now') WHERE id = ?",
    )
    .bind(fact)
    .bind(category)
    .bind(fact_id)
    .execute(db)
    .await
    .context("Failed to update profile fact")?;
    Ok(())
}

pub async fn delete_fact(db: &SqlitePool, user_id: &str, fact_id: &str) -> Result<bool> {
    let result = sqlx::query("DELETE FROM user_profile WHERE id = ? AND user_id = ?")
        .bind(fact_id)
        .bind(user_id)
        .execute(db)
        .await
        .context("Failed to delete profile fact")?;

    Ok(result.rows_affected() > 0)
}

// ── Context injection ────────────────────────────────────────────────────────

/// Build the "## About this user" context block from profile facts.
///
/// Returns `None` if there are no facts.  Loads up to [`MAX_PER_CATEGORY`]
/// facts per category so that no single category can crowd out others.
const CATEGORIES: &[&str] = &["personal", "preferences", "location", "work", "other"];
const MAX_PER_CATEGORY: usize = 10;

pub async fn build_context_block(db: &SqlitePool, user_id: &str) -> Result<Option<String>> {
    // Load generously, then balance across categories in Rust.
    let all_facts = list_facts(db, user_id, 200).await?;
    if all_facts.is_empty() {
        return Ok(None);
    }

    let mut ctx = String::from("## About this user\n\n");
    ctx.push_str(
        "These are known facts about this user. Use them naturally when relevant, but do not repeat them back to the user.\n\n",
    );

    for &cat in CATEGORIES {
        let mut count = 0usize;
        for fact in &all_facts {
            let effective_cat = if fact.category.is_empty() {
                "other"
            } else {
                &fact.category
            };
            if effective_cat != cat {
                continue;
            }
            let label = category_label(cat);
            ctx.push_str(&format!("- [{}] {}\n", label, fact.fact));
            count += 1;
            if count >= MAX_PER_CATEGORY {
                break;
            }
        }
    }

    Ok(Some(ctx))
}

fn category_label(cat: &str) -> &'static str {
    match cat {
        "personal" => "Personal",
        "preferences" => "Preferences",
        "location" => "Location",
        "work" => "Work & Skills",
        _ => "Other",
    }
}

// ── Periodic optimization ────────────────────────────────────────────────────

/// Minimum number of facts before optimization kicks in.  Below this there is
/// nothing meaningful to consolidate.
const OPTIMIZE_MIN_FACTS: usize = 5;

/// Run optimization for every user that has profile facts.
pub async fn run_optimization(db: &SqlitePool, creds: &CredentialStore) -> Result<()> {
    let user_ids: Vec<String> = sqlx::query_scalar("SELECT DISTINCT user_id FROM user_profile")
        .fetch_all(db)
        .await
        .context("Failed to list users with profile facts")?;

    for user_id in &user_ids {
        if let Err(e) = optimize_facts(db, creds, user_id).await {
            warn!(user_id = %user_id, "Profile optimization failed (non-fatal): {e}");
        }
    }

    Ok(())
}

/// Use an LLM to deduplicate, recategorize and consolidate profile facts for
/// a single user.  Replaces existing facts with the optimized set in a single
/// transaction.
async fn optimize_facts(db: &SqlitePool, creds: &CredentialStore, user_id: &str) -> Result<()> {
    let facts = list_facts(db, user_id, 500).await?;
    if facts.len() < OPTIMIZE_MIN_FACTS {
        return Ok(());
    }

    // Build a numbered list so the LLM sees everything.
    let facts_text: String = facts
        .iter()
        .enumerate()
        .map(|(i, f)| format!("{}. [{}] {}", i + 1, f.category, f.fact))
        .collect::<Vec<_>>()
        .join("\n");

    let user_lang = crate::i18n::user_language(db, user_id).await;
    let lang_name = match user_lang.as_str() {
        "de" => "German",
        "en" => "English",
        _ => "German",
    };

    let system_prompt = format!(
        r#"You are a profile-fact optimizer. You will receive a numbered list of facts about a user.

Your job:
1. **Deduplicate**: merge facts that say the same thing.
2. **Correct categories**: assign each fact to exactly one of: personal, preferences, location, work, other.
3. **Consolidate**: if multiple facts can be combined into one clearer statement, do so.  Keep atomic facts atomic — only merge when they genuinely overlap.
4. **Preserve meaning**: never invent information. If two facts conflict, keep the one that sounds more recent or specific.
5. **Language**: write ALL facts in {lang}. This is the user's preferred language. Translate any facts that are in a different language.

Current facts:
{facts}

Respond with a JSON array. Each element must have:
- "category": one of "personal", "preferences", "location", "work", "other"
- "fact": a short, clear statement in {lang}

Respond ONLY with the JSON array, nothing else."#,
        facts = facts_text,
        lang = lang_name
    );

    // Use the default agent's model for background tasks.
    let resolved = crate::agents::model::resolve_model(db, "default").await?;
    let provider = load_provider(db, &resolved.provider_id, &resolved.model_id, creds).await?;

    let messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user("Optimize the facts above.".to_string()),
    ];

    let response = provider.chat(&messages, &[], 0.1).await?;

    let text = match response {
        LlmResponse::Text(t) => t,
        _ => return Ok(()),
    };

    // Parse response (handle optional code fences).
    let text = text.trim();
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
    struct OptimizedFact {
        category: String,
        fact: String,
    }

    let optimized: Vec<OptimizedFact> = match serde_json::from_str(&json_str) {
        Ok(f) => f,
        Err(e) => {
            warn!("Failed to parse profile optimization response: {e}");
            return Ok(());
        }
    };

    if optimized.is_empty() {
        return Ok(());
    }

    let valid_categories = ["personal", "preferences", "location", "work", "other"];

    // Only replace if the optimized set is meaningfully smaller or the same
    // size — never let the LLM inflate the fact count.
    if optimized.len() > facts.len() {
        warn!(
            user_id = %user_id,
            before = facts.len(),
            after = optimized.len(),
            "LLM returned more facts than input — skipping optimization"
        );
        return Ok(());
    }

    // Replace: delete old facts and insert optimized ones.
    let mut tx = db.begin().await.context("Failed to begin transaction")?;

    sqlx::query("DELETE FROM user_profile WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .context("Failed to clear old profile facts")?;

    for fact in &optimized {
        let cat = if valid_categories.contains(&fact.category.as_str()) {
            &fact.category
        } else {
            "other"
        };

        if fact.fact.trim().is_empty() {
            continue;
        }

        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO user_profile (id, user_id, fact_text, category, source, agent_id) VALUES (?, ?, ?, ?, 'auto', 'system')",
        )
        .bind(&id)
        .bind(user_id)
        .bind(fact.fact.trim())
        .bind(cat)
        .execute(&mut *tx)
        .await
        .context("Failed to insert optimized fact")?;
    }

    tx.commit()
        .await
        .context("Failed to commit optimized facts")?;

    info!(
        user_id = %user_id,
        before = facts.len(),
        after = optimized.len(),
        "Profile facts optimized"
    );

    Ok(())
}
