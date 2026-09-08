use std::{str::FromStr, time::Duration};

use anyhow::{Context, Result, bail};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use tracing::info;

const MAX_CONNECTIONS: u32 = 10;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn connect(database_url: &str) -> Result<SqlitePool> {
    info!("Connecting to database: {}", database_url);

    let options = SqliteConnectOptions::from_str(database_url)
        .context("invalid SQLite database URL")?
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(BUSY_TIMEOUT);
    let pool = SqlitePoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .connect_with(options)
        .await?;

    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&pool)
        .await?;
    let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&pool)
        .await?;
    let busy_timeout_ms: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(&pool)
        .await?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        bail!("SQLite refused WAL mode; effective journal mode is '{journal_mode}'");
    }
    if foreign_keys != 1 {
        bail!("SQLite foreign-key enforcement is disabled");
    }
    info!(
        journal_mode,
        foreign_keys, busy_timeout_ms, "SQLite connection settings verified"
    );

    run_migrations(&pool).await?;

    Ok(pool)
}

async fn run_migrations(pool: &SqlitePool) -> Result<()> {
    info!("Running database migrations");
    sqlx::migrate!("./migrations").run(pool).await?;
    info!("Migrations complete");
    Ok(())
}

pub async fn get_instance_id(db: &SqlitePool) -> Result<String> {
    let id = sqlx::query_scalar::<_, String>(
        "SELECT value FROM instance_meta WHERE key = 'instance_id'",
    )
    .fetch_one(db)
    .await
    .context("Failed to load instance_id from instance_meta")?;

    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_pool_connection_uses_required_sqlite_settings() {
        let root = std::env::var_os("KIROCREW_SCRATCH")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let db_path = root.join(format!("selu-db-settings-{}.db", uuid::Uuid::new_v4()));
        let database_url = format!("sqlite://{}?mode=rwc", db_path.display());
        let pool = connect(&database_url).await.expect("connect test database");

        let mut connections = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            connections.push(pool.acquire().await.expect("acquire pooled connection"));
        }
        for connection in &mut connections {
            let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
                .fetch_one(&mut **connection)
                .await
                .expect("read journal mode");
            let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
                .fetch_one(&mut **connection)
                .await
                .expect("read foreign-key setting");
            let busy_timeout_ms: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
                .fetch_one(&mut **connection)
                .await
                .expect("read busy timeout");

            assert_eq!(journal_mode, "wal");
            assert_eq!(foreign_keys, 1);
            assert_eq!(busy_timeout_ms, BUSY_TIMEOUT.as_millis() as i64);
        }

        drop(connections);
        pool.close().await;
        for path in [
            db_path.clone(),
            db_path.with_extension("db-wal"),
            db_path.with_extension("db-shm"),
        ] {
            let _ = std::fs::remove_file(path);
        }
    }
}
