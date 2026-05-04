/// Agent self-improvement: behavioral lessons learned from interaction patterns.
///
/// Tracks per-turn quality signals (explicit feedback + implicit metrics),
/// extracts behavioral insights via LLM reflection, and injects active
/// lessons into the agent context window for future turns.
///
/// Scoped per (agent_id, user_id) — same isolation model as memory/storage.
use anyhow::{Context, Result};
use serde::Deserialize;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::permissions::store::CredentialStore;

// ── Types ────────────────────────────────────────────────────────────────────

/// A per-turn quality signal combining explicit feedback and implicit metrics.
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
    pub was_retry: bool,
    pub was_abandoned: bool,
    pub agent_switched: bool,
    pub user_message_preview: Option<String>,
    pub agent_tools_used: Option<String>,
    pub created_at: String,
}

/// Data needed to record a turn signal (collected from engine.rs post-turn).
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

/// A behavioral insight (lesson) extracted from turn signal patterns.
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
    pub promotion_threshold: i64,
    pub auto_paused: bool,
    pub created_at: String,
    pub activated_at: Option<String>,
    pub updated_at: String,
}

/// LLM extraction output format.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ExtractedInsight {
    pub lesson: String,
    pub evidence_summary: String,
    #[serde(rename = "type")]
    pub insight_type: String,
    pub confidence: f64,
}

// ── Turn signal persistence ──────────────────────────────────────────────────

/// Record a turn signal after an agent turn completes.
pub async fn record_turn_signal(db: &SqlitePool, data: &TurnSignalData) -> Result<String> {
    let id = Uuid::new_v4().to_string();

    // Count existing signals to derive turn_index
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

/// Update the user rating on the most recent turn signal for a thread.
pub async fn rate_turn(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
    thread_id: &str,
    rating: i64,
) -> Result<bool> {
    let clamped = rating.clamp(-1, 1);
    let result = sqlx::query(
        "UPDATE turn_signals SET user_rating = ? \
         WHERE id = (
             SELECT id FROM turn_signals \
             WHERE agent_id = ? AND user_id = ? AND thread_id = ? \
             ORDER BY created_at DESC LIMIT 1
         )",
    )
    .bind(clamped)
    .bind(agent_id)
    .bind(user_id)
    .bind(thread_id)
    .execute(db)
    .await
    .context("Failed to rate turn")?;

    Ok(result.rows_affected() > 0)
}

/// Count total turn signals for an (agent, user) pair.
pub async fn count_signals(db: &SqlitePool, agent_id: &str, user_id: &str) -> Result<i64> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM turn_signals WHERE agent_id = ? AND user_id = ?")
            .bind(agent_id)
            .bind(user_id)
            .fetch_one(db)
            .await
            .context("Failed to count turn signals")?;

    Ok(count)
}

/// Fetch the most recent N turn signals for insight extraction.
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
            bool,
            bool,
            bool,
            Option<String>,
            Option<String>,
            String,
        ),
    >(
        "SELECT id, agent_id, user_id, thread_id, turn_index, \
                user_rating, tool_calls_count, tool_failures_count, tool_loop_iterations, \
                was_retry, was_abandoned, agent_switched, \
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
                was_retry,
                was_abandoned,
                agent_switched,
                user_message_preview,
                agent_tools_used,
                created_at,
            )| {
                TurnSignal {
                    id,
                    agent_id,
                    user_id,
                    thread_id,
                    turn_index,
                    user_rating,
                    tool_calls_count,
                    tool_failures_count,
                    tool_loop_iterations,
                    was_retry,
                    was_abandoned,
                    agent_switched,
                    user_message_preview,
                    agent_tools_used,
                    created_at,
                }
            },
        )
        .collect())
}

// ── Insight CRUD ─────────────────────────────────────────────────────────────

/// Insert a new candidate insight.
pub async fn add_insight(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
    lesson_text: &str,
    insight_type: &str,
    confidence: f64,
) -> Result<String> {
    let id = Uuid::new_v4().to_string();
    let valid_types = [
        "tool_usage",
        "communication_style",
        "workflow_pattern",
        "error_prevention",
    ];
    let itype = if valid_types.contains(&insight_type) {
        insight_type
    } else {
        "workflow_pattern"
    };

    sqlx::query(
        "INSERT INTO agent_insights \
         (id, agent_id, user_id, lesson_text, insight_type, confidence) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(agent_id)
    .bind(user_id)
    .bind(lesson_text)
    .bind(itype)
    .bind(confidence)
    .execute(db)
    .await
    .context("Failed to add insight")?;

    debug!(agent_id = %agent_id, user_id = %user_id, insight_type = %itype, "Added candidate insight");
    Ok(id)
}

/// Get all active insights for context injection.
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
            i64,
            bool,
            String,
            Option<String>,
            String,
        ),
    >(
        "SELECT id, agent_id, user_id, lesson_text, insight_type, status, \
                confidence, supporting_signals, promotion_threshold, auto_paused, \
                created_at, activated_at, updated_at \
         FROM agent_insights \
         WHERE agent_id = ? AND user_id = ? AND status = 'active' \
         ORDER BY confidence DESC \
         LIMIT 10",
    )
    .bind(agent_id)
    .bind(user_id)
    .fetch_all(db)
    .await
    .context("Failed to load active insights")?;

    Ok(rows.into_iter().map(row_to_insight).collect())
}

/// Get all insights (any status) for the dashboard.
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
            i64,
            bool,
            String,
            Option<String>,
            String,
        ),
    >(
        "SELECT id, agent_id, user_id, lesson_text, insight_type, status, \
                confidence, supporting_signals, promotion_threshold, auto_paused, \
                created_at, activated_at, updated_at \
         FROM agent_insights \
         WHERE agent_id = ? AND user_id = ? AND status IN ('active', 'candidate', 'paused') \
         ORDER BY \
             CASE status WHEN 'active' THEN 0 WHEN 'candidate' THEN 1 ELSE 2 END, \
             confidence DESC",
    )
    .bind(agent_id)
    .bind(user_id)
    .fetch_all(db)
    .await
    .context("Failed to list insights")?;

    Ok(rows.into_iter().map(row_to_insight).collect())
}

/// Update an insight's status (pause, reject, activate).
pub async fn update_insight_status(
    db: &SqlitePool,
    insight_id: &str,
    new_status: &str,
) -> Result<bool> {
    let valid = ["candidate", "active", "paused", "rejected", "superseded"];
    if !valid.contains(&new_status) {
        anyhow::bail!("Invalid insight status: {new_status}");
    }

    let activated_clause = if new_status == "active" {
        ", activated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now')"
    } else {
        ""
    };

    let query = format!(
        "UPDATE agent_insights SET status = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now'){} WHERE id = ?",
        activated_clause
    );

    let result = sqlx::query(&query)
        .bind(new_status)
        .bind(insight_id)
        .execute(db)
        .await
        .context("Failed to update insight status")?;

    Ok(result.rows_affected() > 0)
}

/// Increment supporting_signals and update confidence for an existing insight.
#[allow(dead_code)]
pub async fn reinforce_insight(
    db: &SqlitePool,
    insight_id: &str,
    new_confidence: f64,
) -> Result<()> {
    sqlx::query(
        "UPDATE agent_insights \
         SET supporting_signals = supporting_signals + 1, \
             confidence = ?, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') \
         WHERE id = ?",
    )
    .bind(new_confidence)
    .bind(insight_id)
    .execute(db)
    .await
    .context("Failed to reinforce insight")?;

    Ok(())
}

/// Delete all improvement data for an agent+user (full reset).
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

    sqlx::query("DELETE FROM improvement_metrics WHERE agent_id = ? AND user_id = ?")
        .bind(agent_id)
        .bind(user_id)
        .execute(db)
        .await?;

    debug!(agent_id = %agent_id, user_id = %user_id, "Reset all improvement data");
    Ok(())
}

/// Delete all improvement data for an agent (called on uninstall).
pub async fn delete_all_for_agent(db: &SqlitePool, agent_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM turn_signals WHERE agent_id = ?")
        .bind(agent_id)
        .execute(db)
        .await?;
    sqlx::query("DELETE FROM agent_insights WHERE agent_id = ?")
        .bind(agent_id)
        .execute(db)
        .await?;
    sqlx::query("DELETE FROM improvement_metrics WHERE agent_id = ?")
        .bind(agent_id)
        .execute(db)
        .await?;

    debug!(agent_id = %agent_id, "Deleted all improvement data for agent");
    Ok(())
}

// ── Post-turn processing ─────────────────────────────────────────────────────

/// Main entry point called as a background task after each agent turn.
///
/// 1. Persists the turn signal (always).
/// 2. Every 5th turn (or on negative rating), triggers insight extraction.
///
/// Best-effort: failures are logged but never block the agent turn.
pub async fn process_turn_signal(
    db: &SqlitePool,
    creds: &crate::permissions::store::CredentialStore,
    data: TurnSignalData,
    had_negative_rating: bool,
) -> Result<()> {
    // 1. Record signal
    let _signal_id = record_turn_signal(db, &data).await?;

    // 2. Check if we should extract insights
    let total = count_signals(db, &data.agent_id, &data.user_id).await?;
    let should_extract = had_negative_rating || (total > 0 && total % 5 == 0);

    if should_extract {
        if let Err(e) = extract_insights(db, creds, &data.agent_id, &data.user_id).await {
            warn!("Insight extraction failed (non-fatal): {e}");
        }
    }

    Ok(())
}

/// Extract behavioral insights by having the LLM reflect on recent signals.
async fn extract_insights(
    db: &SqlitePool,
    creds: &crate::permissions::store::CredentialStore,
    agent_id: &str,
    user_id: &str,
) -> Result<()> {
    let signals = recent_signals(db, agent_id, user_id, 5).await?;
    if signals.is_empty() {
        return Ok(());
    }

    let active = get_active_insights(db, agent_id, user_id)
        .await
        .unwrap_or_default();

    // Build signal summary for the LLM
    let mut signal_summary = String::new();
    for s in &signals {
        let rating_str = match s.user_rating {
            Some(1) => "positive",
            Some(-1) => "negative",
            _ => "no rating",
        };
        signal_summary.push_str(&format!(
            "- Turn {}: {} | tools: {} calls, {} failures, {} iterations | feedback: {}\n",
            s.turn_index,
            s.user_message_preview.as_deref().unwrap_or("(no preview)"),
            s.tool_calls_count,
            s.tool_failures_count,
            s.tool_loop_iterations,
            rating_str,
        ));
    }

    let existing_lessons: String = active
        .iter()
        .map(|i| format!("- {}", i.lesson_text))
        .collect::<Vec<_>>()
        .join("\n");

    let system_prompt = format!(
        r#"You analyse recent interactions between an AI agent and a user to identify behavioral lessons the agent should learn.

A behavioral lesson must be:
- Specific and actionable (not vague like "be more helpful")
- Supported by at least 2 signals showing the same pattern
- About AGENT behavior (how to use tools, communication style, workflow patterns)
- NOT about user facts or preferences (those are handled separately)

Recent interaction signals:
{signals}

Already-active lessons (do not duplicate):
{existing}

Respond with a JSON array. Each element:
- "lesson": a clear, specific instruction for the agent
- "evidence_summary": brief explanation of what signals support this
- "type": one of "tool_usage", "communication_style", "workflow_pattern", "error_prevention"
- "confidence": 0.0 to 1.0

If no new lessons can be extracted, respond with: []

Respond ONLY with the JSON array."#,
        signals = signal_summary,
        existing = if existing_lessons.is_empty() {
            "(none yet)".to_string()
        } else {
            existing_lessons
        }
    );

    let resolved = crate::agents::model::resolve_model(db, agent_id).await?;
    let provider =
        crate::llm::registry::load_provider(db, &resolved.provider_id, &resolved.model_id, creds)
            .await?;

    use crate::llm::provider::{ChatMessage, LlmResponse};

    let messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user("Analyse the signals above and extract behavioral lessons.".to_string()),
    ];

    let response = provider.chat(&messages, &[], 0.1).await?;
    let text = match response {
        LlmResponse::Text(t) => t,
        _ => return Ok(()),
    };

    // Parse JSON (handle markdown code fences)
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

    let insights: Vec<ExtractedInsight> = match serde_json::from_str(&json_str) {
        Ok(i) => i,
        Err(e) => {
            debug!("Failed to parse insight extraction response: {e}");
            return Ok(());
        }
    };

    for insight in insights {
        if insight.lesson.trim().is_empty() {
            continue;
        }
        let confidence = insight.confidence.clamp(0.0, 1.0);
        if let Err(e) = add_insight(
            db,
            agent_id,
            user_id,
            insight.lesson.trim(),
            &insight.insight_type,
            confidence,
        )
        .await
        {
            warn!("Failed to store extracted insight: {e}");
        }
    }

    Ok(())
}

// ── Context injection ────────────────────────────────────────────────────────

/// Build the behavioral lessons context block for injection into the system prompt.
///
/// Returns `None` if there are no active lessons.
pub async fn build_context_block(
    db: &SqlitePool,
    agent_id: &str,
    user_id: &str,
) -> Result<Option<String>> {
    let insights = get_active_insights(db, agent_id, user_id).await?;
    if insights.is_empty() {
        return Ok(None);
    }

    let mut ctx = String::from(
        "## Behavioral lessons (learned from past interactions with this user)\n\n\
         These are patterns you've learned from previous conversations. \
         Follow them unless the user explicitly asks otherwise.\n\n",
    );

    for insight in &insights {
        ctx.push_str(&format!(
            "- {} (confirmed {} times)\n",
            insight.lesson_text, insight.supporting_signals
        ));
    }

    Ok(Some(ctx))
}

// ── Helpers ──────────────────────────────────────────────────────────────────

// ── Periodic aggregation ─────────────────────────────────────────────────────

/// Runs periodically (every 6 hours) to:
/// 1. Promote candidates with enough supporting signals.
/// 2. Auto-pause insights that correlate with degradation.
/// 3. Prune old turn signals (>90 days).
/// 4. Aggregate daily metrics.
pub async fn run_periodic_aggregation(db: &SqlitePool) -> Result<()> {
    // 1. Promote eligible candidates
    let promoted = sqlx::query(
        "UPDATE agent_insights \
         SET status = 'active', \
             activated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now'), \
             updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') \
         WHERE status = 'candidate' \
           AND supporting_signals >= promotion_threshold",
    )
    .execute(db)
    .await?;

    if promoted.rows_affected() > 0 {
        debug!(
            count = promoted.rows_affected(),
            "Promoted candidate insights to active"
        );
    }

    // 2. Enforce hard cap of 10 active insights per (agent, user).
    //    Demote lowest-confidence active insights beyond the cap.
    sqlx::query(
        "UPDATE agent_insights SET status = 'candidate', \
             updated_at = strftime('%Y-%m-%dT%H:%M:%f', 'now') \
         WHERE id IN (
             SELECT i.id FROM agent_insights i
             WHERE i.status = 'active'
             AND (SELECT COUNT(*) FROM agent_insights i2
                  WHERE i2.agent_id = i.agent_id AND i2.user_id = i.user_id
                    AND i2.status = 'active' AND i2.confidence > i.confidence) >= 10
         )",
    )
    .execute(db)
    .await?;

    // 3. Prune old turn signals (>90 days)
    let pruned = sqlx::query(
        "DELETE FROM turn_signals \
         WHERE created_at < strftime('%Y-%m-%dT%H:%M:%f', 'now', '-90 days')",
    )
    .execute(db)
    .await?;

    if pruned.rows_affected() > 0 {
        debug!(count = pruned.rows_affected(), "Pruned old turn signals");
    }

    // 4. Prune stale candidates (>14 days old, insufficient support)
    sqlx::query(
        "DELETE FROM agent_insights \
         WHERE status = 'candidate' \
           AND supporting_signals < promotion_threshold \
           AND created_at < strftime('%Y-%m-%dT%H:%M:%f', 'now', '-14 days')",
    )
    .execute(db)
    .await?;

    // 5. Aggregate daily metrics for today
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    sqlx::query(
        "INSERT OR REPLACE INTO improvement_metrics \
         (id, agent_id, user_id, date, total_turns, positive_ratings, negative_ratings, \
          tool_failures, avg_tool_iterations, retry_count) \
         SELECT \
             agent_id || ':' || user_id || ':' || ? AS id, \
             agent_id, user_id, ?, \
             COUNT(*), \
             SUM(CASE WHEN user_rating = 1 THEN 1 ELSE 0 END), \
             SUM(CASE WHEN user_rating = -1 THEN 1 ELSE 0 END), \
             SUM(tool_failures_count), \
             AVG(tool_loop_iterations), \
             SUM(CASE WHEN was_retry = 1 THEN 1 ELSE 0 END) \
         FROM turn_signals \
         WHERE date(created_at) = ? \
         GROUP BY agent_id, user_id",
    )
    .bind(&today)
    .bind(&today)
    .bind(&today)
    .execute(db)
    .await?;

    debug!("Periodic improvement aggregation complete");
    Ok(())
}

// ── Periodic insight optimization ────────────────────────────────────────────

/// Minimum number of candidate insights before optimization kicks in.
/// Below this there is nothing meaningful to consolidate.
const OPTIMIZE_MIN_INSIGHTS: usize = 3;

/// Run insight optimization for every (agent_id, user_id) pair that has
/// candidate or active insights.  Modeled after `profile::run_optimization`.
pub async fn run_optimization(db: &SqlitePool, creds: &CredentialStore) -> Result<()> {
    let pairs: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT agent_id, user_id FROM agent_insights \
         WHERE status IN ('candidate', 'active')",
    )
    .fetch_all(db)
    .await
    .context("Failed to list agent/user pairs with insights")?;

    for (agent_id, user_id) in &pairs {
        if let Err(e) = optimize_insights(db, creds, agent_id, user_id).await {
            warn!(
                agent_id = %agent_id,
                user_id = %user_id,
                "Insight optimization failed (non-fatal): {e}"
            );
        }
    }

    Ok(())
}

/// Use an LLM to deduplicate, merge, and consolidate behavioral insights for
/// a single (agent, user) pair.  Semantically similar candidates are merged,
/// their `supporting_signals` are summed, and the consolidated list replaces
/// the old one atomically.
///
/// This is the missing link that allows `supporting_signals` to actually grow
/// past the promotion threshold (default 3), enabling `run_periodic_aggregation`
/// to promote candidates to active.
async fn optimize_insights(
    db: &SqlitePool,
    creds: &CredentialStore,
    agent_id: &str,
    user_id: &str,
) -> Result<()> {
    // Load all candidate + active insights for this pair.
    let insights = sqlx::query_as::<
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
            i64,
            bool,
            String,
            Option<String>,
            String,
        ),
    >(
        "SELECT id, agent_id, user_id, lesson_text, insight_type, status, \
                confidence, supporting_signals, promotion_threshold, auto_paused, \
                created_at, activated_at, updated_at \
         FROM agent_insights \
         WHERE agent_id = ? AND user_id = ? AND status IN ('candidate', 'active') \
         ORDER BY status, confidence DESC",
    )
    .bind(agent_id)
    .bind(user_id)
    .fetch_all(db)
    .await
    .context("Failed to load insights for optimization")?;

    let insights: Vec<Insight> = insights.into_iter().map(row_to_insight).collect();

    if insights.len() < OPTIMIZE_MIN_INSIGHTS {
        return Ok(());
    }

    // Build a numbered list so the LLM sees everything.
    let insights_text: String = insights
        .iter()
        .enumerate()
        .map(|(i, ins)| {
            format!(
                "{}. [{}] [{}] {} (confidence: {:.2}, supporting_signals: {}, status: {})",
                i + 1,
                ins.insight_type,
                ins.status,
                ins.lesson_text,
                ins.confidence,
                ins.supporting_signals,
                ins.status,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let system_prompt = format!(
        r#"You are a behavioral-insight optimizer for an AI agent. You will receive a numbered list of behavioral lessons (insights) about how the agent should behave with a specific user.

Your job:
1. **Deduplicate**: merge insights that teach the same lesson (even if worded differently).
2. **Sum signals**: when merging, set `supporting_signals` to the SUM of all merged insights' signals.
3. **Max confidence**: when merging, set `confidence` to the MAX of all merged insights' confidence values.
4. **Preserve status**: if ANY merged insight was "active", the result should be "active". Otherwise "candidate".
5. **Preserve type**: keep the most specific `type` from the merged group.
6. **Consolidate**: combine overlapping lessons into one clearer statement. Keep atomic lessons atomic.
7. **Preserve meaning**: never invent new lessons. Only merge what genuinely overlaps.

Current insights:
{insights}

Respond with a JSON array. Each element must have:
- "lesson": a clear, specific behavioral instruction
- "type": one of "tool_usage", "communication_style", "workflow_pattern", "error_prevention"
- "confidence": 0.0 to 1.0 (max of merged)
- "supporting_signals": integer (sum of merged)
- "status": "candidate" or "active"

Respond ONLY with the JSON array, nothing else."#,
        insights = insights_text,
    );

    // Use the default agent's model for background tasks.
    let resolved = crate::agents::model::resolve_model(db, "default").await?;
    let provider =
        crate::llm::registry::load_provider(db, &resolved.provider_id, &resolved.model_id, creds)
            .await?;

    use crate::llm::provider::{ChatMessage, LlmResponse};

    let messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user("Optimize the insights above.".to_string()),
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
    struct OptimizedInsight {
        lesson: String,
        #[serde(rename = "type")]
        insight_type: String,
        confidence: f64,
        supporting_signals: i64,
        status: String,
    }

    let optimized: Vec<OptimizedInsight> = match serde_json::from_str(&json_str) {
        Ok(o) => o,
        Err(e) => {
            warn!("Failed to parse insight optimization response: {e}");
            return Ok(());
        }
    };

    if optimized.is_empty() {
        return Ok(());
    }

    // Safety guard: never let the LLM inflate the insight count.
    if optimized.len() > insights.len() {
        warn!(
            agent_id = %agent_id,
            user_id = %user_id,
            before = insights.len(),
            after = optimized.len(),
            "LLM returned more insights than input — skipping optimization"
        );
        return Ok(());
    }

    let valid_types = [
        "tool_usage",
        "communication_style",
        "workflow_pattern",
        "error_prevention",
    ];
    let valid_statuses = ["candidate", "active"];

    // Replace: delete old candidate+active insights and insert optimized ones.
    let mut tx = db.begin().await.context("Failed to begin transaction")?;

    sqlx::query(
        "DELETE FROM agent_insights \
         WHERE agent_id = ? AND user_id = ? AND status IN ('candidate', 'active')",
    )
    .bind(agent_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await
    .context("Failed to clear old insights")?;

    for ins in &optimized {
        if ins.lesson.trim().is_empty() {
            continue;
        }

        let itype = if valid_types.contains(&ins.insight_type.as_str()) {
            &ins.insight_type
        } else {
            "workflow_pattern"
        };

        let status = if valid_statuses.contains(&ins.status.as_str()) {
            &ins.status
        } else {
            "candidate"
        };

        let confidence = ins.confidence.clamp(0.0, 1.0);
        let signals = ins.supporting_signals.max(1);

        let id = Uuid::new_v4().to_string();
        let activated_at = if status == "active" {
            Some(
                chrono::Utc::now()
                    .format("%Y-%m-%dT%H:%M:%S%.3f")
                    .to_string(),
            )
        } else {
            None
        };

        sqlx::query(
            "INSERT INTO agent_insights \
             (id, agent_id, user_id, lesson_text, insight_type, status, \
              confidence, supporting_signals, activated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(agent_id)
        .bind(user_id)
        .bind(ins.lesson.trim())
        .bind(itype)
        .bind(status)
        .bind(confidence)
        .bind(signals)
        .bind(&activated_at)
        .execute(&mut *tx)
        .await
        .context("Failed to insert optimized insight")?;
    }

    tx.commit()
        .await
        .context("Failed to commit optimized insights")?;

    info!(
        agent_id = %agent_id,
        user_id = %user_id,
        before = insights.len(),
        after = optimized.len(),
        "Behavioral insights optimized"
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
        promotion_threshold,
        auto_paused,
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
        i64,
        bool,
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
        promotion_threshold,
        auto_paused,
        created_at,
        activated_at,
        updated_at,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

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
                was_retry INTEGER NOT NULL DEFAULT 0,
                was_abandoned INTEGER NOT NULL DEFAULT 0,
                agent_switched INTEGER NOT NULL DEFAULT 0,
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
                promotion_threshold INTEGER NOT NULL DEFAULT 3,
                auto_paused INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f','now')),
                activated_at TEXT,
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f','now'))
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "CREATE TABLE improvement_metrics (
                id TEXT PRIMARY KEY NOT NULL,
                agent_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                date TEXT NOT NULL,
                total_turns INTEGER NOT NULL DEFAULT 0,
                positive_ratings INTEGER NOT NULL DEFAULT 0,
                negative_ratings INTEGER NOT NULL DEFAULT 0,
                tool_failures INTEGER NOT NULL DEFAULT 0,
                avg_tool_iterations REAL NOT NULL DEFAULT 0.0,
                retry_count INTEGER NOT NULL DEFAULT 0,
                UNIQUE(agent_id, user_id, date)
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
    async fn test_record_and_count_signals() {
        let db = test_db().await;
        let data = sample_signal_data();

        record_turn_signal(&db, &data).await.unwrap();
        record_turn_signal(&db, &data).await.unwrap();

        let count = count_signals(&db, "agent-1", "user-1").await.unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn test_recent_signals() {
        let db = test_db().await;
        let data = sample_signal_data();

        record_turn_signal(&db, &data).await.unwrap();
        record_turn_signal(&db, &data).await.unwrap();

        let signals = recent_signals(&db, "agent-1", "user-1", 5).await.unwrap();
        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0].tool_calls_count, 2);
    }

    #[tokio::test]
    async fn test_add_and_get_insights() {
        let db = test_db().await;

        add_insight(&db, "agent-1", "user-1", "Use ISO dates", "tool_usage", 0.8)
            .await
            .unwrap();

        // Still candidate, so get_active should return empty
        let active = get_active_insights(&db, "agent-1", "user-1").await.unwrap();
        assert!(active.is_empty());

        // List should include candidates
        let all = list_insights(&db, "agent-1", "user-1").await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, "candidate");
    }

    #[tokio::test]
    async fn test_promote_insight() {
        let db = test_db().await;

        let id = add_insight(&db, "agent-1", "user-1", "Use ISO dates", "tool_usage", 0.8)
            .await
            .unwrap();

        update_insight_status(&db, &id, "active").await.unwrap();

        let active = get_active_insights(&db, "agent-1", "user-1").await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].lesson_text, "Use ISO dates");
        assert!(active[0].activated_at.is_some());
    }

    #[tokio::test]
    async fn test_reset_all() {
        let db = test_db().await;
        let data = sample_signal_data();

        record_turn_signal(&db, &data).await.unwrap();
        add_insight(&db, "agent-1", "user-1", "lesson", "tool_usage", 0.5)
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

    #[tokio::test]
    async fn test_signal_isolation() {
        let db = test_db().await;
        let mut data = sample_signal_data();
        record_turn_signal(&db, &data).await.unwrap();

        data.agent_id = "agent-2".into();
        record_turn_signal(&db, &data).await.unwrap();

        assert_eq!(count_signals(&db, "agent-1", "user-1").await.unwrap(), 1);
        assert_eq!(count_signals(&db, "agent-2", "user-1").await.unwrap(), 1);
    }

    #[test]
    fn test_parse_extracted_insights() {
        let json = r#"[{"lesson":"Use ISO-8601 dates","evidence_summary":"2 failures","type":"tool_usage","confidence":0.85}]"#;
        let insights: Vec<ExtractedInsight> = serde_json::from_str(json).unwrap();
        assert_eq!(insights.len(), 1);
        assert_eq!(insights[0].lesson, "Use ISO-8601 dates");
        assert_eq!(insights[0].insight_type, "tool_usage");
    }

    #[test]
    fn test_parse_empty_insights() {
        let json = "[]";
        let insights: Vec<ExtractedInsight> = serde_json::from_str(json).unwrap();
        assert!(insights.is_empty());
    }

    #[tokio::test]
    async fn test_build_context_block_empty() {
        let db = test_db().await;
        let ctx = build_context_block(&db, "agent-1", "user-1").await.unwrap();
        assert!(ctx.is_none());
    }

    #[tokio::test]
    async fn test_build_context_block_with_active() {
        let db = test_db().await;

        let id = add_insight(
            &db,
            "agent-1",
            "user-1",
            "Always include timezone",
            "workflow_pattern",
            0.9,
        )
        .await
        .unwrap();
        update_insight_status(&db, &id, "active").await.unwrap();

        let ctx = build_context_block(&db, "agent-1", "user-1").await.unwrap();
        assert!(ctx.is_some());
        let text = ctx.unwrap();
        assert!(text.contains("Always include timezone"));
        assert!(text.contains("confirmed"));
    }
}
