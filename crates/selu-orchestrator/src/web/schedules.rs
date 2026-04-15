use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::{NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;
use tracing::error;

use crate::schedules;
use crate::schedules::nl_to_cron;
use crate::state::AppState;
use crate::web::BasePath;
use crate::web::auth::AuthUser;

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ScheduleView {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub cron_description: String,
    pub cron_expression: String,
    pub active: bool,
    pub one_shot: bool,
    pub last_run_at: String,
    pub next_run_at: String,
    pub pipe_names: Vec<String>,
    pub pipe_ids: Vec<String>,
    pub pipe_ids_csv: String,
    pub pipe_checks: Vec<PipeOptionChecked>,
}

/// A pipe option with a pre-computed "checked" flag for a specific schedule.
#[derive(Debug, Clone)]
pub struct PipeOptionChecked {
    pub id: String,
    pub name: String,
    pub checked: bool,
}

#[derive(Debug, Clone)]
pub struct PipeOption {
    pub id: String,
    pub name: String,
}

// ── Common timezone list ─────────────────────────────────────────────────────

pub const COMMON_TIMEZONES: &[(&str, &str)] = &[
    ("UTC", "UTC"),
    ("Europe/London", "London (GMT/BST)"),
    ("Europe/Berlin", "Berlin (CET/CEST)"),
    ("Europe/Paris", "Paris (CET/CEST)"),
    ("Europe/Zurich", "Zurich (CET/CEST)"),
    ("Europe/Vienna", "Vienna (CET/CEST)"),
    ("Europe/Amsterdam", "Amsterdam (CET/CEST)"),
    ("Europe/Rome", "Rome (CET/CEST)"),
    ("Europe/Madrid", "Madrid (CET/CEST)"),
    ("Europe/Stockholm", "Stockholm (CET/CEST)"),
    ("Europe/Helsinki", "Helsinki (EET/EEST)"),
    ("Europe/Athens", "Athens (EET/EEST)"),
    ("Europe/Moscow", "Moscow (MSK)"),
    ("America/New_York", "New York (EST/EDT)"),
    ("America/Chicago", "Chicago (CST/CDT)"),
    ("America/Denver", "Denver (MST/MDT)"),
    ("America/Los_Angeles", "Los Angeles (PST/PDT)"),
    ("America/Toronto", "Toronto (EST/EDT)"),
    ("America/Sao_Paulo", "S\u{00e3}o Paulo (BRT)"),
    ("Asia/Tokyo", "Tokyo (JST)"),
    ("Asia/Shanghai", "Shanghai (CST)"),
    ("Asia/Kolkata", "India (IST)"),
    ("Asia/Dubai", "Dubai (GST)"),
    ("Asia/Singapore", "Singapore (SGT)"),
    ("Australia/Sydney", "Sydney (AEST/AEDT)"),
    ("Pacific/Auckland", "Auckland (NZST/NZDT)"),
];

// ── Template ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "schedules.html")]
struct SchedulesTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    schedules: Vec<ScheduleView>,
    pipes: Vec<PipeOption>,
    user_timezone: String,
    timezones: Vec<(&'static str, &'static str)>,
    error: Option<String>,
    success: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SchedulesQuery {
    pub error: Option<String>,
    pub success: Option<String>,
}

// ── Form structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateScheduleForm {
    pub name: String,
    pub prompt: String,
    pub when_text: String,
    pub pipe_ids: Option<String>, // comma-separated or single value
}

#[derive(Debug, Deserialize)]
pub struct UpdateScheduleForm {
    pub name: String,
    pub prompt: String,
    pub when_text: String,
    pub pipe_ids: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePipesForm {
    pub pipe_ids: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TimezoneForm {
    pub timezone: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /schedules — show schedule list + add form
pub async fn schedules_index(
    user: AuthUser,
    Query(q): Query<SchedulesQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    // Get user timezone
    let user_timezone =
        sqlx::query_scalar!("SELECT timezone FROM users WHERE id = ?", user.user_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "UTC".to_string());

    // Fetch user's pipes first (needed for schedule card pipe checkboxes)
    let pipes: Vec<PipeOption> = sqlx::query!(
        "SELECT id, name FROM pipes WHERE user_id = ? AND active = 1 ORDER BY name",
        user.user_id,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| PipeOption {
        id: r.id.unwrap_or_default(),
        name: r.name,
    })
    .collect();

    let schedule_rows = schedules::list_schedules(&state.db, &user.user_id)
        .await
        .unwrap_or_default();

    let schedules: Vec<ScheduleView> = schedule_rows
        .into_iter()
        .map(|s| {
            let pipe_checks: Vec<PipeOptionChecked> = pipes
                .iter()
                .map(|p| PipeOptionChecked {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    checked: s.pipe_ids.contains(&p.id),
                })
                .collect();
            let pipe_ids_csv = s.pipe_ids.join(",");
            ScheduleView {
                id: s.id,
                name: s.name,
                prompt: s.prompt,
                cron_description: s.cron_description,
                cron_expression: s.cron_expression,
                active: s.active,
                one_shot: s.one_shot,
                last_run_at: s
                    .last_run_at
                    .map(|v| format_utc_for_timezone(&v, &user_timezone))
                    .unwrap_or_else(|| "Never".to_string()),
                next_run_at: format_utc_for_timezone(&s.next_run_at, &user_timezone),
                pipe_names: s.pipe_names,
                pipe_ids: s.pipe_ids,
                pipe_ids_csv,
                pipe_checks,
            }
        })
        .collect();

    match (SchedulesTemplate {
        active_nav: "schedules",
        is_admin: user.is_admin,
        base_path,
        schedules,
        pipes,
        user_timezone,
        timezones: COMMON_TIMEZONES.to_vec(),
        error: q.error,
        success: q.success,
    })
    .render()
    {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn format_utc_for_timezone(value: &str, timezone: &str) -> String {
    let naive = match NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S") {
        Ok(dt) => dt,
        Err(_) => return value.to_string(),
    };

    let tz: chrono_tz::Tz = match timezone.parse() {
        Ok(tz) => tz,
        Err(_) => return format!("{} UTC", value),
    };

    let utc_dt = Utc.from_utc_datetime(&naive);
    utc_dt
        .with_timezone(&tz)
        .format("%Y-%m-%d %H:%M:%S %Z")
        .to_string()
}

/// POST /schedules — create a new schedule
pub async fn schedules_create(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<CreateScheduleForm>,
) -> Response {
    if form.name.trim().is_empty()
        || form.prompt.trim().is_empty()
        || form.when_text.trim().is_empty()
    {
        return Redirect::to(&format!(
            "{}/schedules?error=Name%2C+prompt%2C+and+timing+are+required.",
            base_path
        ))
        .into_response();
    }

    // Collect pipe IDs from the form (checkboxes send multiple values or comma-separated)
    let pipe_ids: Vec<String> = form
        .pipe_ids
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if pipe_ids.is_empty() {
        return Redirect::to(&format!(
            "{}/schedules?error=Please+select+at+least+one+pipe.",
            base_path
        ))
        .into_response();
    }

    // Use LLM to parse the timing
    let provider = match resolve_provider(&state).await {
        Ok(p) => p,
        Err(msg) => {
            return Redirect::to(&format!(
                "{}/schedules?error={}",
                base_path,
                urlencoding::encode(&msg)
            ))
            .into_response();
        }
    };

    // Get user timezone
    let timezone = sqlx::query_scalar!("SELECT timezone FROM users WHERE id = ?", user.user_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "UTC".to_string());

    let now_utc = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let timing_result = match nl_to_cron::parse_timing(
        &form.when_text,
        &provider,
        &now_utc,
        &timezone,
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            error!("Failed to parse timing '{}': {e}", form.when_text);
            return Redirect::to(&format!(
                    "{}/schedules?error={}",
                    base_path,
                    urlencoding::encode(&format!(
                    "Couldn't understand the timing. Try something like \"every day at 8am\" or \"next Sunday at 10am\".\n{}",
                    e
                ))
                ))
                .into_response();
        }
    };

    match timing_result {
        nl_to_cron::TimingResult::Recurring {
            cron_expression,
            description,
        } => {
            match schedules::create_schedule(
                &state.db,
                &user.user_id,
                None,
                form.name.trim(),
                form.prompt.trim(),
                &cron_expression,
                &description,
                &timezone,
                &pipe_ids,
            )
            .await
            {
                Ok(_) => Redirect::to(&format!(
                    "{}/schedules?success=Schedule+created.",
                    base_path
                ))
                .into_response(),
                Err(e) => {
                    error!("Failed to create schedule: {e}");
                    Redirect::to(&format!(
                        "{}/schedules?error=Failed+to+create+schedule.+Please+try+again.",
                        base_path
                    ))
                    .into_response()
                }
            }
        }
        nl_to_cron::TimingResult::OneShot {
            fire_at,
            description,
        } => {
            let fire_at_dt = match chrono::DateTime::parse_from_rfc3339(&fire_at)
                .or_else(|_| {
                    chrono::NaiveDateTime::parse_from_str(&fire_at, "%Y-%m-%dT%H:%M:%S")
                        .map(|dt| dt.and_utc().fixed_offset())
                })
                .map(|dt| dt.with_timezone(&Utc))
            {
                Ok(dt) => dt,
                Err(e) => {
                    error!("Failed to parse fire_at '{}': {e}", fire_at);
                    return Redirect::to(&format!(
                        "{}/schedules?error={}",
                        base_path,
                        urlencoding::encode(&format!("Couldn't understand the timing: {}", e))
                    ))
                    .into_response();
                }
            };

            match schedules::create_reminder(
                &state.db,
                &user.user_id,
                None,
                form.name.trim(),
                form.prompt.trim(),
                fire_at_dt,
                &description,
                &pipe_ids,
            )
            .await
            {
                Ok(_) => Redirect::to(&format!(
                    "{}/schedules?success=Reminder+created.",
                    base_path
                ))
                .into_response(),
                Err(e) => {
                    error!("Failed to create reminder: {e}");
                    Redirect::to(&format!(
                        "{}/schedules?error=Failed+to+create+reminder.+Please+try+again.",
                        base_path
                    ))
                    .into_response()
                }
            }
        }
    }
}

/// DELETE /schedules/{id} — delete a schedule (HTMX)
pub async fn schedules_delete(
    user: AuthUser,
    Path(schedule_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    match schedules::delete_schedule(&state.db, &schedule_id, &user.user_id).await {
        Ok(true) => Html("").into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Failed to delete schedule {schedule_id}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// POST /schedules/{id} — update a schedule (form submit, full page redirect)
pub async fn schedules_update(
    user: AuthUser,
    Path(schedule_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<UpdateScheduleForm>,
) -> Response {
    if form.name.trim().is_empty()
        || form.prompt.trim().is_empty()
        || form.when_text.trim().is_empty()
    {
        return Redirect::to(&format!(
            "{}/schedules?error=Name%2C+prompt%2C+and+timing+are+required.",
            base_path
        ))
        .into_response();
    }

    let pipe_ids: Vec<String> = form
        .pipe_ids
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if pipe_ids.is_empty() {
        return Redirect::to(&format!(
            "{}/schedules?error=Please+select+at+least+one+pipe.",
            base_path
        ))
        .into_response();
    }

    // Use LLM to parse the timing
    let provider = match resolve_provider(&state).await {
        Ok(p) => p,
        Err(msg) => {
            return Redirect::to(&format!(
                "{}/schedules?error={}",
                base_path,
                urlencoding::encode(&msg)
            ))
            .into_response();
        }
    };

    let timezone = sqlx::query_scalar!("SELECT timezone FROM users WHERE id = ?", user.user_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "UTC".to_string());

    let now_utc = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let timing_result = match nl_to_cron::parse_timing(
        &form.when_text,
        &provider,
        &now_utc,
        &timezone,
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            error!("Failed to parse timing '{}': {e}", form.when_text);
            return Redirect::to(&format!(
                "{}/schedules?error={}",
                base_path,
                urlencoding::encode(&format!(
                    "Couldn't understand the timing. Try something like \"every day at 8am\" or \"next Sunday at 10am\".\n{}",
                    e
                ))
            ))
            .into_response();
        }
    };

    match timing_result {
        nl_to_cron::TimingResult::Recurring {
            cron_expression,
            description,
        } => {
            match schedules::update_schedule(
                &state.db,
                &schedule_id,
                &user.user_id,
                form.name.trim(),
                form.prompt.trim(),
                &cron_expression,
                &description,
                &timezone,
                &pipe_ids,
            )
            .await
            {
                Ok(true) => Redirect::to(&format!(
                    "{}/schedules?success=Schedule+updated.",
                    base_path
                ))
                .into_response(),
                Ok(false) => Redirect::to(&format!(
                    "{}/schedules?error=Schedule+not+found.",
                    base_path
                ))
                .into_response(),
                Err(e) => {
                    error!("Failed to update schedule {schedule_id}: {e}");
                    Redirect::to(&format!(
                        "{}/schedules?error=Failed+to+update+schedule.+Please+try+again.",
                        base_path
                    ))
                    .into_response()
                }
            }
        }
        nl_to_cron::TimingResult::OneShot { .. } => {
            // Can't change a recurring schedule to one-shot via edit
            Redirect::to(&format!(
                "{}/schedules?error={}",
                base_path,
                urlencoding::encode("Editing only supports recurring schedules. Delete and re-create for one-time reminders.")
            ))
            .into_response()
        }
    }
}

/// POST /schedules/{id}/toggle — toggle active state (HTMX)
pub async fn schedules_toggle(
    user: AuthUser,
    Path(schedule_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    match schedules::toggle_schedule(&state.db, &schedule_id, &user.user_id).await {
        Ok(true) => {
            // Return the updated toggle button HTML
            let row = sqlx::query!("SELECT active FROM schedules WHERE id = ?", schedule_id,)
                .fetch_optional(&state.db)
                .await
                .ok()
                .flatten();

            let active = row.map(|r| r.active != 0).unwrap_or(false);
            if active {
                Html(r#"<span class="inline-flex items-center gap-1 px-2 py-0.5 rounded-full text-[10px] font-medium bg-emerald-500/10 text-emerald-400 border border-emerald-500/20"><span class="w-1 h-1 rounded-full bg-emerald-400"></span><span data-i18n="schedules.on">On</span></span>"#.to_string()).into_response()
            } else {
                Html(r#"<span class="inline-flex items-center px-2 py-0.5 rounded-full text-[10px] font-medium bg-rosie/10 text-rosie border border-rosie/20"><span data-i18n="schedules.off">Off</span></span>"#.to_string()).into_response()
            }
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Failed to toggle schedule {schedule_id}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// POST /schedules/{id}/pipes — update pipe assignments (HTMX)
pub async fn schedules_update_pipes(
    user: AuthUser,
    Path(schedule_id): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<UpdatePipesForm>,
) -> Response {
    let pipe_ids: Vec<String> = form
        .pipe_ids
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    match schedules::update_pipes(&state.db, &schedule_id, &user.user_id, &pipe_ids).await {
        Ok(true) => Html("").into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Failed to update pipes for schedule {schedule_id}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// POST /user/timezone — update user timezone
pub async fn user_set_timezone(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<TimezoneForm>,
) -> Response {
    // Validate timezone
    if form.timezone.parse::<chrono_tz::Tz>().is_err() {
        return Redirect::to(&format!("{}/schedules?error=Invalid+timezone.", base_path))
            .into_response();
    }

    if let Err(e) = sqlx::query!(
        "UPDATE users SET timezone = ? WHERE id = ?",
        form.timezone,
        user.user_id,
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to set timezone: {e}");
        return Redirect::to(&format!(
            "{}/schedules?error=Failed+to+save+timezone.",
            base_path
        ))
        .into_response();
    }

    Redirect::to(&format!(
        "{}/schedules?success=Timezone+updated.",
        base_path
    ))
    .into_response()
}

// ── Helpers ──────────────────────────────────────────────────────────────────

async fn resolve_provider(
    state: &AppState,
) -> Result<std::sync::Arc<dyn crate::llm::provider::LlmProvider>, String> {
    let resolved = crate::agents::model::resolve_model(&state.db, "default")
        .await
        .map_err(|e| format!("No LLM provider configured: {}", e))?;

    state
        .provider_cache
        .get_or_load(
            &state.db,
            &resolved.provider_id,
            &resolved.model_id,
            &state.credentials,
        )
        .await
        .map_err(|e| format!("Couldn't load LLM provider: {}", e))
}
