use anyhow::anyhow;
use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{Duration, Utc};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use thiserror::Error;
use uuid::Uuid;

use crate::agents::{access::NO_AGENTS_SENTINEL, profile};

const MIN_PASSWORD_LEN: usize = 8;
const MAX_DISPLAY_NAME_LEN: usize = 120;
const MAX_USERNAME_LEN: usize = 80;
const MAX_TIMEZONE_LEN: usize = 64;
const MAX_PROFILE_FACT_LEN: usize = 2_000;
const PAIRING_TOKEN_BYTES: usize = 32;
const PAIRING_TTL_MINUTES: i64 = 5;

#[derive(Debug, Error)]
pub enum AccountError {
    #[error("{message}")]
    Invalid {
        code: &'static str,
        message: &'static str,
    },
    #[error("The requested user was not found.")]
    UserNotFound,
    #[error("The requested profile fact was not found.")]
    ProfileFactNotFound,
    #[error("That username is already in use.")]
    UsernameTaken,
    #[error("The current password is incorrect.")]
    InvalidCurrentPassword,
    #[error("You cannot delete your own account.")]
    SelfDeletionForbidden,
    #[error("At least one administrator must remain.")]
    LastAdminRequired,
    #[error("Feedback is temporarily unavailable.")]
    FeedbackUnavailable,
    #[error("The account operation could not be completed.")]
    Internal(#[source] anyhow::Error),
}

impl AccountError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid { code, .. } => code,
            Self::UserNotFound => "user_not_found",
            Self::ProfileFactNotFound => "profile_fact_not_found",
            Self::UsernameTaken => "username_taken",
            Self::InvalidCurrentPassword => "invalid_current_password",
            Self::SelfDeletionForbidden => "self_deletion_forbidden",
            Self::LastAdminRequired => "last_admin_required",
            Self::FeedbackUnavailable => "feedback_unavailable",
            Self::Internal(_) => "internal_error",
        }
    }
}

fn internal(error: impl Into<anyhow::Error>) -> AccountError {
    AccountError::Internal(error.into())
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UserAccount {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub is_admin: bool,
    pub language: String,
    pub timezone: String,
    pub created_at: String,
    pub allowed_agent_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CreateUser {
    pub username: String,
    pub display_name: String,
    pub password: String,
    pub is_admin: bool,
    pub language: String,
    pub timezone: String,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateOwnProfile {
    pub display_name: Option<String>,
    pub language: Option<String>,
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateUser {
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub is_admin: Option<bool>,
    pub language: Option<String>,
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PasswordChangeReceipt {
    pub status: &'static str,
    pub other_sessions_revoked: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentAccess {
    pub user_id: String,
    /// Empty means unrestricted access; `["__none__"]` means explicitly none.
    pub allowed_agent_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProfileFact {
    pub id: String,
    pub fact: String,
    pub category: String,
    pub source: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PairingToken {
    pub token: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GeneralFeedback {
    pub category: String,
    pub title: Option<String>,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FeedbackReceipt {
    pub status: &'static str,
    pub issue_number: u64,
    pub issue_url: String,
}

#[derive(Debug, Serialize)]
struct GatewayFeedbackRequest<'a> {
    category: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    description: &'a str,
}

#[derive(Debug, Deserialize)]
struct GatewayFeedbackResponse {
    issue_url: String,
    issue_number: u64,
}

pub async fn get_user(db: &SqlitePool, user_id: &str) -> Result<UserAccount, AccountError> {
    let row = sqlx::query!(
        r#"SELECT id, username, display_name, is_admin, language, timezone, created_at
           FROM users WHERE id = ?"#,
        user_id
    )
    .fetch_optional(db)
    .await
    .map_err(internal)?
    .ok_or(AccountError::UserNotFound)?;

    let allowed_agent_ids = sqlx::query_scalar!(
        "SELECT agent_id FROM user_agent_access WHERE user_id = ? ORDER BY agent_id",
        user_id
    )
    .fetch_all(db)
    .await
    .map_err(internal)?;

    Ok(UserAccount {
        id: row.id.unwrap_or_default(),
        username: row.username,
        display_name: row.display_name,
        is_admin: row.is_admin != 0,
        language: row.language,
        timezone: row.timezone,
        created_at: row.created_at,
        allowed_agent_ids,
    })
}

pub async fn list_users(db: &SqlitePool) -> Result<Vec<UserAccount>, AccountError> {
    let ids = sqlx::query_scalar!("SELECT id FROM users ORDER BY created_at, username")
        .fetch_all(db)
        .await
        .map_err(internal)?;
    let mut users = Vec::with_capacity(ids.len());
    for id in ids.into_iter().flatten() {
        users.push(get_user(db, &id).await?);
    }
    Ok(users)
}

pub async fn create_user(db: &SqlitePool, input: CreateUser) -> Result<UserAccount, AccountError> {
    let username = validate_username(&input.username)?;
    let display_name = validate_display_name(&input.display_name, &username)?;
    let language = validate_language(&input.language)?;
    let timezone = validate_timezone(&input.timezone)?;
    validate_new_password(&input.password)?;
    let password_hash = hash_password(&input.password)?;
    let user_id = Uuid::new_v4().to_string();
    let is_admin = if input.is_admin { 1_i64 } else { 0_i64 };
    let pipe_id = Uuid::new_v4().to_string();
    let pipe_name = format!("{display_name}'s Chat");

    let mut tx = db.begin().await.map_err(internal)?;
    let inserted = sqlx::query!(
        r#"INSERT INTO users
           (id, username, display_name, password_hash, is_admin, language, timezone)
           VALUES (?, ?, ?, ?, ?, ?, ?)"#,
        user_id,
        username,
        display_name,
        password_hash,
        is_admin,
        language,
        timezone
    )
    .execute(&mut *tx)
    .await;
    if let Err(error) = inserted {
        if error
            .as_database_error()
            .is_some_and(|db_error| db_error.is_unique_violation())
        {
            return Err(AccountError::UsernameTaken);
        }
        return Err(internal(error));
    }

    sqlx::query!(
        r#"INSERT INTO pipes
           (id, user_id, name, transport, inbound_token, outbound_url, default_agent_id)
           VALUES (?, ?, ?, 'web', ?, 'internal://web', NULL)"#,
        pipe_id,
        user_id,
        pipe_name,
        ""
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    get_user(db, &user_id).await
}

pub async fn update_own_profile(
    db: &SqlitePool,
    user_id: &str,
    input: UpdateOwnProfile,
) -> Result<UserAccount, AccountError> {
    let display_name = input
        .display_name
        .map(|value| validate_display_name(&value, ""))
        .transpose()?;
    let language = input
        .language
        .map(|value| validate_language(&value))
        .transpose()?;
    let timezone = input
        .timezone
        .map(|value| validate_timezone(&value))
        .transpose()?;

    let result = sqlx::query!(
        r#"UPDATE users SET
             display_name = COALESCE(?, display_name),
             language = COALESCE(?, language),
             timezone = COALESCE(?, timezone)
           WHERE id = ?"#,
        display_name,
        language,
        timezone,
        user_id
    )
    .execute(db)
    .await
    .map_err(internal)?;
    if result.rows_affected() == 0 {
        return Err(AccountError::UserNotFound);
    }
    get_user(db, user_id).await
}

pub async fn update_user(
    db: &SqlitePool,
    target_user_id: &str,
    input: UpdateUser,
) -> Result<UserAccount, AccountError> {
    let current = get_user(db, target_user_id).await?;
    let username = input
        .username
        .map(|value| validate_username(&value))
        .transpose()?;
    let display_name = input
        .display_name
        .map(|value| validate_display_name(&value, ""))
        .transpose()?;
    let language = input
        .language
        .map(|value| validate_language(&value))
        .transpose()?;
    let timezone = input
        .timezone
        .map(|value| validate_timezone(&value))
        .transpose()?;
    let is_admin = input
        .is_admin
        .map(|is_admin| if is_admin { 1_i64 } else { 0_i64 });

    let mut tx = db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    if current.is_admin && input.is_admin == Some(false) {
        let admin_count = sqlx::query_scalar!("SELECT COUNT(*) FROM users WHERE is_admin = 1")
            .fetch_one(&mut *tx)
            .await
            .map_err(internal)?;
        if admin_count <= 1 {
            return Err(AccountError::LastAdminRequired);
        }
    }

    let result = sqlx::query!(
        r#"UPDATE users SET
             username = COALESCE(?, username),
             display_name = COALESCE(?, display_name),
             is_admin = COALESCE(?, is_admin),
             language = COALESCE(?, language),
             timezone = COALESCE(?, timezone)
           WHERE id = ?"#,
        username,
        display_name,
        is_admin,
        language,
        timezone,
        target_user_id
    )
    .execute(&mut *tx)
    .await;
    match result {
        Ok(result) if result.rows_affected() == 0 => return Err(AccountError::UserNotFound),
        Ok(_) => {}
        Err(error)
            if error
                .as_database_error()
                .is_some_and(|db_error| db_error.is_unique_violation()) =>
        {
            return Err(AccountError::UsernameTaken);
        }
        Err(error) => return Err(internal(error)),
    }
    tx.commit().await.map_err(internal)?;
    get_user(db, target_user_id).await
}

pub async fn delete_user(
    db: &SqlitePool,
    caller_user_id: &str,
    target_user_id: &str,
) -> Result<(), AccountError> {
    if caller_user_id == target_user_id {
        return Err(AccountError::SelfDeletionForbidden);
    }

    let mut tx = db.begin_with("BEGIN IMMEDIATE").await.map_err(internal)?;
    let target_is_admin =
        sqlx::query_scalar!("SELECT is_admin FROM users WHERE id = ?", target_user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .ok_or(AccountError::UserNotFound)?;
    if target_is_admin != 0 {
        let admin_count = sqlx::query_scalar!("SELECT COUNT(*) FROM users WHERE is_admin = 1")
            .fetch_one(&mut *tx)
            .await
            .map_err(internal)?;
        if admin_count <= 1 {
            return Err(AccountError::LastAdminRequired);
        }
    }

    // Older schemas intentionally predate widespread ON DELETE CASCADE. Remove
    // non-cascading children in dependency order so account deletion remains
    // atomic with foreign-key enforcement enabled.
    sqlx::query!(
        "DELETE FROM thread_artifacts WHERE user_id = ?",
        target_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query!(
        r#"DELETE FROM thread_reply_guids
           WHERE thread_id IN (SELECT id FROM threads WHERE user_id = ?)
              OR pipe_id IN (SELECT id FROM pipes WHERE user_id = ?)"#,
        target_user_id,
        target_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query!(
        r#"DELETE FROM thread_agent_sessions
           WHERE thread_id IN (SELECT id FROM threads WHERE user_id = ?)
              OR session_id IN (SELECT id FROM sessions WHERE user_id = ?)"#,
        target_user_id,
        target_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query!(
        r#"DELETE FROM messages
           WHERE thread_id IN (SELECT id FROM threads WHERE user_id = ?)
              OR session_id IN (SELECT id FROM sessions WHERE user_id = ?)
              OR pipe_id IN (SELECT id FROM pipes WHERE user_id = ?)"#,
        target_user_id,
        target_user_id,
        target_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query!(
        "DELETE FROM agent_events WHERE source_session_id IN (SELECT id FROM sessions WHERE user_id = ?)",
        target_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query!(
        "DELETE FROM workspaces WHERE session_id IN (SELECT id FROM sessions WHERE user_id = ?)",
        target_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query!("DELETE FROM threads WHERE user_id = ?", target_user_id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    sqlx::query!("DELETE FROM sessions WHERE user_id = ?", target_user_id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    sqlx::query!(
        "DELETE FROM bluebubbles_configs WHERE pipe_id IN (SELECT id FROM pipes WHERE user_id = ?)",
        target_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query!(
        "DELETE FROM telegram_configs WHERE pipe_id IN (SELECT id FROM pipes WHERE user_id = ?)",
        target_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query!(
        "DELETE FROM whatsapp_configs WHERE pipe_id IN (SELECT id FROM pipes WHERE user_id = ?)",
        target_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query!(
        "DELETE FROM user_sender_refs WHERE user_id = ?",
        target_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query!("DELETE FROM schedules WHERE user_id = ?", target_user_id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    sqlx::query!("DELETE FROM pipes WHERE user_id = ?", target_user_id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    sqlx::query!(
        "DELETE FROM user_credentials WHERE user_id = ?",
        target_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query!(
        "DELETE FROM event_subscriptions WHERE user_id = ?",
        target_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query!("DELETE FROM web_sessions WHERE user_id = ?", target_user_id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    sqlx::query!(
        "DELETE FROM mobile_setup_tokens WHERE user_id = ?",
        target_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query!(
        "DELETE FROM mobile_device_tokens WHERE user_id = ?",
        target_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query!("DELETE FROM users WHERE id = ?", target_user_id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(())
}

pub async fn change_password(
    db: &SqlitePool,
    user_id: &str,
    current_session_id: Option<&str>,
    current_password: &str,
    new_password: &str,
) -> Result<PasswordChangeReceipt, AccountError> {
    validate_new_password(new_password)?;
    let stored_hash = sqlx::query_scalar!("SELECT password_hash FROM users WHERE id = ?", user_id)
        .fetch_optional(db)
        .await
        .map_err(internal)?
        .ok_or(AccountError::UserNotFound)?;
    let parsed_hash = PasswordHash::new(&stored_hash).map_err(|error| internal(anyhow!(error)))?;
    if Argon2::default()
        .verify_password(current_password.as_bytes(), &parsed_hash)
        .is_err()
    {
        return Err(AccountError::InvalidCurrentPassword);
    }
    let new_hash = hash_password(new_password)?;

    let mut tx = db.begin().await.map_err(internal)?;
    sqlx::query!(
        "UPDATE users SET password_hash = ? WHERE id = ?",
        new_hash,
        user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    let revoked = if let Some(session_id) = current_session_id {
        sqlx::query!(
            "DELETE FROM web_sessions WHERE user_id = ? AND id != ?",
            user_id,
            session_id
        )
        .execute(&mut *tx)
        .await
        .map_err(internal)?
        .rows_affected()
    } else {
        sqlx::query!("DELETE FROM web_sessions WHERE user_id = ?", user_id)
            .execute(&mut *tx)
            .await
            .map_err(internal)?
            .rows_affected()
    };
    tx.commit().await.map_err(internal)?;
    Ok(PasswordChangeReceipt {
        status: "password_changed",
        other_sessions_revoked: revoked,
    })
}

pub async fn set_agent_access(
    db: &SqlitePool,
    target_user_id: &str,
    selected_agent_ids: &[String],
    all_agent_ids: &[String],
) -> Result<AgentAccess, AccountError> {
    if selected_agent_ids
        .iter()
        .any(|id| id == NO_AGENTS_SENTINEL || !all_agent_ids.contains(id))
    {
        return Err(AccountError::Invalid {
            code: "invalid_agent_access",
            message: "Agent access contains an unknown agent.",
        });
    }
    let user_count = sqlx::query_scalar!("SELECT COUNT(*) FROM users WHERE id = ?", target_user_id)
        .fetch_one(db)
        .await
        .map_err(internal)?;
    if user_count == 0 {
        return Err(AccountError::UserNotFound);
    }

    let all_selected = !all_agent_ids.is_empty()
        && all_agent_ids
            .iter()
            .all(|id| selected_agent_ids.contains(id));
    let mut tx = db.begin().await.map_err(internal)?;
    sqlx::query!(
        "DELETE FROM user_agent_access WHERE user_id = ?",
        target_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;

    if selected_agent_ids.is_empty() && !all_agent_ids.is_empty() {
        sqlx::query!(
            "INSERT INTO user_agent_access (user_id, agent_id) VALUES (?, ?)",
            target_user_id,
            NO_AGENTS_SENTINEL
        )
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    } else if !all_selected {
        for agent_id in selected_agent_ids {
            sqlx::query!(
                "INSERT INTO user_agent_access (user_id, agent_id) VALUES (?, ?)",
                target_user_id,
                agent_id
            )
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        }
    }
    tx.commit().await.map_err(internal)?;

    let allowed_agent_ids = sqlx::query_scalar!(
        "SELECT agent_id FROM user_agent_access WHERE user_id = ? ORDER BY agent_id",
        target_user_id
    )
    .fetch_all(db)
    .await
    .map_err(internal)?;
    Ok(AgentAccess {
        user_id: target_user_id.to_string(),
        allowed_agent_ids,
    })
}

pub async fn list_profile_facts(
    db: &SqlitePool,
    owner_user_id: &str,
) -> Result<Vec<ProfileFact>, AccountError> {
    profile::list_facts(db, owner_user_id, 200)
        .await
        .map(|facts| {
            facts
                .into_iter()
                .map(|fact| ProfileFact {
                    id: fact.id,
                    fact: fact.fact,
                    category: fact.category,
                    source: fact.source,
                    updated_at: fact.updated_at,
                })
                .collect()
        })
        .map_err(internal)
}

pub async fn create_profile_fact(
    db: &SqlitePool,
    owner_user_id: &str,
    fact: &str,
    category: Option<&str>,
) -> Result<String, AccountError> {
    let (fact, category) = validate_profile_fact(fact, category)?;
    profile::add_fact(db, owner_user_id, &fact, &category, "manual", "system")
        .await
        .map_err(internal)
}

pub async fn update_profile_fact(
    db: &SqlitePool,
    owner_user_id: &str,
    fact_id: &str,
    fact: &str,
    category: Option<&str>,
) -> Result<(), AccountError> {
    let (fact, category) = validate_profile_fact(fact, category)?;
    match profile::update_fact(db, owner_user_id, fact_id, &fact, &category)
        .await
        .map_err(internal)?
    {
        true => Ok(()),
        false => Err(AccountError::ProfileFactNotFound),
    }
}

pub async fn delete_profile_fact(
    db: &SqlitePool,
    owner_user_id: &str,
    fact_id: &str,
) -> Result<(), AccountError> {
    match profile::delete_fact(db, owner_user_id, fact_id)
        .await
        .map_err(internal)?
    {
        true => Ok(()),
        false => Err(AccountError::ProfileFactNotFound),
    }
}

pub async fn create_pairing_token(
    db: &SqlitePool,
    owner_user_id: &str,
) -> Result<PairingToken, AccountError> {
    let token = random_token(PAIRING_TOKEN_BYTES)?;
    let expires_at = (Utc::now() + Duration::minutes(PAIRING_TTL_MINUTES))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let mut tx = db.begin().await.map_err(internal)?;
    sqlx::query!(
        "DELETE FROM mobile_setup_tokens WHERE user_id = ?",
        owner_user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    sqlx::query!(
        "INSERT INTO mobile_setup_tokens (token, user_id, expires_at) VALUES (?, ?, ?)",
        token,
        owner_user_id,
        expires_at
    )
    .execute(&mut *tx)
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(PairingToken { token, expires_at })
}

pub async fn submit_general_feedback(
    client: &reqwest::Client,
    marketplace_url: &str,
    instance_id: &str,
    feedback: &GeneralFeedback,
) -> Result<FeedbackReceipt, AccountError> {
    validate_feedback(feedback)?;
    let api_base = feedback_api_base_url(marketplace_url)?;
    let endpoint = format!("{api_base}/feedback");
    let response = client
        .post(endpoint)
        .header("x-instance-id", instance_id)
        .json(&GatewayFeedbackRequest {
            category: &feedback.category,
            title: feedback.title.as_deref(),
            description: feedback.description.trim(),
        })
        .send()
        .await
        .map_err(|_| AccountError::FeedbackUnavailable)?;
    if !response.status().is_success() {
        return Err(AccountError::FeedbackUnavailable);
    }
    let response = response
        .json::<GatewayFeedbackResponse>()
        .await
        .map_err(|_| AccountError::FeedbackUnavailable)?;
    let issue_url = safe_receipt_url(&response.issue_url)?;
    Ok(FeedbackReceipt {
        status: "accepted",
        issue_number: response.issue_number,
        issue_url,
    })
}

fn validate_username(value: &str) -> Result<String, AccountError> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_USERNAME_LEN {
        return Err(AccountError::Invalid {
            code: "invalid_username",
            message: "Username must be between 1 and 80 characters.",
        });
    }
    Ok(value.to_string())
}

fn validate_display_name(value: &str, fallback: &str) -> Result<String, AccountError> {
    let value = value.trim();
    let value = if value.is_empty() { fallback } else { value };
    if value.is_empty() || value.len() > MAX_DISPLAY_NAME_LEN {
        return Err(AccountError::Invalid {
            code: "invalid_display_name",
            message: "Display name must be between 1 and 120 characters.",
        });
    }
    Ok(value.to_string())
}

fn validate_language(value: &str) -> Result<String, AccountError> {
    match value.trim() {
        "en" => Ok("en".to_string()),
        "de" => Ok("de".to_string()),
        _ => Err(AccountError::Invalid {
            code: "invalid_language",
            message: "Language must be either 'en' or 'de'.",
        }),
    }
}

fn validate_timezone(value: &str) -> Result<String, AccountError> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_TIMEZONE_LEN || value.parse::<chrono_tz::Tz>().is_err()
    {
        return Err(AccountError::Invalid {
            code: "invalid_timezone",
            message: "Timezone must be a valid IANA timezone name.",
        });
    }
    Ok(value.to_string())
}

fn validate_new_password(password: &str) -> Result<(), AccountError> {
    if password.len() < MIN_PASSWORD_LEN {
        return Err(AccountError::Invalid {
            code: "weak_password",
            message: "Password must be at least 8 characters long.",
        });
    }
    Ok(())
}

fn validate_profile_fact(
    fact: &str,
    category: Option<&str>,
) -> Result<(String, String), AccountError> {
    let fact = fact.trim();
    if fact.is_empty() || fact.len() > MAX_PROFILE_FACT_LEN {
        return Err(AccountError::Invalid {
            code: "invalid_profile_fact",
            message: "Profile fact must be between 1 and 2000 characters.",
        });
    }
    let category = category.unwrap_or("other").trim();
    if category.is_empty() || category.len() > 64 {
        return Err(AccountError::Invalid {
            code: "invalid_profile_category",
            message: "Profile category must be between 1 and 64 characters.",
        });
    }
    Ok((fact.to_string(), category.to_string()))
}

fn validate_feedback(feedback: &GeneralFeedback) -> Result<(), AccountError> {
    if !matches!(
        feedback.category.as_str(),
        "bug" | "idea" | "question" | "other"
    ) {
        return Err(AccountError::Invalid {
            code: "invalid_feedback_category",
            message: "Feedback category must be bug, idea, question, or other.",
        });
    }
    if feedback
        .title
        .as_deref()
        .is_some_and(|title| title.trim().is_empty() || title.len() > 100)
    {
        return Err(AccountError::Invalid {
            code: "invalid_feedback_title",
            message: "Feedback title must be between 1 and 100 characters.",
        });
    }
    let description_len = feedback.description.trim().len();
    if !(10..=2_000).contains(&description_len) {
        return Err(AccountError::Invalid {
            code: "invalid_feedback_description",
            message: "Feedback description must be between 10 and 2000 characters.",
        });
    }
    Ok(())
}

fn feedback_api_base_url(marketplace_url: &str) -> Result<String, AccountError> {
    let trimmed = marketplace_url.trim_end_matches('/');
    let marker = "/marketplace/agents";
    trimmed
        .find(marker)
        .map(|index| trimmed[..index].to_string())
        .filter(|base| !base.is_empty())
        .ok_or(AccountError::FeedbackUnavailable)
}

fn safe_receipt_url(value: &str) -> Result<String, AccountError> {
    let parsed = reqwest::Url::parse(value).map_err(|_| AccountError::FeedbackUnavailable)?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(AccountError::FeedbackUnavailable);
    }
    Ok(parsed.to_string())
}

fn random_token(byte_len: usize) -> Result<String, AccountError> {
    let mut bytes = vec![0_u8; byte_len];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| internal(anyhow!("secure random generation failed")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn hash_password(password: &str) -> Result<String, AccountError> {
    let mut salt = [0_u8; 16];
    SystemRandom::new()
        .fill(&mut salt)
        .map_err(|_| internal(anyhow!("secure random generation failed")))?;
    Argon2::default()
        .hash_password_with_salt(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| internal(anyhow!(error)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_db() -> SqlitePool {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        db
    }

    fn user(username: &str, admin: bool) -> CreateUser {
        CreateUser {
            username: username.to_string(),
            display_name: username.to_uppercase(),
            password: "password-123".to_string(),
            is_admin: admin,
            language: "en".to_string(),
            timezone: "Europe/Berlin".to_string(),
        }
    }

    #[tokio::test]
    async fn account_crud_never_returns_password_material() {
        let db = test_db().await;
        let created = create_user(&db, user("alice", true)).await.unwrap();
        assert_eq!(created.username, "alice");
        assert_eq!(created.timezone, "Europe/Berlin");
        assert_eq!(list_users(&db).await.unwrap(), vec![created.clone()]);

        let updated = update_user(
            &db,
            &created.id,
            UpdateUser {
                display_name: Some("Alice Updated".to_string()),
                language: Some("de".to_string()),
                ..UpdateUser::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.display_name, "Alice Updated");
        assert_eq!(updated.language, "de");
        let json = serde_json::to_string(&updated).unwrap();
        assert!(!json.contains("password"));
        assert!(!json.contains("hash"));
    }

    #[tokio::test]
    async fn own_profile_only_updates_safe_fields() {
        let db = test_db().await;
        let created = create_user(&db, user("alice", true)).await.unwrap();
        let updated = update_own_profile(
            &db,
            &created.id,
            UpdateOwnProfile {
                display_name: Some("Alice".to_string()),
                language: Some("de".to_string()),
                timezone: Some("America/New_York".to_string()),
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.username, "alice");
        assert!(updated.is_admin);
        assert_eq!(updated.timezone, "America/New_York");

        let error = update_own_profile(
            &db,
            &created.id,
            UpdateOwnProfile {
                timezone: Some("not/a-zone".to_string()),
                ..UpdateOwnProfile::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "invalid_timezone");
    }

    #[tokio::test]
    async fn password_change_verifies_current_and_revokes_other_sessions() {
        let db = test_db().await;
        let created = create_user(&db, user("alice", true)).await.unwrap();
        for id in ["current", "other"] {
            sqlx::query!(
                "INSERT INTO web_sessions (id, user_id, expires_at) VALUES (?, ?, datetime('now', '+1 day'))",
                id,
                created.id
            )
            .execute(&db)
            .await
            .unwrap();
        }

        assert!(matches!(
            change_password(&db, &created.id, Some("current"), "wrong", "new-password").await,
            Err(AccountError::InvalidCurrentPassword)
        ));
        let receipt = change_password(
            &db,
            &created.id,
            Some("current"),
            "password-123",
            "new-password",
        )
        .await
        .unwrap();
        assert_eq!(receipt.other_sessions_revoked, 1);
        assert_eq!(
            sqlx::query_scalar!(
                "SELECT COUNT(*) FROM web_sessions WHERE user_id = ?",
                created.id
            )
            .fetch_one(&db)
            .await
            .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn admin_safeguards_block_self_delete_and_last_admin_demotion() {
        let db = test_db().await;
        let admin = create_user(&db, user("admin", true)).await.unwrap();
        let member = create_user(&db, user("member", false)).await.unwrap();
        assert!(matches!(
            delete_user(&db, &admin.id, &admin.id).await,
            Err(AccountError::SelfDeletionForbidden)
        ));
        assert!(matches!(
            update_user(
                &db,
                &admin.id,
                UpdateUser {
                    is_admin: Some(false),
                    ..UpdateUser::default()
                }
            )
            .await,
            Err(AccountError::LastAdminRequired)
        ));
        delete_user(&db, &admin.id, &member.id).await.unwrap();
        assert!(matches!(
            get_user(&db, &member.id).await,
            Err(AccountError::UserNotFound)
        ));
    }

    #[tokio::test]
    async fn profile_facts_are_always_scoped_to_the_owner() {
        let db = test_db().await;
        let owner = create_user(&db, user("owner", true)).await.unwrap();
        let other = create_user(&db, user("other", false)).await.unwrap();
        let fact_id = create_profile_fact(&db, &owner.id, "Likes Rust", Some("preferences"))
            .await
            .unwrap();
        assert_eq!(list_profile_facts(&db, &owner.id).await.unwrap().len(), 1);
        assert!(list_profile_facts(&db, &other.id).await.unwrap().is_empty());
        assert!(matches!(
            update_profile_fact(&db, &other.id, &fact_id, "Changed", None).await,
            Err(AccountError::ProfileFactNotFound)
        ));
        assert!(matches!(
            delete_profile_fact(&db, &other.id, &fact_id).await,
            Err(AccountError::ProfileFactNotFound)
        ));
        delete_profile_fact(&db, &owner.id, &fact_id).await.unwrap();
    }

    #[tokio::test]
    async fn agent_access_preserves_unrestricted_none_and_subset_semantics() {
        let db = test_db().await;
        let owner = create_user(&db, user("owner", true)).await.unwrap();
        let all = vec!["alpha".to_string(), "beta".to_string()];
        let none = set_agent_access(&db, &owner.id, &[], &all).await.unwrap();
        assert_eq!(none.allowed_agent_ids, vec![NO_AGENTS_SENTINEL]);
        let subset = set_agent_access(&db, &owner.id, &["alpha".to_string()], &all)
            .await
            .unwrap();
        assert_eq!(subset.allowed_agent_ids, vec!["alpha"]);
        let unrestricted = set_agent_access(&db, &owner.id, &all, &all).await.unwrap();
        assert!(unrestricted.allowed_agent_ids.is_empty());
    }

    #[tokio::test]
    async fn pairing_token_is_random_single_use_storage_with_five_minute_expiry() {
        let db = test_db().await;
        let owner = create_user(&db, user("owner", true)).await.unwrap();
        let pairing = create_pairing_token(&db, &owner.id).await.unwrap();
        assert!(pairing.token.len() >= 40);
        let row = sqlx::query!(
            "SELECT user_id, used, expires_at FROM mobile_setup_tokens WHERE token = ?",
            pairing.token
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(row.user_id, owner.id);
        assert_eq!(row.used, 0);
        let expires_at =
            chrono::NaiveDateTime::parse_from_str(&row.expires_at, "%Y-%m-%d %H:%M:%S").unwrap();
        let remaining = expires_at.and_utc() - Utc::now();
        assert!(remaining > Duration::minutes(4));
        assert!(remaining <= Duration::minutes(5));
    }

    #[tokio::test]
    async fn creating_pairing_token_invalidates_previous_live_token() {
        let db = test_db().await;
        let owner = create_user(&db, user("owner", true)).await.unwrap();
        let first = create_pairing_token(&db, &owner.id).await.unwrap();
        let second = create_pairing_token(&db, &owner.id).await.unwrap();

        let first_row = sqlx::query!(
            "SELECT user_id, used, expires_at FROM mobile_setup_tokens WHERE token = ?",
            first.token
        )
        .fetch_optional(&db)
        .await
        .unwrap();
        let second_row = sqlx::query!(
            "SELECT user_id, used, expires_at FROM mobile_setup_tokens WHERE token = ?",
            second.token
        )
        .fetch_optional(&db)
        .await
        .unwrap();

        assert!(first_row.is_none());
        assert_eq!(second_row.unwrap().user_id, owner.id);
    }

    #[test]
    fn feedback_validation_and_receipt_url_are_safe() {
        let valid = GeneralFeedback {
            category: "bug".to_string(),
            title: Some("A short title".to_string()),
            description: "A detailed description".to_string(),
        };
        validate_feedback(&valid).unwrap();
        assert_eq!(
            feedback_api_base_url("https://selu.bot/api/marketplace/agents").unwrap(),
            "https://selu.bot/api"
        );
        assert!(safe_receipt_url("https://github.com/selu/issues/1").is_ok());
        assert!(safe_receipt_url("javascript:alert(1)").is_err());
        assert!(safe_receipt_url("https://secret@example.com/issues/1").is_err());
    }
}
