use anyhow::{Context, Result, anyhow};
use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use chrono::{Duration, Utc};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Serialize;
use sqlx::SqlitePool;
use uuid::Uuid;

pub const SESSION_COOKIE: &str = "selu_session";
pub const SESSION_TTL_DAYS: i64 = 7;

/// User information resolved from a valid session.
/// Shared by page, JSON API, and mobile authentication.
#[derive(Debug, Clone, Serialize)]
pub struct SessionUser {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub is_admin: bool,
    pub language: String,
}

#[derive(Debug)]
pub struct SessionGrant {
    pub session_id: String,
    pub user: SessionUser,
}

#[derive(Debug)]
pub enum SetupOutcome {
    Created(SessionGrant),
    AlreadyConfigured,
}

/// Resolve a session ID against the database.
pub async fn resolve_session(db: &SqlitePool, session_id: &str) -> Result<Option<SessionUser>> {
    let row = sqlx::query(
        r#"SELECT ws.user_id, u.username, u.display_name, u.is_admin, u.language
           FROM web_sessions ws
           JOIN users u ON u.id = ws.user_id
           WHERE ws.id = ? AND ws.expires_at > datetime('now')"#,
    )
    .bind(session_id)
    .fetch_optional(db)
    .await
    .context("failed to validate web session")?;

    use sqlx::Row;
    row.map(|row| {
        Ok::<SessionUser, sqlx::Error>(SessionUser {
            user_id: row.try_get("user_id")?,
            username: row.try_get("username")?,
            display_name: row.try_get("display_name")?,
            is_admin: row.try_get::<i64, _>("is_admin")? != 0,
            language: row.try_get("language")?,
        })
    })
    .transpose()
    .context("failed to decode session user")
}

/// Backwards-compatible validation used by legacy page extractors.
pub async fn validate_session(db: &SqlitePool, session_id: &str) -> Option<SessionUser> {
    resolve_session(db, session_id).await.ok().flatten()
}

pub async fn users_exist(db: &SqlitePool) -> Result<bool> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(db)
        .await
        .context("failed to inspect authentication setup state")?;
    Ok(count > 0)
}

pub async fn login(
    db: &SqlitePool,
    username: &str,
    password: &str,
) -> Result<Option<SessionGrant>> {
    use sqlx::Row;

    let username = username.trim();
    let row = sqlx::query(
        "SELECT id, username, display_name, password_hash, is_admin, language FROM users WHERE username = ?",
    )
    .bind(username)
    .fetch_optional(db)
    .await
    .context("failed to look up login user")?;

    let Some(row) = row else {
        return Ok(None);
    };
    let password_hash: String = row
        .try_get("password_hash")
        .context("failed to decode password hash")?;
    let Ok(hash) = PasswordHash::new(&password_hash) else {
        return Ok(None);
    };
    if Argon2::default()
        .verify_password(password.as_bytes(), &hash)
        .is_err()
    {
        return Ok(None);
    }

    let user = SessionUser {
        user_id: row.try_get("id").context("failed to decode user id")?,
        username: row
            .try_get("username")
            .context("failed to decode username")?,
        display_name: row
            .try_get("display_name")
            .context("failed to decode display name")?,
        is_admin: row
            .try_get::<i64, _>("is_admin")
            .context("failed to decode admin flag")?
            != 0,
        language: row
            .try_get("language")
            .context("failed to decode language")?,
    };
    let session_id = create_session(db, &user.user_id).await?;
    Ok(Some(SessionGrant { session_id, user }))
}

pub async fn create_session(db: &SqlitePool, user_id: &str) -> Result<String> {
    let session_id = Uuid::new_v4().to_string();
    let expires_at = session_expiry();
    sqlx::query("INSERT INTO web_sessions (id, user_id, expires_at) VALUES (?, ?, ?)")
        .bind(&session_id)
        .bind(user_id)
        .bind(expires_at)
        .execute(db)
        .await
        .context("failed to create web session")?;
    Ok(session_id)
}

pub async fn delete_session(db: &SqlitePool, session_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM web_sessions WHERE id = ?")
        .bind(session_id)
        .execute(db)
        .await
        .context("failed to delete web session")?;
    Ok(())
}

/// Atomically creates the first administrator, their default web pipe, provider
/// catalogue entries, and the automatic login session. `BEGIN IMMEDIATE`
/// acquires SQLite's write lock before checking the user table, so concurrent
/// setup attempts serialize and only one can create the first user.
pub async fn setup_first_admin(
    db: &SqlitePool,
    username: &str,
    display_name: &str,
    password: &str,
    language: &str,
) -> Result<SetupOutcome> {
    let username = username.trim();
    if username.is_empty() || password.is_empty() {
        return Err(anyhow!("username and password are required"));
    }
    let display_name = if display_name.trim().is_empty() {
        username
    } else {
        display_name.trim()
    };
    let language = if language.eq_ignore_ascii_case("de") {
        "de"
    } else {
        "en"
    };
    let password_hash = hash_password(password)?;
    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let expires_at = session_expiry();

    let mut tx = db
        .begin_with("BEGIN IMMEDIATE")
        .await
        .context("failed to begin first-admin setup transaction")?;

    let inserted = sqlx::query(
        r#"INSERT INTO users (id, username, display_name, password_hash, is_admin, language)
           SELECT ?, ?, ?, ?, 1, ?
           WHERE NOT EXISTS (SELECT 1 FROM users)"#,
    )
    .bind(&user_id)
    .bind(username)
    .bind(display_name)
    .bind(password_hash)
    .bind(language)
    .execute(&mut *tx)
    .await
    .context("failed to create first administrator")?;

    if inserted.rows_affected() == 0 {
        tx.rollback()
            .await
            .context("failed to close rejected setup transaction")?;
        return Ok(SetupOutcome::AlreadyConfigured);
    }

    let pipe_id = Uuid::new_v4().to_string();
    let pipe_name = format!("{}'s Chat", display_name);
    sqlx::query(
        r#"INSERT INTO pipes
           (id, user_id, name, transport, inbound_token, outbound_url, default_agent_id)
           VALUES (?, ?, ?, 'web', ?, 'internal://web', NULL)"#,
    )
    .bind(pipe_id)
    .bind(&user_id)
    .bind(pipe_name)
    .bind("")
    .execute(&mut *tx)
    .await
    .context("failed to create first administrator web pipe")?;

    for (id, name, base_url) in [
        ("bedrock", "Amazon Bedrock", ""),
        ("anthropic", "Anthropic Claude", ""),
        ("openai", "OpenAI", ""),
        ("grok", "xAI Grok", "https://api.x.ai"),
    ] {
        sqlx::query(
            r#"INSERT OR IGNORE INTO llm_providers
               (id, display_name, api_key_encrypted, base_url, active)
               VALUES (?, ?, '', ?, 1)"#,
        )
        .bind(id)
        .bind(name)
        .bind(base_url)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("failed to seed provider {id}"))?;
    }

    sqlx::query("INSERT INTO web_sessions (id, user_id, expires_at) VALUES (?, ?, ?)")
        .bind(&session_id)
        .bind(&user_id)
        .bind(expires_at)
        .execute(&mut *tx)
        .await
        .context("failed to create first administrator session")?;

    tx.commit()
        .await
        .context("failed to commit first-admin setup")?;

    Ok(SetupOutcome::Created(SessionGrant {
        session_id,
        user: SessionUser {
            user_id,
            username: username.to_string(),
            display_name: display_name.to_string(),
            is_admin: true,
            language: language.to_string(),
        },
    }))
}

fn hash_password(password: &str) -> Result<String> {
    let rng = SystemRandom::new();
    let mut salt = [0_u8; 16];
    rng.fill(&mut salt)
        .map_err(|_| anyhow!("failed to generate password salt"))?;
    Argon2::default()
        .hash_password_with_salt(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| anyhow!("failed to hash password: {error}"))
}

fn session_expiry() -> String {
    (Utc::now() + Duration::days(SESSION_TTL_DAYS))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
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

    #[tokio::test]
    async fn setup_creates_complete_first_admin_and_session() {
        let db = test_db().await;
        let SetupOutcome::Created(grant) = setup_first_admin(&db, "owner", "Owner", "secret", "en")
            .await
            .unwrap()
        else {
            panic!("setup should create the first user")
        };

        assert!(grant.user.is_admin);
        assert_eq!(grant.user.username, "owner");
        assert_eq!(grant.user.language, "en");
        assert!(
            resolve_session(&db, &grant.session_id)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM pipes WHERE user_id = ? AND transport = 'web' AND active = 1",
            )
            .bind(&grant.user.user_id)
            .fetch_one(&db)
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM llm_providers")
                .fetch_one(&db)
                .await
                .unwrap(),
            4
        );
    }

    #[tokio::test]
    async fn setup_never_creates_a_second_admin() {
        let db = test_db().await;
        assert!(matches!(
            setup_first_admin(&db, "first", "First", "secret", "en")
                .await
                .unwrap(),
            SetupOutcome::Created(_)
        ));
        assert!(matches!(
            setup_first_admin(&db, "second", "Second", "secret", "de")
                .await
                .unwrap(),
            SetupOutcome::AlreadyConfigured
        ));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users")
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn login_rejects_bad_password_and_creates_valid_session() {
        let db = test_db().await;
        setup_first_admin(&db, "owner", "Owner", "secret", "en")
            .await
            .unwrap();

        assert!(login(&db, "owner", "wrong").await.unwrap().is_none());
        let grant = login(&db, "owner", "secret").await.unwrap().unwrap();
        assert_eq!(grant.user.username, "owner");
        assert!(
            resolve_session(&db, &grant.session_id)
                .await
                .unwrap()
                .is_some()
        );
    }
}
