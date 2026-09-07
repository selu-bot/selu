use tracing::error;
use uuid::Uuid;

/// Ensure the given user has an active browser-chat pipe. Browser-chat pipes do
/// not expose the generic inbound webhook, so they intentionally store no token.
pub async fn ensure_web_pipe(db: &sqlx::SqlitePool, user_id: &str, display_name: &str) {
    let existing = sqlx::query_scalar::<_, String>(
        "SELECT id FROM pipes WHERE user_id = ? AND transport = 'web' AND active = 1 LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(db)
    .await;
    if matches!(existing, Ok(Some(_))) {
        return;
    }

    let id = Uuid::new_v4().to_string();
    let pipe_name = format!("{display_name}'s Chat");
    if let Err(error) = sqlx::query(
        "INSERT INTO pipes \
         (id, user_id, name, transport, inbound_token, outbound_url, default_agent_id) \
         VALUES (?, ?, ?, 'web', '', 'internal://web', NULL)",
    )
    .bind(id)
    .bind(user_id)
    .bind(pipe_name)
    .execute(db)
    .await
    {
        error!(user_id, error = %error, "Failed to create browser-chat pipe");
    }
}

pub async fn backfill_web_pipes(db: &sqlx::SqlitePool) {
    use sqlx::Row as _;
    let users = match sqlx::query("SELECT id, display_name FROM users")
        .fetch_all(db)
        .await
    {
        Ok(users) => users,
        Err(error) => {
            error!(error = %error, "Failed to load users for browser-chat pipe backfill");
            return;
        }
    };
    for user in users {
        let id: String = user.try_get("id").unwrap_or_default();
        let display_name: String = user.try_get("display_name").unwrap_or_default();
        ensure_web_pipe(db, &id, &display_name).await;
    }
}
