use anyhow::{Context, Result};
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
use tracing::info;

pub async fn connect(database_url: &str) -> Result<SqlitePool> {
    info!("Connecting to database: {}", database_url);

    let pool = SqlitePoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await?;

    // Enable WAL mode for better concurrent read performance
    sqlx::query("PRAGMA journal_mode=WAL;")
        .execute(&pool)
        .await?;
    // Enforce foreign key constraints (off by default in SQLite)
    sqlx::query("PRAGMA foreign_keys=ON;")
        .execute(&pool)
        .await?;
    info!("SQLite pragmas set: journal_mode=WAL, foreign_keys=ON");

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
