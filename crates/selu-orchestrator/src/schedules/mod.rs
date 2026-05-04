/// Schedule management: CRUD operations and cron helpers.
///
/// Schedules allow users to run agent prompts on a recurring basis.
/// Each schedule has a cron expression (parsed from natural language),
/// a prompt, and one or more target pipes.
pub mod executor;
pub mod nl_to_cron;

use anyhow::{Context, Result};
use chrono::{DateTime, LocalResult, NaiveDateTime, TimeZone, Utc};
use cron::Schedule as CronSchedule;
use sqlx::SqlitePool;
use std::str::FromStr;
use tracing::error;
use uuid::Uuid;

// ── View types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ScheduleRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub prompt: String,
    pub cron_expression: String,
    pub cron_description: String,
    pub active: bool,
    pub one_shot: bool,
    pub last_run_at: Option<String>,
    pub next_run_at: String,
    pub created_at: String,
    pub pipe_ids: Vec<String>,
    pub pipe_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DueSchedule {
    pub id: String,
    pub user_id: String,
    pub agent_id: Option<String>,
    pub name: String,
    pub prompt: String,
    pub cron_expression: String,
    pub timezone: String,
    pub one_shot: bool,
    pub pipe_ids: Vec<String>,
}

// ── Cron helpers ─────────────────────────────────────────────────────────────

/// Adjust the day-of-week field from standard cron convention (0=Sun..6=Sat)
/// to the `cron` crate's convention (1=Sun..7=Sat).
///
/// The 6-field format is: second minute hour day-of-month month day-of-week.
/// This function increments each numeric weekday value by 1 in the last field.
fn adjust_weekday_field(cron_expr: &str) -> Result<String> {
    let fields: Vec<&str> = cron_expr.split_whitespace().collect();
    if fields.len() != 6 {
        anyhow::bail!(
            "Expected 6-field cron expression, got {}: '{}'",
            fields.len(),
            cron_expr
        );
    }

    let dow = fields[5];

    // Wildcard needs no adjustment
    if dow == "*" || dow == "?" {
        return Ok(cron_expr.to_string());
    }

    // Process comma-separated parts (e.g. "1,3,5" or "1-5,0")
    let adjusted_parts: Vec<String> = dow
        .split(',')
        .map(|part| adjust_dow_part(part))
        .collect::<Result<Vec<_>>>()?;

    let adjusted_dow = adjusted_parts.join(",");
    Ok(format!(
        "{} {} {} {} {} {}",
        fields[0], fields[1], fields[2], fields[3], fields[4], adjusted_dow
    ))
}

/// Adjust a single day-of-week part: a number, range (e.g. "1-5"), or step (e.g. "*/2").
fn adjust_dow_part(part: &str) -> Result<String> {
    // Step expressions like */2 or 1-5/2
    if let Some((base, step)) = part.split_once('/') {
        let adjusted_base = if base == "*" {
            "*".to_string()
        } else {
            adjust_dow_part(base)?
        };
        return Ok(format!("{}/{}", adjusted_base, step));
    }

    // Range like 1-5
    if let Some((start, end)) = part.split_once('-') {
        let s: u8 = start
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid weekday: {}", start))?;
        let e: u8 = end
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid weekday: {}", end))?;
        return Ok(format!("{}-{}", shift_dow(s), shift_dow(e)));
    }

    // Single number
    let n: u8 = part
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid weekday: {}", part))?;
    Ok(shift_dow(n).to_string())
}

/// Map standard weekday (0/7=Sun, 1=Mon..6=Sat) to crate weekday (1=Sun..7=Sat).
fn shift_dow(n: u8) -> u8 {
    match n {
        0 | 7 => 1, // Sunday
        1..=6 => n + 1,
        _ => n + 1, // let the crate reject out-of-range values
    }
}

/// Parse a cron expression, adjusting the weekday field for the crate's convention.
fn parse_cron(cron_expr: &str) -> Result<CronSchedule> {
    let adjusted = adjust_weekday_field(cron_expr)?;
    CronSchedule::from_str(&adjusted)
        .map_err(|e| anyhow::anyhow!("Invalid cron expression '{}': {}", cron_expr, e))
}

/// Compute the next run time for a cron expression in the user's timezone,
/// returning the result as UTC.
pub fn compute_next_run(
    cron_expr: &str,
    timezone: &str,
    after: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    let tz: chrono_tz::Tz = timezone
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid timezone: {}", timezone))?;

    let schedule = parse_cron(cron_expr)?;

    // Convert `after` to the user's local time, find the next occurrence,
    // then convert back to UTC.
    let local_after = after.with_timezone(&tz);
    let next_local = schedule
        .after(&local_after)
        .next()
        .ok_or_else(|| anyhow::anyhow!("No future occurrence for cron '{}'", cron_expr))?;

    Ok(next_local.with_timezone(&Utc))
}

/// Validate that a cron expression is parseable.
pub fn validate_cron(cron_expr: &str) -> Result<()> {
    parse_cron(cron_expr)?;
    Ok(())
}

// ── CRUD operations ──────────────────────────────────────────────────────────

/// Create a new schedule with associated pipes.
pub async fn create_schedule(
    db: &SqlitePool,
    user_id: &str,
    agent_id: Option<&str>,
    name: &str,
    prompt: &str,
    cron_expression: &str,
    cron_description: &str,
    timezone: &str,
    pipe_ids: &[String],
) -> Result<String> {
    validate_cron(cron_expression)?;

    let next_run = compute_next_run(cron_expression, timezone, Utc::now())?;
    let next_run_str = next_run.format("%Y-%m-%d %H:%M:%S").to_string();

    let id = Uuid::new_v4().to_string();

    sqlx::query!(
        r#"INSERT INTO schedules (id, user_id, agent_id, name, prompt, cron_expression, cron_description, next_run_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
        id,
        user_id,
        agent_id,
        name,
        prompt,
        cron_expression,
        cron_description,
        next_run_str,
    )
    .execute(db)
    .await
    .context("Failed to insert schedule")?;

    // Insert pipe associations
    for pipe_id in pipe_ids {
        sqlx::query!(
            "INSERT INTO schedule_pipes (schedule_id, pipe_id) VALUES (?, ?)",
            id,
            pipe_id,
        )
        .execute(db)
        .await
        .context("Failed to insert schedule pipe association")?;
    }

    Ok(id)
}

/// Create a one-shot reminder that fires once at a specific time.
///
/// Unlike recurring schedules, reminders use a direct `fire_at` timestamp
/// instead of a cron expression. After firing, the executor deactivates them.
/// Fired reminders are automatically cleaned up after 7 days.
pub async fn create_reminder(
    db: &SqlitePool,
    user_id: &str,
    agent_id: Option<&str>,
    name: &str,
    prompt: &str,
    fire_at: DateTime<Utc>,
    description: &str,
    pipe_ids: &[String],
) -> Result<String> {
    let fire_at_str = fire_at.format("%Y-%m-%d %H:%M:%S").to_string();
    let id = Uuid::new_v4().to_string();

    sqlx::query!(
        r#"INSERT INTO schedules (id, user_id, agent_id, name, prompt, cron_expression, cron_description, next_run_at, one_shot)
           VALUES (?, ?, ?, ?, ?, '', ?, ?, 1)"#,
        id,
        user_id,
        agent_id,
        name,
        prompt,
        description,
        fire_at_str,
    )
    .execute(db)
    .await
    .context("Failed to insert reminder")?;

    // Insert pipe associations
    for pipe_id in pipe_ids {
        sqlx::query!(
            "INSERT INTO schedule_pipes (schedule_id, pipe_id) VALUES (?, ?)",
            id,
            pipe_id,
        )
        .execute(db)
        .await
        .context("Failed to insert reminder pipe association")?;
    }

    Ok(id)
}

/// List all schedules for a user, with their associated pipe names.
pub async fn list_schedules(db: &SqlitePool, user_id: &str) -> Result<Vec<ScheduleRow>> {
    let rows = sqlx::query!(
        r#"SELECT id, user_id, name, prompt, cron_expression, cron_description,
                  active, one_shot, last_run_at, next_run_at, created_at
           FROM schedules
           WHERE user_id = ?
           ORDER BY created_at DESC"#,
        user_id,
    )
    .fetch_all(db)
    .await
    .context("Failed to list schedules")?;

    let mut schedules = Vec::with_capacity(rows.len());
    for r in rows {
        let schedule_id = r.id.clone().unwrap_or_default();

        let pipes = sqlx::query!(
            r#"SELECT sp.pipe_id, p.name
               FROM schedule_pipes sp
               LEFT JOIN pipes p ON sp.pipe_id = p.id
               WHERE sp.schedule_id = ?"#,
            schedule_id,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default();

        let pipe_ids: Vec<String> = pipes.iter().map(|p| p.pipe_id.clone()).collect();
        let pipe_names: Vec<String> = pipes.iter().map(|p| p.name.clone()).collect();

        schedules.push(ScheduleRow {
            id: schedule_id,
            user_id: r.user_id,
            name: r.name,
            prompt: r.prompt,
            cron_expression: r.cron_expression,
            cron_description: r.cron_description,
            active: r.active != 0,
            one_shot: r.one_shot != 0,
            last_run_at: r.last_run_at,
            next_run_at: r.next_run_at,
            created_at: r.created_at,
            pipe_ids,
            pipe_names,
        });
    }

    Ok(schedules)
}

/// Update a schedule's name, prompt, timing, and pipe associations.
pub async fn update_schedule(
    db: &SqlitePool,
    schedule_id: &str,
    user_id: &str,
    name: &str,
    prompt: &str,
    cron_expression: &str,
    cron_description: &str,
    timezone: &str,
    pipe_ids: &[String],
) -> Result<bool> {
    // Verify ownership
    let exists = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM schedules WHERE id = ? AND user_id = ?",
        schedule_id,
        user_id,
    )
    .fetch_one(db)
    .await?;

    if exists == 0 {
        return Ok(false);
    }

    validate_cron(cron_expression)?;

    let next_run = compute_next_run(cron_expression, timezone, Utc::now())?;
    let next_run_str = next_run.format("%Y-%m-%d %H:%M:%S").to_string();

    sqlx::query!(
        r#"UPDATE schedules
           SET name = ?, prompt = ?, cron_expression = ?, cron_description = ?, next_run_at = ?
           WHERE id = ? AND user_id = ?"#,
        name,
        prompt,
        cron_expression,
        cron_description,
        next_run_str,
        schedule_id,
        user_id,
    )
    .execute(db)
    .await
    .context("Failed to update schedule")?;

    // Replace pipe associations
    sqlx::query!(
        "DELETE FROM schedule_pipes WHERE schedule_id = ?",
        schedule_id
    )
    .execute(db)
    .await?;

    for pipe_id in pipe_ids {
        sqlx::query!(
            "INSERT INTO schedule_pipes (schedule_id, pipe_id) VALUES (?, ?)",
            schedule_id,
            pipe_id,
        )
        .execute(db)
        .await?;
    }

    Ok(true)
}

/// Delete a schedule (only if owned by the user).
pub async fn delete_schedule(db: &SqlitePool, schedule_id: &str, user_id: &str) -> Result<bool> {
    // schedule_pipes rows are cascade-deleted
    let result = sqlx::query!(
        "DELETE FROM schedules WHERE id = ? AND user_id = ?",
        schedule_id,
        user_id,
    )
    .execute(db)
    .await
    .context("Failed to delete schedule")?;

    Ok(result.rows_affected() > 0)
}

/// Toggle a schedule's active state.
pub async fn toggle_schedule(db: &SqlitePool, schedule_id: &str, user_id: &str) -> Result<bool> {
    // Flip the active flag
    let result = sqlx::query!(
        "UPDATE schedules SET active = CASE WHEN active = 1 THEN 0 ELSE 1 END WHERE id = ? AND user_id = ?",
        schedule_id,
        user_id,
    )
    .execute(db)
    .await
    .context("Failed to toggle schedule")?;

    if result.rows_affected() == 0 {
        return Ok(false);
    }

    // If re-activated, recompute next_run_at
    let row = sqlx::query!(
        "SELECT active, cron_expression, one_shot FROM schedules WHERE id = ?",
        schedule_id,
    )
    .fetch_optional(db)
    .await?;

    if let Some(r) = row {
        if r.active != 0 && r.one_shot == 0 {
            // Only recompute for recurring schedules (one-shot reminders
            // should not be re-activated since their fire time has passed)
            // Look up user timezone
            let tz = sqlx::query_scalar!("SELECT timezone FROM users WHERE id = ?", user_id)
                .fetch_optional(db)
                .await?
                .unwrap_or_else(|| "UTC".to_string());

            match compute_next_run(&r.cron_expression, &tz, Utc::now()) {
                Ok(next) => {
                    let next_str = next.format("%Y-%m-%d %H:%M:%S").to_string();
                    let _ = sqlx::query!(
                        "UPDATE schedules SET next_run_at = ? WHERE id = ?",
                        next_str,
                        schedule_id,
                    )
                    .execute(db)
                    .await;
                }
                Err(e) => error!("Failed to recompute next_run for {}: {}", schedule_id, e),
            }
        }
    }

    Ok(true)
}

/// Update the pipe associations for a schedule.
pub async fn update_pipes(
    db: &SqlitePool,
    schedule_id: &str,
    user_id: &str,
    pipe_ids: &[String],
) -> Result<bool> {
    // Verify ownership
    let exists = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM schedules WHERE id = ? AND user_id = ?",
        schedule_id,
        user_id,
    )
    .fetch_one(db)
    .await?;

    if exists == 0 {
        return Ok(false);
    }

    // Replace all pipe associations
    sqlx::query!(
        "DELETE FROM schedule_pipes WHERE schedule_id = ?",
        schedule_id
    )
    .execute(db)
    .await?;

    for pipe_id in pipe_ids {
        sqlx::query!(
            "INSERT INTO schedule_pipes (schedule_id, pipe_id) VALUES (?, ?)",
            schedule_id,
            pipe_id,
        )
        .execute(db)
        .await?;
    }

    Ok(true)
}

/// Find a schedule by fuzzy name match (for /schedule delete command).
pub async fn find_by_name(
    db: &SqlitePool,
    user_id: &str,
    name_query: &str,
) -> Result<Option<ScheduleRow>> {
    let pattern = format!("%{}%", name_query);
    let row = sqlx::query!(
        r#"SELECT id, user_id, name, prompt, cron_expression, cron_description,
                  active, one_shot, last_run_at, next_run_at, created_at
           FROM schedules
           WHERE user_id = ? AND name LIKE ?
           LIMIT 1"#,
        user_id,
        pattern,
    )
    .fetch_optional(db)
    .await?;

    match row {
        Some(r) => {
            let schedule_id = r.id.clone().unwrap_or_default();
            let pipes = sqlx::query!(
                "SELECT pipe_id FROM schedule_pipes WHERE schedule_id = ?",
                schedule_id,
            )
            .fetch_all(db)
            .await
            .unwrap_or_default();

            Ok(Some(ScheduleRow {
                id: schedule_id,
                user_id: r.user_id,
                name: r.name,
                prompt: r.prompt,
                cron_expression: r.cron_expression,
                cron_description: r.cron_description,
                active: r.active != 0,
                one_shot: r.one_shot != 0,
                last_run_at: r.last_run_at,
                next_run_at: r.next_run_at,
                created_at: r.created_at,
                pipe_ids: pipes.into_iter().map(|p| p.pipe_id).collect(),
                pipe_names: vec![],
            }))
        }
        None => Ok(None),
    }
}

/// Fetch all due schedules (active and next_run_at <= now).
pub async fn fetch_due_schedules(db: &SqlitePool) -> Result<Vec<DueSchedule>> {
    let now = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let rows = sqlx::query!(
        r#"SELECT s.id, s.user_id, s.agent_id, s.name, s.prompt, s.cron_expression, s.one_shot, u.timezone
           FROM schedules s
           LEFT JOIN users u ON s.user_id = u.id
           WHERE s.active = 1 AND s.next_run_at <= ?"#,
        now,
    )
    .fetch_all(db)
    .await
    .context("Failed to fetch due schedules")?;

    let mut schedules = Vec::with_capacity(rows.len());
    for r in rows {
        let schedule_id = r.id.clone().unwrap_or_default();
        let pipe_rows = sqlx::query_scalar!(
            "SELECT pipe_id FROM schedule_pipes WHERE schedule_id = ?",
            schedule_id,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default();

        schedules.push(DueSchedule {
            id: schedule_id,
            user_id: r.user_id,
            agent_id: r.agent_id,
            name: r.name,
            prompt: r.prompt,
            cron_expression: r.cron_expression,
            timezone: r.timezone,
            one_shot: r.one_shot != 0,
            pipe_ids: pipe_rows,
        });
    }

    Ok(schedules)
}

/// Mark a schedule as just-run and advance next_run_at (or deactivate if one-shot).
pub async fn mark_executed(
    db: &SqlitePool,
    schedule_id: &str,
    timezone: &str,
    cron_expr: &str,
    one_shot: bool,
) {
    let now = Utc::now();
    let now_str = now.format("%Y-%m-%d %H:%M:%S").to_string();

    // One-shot reminders: just deactivate, no next run to compute.
    if one_shot {
        let _ = sqlx::query!(
            "UPDATE schedules SET active = 0, last_run_at = ? WHERE id = ?",
            now_str,
            schedule_id,
        )
        .execute(db)
        .await;
        return;
    }

    let next_run_str = match compute_next_run(cron_expr, timezone, now) {
        Ok(next) => next.format("%Y-%m-%d %H:%M:%S").to_string(),
        Err(e) => {
            error!(
                "Failed to compute next run for schedule {}: {}. Deactivating.",
                schedule_id, e
            );
            // Deactivate if we can't compute the next run
            let _ = sqlx::query!(
                "UPDATE schedules SET active = 0, last_run_at = ? WHERE id = ?",
                now_str,
                schedule_id,
            )
            .execute(db)
            .await;
            return;
        }
    };

    let _ = sqlx::query!(
        "UPDATE schedules SET last_run_at = ?, next_run_at = ? WHERE id = ?",
        now_str,
        next_run_str,
        schedule_id,
    )
    .execute(db)
    .await;
}

/// Delete fired one-shot reminders older than 7 days.
///
/// Called periodically from the executor tick to prevent unbounded growth
/// of stale reminder rows.
pub async fn cleanup_fired_reminders(db: &SqlitePool) {
    let result = sqlx::query!(
        "DELETE FROM schedules WHERE one_shot = 1 AND active = 0 AND last_run_at < datetime('now', '-7 days')"
    )
    .execute(db)
    .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => {
            tracing::info!("Cleaned up {} fired reminder(s)", r.rows_affected());
        }
        Err(e) => {
            error!("Failed to clean up fired reminders: {e}");
        }
        _ => {}
    }
}

// ── Built-in tool: set_reminder ──────────────────────────────────────────────

use crate::llm::provider::ToolSpec;

/// Tool spec for `set_schedule` — schedule a recurring future agent action.
pub fn set_schedule_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "set_schedule".to_string(),
        description: "Set a recurring schedule using a 6-field cron expression. \
             Use this when the user asks for repeating tasks (daily, weekdays, every Monday, etc.). \
             The schedule runs the prompt as a new agent turn each time. \
             Cron format is: second minute hour day-of-month month day-of-week (0=Sunday..6=Saturday)."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "What the agent should do on each run"
                },
                "cron_expression": {
                    "type": "string",
                    "description": "6-field cron expression (e.g. 0 45 6 * * 1-5 for weekdays at 06:45)"
                },
                "cron_description": {
                    "type": "string",
                    "description": "Short human-readable timing description (e.g. Weekdays at 6:45 AM)"
                },
                "name": {
                    "type": "string",
                    "description": "Short name for the schedule (kebab-case, max 30 characters, e.g. 'morning-summary')"
                },
                "timezone": {
                    "type": "string",
                    "description": "IANA timezone for interpreting cron times (e.g. Europe/Berlin, America/New_York). Defaults to UTC."
                }
            },
            "required": ["prompt", "cron_expression", "cron_description", "name"]
        }),
    }
}

/// Handle a `set_schedule` tool call from an agent.
pub async fn dispatch_set_schedule(
    db: &SqlitePool,
    user_id: &str,
    pipe_id: &str,
    agent_id: &str,
    args: &serde_json::Value,
) -> anyhow::Result<String> {
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: prompt"))?;

    let cron_expression = args
        .get("cron_expression")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: cron_expression"))?;

    let cron_description = args
        .get("cron_description")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: cron_description"))?;

    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: name"))?;

    let timezone = match args
        .get("timezone")
        .and_then(|v| v.as_str())
        .filter(|tz| !tz.trim().is_empty())
    {
        Some(tz) => tz.to_string(),
        None => sqlx::query_scalar::<_, String>("SELECT timezone FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "UTC".to_string()),
    };

    validate_cron(cron_expression)?;
    let id = create_schedule(
        db,
        user_id,
        Some(agent_id),
        name,
        prompt,
        cron_expression,
        cron_description,
        &timezone,
        &[pipe_id.to_string()],
    )
    .await?;

    Ok(serde_json::json!({
        "status": "created",
        "id": id,
        "name": name,
        "cron_expression": cron_expression,
        "cron_description": cron_description,
        "timezone": timezone,
        "prompt": prompt
    })
    .to_string())
}

/// Tool spec for `set_reminder` — schedule a one-time future agent action.
pub fn set_reminder_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "set_reminder".to_string(),
        description: "Set a one-time reminder that fires at a specific time. \
             When the reminder fires, the prompt is executed as a new agent turn — \
             the agent can then use any of its tools (check weather, send emails, etc.). \
             Use this when the user wants something to happen at a specific future time. \
             IMPORTANT: Write the prompt in the same language the user is using. \
             If the user speaks German, the prompt must be in German. \
             The fire_at can be either: \
             - an ISO 8601 datetime with timezone/offset (e.g. 2026-03-08T10:00:00Z or 2026-03-08T11:00:00+01:00), or \
             - a local datetime without timezone (e.g. 2026-03-08T11:00:00), interpreted in the user's timezone. \
             Prefer passing the user's local time directly."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "What the agent should do when the reminder fires. MUST be in the user's language."
                },
                "fire_at": {
                    "type": "string",
                    "description": "Reminder time as ISO 8601. With timezone/offset it is used directly; without timezone it is interpreted in the user's timezone."
                },
                "name": {
                    "type": "string",
                    "description": "Short name for the reminder (kebab-case, max 30 characters, e.g. 'check-weather-sunday')"
                },
                "timezone": {
                    "type": "string",
                    "description": "Optional IANA timezone override for naive fire_at values (e.g. Europe/Berlin). Defaults to the user's timezone."
                }
            },
            "required": ["prompt", "fire_at", "name"]
        }),
    }
}

/// Handle a `set_reminder` tool call from an agent.
pub async fn dispatch_set_reminder(
    db: &SqlitePool,
    user_id: &str,
    pipe_id: &str,
    agent_id: &str,
    args: &serde_json::Value,
) -> anyhow::Result<String> {
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: prompt"))?;

    let fire_at_str = args
        .get("fire_at")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: fire_at"))?;

    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: name"))?;

    let timezone = match args
        .get("timezone")
        .and_then(|v| v.as_str())
        .filter(|tz| !tz.trim().is_empty())
    {
        Some(tz) => tz.to_string(),
        None => sqlx::query_scalar::<_, String>("SELECT timezone FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "UTC".to_string()),
    };

    let fire_at = parse_user_datetime_to_utc(fire_at_str, &timezone)?;

    // Validate the time is in the future
    if fire_at <= Utc::now() {
        return Ok(serde_json::json!({
            "error": "The reminder time must be in the future."
        })
        .to_string());
    }

    let id = create_reminder(
        db,
        user_id,
        Some(agent_id),
        name,
        prompt,
        fire_at,
        &format!(
            "Reminder for {}",
            fire_at_in_timezone(&fire_at, &timezone)
                .unwrap_or_else(|| fire_at.format("%Y-%m-%d %H:%M UTC").to_string())
        ),
        &[pipe_id.to_string()],
    )
    .await?;

    Ok(serde_json::json!({
        "status": "created",
        "id": id,
        "name": name,
        "fire_at": fire_at.to_rfc3339(),
        "timezone": timezone,
        "prompt": prompt
    })
    .to_string())
}

fn parse_user_datetime_to_utc(input: &str, timezone: &str) -> Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(input) {
        return Ok(dt.with_timezone(&Utc));
    }

    let tz: chrono_tz::Tz = timezone
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid timezone '{}'", timezone))?;

    let naive = NaiveDateTime::parse_from_str(input, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(input, "%Y-%m-%d %H:%M:%S"))
        .map_err(|e| {
            anyhow::anyhow!(
                "Invalid fire_at datetime '{}': {}. Use ISO 8601, e.g. 2026-03-08T11:00:00 or 2026-03-08T10:00:00Z",
                input,
                e
            )
        })?;

    let local_dt = match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) => dt,
        LocalResult::Ambiguous(a, _) => a,
        LocalResult::None => {
            return Err(anyhow::anyhow!(
                "The local time '{}' does not exist in timezone '{}' (DST transition).",
                input,
                timezone
            ));
        }
    };

    Ok(local_dt.with_timezone(&Utc))
}

fn fire_at_in_timezone(fire_at: &DateTime<Utc>, timezone: &str) -> Option<String> {
    let tz: chrono_tz::Tz = timezone.parse().ok()?;
    Some(
        fire_at
            .with_timezone(&tz)
            .format("%Y-%m-%d %H:%M %Z")
            .to_string(),
    )
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_next_run_utc() {
        // "Every day at 06:45" in UTC
        let cron = "0 45 6 * * *";
        let after = chrono::NaiveDate::from_ymd_opt(2026, 3, 3)
            .unwrap()
            .and_hms_opt(7, 0, 0)
            .unwrap()
            .and_utc();

        let next = compute_next_run(cron, "UTC", after).unwrap();
        // Should be 2026-03-04 06:45:00 UTC (next day since 07:00 > 06:45)
        assert_eq!(
            next.format("%Y-%m-%d %H:%M").to_string(),
            "2026-03-04 06:45"
        );
    }

    #[test]
    fn test_compute_next_run_with_timezone() {
        // "Every day at 06:45" in Europe/Berlin (UTC+1 in winter)
        let cron = "0 45 6 * * *";
        let after = chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
            .unwrap()
            .and_hms_opt(4, 0, 0)
            .unwrap()
            .and_utc(); // 04:00 UTC = 05:00 Berlin

        let next = compute_next_run(cron, "Europe/Berlin", after).unwrap();
        // 06:45 Berlin = 05:45 UTC in winter (CET = UTC+1)
        assert_eq!(
            next.format("%Y-%m-%d %H:%M").to_string(),
            "2026-01-15 05:45"
        );
    }

    #[test]
    fn test_validate_cron_valid() {
        assert!(validate_cron("0 45 6 * * 1-5").is_ok());
        assert!(validate_cron("0 0 */2 * * *").is_ok());
    }

    #[test]
    fn test_validate_cron_invalid() {
        assert!(validate_cron("not a cron").is_err());
        assert!(validate_cron("").is_err());
    }

    #[test]
    fn test_parse_user_datetime_to_utc_naive_uses_timezone() {
        let dt = parse_user_datetime_to_utc("2026-03-04T11:10:00", "Europe/Berlin").unwrap();
        // March in Berlin is CET (UTC+1) until DST switch later in month.
        assert_eq!(dt.format("%Y-%m-%d %H:%M").to_string(), "2026-03-04 10:10");
    }

    #[test]
    fn test_parse_user_datetime_to_utc_with_offset_keeps_absolute_time() {
        let dt = parse_user_datetime_to_utc("2026-03-04T11:10:00+01:00", "UTC").unwrap();
        assert_eq!(dt.format("%Y-%m-%d %H:%M").to_string(), "2026-03-04 10:10");
    }

    #[test]
    fn test_weekday_cron_1_to_5_means_monday_to_friday() {
        // 0 0 21 * * 1-5 = weekdays at 21:00
        // After Friday 2026-04-17 22:00 UTC, next should be Monday 2026-04-20
        let cron = "0 0 21 * * 1-5";
        let after = chrono::NaiveDate::from_ymd_opt(2026, 4, 17)
            .unwrap()
            .and_hms_opt(22, 0, 0)
            .unwrap()
            .and_utc();

        let next = compute_next_run(cron, "UTC", after).unwrap();
        // Must be Monday 2026-04-20, NOT Sunday 2026-04-19
        assert_eq!(
            next.format("%Y-%m-%d %H:%M %A").to_string(),
            "2026-04-20 21:00 Monday"
        );
    }
}
