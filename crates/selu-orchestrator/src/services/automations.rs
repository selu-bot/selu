//! Caller-owned automation domain service built on the existing schedules engine.

use std::{collections::HashSet, sync::Arc};

use anyhow::Context;
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::{
    agents::{access, model},
    llm::provider::LlmProvider,
    schedules::{self, nl_to_cron},
    state::AppState,
};

#[derive(Debug, Clone, Deserialize)]
pub struct TimingInput {
    #[serde(rename = "type", alias = "kind")]
    pub timing_type: String,
    #[serde(default, alias = "cron", alias = "expression")]
    pub cron_expression: Option<String>,
    #[serde(default, alias = "at")]
    pub fire_at: Option<String>,
    #[serde(default, alias = "when", alias = "when_text")]
    pub text: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NormalizedTiming {
    Recurring {
        cron_expression: String,
        description: String,
        timezone: String,
    },
    OneShot {
        fire_at: String,
        description: String,
        timezone: String,
    },
}

#[derive(Debug, Clone)]
pub struct AutomationWrite {
    pub name: String,
    pub prompt: String,
    pub agent_id: Option<String>,
    pub pipe_ids: Vec<String>,
    pub timing: NormalizedTiming,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeliveryDestination {
    pub pipe_id: String,
    pub name: String,
    pub transport: String,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Automation {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub agent_id: Option<String>,
    pub pipe_ids: Vec<String>,
    pub delivery_destinations: Vec<DeliveryDestination>,
    pub timing: NormalizedTiming,
    pub active: bool,
    pub next_run_at: String,
    pub last_run_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AutomationError {
    #[error("automation not found")]
    NotFound,
    #[error("{message}")]
    BadRequest { code: &'static str, message: String },
    #[error("{message}")]
    Validation { code: &'static str, message: String },
    #[error("{message}")]
    Conflict { code: &'static str, message: String },
    #[error("{message}")]
    Unavailable { code: &'static str, message: String },
    #[error(transparent)]
    Database(#[from] anyhow::Error),
}

impl AutomationError {
    fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::BadRequest {
            code,
            message: message.into(),
        }
    }

    fn validation(code: &'static str, message: impl Into<String>) -> Self {
        Self::Validation {
            code,
            message: message.into(),
        }
    }

    fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::Conflict {
            code,
            message: message.into(),
        }
    }
}

pub async fn user_timezone(db: &SqlitePool, user_id: &str) -> Result<String, AutomationError> {
    sqlx::query_scalar!("SELECT timezone FROM users WHERE id = ?", user_id)
        .fetch_optional(db)
        .await
        .context("failed to read user timezone")?
        .ok_or(AutomationError::NotFound)
}

pub async fn list_delivery_destinations(
    db: &SqlitePool,
    user_id: &str,
) -> Result<Vec<DeliveryDestination>, AutomationError> {
    Ok(sqlx::query!(
        r#"SELECT id as "pipe_id!", name, transport, active
           FROM pipes
           WHERE user_id = ? AND active = 1
           ORDER BY CASE WHEN transport = 'web' THEN 0 ELSE 1 END, name, id"#,
        user_id,
    )
    .fetch_all(db)
    .await
    .context("failed to list automation destinations")?
    .into_iter()
    .map(|pipe| DeliveryDestination {
        pipe_id: pipe.pipe_id,
        name: pipe.name,
        transport: pipe.transport,
        active: pipe.active != 0,
    })
    .collect())
}

pub async fn validate_agent_access(
    state: &AppState,
    user_id: &str,
    agent_id: Option<&str>,
) -> Result<(), AutomationError> {
    let Some(agent_id) = agent_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    let all_agents = state.agents.load_full();
    let visible = access::visible_agents(&state.db, user_id, all_agents.as_ref()).await;
    if visible.contains_key(agent_id) {
        Ok(())
    } else {
        Err(AutomationError::validation(
            "automation.invalid_agent",
            format!("agent_id '{agent_id}' is not available to the caller."),
        ))
    }
}

pub async fn normalize_timing(
    state: &AppState,
    user_id: &str,
    agent_id: Option<&str>,
    input: TimingInput,
) -> Result<NormalizedTiming, AutomationError> {
    let timezone = match input.timezone.as_deref().map(str::trim) {
        Some(value) if !value.is_empty() => validate_timezone(value)?,
        _ => user_timezone(&state.db, user_id).await?,
    };

    match input.timing_type.trim().to_ascii_lowercase().as_str() {
        "cron" | "recurring" => {
            let expression = required_timing_value(
                input.cron_expression,
                "automation.cron_required",
                "timing.cron_expression is required for recurring automations.",
            )?;
            schedules::validate_cron(&expression).map_err(|error| {
                AutomationError::validation(
                    "automation.invalid_cron",
                    format!("The cron expression is invalid: {error}"),
                )
            })?;
            let description = non_empty_or(input.description, expression.clone());
            Ok(NormalizedTiming::Recurring {
                cron_expression: expression,
                description,
                timezone,
            })
        }
        "one_shot" | "once" | "at" => {
            let value = required_timing_value(
                input.fire_at,
                "automation.fire_at_required",
                "timing.fire_at is required for one-shot automations.",
            )?;
            let fire_at = parse_future_rfc3339(&value)?;
            let description = non_empty_or(
                input.description,
                format!("Once at {}", fire_at.to_rfc3339()),
            );
            Ok(NormalizedTiming::OneShot {
                fire_at: fire_at.to_rfc3339(),
                description,
                timezone,
            })
        }
        "natural_language" | "natural" => {
            let text = required_timing_value(
                input.text,
                "automation.timing_text_required",
                "timing.text is required for natural-language timing.",
            )?;
            let provider = resolve_provider(state, agent_id.unwrap_or("default")).await?;
            let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
            let parsed = nl_to_cron::parse_timing(&text, &provider, &now, &timezone)
                .await
                .map_err(|error| AutomationError::Unavailable {
                    code: "automation.timing_provider_unavailable",
                    message: format!("Natural-language timing could not be resolved: {error}"),
                })?;
            match parsed {
                nl_to_cron::TimingResult::Recurring {
                    cron_expression,
                    description,
                } => Ok(NormalizedTiming::Recurring {
                    cron_expression,
                    description,
                    timezone,
                }),
                nl_to_cron::TimingResult::OneShot {
                    fire_at,
                    description,
                } => {
                    let fire_at = parse_future_rfc3339(&fire_at)?;
                    Ok(NormalizedTiming::OneShot {
                        fire_at: fire_at.to_rfc3339(),
                        description,
                        timezone,
                    })
                }
            }
        }
        _ => Err(AutomationError::bad_request(
            "automation.invalid_timing_type",
            "timing.type must be cron, one_shot, or natural_language.",
        )),
    }
}

pub async fn list_automations(
    db: &SqlitePool,
    user_id: &str,
) -> Result<Vec<Automation>, AutomationError> {
    let timezone = user_timezone(db, user_id).await?;
    let rows = sqlx::query!(
        r#"SELECT id as "id!"
                FROM schedules
                WHERE user_id = ?
                ORDER BY created_at DESC"#,
        user_id,
    )
    .fetch_all(db)
    .await
    .context("failed to list automations")?;

    let mut automations = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(automation) = load_automation(db, user_id, &row.id, &timezone).await? {
            automations.push(automation);
        }
    }
    Ok(automations)
}

pub async fn get_automation(
    db: &SqlitePool,
    user_id: &str,
    automation_id: &str,
) -> Result<Option<Automation>, AutomationError> {
    let timezone = user_timezone(db, user_id).await?;
    load_automation(db, user_id, automation_id, &timezone).await
}

pub async fn create_automation(
    db: &SqlitePool,
    user_id: &str,
    mut write: AutomationWrite,
) -> Result<Automation, AutomationError> {
    normalize_write(&mut write)?;
    validate_delivery_destinations(db, user_id, &write.pipe_ids).await?;
    ensure_name_available(db, user_id, &write.name, None).await?;

    let id = match &write.timing {
        NormalizedTiming::Recurring {
            cron_expression,
            description,
            timezone,
        } => schedules::create_schedule(
            db,
            user_id,
            write.agent_id.as_deref(),
            &write.name,
            &write.prompt,
            cron_expression,
            description,
            timezone,
            &write.pipe_ids,
        )
        .await
        .context("failed to create recurring automation")?,
        NormalizedTiming::OneShot {
            fire_at,
            description,
            ..
        } => {
            let fire_at = parse_future_rfc3339(fire_at)?;
            schedules::create_reminder(
                db,
                user_id,
                write.agent_id.as_deref(),
                &write.name,
                &write.prompt,
                fire_at,
                description,
                &write.pipe_ids,
            )
            .await
            .context("failed to create one-shot automation")?
        }
    };

    get_automation(db, user_id, &id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("created automation was not found").into())
}

pub async fn update_automation(
    db: &SqlitePool,
    user_id: &str,
    automation_id: &str,
    mut write: AutomationWrite,
) -> Result<Automation, AutomationError> {
    normalize_write(&mut write)?;
    if get_automation(db, user_id, automation_id).await?.is_none() {
        return Err(AutomationError::NotFound);
    }
    validate_delivery_destinations(db, user_id, &write.pipe_ids).await?;
    ensure_name_available(db, user_id, &write.name, Some(automation_id)).await?;

    match &write.timing {
        NormalizedTiming::Recurring {
            cron_expression,
            description,
            timezone,
        } => {
            let updated = schedules::update_schedule(
                db,
                automation_id,
                user_id,
                &write.name,
                &write.prompt,
                cron_expression,
                description,
                timezone,
                &write.pipe_ids,
            )
            .await
            .context("failed to update recurring automation")?;
            if !updated {
                return Err(AutomationError::NotFound);
            }
            sqlx::query!(
                "UPDATE schedules SET agent_id = ?, one_shot = 0 WHERE id = ? AND user_id = ?",
                write.agent_id,
                automation_id,
                user_id,
            )
            .execute(db)
            .await
            .context("failed to update automation agent")?;
        }
        NormalizedTiming::OneShot {
            fire_at,
            description,
            ..
        } => {
            let fire_at = parse_future_rfc3339(fire_at)?;
            let fire_at_db = to_db_timestamp(fire_at);
            let result = sqlx::query!(
                r#"UPDATE schedules
                   SET name = ?, prompt = ?, agent_id = ?, cron_expression = '',
                       cron_description = ?, one_shot = 1, next_run_at = ?
                   WHERE id = ? AND user_id = ?"#,
                write.name,
                write.prompt,
                write.agent_id,
                description,
                fire_at_db,
                automation_id,
                user_id,
            )
            .execute(db)
            .await
            .context("failed to update one-shot automation")?;
            if result.rows_affected() == 0 {
                return Err(AutomationError::NotFound);
            }
            replace_delivery_destinations(db, automation_id, &write.pipe_ids).await?;
        }
    }

    get_automation(db, user_id, automation_id)
        .await?
        .ok_or(AutomationError::NotFound)
}

pub async fn delete_automation(
    db: &SqlitePool,
    user_id: &str,
    automation_id: &str,
) -> Result<(), AutomationError> {
    if schedules::delete_schedule(db, automation_id, user_id)
        .await
        .context("failed to delete automation")?
    {
        Ok(())
    } else {
        Err(AutomationError::NotFound)
    }
}

pub async fn set_automation_state(
    db: &SqlitePool,
    user_id: &str,
    automation_id: &str,
    active: bool,
) -> Result<Automation, AutomationError> {
    let timezone = user_timezone(db, user_id).await?;
    let row = sqlx::query!(
        r#"SELECT active, one_shot, cron_expression, next_run_at
           FROM schedules WHERE id = ? AND user_id = ?"#,
        automation_id,
        user_id,
    )
    .fetch_optional(db)
    .await
    .context("failed to read automation state")?
    .ok_or(AutomationError::NotFound)?;

    if (row.active != 0) != active {
        if active {
            let next_run_at = if row.one_shot != 0 {
                let fire_at = parse_db_timestamp(&row.next_run_at)?;
                if fire_at <= Utc::now() {
                    return Err(AutomationError::conflict(
                        "automation.one_shot_expired",
                        "A one-shot automation cannot be activated after its scheduled time.",
                    ));
                }
                row.next_run_at
            } else {
                schedules::compute_next_run(&row.cron_expression, &timezone, Utc::now())
                    .map(to_db_timestamp)
                    .map_err(|error| {
                        AutomationError::validation(
                            "automation.invalid_cron",
                            format!("The stored cron expression is invalid: {error}"),
                        )
                    })?
            };
            sqlx::query!(
                "UPDATE schedules SET active = 1, next_run_at = ? WHERE id = ? AND user_id = ?",
                next_run_at,
                automation_id,
                user_id,
            )
            .execute(db)
            .await
            .context("failed to activate automation")?;
        } else {
            sqlx::query!(
                "UPDATE schedules SET active = 0 WHERE id = ? AND user_id = ?",
                automation_id,
                user_id,
            )
            .execute(db)
            .await
            .context("failed to deactivate automation")?;
        }
    }

    load_automation(db, user_id, automation_id, &timezone)
        .await?
        .ok_or(AutomationError::NotFound)
}

pub async fn set_user_timezone(
    db: &SqlitePool,
    user_id: &str,
    timezone: &str,
) -> Result<String, AutomationError> {
    let timezone = validate_timezone(timezone)?;
    let result = sqlx::query!(
        "UPDATE users SET timezone = ? WHERE id = ?",
        timezone,
        user_id,
    )
    .execute(db)
    .await
    .context("failed to update user timezone")?;
    if result.rows_affected() == 0 {
        return Err(AutomationError::NotFound);
    }
    Ok(timezone)
}

async fn load_automation(
    db: &SqlitePool,
    user_id: &str,
    automation_id: &str,
    timezone: &str,
) -> Result<Option<Automation>, AutomationError> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", name, prompt, agent_id, cron_expression,
                  cron_description, active, one_shot, last_run_at, next_run_at, created_at
           FROM schedules
           WHERE id = ? AND user_id = ?"#,
        automation_id,
        user_id,
    )
    .fetch_optional(db)
    .await
    .context("failed to read automation")?;
    let Some(row) = row else {
        return Ok(None);
    };

    let destinations = sqlx::query!(
        r#"SELECT p.id as "pipe_id!", p.name, p.transport, p.active
           FROM schedule_pipes sp
           JOIN pipes p ON p.id = sp.pipe_id
           WHERE sp.schedule_id = ? AND p.user_id = ?
           ORDER BY p.name, p.id"#,
        automation_id,
        user_id,
    )
    .fetch_all(db)
    .await
    .context("failed to read automation destinations")?
    .into_iter()
    .map(|pipe| DeliveryDestination {
        pipe_id: pipe.pipe_id,
        name: pipe.name,
        transport: pipe.transport,
        active: pipe.active != 0,
    })
    .collect::<Vec<_>>();

    let timing = if row.one_shot != 0 {
        NormalizedTiming::OneShot {
            fire_at: db_timestamp_to_rfc3339(&row.next_run_at)?,
            description: row.cron_description,
            timezone: timezone.to_string(),
        }
    } else {
        NormalizedTiming::Recurring {
            cron_expression: row.cron_expression,
            description: row.cron_description,
            timezone: timezone.to_string(),
        }
    };

    Ok(Some(Automation {
        id: row.id,
        name: row.name,
        prompt: row.prompt,
        agent_id: row.agent_id,
        pipe_ids: destinations
            .iter()
            .map(|destination| destination.pipe_id.clone())
            .collect(),
        delivery_destinations: destinations,
        timing,
        active: row.active != 0,
        next_run_at: db_timestamp_to_rfc3339(&row.next_run_at)?,
        last_run_at: row
            .last_run_at
            .as_deref()
            .map(db_timestamp_to_rfc3339)
            .transpose()?,
        created_at: db_timestamp_to_rfc3339(&row.created_at)?,
    }))
}

async fn resolve_provider(
    state: &AppState,
    agent_id: &str,
) -> Result<Arc<dyn LlmProvider>, AutomationError> {
    let resolved = model::resolve_model(&state.db, agent_id)
        .await
        .map_err(|error| AutomationError::Unavailable {
            code: "automation.timing_provider_unavailable",
            message: format!("No timing provider is configured: {error}"),
        })?;
    state
        .provider_cache
        .get_or_load(
            &state.db,
            &resolved.provider_id,
            &resolved.model_id,
            &state.credentials,
        )
        .await
        .map_err(|error| AutomationError::Unavailable {
            code: "automation.timing_provider_unavailable",
            message: format!("The timing provider is unavailable: {error}"),
        })
}

fn normalize_write(write: &mut AutomationWrite) -> Result<(), AutomationError> {
    write.name = write.name.trim().to_string();
    write.prompt = write.prompt.trim().to_string();
    write.agent_id = write
        .agent_id
        .take()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    write.pipe_ids = deduplicate_non_empty(&write.pipe_ids);

    if write.name.is_empty() {
        return Err(AutomationError::validation(
            "automation.name_required",
            "name is required.",
        ));
    }
    if write.name.chars().count() > 120 {
        return Err(AutomationError::validation(
            "automation.name_too_long",
            "name must not exceed 120 characters.",
        ));
    }
    if write.prompt.is_empty() {
        return Err(AutomationError::validation(
            "automation.prompt_required",
            "prompt is required.",
        ));
    }
    if write.pipe_ids.is_empty() {
        return Err(AutomationError::validation(
            "automation.delivery_required",
            "At least one pipe_id is required.",
        ));
    }
    Ok(())
}

async fn validate_delivery_destinations(
    db: &SqlitePool,
    user_id: &str,
    pipe_ids: &[String],
) -> Result<(), AutomationError> {
    for pipe_id in pipe_ids {
        let owned = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM pipes WHERE id = ? AND user_id = ? AND active = 1",
            pipe_id,
            user_id,
        )
        .fetch_one(db)
        .await
        .context("failed to validate automation destination")?;
        if owned == 0 {
            return Err(AutomationError::validation(
                "automation.invalid_delivery_destination",
                format!("pipe_id '{pipe_id}' is not an active caller-owned pipe."),
            ));
        }
    }
    Ok(())
}

async fn ensure_name_available(
    db: &SqlitePool,
    user_id: &str,
    name: &str,
    except_id: Option<&str>,
) -> Result<(), AutomationError> {
    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) FROM schedules
           WHERE user_id = ? AND lower(name) = lower(?) AND (? IS NULL OR id != ?)"#,
        user_id,
        name,
        except_id,
        except_id,
    )
    .fetch_one(db)
    .await
    .context("failed to validate automation name")?;
    if count > 0 {
        Err(AutomationError::conflict(
            "automation.name_conflict",
            "An automation with that name already exists.",
        ))
    } else {
        Ok(())
    }
}

async fn replace_delivery_destinations(
    db: &SqlitePool,
    automation_id: &str,
    pipe_ids: &[String],
) -> Result<(), AutomationError> {
    let mut transaction = db
        .begin()
        .await
        .context("failed to begin destination update")?;
    sqlx::query!(
        "DELETE FROM schedule_pipes WHERE schedule_id = ?",
        automation_id,
    )
    .execute(&mut *transaction)
    .await
    .context("failed to clear automation destinations")?;
    for pipe_id in pipe_ids {
        sqlx::query!(
            "INSERT INTO schedule_pipes (schedule_id, pipe_id) VALUES (?, ?)",
            automation_id,
            pipe_id,
        )
        .execute(&mut *transaction)
        .await
        .context("failed to add automation destination")?;
    }
    transaction
        .commit()
        .await
        .context("failed to commit automation destinations")?;
    Ok(())
}

fn required_timing_value(
    value: Option<String>,
    code: &'static str,
    message: &'static str,
) -> Result<String, AutomationError> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AutomationError::bad_request(code, message))
}

fn non_empty_or(value: Option<String>, fallback: String) -> String {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
}

fn validate_timezone(timezone: &str) -> Result<String, AutomationError> {
    let timezone = timezone.trim();
    timezone
        .parse::<chrono_tz::Tz>()
        .map(|_| timezone.to_string())
        .map_err(|_| {
            AutomationError::validation(
                "automation.invalid_timezone",
                format!("'{timezone}' is not a valid IANA timezone."),
            )
        })
}

fn parse_future_rfc3339(value: &str) -> Result<DateTime<Utc>, AutomationError> {
    let fire_at = DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| {
            AutomationError::validation(
                "automation.invalid_fire_at",
                "fire_at must be an RFC3339 timestamp with an offset.",
            )
        })?;
    if fire_at <= Utc::now() {
        return Err(AutomationError::validation(
            "automation.fire_at_not_future",
            "fire_at must be in the future.",
        ));
    }
    Ok(fire_at)
}

fn parse_db_timestamp(value: &str) -> Result<DateTime<Utc>, AutomationError> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .map(|value| Utc.from_utc_datetime(&value))
        .map_err(|error| {
            anyhow::anyhow!("invalid stored schedule timestamp '{value}': {error}").into()
        })
}

fn db_timestamp_to_rfc3339(value: &str) -> Result<String, AutomationError> {
    Ok(parse_db_timestamp(value)?.to_rfc3339())
}

fn to_db_timestamp(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn deduplicate_non_empty(values: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .filter(|value| seen.insert((*value).to_string()))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE users (id TEXT PRIMARY KEY, timezone TEXT NOT NULL DEFAULT 'UTC')",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE pipes (
                id TEXT PRIMARY KEY, user_id TEXT NOT NULL, name TEXT NOT NULL,
                transport TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1
            )",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE schedules (
                id TEXT PRIMARY KEY, user_id TEXT NOT NULL, agent_id TEXT,
                name TEXT NOT NULL, prompt TEXT NOT NULL, cron_expression TEXT NOT NULL,
                cron_description TEXT NOT NULL DEFAULT '', active INTEGER NOT NULL DEFAULT 1,
                one_shot INTEGER NOT NULL DEFAULT 0, last_run_at TEXT,
                next_run_at TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE schedule_pipes (
                schedule_id TEXT NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
                pipe_id TEXT NOT NULL REFERENCES pipes(id) ON DELETE CASCADE,
                PRIMARY KEY (schedule_id, pipe_id)
            )",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO users (id, timezone) VALUES ('alice', 'Europe/Berlin'), ('bob', 'UTC')",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO pipes (id, user_id, name, transport) VALUES
             ('alice-web', 'alice', 'Web', 'web'), ('bob-web', 'bob', 'Web', 'web')",
        )
        .execute(&db)
        .await
        .unwrap();
        db
    }

    #[tokio::test]
    async fn lists_only_active_owned_delivery_destinations() {
        let db = test_db().await;
        sqlx::query(
            "INSERT INTO pipes (id, user_id, name, transport, active)
             VALUES ('alice-paused', 'alice', 'Paused', 'webhook', 0)",
        )
        .execute(&db)
        .await
        .unwrap();

        let alice = list_delivery_destinations(&db, "alice").await.unwrap();
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].pipe_id, "alice-web");
        assert_eq!(alice[0].transport, "web");

        let bob = list_delivery_destinations(&db, "bob").await.unwrap();
        assert_eq!(bob.len(), 1);
        assert_eq!(bob[0].pipe_id, "bob-web");
    }

    fn recurring_write(pipe_id: &str) -> AutomationWrite {
        AutomationWrite {
            name: "Morning summary".to_string(),
            prompt: "Summarize the day".to_string(),
            agent_id: Some("default".to_string()),
            pipe_ids: vec![pipe_id.to_string(), pipe_id.to_string()],
            timing: NormalizedTiming::Recurring {
                cron_expression: "0 0 8 * * *".to_string(),
                description: "Daily at 08:00".to_string(),
                timezone: "Europe/Berlin".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn creates_normalized_automation_and_hides_it_cross_user() {
        let db = test_db().await;
        let created = create_automation(&db, "alice", recurring_write("alice-web"))
            .await
            .unwrap();
        assert_eq!(created.pipe_ids, vec!["alice-web"]);
        assert_eq!(created.delivery_destinations[0].transport, "web");
        assert!(created.next_run_at.ends_with("+00:00"));
        assert!(
            get_automation(&db, "bob", &created.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            delete_automation(&db, "bob", &created.id).await,
            Err(AutomationError::NotFound)
        ));
    }

    #[tokio::test]
    async fn rejects_cross_user_delivery_destination() {
        let db = test_db().await;
        let error = create_automation(&db, "alice", recurring_write("bob-web"))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AutomationError::Validation {
                code: "automation.invalid_delivery_destination",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn duplicate_names_conflict_only_within_the_owner_scope() {
        let db = test_db().await;
        create_automation(&db, "alice", recurring_write("alice-web"))
            .await
            .unwrap();
        let error = create_automation(&db, "alice", recurring_write("alice-web"))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AutomationError::Conflict {
                code: "automation.name_conflict",
                ..
            }
        ));
        create_automation(&db, "bob", recurring_write("bob-web"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn expired_one_shot_cannot_be_reactivated() {
        let db = test_db().await;
        sqlx::query(
            "INSERT INTO schedules (
                id, user_id, name, prompt, cron_expression, cron_description,
                active, one_shot, next_run_at
             ) VALUES ('expired', 'alice', 'Expired', 'Run', '', 'Past', 0, 1, '2020-01-01 00:00:00')",
        )
        .execute(&db)
        .await
        .unwrap();
        let error = set_automation_state(&db, "alice", "expired", true)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AutomationError::Conflict {
                code: "automation.one_shot_expired",
                ..
            }
        ));
    }

    #[test]
    fn timing_input_accepts_contract_aliases() {
        let timing: TimingInput = serde_json::from_value(serde_json::json!({
            "kind": "cron",
            "expression": "0 0 8 * * *"
        }))
        .unwrap();
        assert_eq!(timing.timing_type, "cron");
        assert_eq!(timing.cron_expression.as_deref(), Some("0 0 8 * * *"));
    }
}
