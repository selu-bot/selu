/// Per-agent behavioral lessons learned from interaction patterns.
///
/// Selu records compact quality signals, reflects on each batch of five turns,
/// and stores actionable lessons for the specific agent and user. Confident,
/// repeatedly-supported lessons become active immediately; uncertain lessons
/// remain suggestions the user can review. This stays separate from the shared
/// user profile and searchable notes because it changes one agent's behavior.
use anyhow::{Context, Result};
use serde::Deserialize;
use sqlx::SqlitePool;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::permissions::store::CredentialStore;

const REFLECTION_INTERVAL: i64 = 5;
const AUTO_ACTIVATE_CONFIDENCE: f64 = 0.85;
const MIN_AUTO_ACTIVATE_SIGNALS: i64 = 2;
const MAX_ACTIVE_INSIGHTS: i64 = 10;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TurnSignal {
    pub id: String,
    pub agent_id: String,
    pub user_id: String,
    pub thread_id: Option<String>,
    pub turn_index: i64,
    pub user_rating: Option<i64>,
    pub tool_calls_count: i64,
    pub tool_failures_count: i64,
    pub tool_loop_iterations: i64,
    pub user_message_preview: Option<String>,
    pub agent_tools_used: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct TurnSignalData {
    pub agent_id: String,
    pub user_id: String,
    pub thread_id: Option<String>,
    pub user_message_preview: Option<String>,
    pub tool_calls_count: i64,
    pub tool_failures_count: i64,
    pub tool_loop_iterations: i64,
    pub agent_tools_used: Vec<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Insight {
    pub id: String,
    pub agent_id: String,
    pub user_id: String,
    pub lesson_text: String,
    pub insight_type: String,
    pub status: String,
    pub confidence: f64,
    pub supporting_signals: i64,
    pub created_at: String,
    pub activated_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ExtractedInsight {
    pub lesson: String,
    pub evidence_summary: String,
    #[serde(rename = "type")]
    pub insight_type: String,
    pub confidence: f64,
    #[serde(default = "default_supporting_signals")]
    pub supporting_signals: i64,
}

fn default_supporting_signals() -> i64 {
    1
}

pub async fn record_turn_signal(db: &SqlitePool, data: &TurnSignalData) -> Result<String> {
    let id = Uuid::new_v4().to_string();
    let turn_index: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM turn_signals WHERE agent_id = ? AND user_id = ?")
            .bind(&data.agent_id)
            .bind(&data.user_id)
            .fetch_one(db)
            .await
            .unwrap_or(0);
    let tools_json = serde_json::to_string(&data.agent_tools_used).unwrap_or_default();

    sqlx::query(
        "INSERT INTO turn_signals \
         (id, agent_id, user_id, thread_id, turn_index, \
          tool_calls_count, tool_failures_count, tool_loop_iterations, \
          user_message_preview, agent_tools_used) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&data.agent_id)
    .bind(&data.user_id)
    .bind(&data.thread_id)
    .bind(turn_index)
    .bind(data.tool_calls_count)
    .bind(data.tool_failures_count)
    .bind(data.tool_loop_iterations)
    .bind(&data.user_message_preview)
    .bind(&tools_json)
    .execute(db)
    .await
    .context("Failed to record turn signal")?;

    debug!(
        agent_id = %data.agent_id,
        user_id = %data.user_id,
        turn_index,
        "Recorded turn signal"
    );
    Ok(id)
}

/// Update the user rating on the most recent signal in a conversation.
pub async fn rate_turn(
    db: &SqlitePool,
    user_id: &str,
    thread_id: &str,
    rating: i64,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE turn_signals SET user_rating = ? \
         WHERE id = (
             SELECT id FROM turn_signals \
             WHERE user_id = ? AND thread_id = ? \
             ORDER BY created_at DESC LIMIT 1
         )",
    )
    .bind(rating.clamp(-1, 1))
    .bind(user_id)
    .bind(thread_id)
    .execute(db)
    .await
    .context("Failed to rate turn")?;

    Ok(result.rows_affected() > 0)
}

pub async fn latest_turn_rating(
    db: &SqlitePool,
    user_id: &str,
    thread_id: &str,
) -> Result<Option<i64>> {
    let rating: Option<Option<i64>> = sqlx::query_scalar(
        "SELECT user_rating FROM turn_signals \
         WHERE user_id = ? AND thread_id = ? \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(user_id)
    .bind(thread_id)
    .fetch_optional(db)
    .await
    .context("Failed to load latest turn rating")?;

    Ok(rating.flatten())
}

pub async fn count_signals(db: &SqlitePool, agent_id: &str, user_id: &str) -> Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM turn_signals WHERE agent_id = ? AND user_id = ?")
        .bind(agent_id)
        .bind(user_id)
        .fetch_one(db)
        .await
        .context("Failed to count turn signals")
}

pub async fn recent_signals(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
    limit: i64,
) -> Result<Vec<TurnSignal>> {
    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            Option<String>,
            i64,
            Option<i64>,
            i64,
            i64,
            i64,
            Option<String>,
            Option<String>,
            String,
        ),
    >(
        "SELECT id, agent_id, user_id, thread_id, turn_index, \
                user_rating, tool_calls_count, tool_failures_count, tool_loop_iterations, \
                user_message_preview, agent_tools_used, created_at \
         FROM turn_signals \
         WHERE agent_id = ? AND user_id = ? \
         ORDER BY created_at DESC \
         LIMIT ?",
    )
    .bind(agent_id)
    .bind(user_id)
    .bind(limit)
    .fetch_all(db)
    .await
    .context("Failed to fetch recent turn signals")?;

    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                agent_id,
                user_id,
                thread_id,
                turn_index,
                user_rating,
                tool_calls_count,
                tool_failures_count,
                tool_loop_iterations,
                user_message_preview,
                agent_tools_used,
                created_at,
            )| TurnSignal {
                id,
                agent_id,
                user_id,
                thread_id,
                turn_index,
                user_rating,
                tool_calls_count,
                tool_failures_count,
                tool_loop_iterations,
                user_message_preview,
                agent_tools_used,
                created_at,
            },
        )
        .collect())
}

fn normalized_insight_type(insight_type: &str) -> &str {
    match insight_type {
        "tool_usage" | "communication_style" | "workflow_pattern" | "error_prevention" => {
            insight_type
        }
        _ => "workflow_pattern",
    }
}

fn qualifies_for_auto_activation(confidence: f64, supporting_signals: i64) -> bool {
    confidence >= AUTO_ACTIVATE_CONFIDENCE && supporting_signals >= MIN_AUTO_ACTIVATE_SIGNALS
}

async fn has_active_slot(db: &SqlitePool, agent_id: &str, user_id: &str) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_insights \
         WHERE agent_id = ? AND user_id = ? AND status = 'active'",
    )
    .bind(agent_id)
    .bind(user_id)
    .fetch_one(db)
    .await
    .context("Failed to count active insights")?;

    Ok(count < MAX_ACTIVE_INSIGHTS)
}

/// Store a lesson, reinforcing an exact existing lesson instead of duplicating it.
/// High-confidence lessons supported by multiple turns activate immediately;
/// other lessons remain candidates for the user to review.
pub async fn add_insight(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
    lesson_text: &str,
    insight_type: &str,
    confidence: f64,
    supporting_signals: i64,
) -> Result<String> {
    let lesson = lesson_text.trim();
    if lesson.is_empty() {
        anyhow::bail!("Insight lesson cannot be empty");
    }

    let confidence = confidence.clamp(0.0, 1.0);
    let supporting_signals = supporting_signals.clamp(1, REFLECTION_INTERVAL);
    let insight_type = normalized_insight_type(insight_type);

    let existing = sqlx::query_as::<_, (String, String, f64, i64)>(
        "SELECT id, status, confidence, supporting_signals \
         FROM agent_insights \
         WHERE agent_id = ? AND user_id = ? \
           AND status IN ('candidate', 'active', 'paused') \
           AND lower(trim(lesson_text)) = lower(trim(?)) \
         LIMIT 1",
    )
    .bind(agent_id)
    .bind(user_id)
    .bind(lesson)
    .fetch_optional(db)
    .await
    .context("Failed to find an existing insight")?;

    if let Some((id, current_status, current_confidence, current_signals)) = existing {
        let confidence = confidence.max(current_confidence);
        let supporting_signals = current_signals.saturating_add(supporting_signals);
        let should_activate = current_status == "candidate"
            && qualifies_for_auto_activation(confidence, supporting_signals)
            && has_active_slot(db, agent_id, user_id).await?;
        let status = if should_activate {
            "active"
        } else {
            current_status.as_str()
        };

        sqlx::query(
            "UPDATE agent_insights \
             SET insight_type = ?, confidence = ?, supporting_signals = ?, status = ?, \
                 activated_at = CASE \
                     WHEN ? = 'active' AND activated_at IS NULL \
                     THEN strftime('%Y-%m-%dT%H:%M:%f', 'now') \
                     ELSE activated_at \
                 END, \
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') \
             WHERE id = ? AND agent_id = ? AND user_id = ?",
        )
        .bind(insight_type)
        .bind(confidence)
        .bind(supporting_signals)
        .bind(status)
        .bind(status)
        .bind(&id)
        .bind(agent_id)
        .bind(user_id)
        .execute(db)
        .await
        .context("Failed to reinforce insight")?;

        return Ok(id);
    }

    let active = qualifies_for_auto_activation(confidence, supporting_signals)
        && has_active_slot(db, agent_id, user_id).await?;
    let status = if active { "active" } else { "candidate" };
    let activated_at = active.then(|| {
        chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3f")
            .to_string()
    });
    let id = Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO agent_insights \
         (id, agent_id, user_id, lesson_text, insight_type, status, \
          confidence, supporting_signals, activated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(agent_id)
    .bind(user_id)
    .bind(lesson)
    .bind(insight_type)
    .bind(status)
    .bind(confidence)
    .bind(supporting_signals)
    .bind(&activated_at)
    .execute(db)
    .await
    .context("Failed to add insight")?;

    debug!(agent_id = %agent_id, user_id = %user_id, status, "Stored behavioral lesson");
    Ok(id)
}

pub async fn get_active_insights(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
) -> Result<Vec<Insight>> {
    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            String,
            String,
            String,
            f64,
            i64,
            String,
            Option<String>,
            String,
        ),
    >(
        "SELECT id, agent_id, user_id, lesson_text, insight_type, status, \
                confidence, supporting_signals, created_at, activated_at, updated_at \
         FROM agent_insights \
         WHERE agent_id = ? AND user_id = ? AND status = 'active' \
         ORDER BY confidence DESC, updated_at DESC \
         LIMIT 10",
    )
    .bind(agent_id)
    .bind(user_id)
    .fetch_all(db)
    .await
    .context("Failed to load active insights")?;

    Ok(rows.into_iter().map(row_to_insight).collect())
}

pub async fn list_insights(db: &SqlitePool, agent_id: &str, user_id: &str) -> Result<Vec<Insight>> {
    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            String,
            String,
            String,
            f64,
            i64,
            String,
            Option<String>,
            String,
        ),
    >(
        "SELECT id, agent_id, user_id, lesson_text, insight_type, status, \
                confidence, supporting_signals, created_at, activated_at, updated_at \
         FROM agent_insights \
         WHERE agent_id = ? AND user_id = ? AND status IN ('active', 'candidate', 'paused') \
         ORDER BY \
             CASE status WHEN 'active' THEN 0 WHEN 'candidate' THEN 1 ELSE 2 END, \
             confidence DESC, updated_at DESC",
    )
    .bind(agent_id)
    .bind(user_id)
    .fetch_all(db)
    .await
    .context("Failed to list insights")?;

    Ok(rows.into_iter().map(row_to_insight).collect())
}

/// Change one lesson belonging to this exact agent/user pair.
pub async fn update_insight_status(
    db: &SqlitePool,
    insight_id: &str,
    agent_id: &str,
    user_id: &str,
    new_status: &str,
) -> Result<bool> {
    let valid = ["candidate", "active", "paused", "rejected", "superseded"];
    if !valid.contains(&new_status) {
        anyhow::bail!("Invalid insight status: {new_status}");
    }

    let result = sqlx::query(
        "UPDATE agent_insights \
         SET status = ?, \
             activated_at = CASE \
                 WHEN ? = 'active' THEN COALESCE(activated_at, strftime('%Y-%m-%dT%H:%M:%f', 'now')) \
                 ELSE activated_at \
             END, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') \
         WHERE id = ? AND agent_id = ? AND user_id = ?",
    )
    .bind(new_status)
    .bind(new_status)
    .bind(insight_id)
    .bind(agent_id)
    .bind(user_id)
    .execute(db)
    .await
    .context("Failed to update insight status")?;

    Ok(result.rows_affected() > 0)
}

pub async fn reset_all(db: &SqlitePool, agent_id: &str, user_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM turn_signals WHERE agent_id = ? AND user_id = ?")
        .bind(agent_id)
        .bind(user_id)
        .execute(db)
        .await?;
    sqlx::query("DELETE FROM agent_insights WHERE agent_id = ? AND user_id = ?")
        .bind(agent_id)
        .bind(user_id)
        .execute(db)
        .await?;

    debug!(agent_id = %agent_id, user_id = %user_id, "Reset behavioral lessons");
    Ok(())
}

pub async fn delete_all_for_agent(db: &SqlitePool, agent_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM turn_signals WHERE agent_id = ?")
        .bind(agent_id)
        .execute(db)
        .await?;
    sqlx::query("DELETE FROM agent_insights WHERE agent_id = ?")
        .bind(agent_id)
        .execute(db)
        .await?;

    debug!(agent_id = %agent_id, "Deleted behavioral lessons for agent");
    Ok(())
}

/// Record a completed turn and reflect after each batch of five turns.
pub async fn process_turn_signal(
    db: &SqlitePool,
    creds: &CredentialStore,
    data: TurnSignalData,
) -> Result<()> {
    record_turn_signal(db, &data).await?;
    let total = count_signals(db, &data.agent_id, &data.user_id).await?;

    if total > 0
        && total % REFLECTION_INTERVAL == 0
        && let Err(error) = extract_insights(db, creds, &data.agent_id, &data.user_id).await
    {
        warn!("Lesson extraction failed (non-fatal): {error}");
    }

    Ok(())
}

async fn extract_insights(
    db: &SqlitePool,
    creds: &CredentialStore,
    agent_id: &str,
    user_id: &str,
) -> Result<()> {
    let signals = recent_signals(db, agent_id, user_id, REFLECTION_INTERVAL).await?;
    if signals.is_empty() {
        return Ok(());
    }

    let existing = list_insights(db, agent_id, user_id)
        .await
        .unwrap_or_default();
    let existing_lessons = existing
        .iter()
        .map(|insight| format!("- [{}] {}", insight.status, insight.lesson_text))
        .collect::<Vec<_>>()
        .join("\n");

    let mut signal_summary = String::new();
    for signal in &signals {
        let rating = match signal.user_rating {
            Some(1) => "positive",
            Some(-1) => "negative",
            _ => "no rating",
        };
        signal_summary.push_str(&format!(
            "- Turn {}: {} | tools: {} calls, {} failures, {} iterations | feedback: {}\n",
            signal.turn_index,
            signal
                .user_message_preview
                .as_deref()
                .unwrap_or("(no preview)"),
            signal.tool_calls_count,
            signal.tool_failures_count,
            signal.tool_loop_iterations,
            rating,
        ));
    }

    let system_prompt = format!(
        r#"You analyse recent interactions between an AI agent and a user to identify behavioral lessons the agent should learn.

A behavioral lesson must be:
- Specific and actionable (not vague like "be more helpful")
- Supported by at least 2 of the recent signals below
- About AGENT behavior (tool use, communication style, workflow, or error prevention)
- NOT a user fact or preference (those are handled by the user profile)
- Different from every existing lesson, including paused and suggested lessons

Recent interaction signals:
{signals}

Existing lessons (do not duplicate or reword):
{existing}

Respond with a JSON array. Each element must have:
- "lesson": a clear, specific instruction for the agent
- "evidence_summary": which signals support it
- "supporting_signals": integer from 2 to 5
- "type": one of "tool_usage", "communication_style", "workflow_pattern", "error_prevention"
- "confidence": 0.0 to 1.0

If no new lesson is justified, respond with: []
Respond ONLY with the JSON array."#,
        signals = signal_summary,
        existing = if existing_lessons.is_empty() {
            "(none yet)".to_string()
        } else {
            existing_lessons
        },
    );

    let resolved = crate::agents::model::resolve_model(db, agent_id).await?;
    let provider =
        crate::llm::registry::load_provider(db, &resolved.provider_id, &resolved.model_id, creds)
            .await?;
    use crate::llm::provider::{ChatMessage, LlmResponse};

    let response = provider
        .chat(
            &[
                ChatMessage::system(system_prompt),
                ChatMessage::user("Extract any justified behavioral lessons.".to_string()),
            ],
            &[],
            0.1,
        )
        .await?;
    let LlmResponse::Text(text) = response else {
        return Ok(());
    };
    let text = text.trim();
    let json = if text.starts_with("```") {
        text.lines()
            .skip(1)
            .take_while(|line| !line.starts_with("```"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text.to_string()
    };

    let insights: Vec<ExtractedInsight> = match serde_json::from_str(&json) {
        Ok(insights) => insights,
        Err(error) => {
            debug!("Failed to parse lesson extraction response: {error}");
            return Ok(());
        }
    };

    for insight in insights {
        if insight.lesson.trim().is_empty() {
            continue;
        }
        if let Err(error) = add_insight(
            db,
            agent_id,
            user_id,
            insight.lesson.trim(),
            &insight.insight_type,
            insight.confidence,
            insight.supporting_signals,
        )
        .await
        {
            warn!("Failed to store extracted lesson: {error}");
        }
    }

    Ok(())
}

pub async fn build_context_block(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
) -> Result<Option<String>> {
    let insights = get_active_insights(db, agent_id, user_id).await?;
    if insights.is_empty() {
        return Ok(None);
    }

    let mut context = String::from(
        "## Behavioral lessons learned for this user\n\n\
         Follow these lessons unless the user explicitly asks otherwise.\n\n",
    );
    for insight in &insights {
        context.push_str(&format!(
            "- {} (supported by {} observations)\n",
            insight.lesson_text, insight.supporting_signals
        ));
    }

    Ok(Some(context))
}

/// Remove raw learning signals once they are no longer useful and discard old,
/// unaccepted suggestions. Active and paused lessons are retained.
pub async fn run_maintenance(db: &SqlitePool) -> Result<()> {
    let signals = sqlx::query(
        "DELETE FROM turn_signals \
         WHERE created_at < strftime('%Y-%m-%dT%H:%M:%f', 'now', '-90 days')",
    )
    .execute(db)
    .await?;
    let candidates = sqlx::query(
        "DELETE FROM agent_insights \
         WHERE status = 'candidate' \
           AND created_at < strftime('%Y-%m-%dT%H:%M:%f', 'now', '-30 days')",
    )
    .execute(db)
    .await?;

    debug!(
        signals = signals.rows_affected(),
        candidates = candidates.rows_affected(),
        "Behavioral lesson maintenance complete"
    );
    Ok(())
}

fn row_to_insight(
    (
        id,
        agent_id,
        user_id,
        lesson_text,
        insight_type,
        status,
        confidence,
        supporting_signals,
        created_at,
        activated_at,
        updated_at,
    ): (
        String,
        String,
        String,
        String,
        String,
        String,
        f64,
        i64,
        String,
        Option<String>,
        String,
    ),
) -> Insight {
    Insight {
        id,
        agent_id,
        user_id,
        lesson_text,
        insight_type,
        status,
        confidence,
        supporting_signals,
        created_at,
        activated_at,
        updated_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE turn_signals (
                id TEXT PRIMARY KEY NOT NULL,
                agent_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                thread_id TEXT,
                turn_index INTEGER NOT NULL DEFAULT 0,
                user_rating INTEGER,
                tool_calls_count INTEGER NOT NULL DEFAULT 0,
                tool_failures_count INTEGER NOT NULL DEFAULT 0,
                tool_loop_iterations INTEGER NOT NULL DEFAULT 0,
                user_message_preview TEXT,
                agent_tools_used TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f','now'))
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE agent_insights (
                id TEXT PRIMARY KEY NOT NULL,
                agent_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                lesson_text TEXT NOT NULL,
                insight_type TEXT NOT NULL DEFAULT 'workflow_pattern',
                status TEXT NOT NULL DEFAULT 'candidate',
                confidence REAL NOT NULL DEFAULT 0.0,
                supporting_signals INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f','now')),
                activated_at TEXT,
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f','now'))
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    fn sample_signal_data() -> TurnSignalData {
        TurnSignalData {
            agent_id: "agent-1".into(),
            user_id: "user-1".into(),
            thread_id: Some("thread-1".into()),
            user_message_preview: Some("What's the weather?".into()),
            tool_calls_count: 2,
            tool_failures_count: 0,
            tool_loop_iterations: 3,
            agent_tools_used: vec!["weather_lookup".into()],
        }
    }

    #[tokio::test]
    async fn records_and_counts_signals() {
        let db = test_db().await;
        let data = sample_signal_data();
        record_turn_signal(&db, &data).await.unwrap();
        record_turn_signal(&db, &data).await.unwrap();
        assert_eq!(count_signals(&db, "agent-1", "user-1").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn rating_targets_latest_signal_in_thread() {
        let db = test_db().await;
        let data = sample_signal_data();
        assert!(!rate_turn(&db, "user-1", "thread-1", 1).await.unwrap());

        let first = record_turn_signal(&db, &data).await.unwrap();
        sqlx::query("UPDATE turn_signals SET created_at = '2000-01-01T00:00:00.000' WHERE id = ?")
            .bind(&first)
            .execute(&db)
            .await
            .unwrap();
        let mut delegated = data.clone();
        delegated.agent_id = "agent-2".into();
        record_turn_signal(&db, &delegated).await.unwrap();

        assert!(rate_turn(&db, "user-1", "thread-1", 5).await.unwrap());
        assert_eq!(
            latest_turn_rating(&db, "user-1", "thread-1").await.unwrap(),
            Some(1)
        );
        assert!(!rate_turn(&db, "user-2", "thread-1", -1).await.unwrap());
    }

    #[tokio::test]
    async fn recent_signals_are_agent_scoped() {
        let db = test_db().await;
        let mut data = sample_signal_data();
        record_turn_signal(&db, &data).await.unwrap();
        data.agent_id = "agent-2".into();
        record_turn_signal(&db, &data).await.unwrap();

        let signals = recent_signals(&db, "agent-1", "user-1", 5).await.unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].tool_calls_count, 2);
    }

    #[tokio::test]
    async fn uncertain_lesson_stays_suggested() {
        let db = test_db().await;
        add_insight(
            &db,
            "agent-1",
            "user-1",
            "Use ISO dates",
            "tool_usage",
            0.8,
            2,
        )
        .await
        .unwrap();

        assert!(
            get_active_insights(&db, "agent-1", "user-1")
                .await
                .unwrap()
                .is_empty()
        );
        let all = list_insights(&db, "agent-1", "user-1").await.unwrap();
        assert_eq!(all[0].status, "candidate");
    }

    #[tokio::test]
    async fn confident_supported_lesson_activates_immediately() {
        let db = test_db().await;
        add_insight(
            &db,
            "agent-1",
            "user-1",
            "Always include timezone",
            "workflow_pattern",
            0.9,
            2,
        )
        .await
        .unwrap();

        let active = get_active_insights(&db, "agent-1", "user-1").await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].lesson_text, "Always include timezone");
        assert!(active[0].activated_at.is_some());
    }

    #[tokio::test]
    async fn exact_duplicate_reinforces_instead_of_duplicating() {
        let db = test_db().await;
        let first = add_insight(
            &db,
            "agent-1",
            "user-1",
            "Use ISO dates",
            "tool_usage",
            0.7,
            1,
        )
        .await
        .unwrap();
        let second = add_insight(
            &db,
            "agent-1",
            "user-1",
            "  use iso dates  ",
            "tool_usage",
            0.9,
            2,
        )
        .await
        .unwrap();

        assert_eq!(first, second);
        let all = list_insights(&db, "agent-1", "user-1").await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].supporting_signals, 3);
        assert_eq!(all[0].status, "active");
    }

    #[tokio::test]
    async fn status_changes_require_matching_agent_and_user() {
        let db = test_db().await;
        let id = add_insight(
            &db,
            "agent-1",
            "user-1",
            "Use ISO dates",
            "tool_usage",
            0.8,
            2,
        )
        .await
        .unwrap();

        assert!(
            !update_insight_status(&db, &id, "agent-2", "user-1", "active")
                .await
                .unwrap()
        );
        assert!(
            !update_insight_status(&db, &id, "agent-1", "user-2", "active")
                .await
                .unwrap()
        );
        assert!(
            update_insight_status(&db, &id, "agent-1", "user-1", "active")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn reset_is_scoped_to_agent_and_user() {
        let db = test_db().await;
        let data = sample_signal_data();
        record_turn_signal(&db, &data).await.unwrap();
        add_insight(
            &db,
            "agent-1",
            "user-1",
            "Use ISO dates",
            "tool_usage",
            0.8,
            2,
        )
        .await
        .unwrap();

        reset_all(&db, "agent-1", "user-1").await.unwrap();
        assert_eq!(count_signals(&db, "agent-1", "user-1").await.unwrap(), 0);
        assert!(
            list_insights(&db, "agent-1", "user-1")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn parses_extracted_lessons() {
        let json = r#"[{"lesson":"Use ISO-8601 dates","evidence_summary":"2 failures","supporting_signals":2,"type":"tool_usage","confidence":0.85}]"#;
        let insights: Vec<ExtractedInsight> = serde_json::from_str(json).unwrap();
        assert_eq!(insights.len(), 1);
        assert_eq!(insights[0].insight_type, "tool_usage");
        assert_eq!(insights[0].supporting_signals, 2);
    }

    #[tokio::test]
    async fn context_contains_only_active_lessons() {
        let db = test_db().await;
        add_insight(
            &db,
            "agent-1",
            "user-1",
            "Always include timezone",
            "workflow_pattern",
            0.9,
            2,
        )
        .await
        .unwrap();

        let context = build_context_block(&db, "agent-1", "user-1")
            .await
            .unwrap()
            .unwrap();
        assert!(context.contains("Always include timezone"));
        assert!(context.contains("supported by 2 observations"));
    }
}
