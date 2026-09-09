use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post, put},
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use tracing::{error, info};
use uuid::Uuid;

use crate::agents::memory;
use crate::agents::profile;
use crate::schedules;
use crate::services::timestamps;
use crate::state::AppState;

const SESSION_TTL_DAYS: i64 = 30;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/mobile/login", post(mobile_login))
        .route("/api/mobile/redeem", post(redeem_setup_token))
        .route("/api/mobile/info", get(instance_info))
        .route("/api/mobile/pipes", get(list_pipes))
        // Profile (About You)
        .route(
            "/api/mobile/profile",
            get(list_profile_facts).post(create_profile_fact),
        )
        .route(
            "/api/mobile/profile/{fact_id}",
            put(update_profile_fact).delete(delete_profile_fact),
        )
        // Agent Memories
        .route(
            "/api/mobile/memories",
            get(list_memories).post(create_memory),
        )
        .route(
            "/api/mobile/memories/{memory_id}",
            put(update_memory).delete(delete_memory),
        )
        // Schedules
        .route("/api/mobile/schedules", get(list_schedules))
        .route(
            "/api/mobile/schedules/{schedule_id}",
            delete(delete_schedule),
        )
        .route(
            "/api/mobile/schedules/{schedule_id}/toggle",
            post(toggle_schedule),
        )
        // Device token registration (for server-initiated push, e.g. schedules)
        .route("/api/mobile/device-token", post(register_device_token))
}

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    session_id: String,
    user_id: String,
    username: String,
    display_name: String,
    is_admin: bool,
    timezone: String,
    expires_at: String,
    instance_id: String,
    push_enabled: bool,
}

#[derive(Serialize)]
struct PipeResponse {
    id: String,
    user_id: String,
    name: String,
    transport: String,
    outbound_url: String,
    default_agent_id: Option<String>,
    active: bool,
    created_at: String,
}

#[derive(Deserialize)]
struct DeviceTokenRequest {
    device_token: String,
}

// ── Auth helper ──────────────────────────────────────────────────────────────

struct MobileUser {
    user_id: String,
    #[allow(dead_code)]
    username: String,
    #[allow(dead_code)]
    is_admin: bool,
}

async fn extract_mobile_user(
    headers: &HeaderMap,
    db: &sqlx::SqlitePool,
) -> Result<MobileUser, StatusCode> {
    let session_id = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|c| {
                let c = c.trim();
                c.strip_prefix("selu_session=").map(|v| v.to_string())
            })
        })
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let s = crate::services::auth::validate_session(db, &session_id)
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;

    Ok(MobileUser {
        user_id: s.user_id,
        username: s.username,
        is_admin: s.is_admin,
    })
}

// ── POST /api/mobile/login ───────────────────────────────────────────────────

async fn mobile_login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> impl IntoResponse {
    use argon2::{
        Argon2,
        password_hash::{PasswordVerifier, phc::PasswordHash},
    };

    let username = req.username.trim().to_string();

    let user = match sqlx::query!(
        "SELECT id, username, display_name, password_hash, is_admin, timezone FROM users WHERE username = ?",
        username
    )
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(u)) => u,
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    };

    let hash = match PasswordHash::new(&user.password_hash) {
        Ok(h) => h,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if Argon2::default()
        .verify_password(req.password.as_bytes(), &hash)
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let session_id = Uuid::new_v4().to_string();
    let user_id = user.id.unwrap_or_default();
    let expires_at = Utc::now() + Duration::days(SESSION_TTL_DAYS);
    let expires_at_db = expires_at.format("%Y-%m-%d %H:%M:%S").to_string();
    let expires_at = expires_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    if let Err(e) = sqlx::query!(
        "INSERT INTO web_sessions (id, user_id, expires_at) VALUES (?, ?, ?)",
        session_id,
        user_id,
        expires_at_db
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to create mobile session: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let instance_id = crate::persistence::db::get_instance_id(&state.db)
        .await
        .unwrap_or_default();

    let push_enabled = crate::services::system_updates::push_notifications_enabled(&state).await;

    Json(LoginResponse {
        session_id,
        user_id,
        username: user.username,
        display_name: user.display_name,
        is_admin: user.is_admin != 0,
        timezone: user.timezone,
        expires_at,
        instance_id,
        push_enabled,
    })
    .into_response()
}

// ── GET /api/mobile/info ─────────────────────────────────────────────────────

#[derive(Serialize)]
struct InstanceInfoResponse {
    instance_id: String,
    push_enabled: bool,
    timezone: String,
}

async fn instance_info(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(user) => user,
        Err(status) => return status.into_response(),
    };

    let instance_id = crate::persistence::db::get_instance_id(&state.db)
        .await
        .unwrap_or_default();
    let push_enabled = crate::services::system_updates::push_notifications_enabled(&state).await;
    let timezone = match sqlx::query_scalar::<_, String>("SELECT timezone FROM users WHERE id = ?")
        .bind(&user.user_id)
        .fetch_one(&state.db)
        .await
    {
        Ok(timezone) => timezone,
        Err(error) => {
            error!(%error, "Failed to load mobile user timezone");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    Json(InstanceInfoResponse {
        instance_id,
        push_enabled,
        timezone,
    })
    .into_response()
}

// ── POST /api/mobile/device-token ────────────────────────────────────────────

async fn register_device_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DeviceTokenRequest>,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    if req.device_token.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }

    match sqlx::query!(
        "INSERT INTO mobile_device_tokens (user_id, device_token, updated_at)
         VALUES (?, ?, datetime('now'))
         ON CONFLICT (user_id, device_token) DO UPDATE SET updated_at = datetime('now')",
        user.user_id,
        req.device_token,
    )
    .execute(&state.db)
    .await
    {
        Ok(_) => {
            info!(user_id = %user.user_id, "Device token registered");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            error!(error = %e, "Failed to register device token");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── GET /api/mobile/pipes ────────────────────────────────────────────────────

async fn list_pipes(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            i64,
            String,
        ),
    >(
        "SELECT id, user_id, name, transport, outbound_url, default_agent_id, active, created_at
         FROM pipes WHERE user_id = ? AND active = 1 ORDER BY created_at",
    )
    .bind(&user.user_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let pipes = match rows
        .into_iter()
        .map(|row| {
            Ok(PipeResponse {
                id: row.0,
                user_id: row.1,
                name: row.2,
                transport: row.3,
                outbound_url: row.4,
                default_agent_id: row.5,
                active: row.6 != 0,
                created_at: timestamps::canonical_utc(&row.7)?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()
    {
        Ok(pipes) => pipes,
        Err(error) => {
            error!(%error, "Failed to canonicalize mobile pipe timestamps");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    Json(pipes).into_response()
}

// ── POST /api/mobile/redeem ──────────────────────────────────────────────────
// Exchange a one-time setup token (from QR code) for a session.

#[derive(Deserialize)]
struct RedeemRequest {
    token: String,
}

async fn redeem_setup_token(
    State(state): State<AppState>,
    Json(req): Json<RedeemRequest>,
) -> impl IntoResponse {
    // Look up the token and verify it hasn't expired
    let user_id: Option<String> = match sqlx::query_scalar(
        "SELECT user_id FROM mobile_setup_tokens WHERE token = ? AND expires_at > datetime('now') AND used = 0",
    )
    .bind(&req.token)
    .fetch_optional(&state.db)
    .await
    {
        Ok(uid) => uid,
        Err(e) => {
            error!("Failed to look up setup token: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let Some(token_user_id) = user_id else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    // Mark the token as used
    let _ = sqlx::query("UPDATE mobile_setup_tokens SET used = 1 WHERE token = ?")
        .bind(&req.token)
        .execute(&state.db)
        .await;

    // Look up the user
    let user = match sqlx::query!(
        "SELECT id, username, display_name, is_admin, timezone FROM users WHERE id = ?",
        token_user_id
    )
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(u)) => u,
        _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    // Create a long-lived session
    let session_id = Uuid::new_v4().to_string();
    let user_id = user.id.unwrap_or_default();
    let expires_at = Utc::now() + Duration::days(SESSION_TTL_DAYS);
    let expires_at_db = expires_at.format("%Y-%m-%d %H:%M:%S").to_string();
    let expires_at = expires_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    if let Err(e) = sqlx::query!(
        "INSERT INTO web_sessions (id, user_id, expires_at) VALUES (?, ?, ?)",
        session_id,
        user_id,
        expires_at_db
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to create session from setup token: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let instance_id = crate::persistence::db::get_instance_id(&state.db)
        .await
        .unwrap_or_default();
    let push_enabled = crate::services::system_updates::push_notifications_enabled(&state).await;

    Json(LoginResponse {
        session_id,
        user_id,
        username: user.username,
        display_name: user.display_name,
        is_admin: user.is_admin != 0,
        timezone: user.timezone,
        expires_at,
        instance_id,
        push_enabled,
    })
    .into_response()
}

// ── GET /api/mobile/profile ─────────────────────────────────────────────────

#[derive(Serialize)]
struct ProfileFactResponse {
    id: String,
    fact: String,
    category: String,
    source: String,
    updated_at: String,
}

async fn list_profile_facts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    match profile::list_facts(&state.db, &user.user_id, 200).await {
        Ok(facts) => {
            let items = facts
                .into_iter()
                .map(|fact| {
                    Ok(ProfileFactResponse {
                        id: fact.id,
                        fact: fact.fact,
                        category: fact.category,
                        source: fact.source,
                        updated_at: timestamps::canonical_utc(&fact.updated_at)?,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>();
            match items {
                Ok(items) => Json(items).into_response(),
                Err(error) => {
                    error!(%error, "Failed to canonicalize mobile profile timestamps");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }
        Err(e) => {
            error!("Failed to list profile facts: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── POST /api/mobile/profile ────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateProfileFactRequest {
    fact: String,
    category: Option<String>,
}

async fn create_profile_fact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateProfileFactRequest>,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    let category = req.category.as_deref().unwrap_or("other");

    match profile::add_fact(
        &state.db,
        &user.user_id,
        req.fact.trim(),
        category,
        "manual",
        "system",
    )
    .await
    {
        Ok(id) => Json(serde_json::json!({ "id": id })).into_response(),
        Err(e) => {
            error!("Failed to create profile fact: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── PUT /api/mobile/profile/{fact_id} ───────────────────────────────────────

#[derive(Deserialize)]
struct UpdateProfileFactRequest {
    fact: String,
    category: Option<String>,
}

async fn update_profile_fact(
    Path(fact_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UpdateProfileFactRequest>,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    let category = req.category.as_deref().unwrap_or("other");

    match profile::update_fact(
        &state.db,
        &user.user_id,
        &fact_id,
        req.fact.trim(),
        category,
    )
    .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Failed to update profile fact: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── DELETE /api/mobile/profile/{fact_id} ────────────────────────────────────

async fn delete_profile_fact(
    Path(fact_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    match profile::delete_fact(&state.db, &user.user_id, &fact_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Failed to delete profile fact: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── GET /api/mobile/memories ────────────────────────────────────────────────

#[derive(Deserialize)]
struct MemoriesQuery {
    search: Option<String>,
}

#[derive(Serialize)]
struct MemoryResponse {
    id: String,
    memory: String,
    tags: String,
    source: String,
    category: String,
    agent_id: String,
    agent_name: String,
    updated_at: String,
}

/// Resolve agent display names for a set of agent IDs.
async fn resolve_agent_names(
    db: &sqlx::SqlitePool,
    agent_ids: &[String],
) -> std::collections::HashMap<String, String> {
    let mut names = std::collections::HashMap::new();
    for aid in agent_ids {
        if names.contains_key(aid) {
            continue;
        }
        if aid == "manual" {
            names.insert(aid.clone(), "Manual".to_string());
            continue;
        }
        let display_name: Option<String> =
            sqlx::query_scalar("SELECT display_name FROM agents WHERE id = ?")
                .bind(aid)
                .fetch_optional(db)
                .await
                .unwrap_or(None);
        names.insert(aid.clone(), display_name.unwrap_or_else(|| aid.clone()));
    }
    names
}

async fn list_memories(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MemoriesQuery>,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    if let Some(search) = &q.search {
        if !search.trim().is_empty() {
            match memory::search_memories(&state.db, &user.user_id, search, 50).await {
                Ok(hits) => {
                    let agent_ids: Vec<String> = hits.iter().map(|h| h.agent_id.clone()).collect();
                    let names = resolve_agent_names(&state.db, &agent_ids).await;
                    let items = hits
                        .into_iter()
                        .map(|hit| {
                            let agent_name = names.get(&hit.agent_id).cloned().unwrap_or_default();
                            Ok(MemoryResponse {
                                id: hit.id,
                                memory: hit.memory,
                                tags: hit.tags,
                                source: hit.source,
                                category: String::new(),
                                agent_id: hit.agent_id,
                                agent_name,
                                updated_at: timestamps::canonical_utc(&hit.updated_at)?,
                            })
                        })
                        .collect::<anyhow::Result<Vec<_>>>();
                    return match items {
                        Ok(items) => Json(items).into_response(),
                        Err(error) => {
                            error!(%error, "Failed to canonicalize mobile memory timestamps");
                            StatusCode::INTERNAL_SERVER_ERROR.into_response()
                        }
                    };
                }
                Err(e) => {
                    error!("Failed to search memories: {e}");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            }
        }
    }

    match memory::list_memories(&state.db, &user.user_id, 200).await {
        Ok(entries) => {
            let agent_ids: Vec<String> = entries.iter().map(|m| m.agent_id.clone()).collect();
            let names = resolve_agent_names(&state.db, &agent_ids).await;
            let items = entries
                .into_iter()
                .map(|entry| {
                    let agent_name = names.get(&entry.agent_id).cloned().unwrap_or_default();
                    Ok(MemoryResponse {
                        id: entry.id,
                        memory: entry.memory,
                        tags: entry.tags,
                        source: entry.source,
                        category: String::new(),
                        agent_id: entry.agent_id,
                        agent_name,
                        updated_at: timestamps::canonical_utc(&entry.updated_at)?,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>();
            match items {
                Ok(items) => Json(items).into_response(),
                Err(error) => {
                    error!(%error, "Failed to canonicalize mobile memory timestamps");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }
        Err(e) => {
            error!("Failed to list memories: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── POST /api/mobile/memories ───────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateMemoryRequest {
    memory: String,
    tags: Option<String>,
}

async fn create_memory(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateMemoryRequest>,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    match memory::add_memory(
        &state.db,
        "manual",
        &user.user_id,
        req.memory.trim(),
        req.tags.as_deref().unwrap_or(""),
        "manual",
    )
    .await
    {
        Ok(id) => Json(serde_json::json!({ "id": id })).into_response(),
        Err(e) => {
            error!("Failed to create memory: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── PUT /api/mobile/memories/{memory_id} ────────────────────────────────────

#[derive(Deserialize)]
struct UpdateMemoryRequest {
    memory: String,
    tags: Option<String>,
}

async fn update_memory(
    Path(memory_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UpdateMemoryRequest>,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    let memory_text = req.memory.trim().to_string();
    let tags = req.tags.as_deref().unwrap_or("").to_string();
    let result = sqlx::query!(
        "UPDATE agent_memories SET memory_text = ?, tags = ?, updated_at = datetime('now') WHERE id = ? AND user_id = ?",
        memory_text,
        tags,
        memory_id,
        user.user_id,
    )
    .execute(&state.db)
    .await;

    match result {
        Ok(r) if r.rows_affected() > 0 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Failed to update memory: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── DELETE /api/mobile/memories/{memory_id} ─────────────────────────────────

async fn delete_memory(
    Path(memory_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    match memory::delete_memory(&state.db, &user.user_id, &memory_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Failed to delete memory: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── GET /api/mobile/schedules ───────────────────────────────────────────────

#[derive(Serialize)]
struct ScheduleResponse {
    id: String,
    name: String,
    prompt: String,
    cron_expression: String,
    cron_description: String,
    timezone: String,
    active: bool,
    one_shot: bool,
    last_run_at: Option<String>,
    next_run_at: String,
    created_at: String,
    pipe_names: Vec<String>,
}

async fn list_schedules(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    match schedules::list_schedules(&state.db, &user.user_id).await {
        Ok(rows) => {
            let items = rows
                .into_iter()
                .map(|schedule| {
                    Ok(ScheduleResponse {
                        id: schedule.id,
                        name: schedule.name,
                        prompt: schedule.prompt,
                        cron_expression: schedule.cron_expression,
                        cron_description: schedule.cron_description,
                        timezone: schedule.timezone,
                        active: schedule.active,
                        one_shot: schedule.one_shot,
                        last_run_at: schedule
                            .last_run_at
                            .as_deref()
                            .map(timestamps::canonical_utc)
                            .transpose()?,
                        next_run_at: timestamps::canonical_utc(&schedule.next_run_at)?,
                        created_at: timestamps::canonical_utc(&schedule.created_at)?,
                        pipe_names: schedule.pipe_names,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>();
            match items {
                Ok(items) => Json(items).into_response(),
                Err(error) => {
                    error!(%error, "Failed to canonicalize mobile schedule timestamps");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }
        Err(e) => {
            error!("Failed to list schedules: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── DELETE /api/mobile/schedules/{schedule_id} ──────────────────────────────

async fn delete_schedule(
    Path(schedule_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    match schedules::delete_schedule(&state.db, &schedule_id, &user.user_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Failed to delete schedule: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── POST /api/mobile/schedules/{schedule_id}/toggle ─────────────────────────

async fn toggle_schedule(
    Path(schedule_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let user = match extract_mobile_user(&headers, &state.db).await {
        Ok(u) => u,
        Err(s) => return s.into_response(),
    };

    match schedules::toggle_schedule(&state.db, &schedule_id, &user.user_id).await {
        Ok(true) => {
            let active =
                sqlx::query_scalar!("SELECT active FROM schedules WHERE id = ?", schedule_id,)
                    .fetch_optional(&state.db)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(0);

            Json(serde_json::json!({ "active": active != 0 })).into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Failed to toggle schedule: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
