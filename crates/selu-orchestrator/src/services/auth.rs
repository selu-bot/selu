use sqlx::SqlitePool;

/// User information resolved from a valid session.
/// Shared between web (AuthUser extractor) and mobile (extract_mobile_user).
pub struct SessionUser {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub is_admin: bool,
    pub language: String,
}

/// Validate a session ID against the database. Returns the associated user
/// if the session exists and has not expired.
pub async fn validate_session(db: &SqlitePool, session_id: &str) -> Option<SessionUser> {
    let row = sqlx::query!(
        r#"SELECT ws.user_id, u.username, u.display_name, u.is_admin, u.language
           FROM web_sessions ws
           JOIN users u ON u.id = ws.user_id
           WHERE ws.id = ? AND ws.expires_at > datetime('now')"#,
        session_id
    )
    .fetch_optional(db)
    .await
    .ok()??;

    Some(SessionUser {
        user_id: row.user_id,
        username: row.username,
        display_name: row.display_name,
        is_admin: row.is_admin != 0,
        language: row.language,
    })
}
