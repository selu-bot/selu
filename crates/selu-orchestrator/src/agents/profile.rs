/// User profile: persistent facts about the user (name, location, preferences,
/// hobbies, family, etc.) that are **always** injected into agent context.
///
/// Unlike agent memory (BM25/search-gated), profile facts are unconditionally
/// available to every agent on every turn — the agent always knows who the user is.
use anyhow::{Context, Result};
use sqlx::SqlitePool;
use tracing::debug;
use uuid::Uuid;

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
    let rows = sqlx::query_as::<_, (String, String, String, String, String, String, String, String)>(
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
        .map(|(id, user_id, fact, category, source, agent_id, created_at, updated_at)| {
            ProfileFact {
                id,
                user_id,
                fact,
                category,
                source,
                agent_id,
                created_at,
                updated_at,
            }
        })
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

/// Build the "## About this user" context block from ALL profile facts.
///
/// Returns `None` if there are no facts.  Capped at 50 to keep context
/// budget reasonable.
pub async fn build_context_block(db: &SqlitePool, user_id: &str) -> Result<Option<String>> {
    let facts = list_facts(db, user_id, 50).await?;
    if facts.is_empty() {
        return Ok(None);
    }

    let mut ctx = String::from("## About this user\n\n");
    ctx.push_str(
        "These are known facts about this user. Use them naturally when relevant, but do not repeat them back to the user.\n\n",
    );

    for fact in &facts {
        let label = category_label(&fact.category);
        ctx.push_str(&format!("- [{}] {}\n", label, fact.fact));
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
