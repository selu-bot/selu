/// Schedule management: CRUD operations and cron helpers.
///
/// Schedules allow users to run agent prompts on a recurring basis.
/// Each schedule has a cron expression (parsed from natural language),
/// a prompt, and one or more target pipes.
pub mod executor;
pub mod nl_to_cron;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
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
    pub prompt: String,
    pub cron_expression: String,
    pub timezone: String,
    pub pipe_ids: Vec<String>,
}

// ── Cron helpers ─────────────────────────────────────────────────────────────

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

    let schedule = CronSchedule::from_str(cron_expr)
        .map_err(|e| anyhow::anyhow!("Invalid cron expression '{}': {}", cron_expr, e))?;

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
    CronSchedule::from_str(cron_expr)
        .map_err(|e| anyhow::anyhow!("Invalid cron expression '{}': {}", cron_expr, e))?;
    Ok(())
}

// ── CRUD operations ──────────────────────────────────────────────────────────

/// Create a new schedule with associated pipes.
pub async fn create_schedule(
    db: &SqlitePool,
    user_id: &str,
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
        r#"INSERT INTO schedules (id, user_id, name, prompt, cron_expression, cron_description, next_run_at)
           VALUES (?, ?, ?, ?, ?, ?, ?)"#,
        id,
        user_id,
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

/// List all schedules for a user, with their associated pipe names.
pub async fn list_schedules(db: &SqlitePool, user_id: &str) -> Result<Vec<ScheduleRow>> {
    let rows = sqlx::query!(
        r#"SELECT id, user_id, name, prompt, cron_expression, cron_description,
                  active, last_run_at, next_run_at, created_at
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
            last_run_at: r.last_run_at,
            next_run_at: r.next_run_at,
            created_at: r.created_at,
            pipe_ids,
            pipe_names,
        });
    }

    Ok(schedules)
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
        "SELECT active, cron_expression FROM schedules WHERE id = ?",
        schedule_id,
    )
    .fetch_optional(db)
    .await?;

    if let Some(r) = row {
        if r.active != 0 {
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
                  active, last_run_at, next_run_at, created_at
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
        r#"SELECT s.id, s.user_id, s.prompt, s.cron_expression, u.timezone
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
            prompt: r.prompt,
            cron_expression: r.cron_expression,
            timezone: r.timezone,
            pipe_ids: pipe_rows,
        });
    }

    Ok(schedules)
}

/// Mark a schedule as just-run and advance next_run_at.
pub async fn mark_executed(db: &SqlitePool, schedule_id: &str, timezone: &str, cron_expr: &str) {
    let now = Utc::now();
    let now_str = now.format("%Y-%m-%d %H:%M:%S").to_string();

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
}
