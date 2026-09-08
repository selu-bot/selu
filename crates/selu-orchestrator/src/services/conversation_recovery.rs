//! Startup recovery for durable conversation runs whose executors lived in a
//! previous process.
//!
//! Conversation execution and approvals are process-local. Consequently, any
//! active run present when a new orchestrator starts is orphaned and cannot
//! resume. Reconciliation makes that fact durable before the API starts.

use anyhow::{Context, Result};
use sqlx::SqlitePool;

const INTERRUPTED_ON_RESTART: &str = "conversation.run_interrupted.restart";

/// Atomically mark runs orphaned by the previous process as interrupted and
/// append the matching terminal events. Safe to call repeatedly.
pub async fn reconcile_orphaned_runs(db: &SqlitePool) -> Result<u64> {
    let mut tx = db
        .begin()
        .await
        .context("begin orphaned conversation run reconciliation")?;
    let orphaned = sqlx::query_as::<_, (String, String, String)>(
        r#"SELECT id, thread_id, user_id
           FROM conversation_runs
           WHERE status IN ('queued','running','waiting_for_approval','cancelling')"#,
    )
    .fetch_all(&mut *tx)
    .await
    .context("list orphaned conversation runs")?;
    let completed_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut interrupted = 0;

    for (run_id, thread_id, user_id) in orphaned {
        let changed = sqlx::query(
            r#"UPDATE conversation_runs
               SET status = 'interrupted', error_code = ?,
                   completed_at = COALESCE(completed_at, ?)
               WHERE id = ?
                 AND status IN ('queued','running','waiting_for_approval','cancelling')"#,
        )
        .bind(INTERRUPTED_ON_RESTART)
        .bind(&completed_at)
        .bind(&run_id)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("interrupt orphaned conversation run {run_id}"))?;
        if changed.rows_affected() == 0 {
            continue;
        }

        let payload_json = serde_json::to_string(&serde_json::json!({
            "run_id": run_id,
            "status": "interrupted",
            "error_code": INTERRUPTED_ON_RESTART,
        }))
        .context("serialize interrupted conversation run event")?;
        sqlx::query(
            r#"INSERT INTO conversation_events
               (user_id, thread_id, run_id, event_type, entity_id, payload_json)
               VALUES (?, ?, ?, 'run.updated', ?, ?)"#,
        )
        .bind(&user_id)
        .bind(&thread_id)
        .bind(&run_id)
        .bind(&run_id)
        .bind(payload_json)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("journal interrupted conversation run {run_id}"))?;
        interrupted += 1;
    }

    tx.commit()
        .await
        .context("commit orphaned conversation run reconciliation")?;
    Ok(interrupted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::conversations::{
        delete_conversation, get_conversation, list_conversations,
    };
    use sqlx::sqlite::SqlitePoolOptions;

    async fn setup_db() -> (SqlitePool, String, String) {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();

        let user_id = uuid::Uuid::new_v4().to_string();
        let pipe_id = uuid::Uuid::new_v4().to_string();
        let thread_id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO users (id, username, display_name, password_hash) VALUES (?, ?, 'Test', 'x')")
            .bind(&user_id)
            .bind(&user_id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pipes (id, user_id, name, transport, inbound_token, outbound_url) VALUES (?, ?, 'Web', 'web', 't', '')")
            .bind(&pipe_id)
            .bind(&user_id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO sessions (id, pipe_id, user_id, agent_id) VALUES ('session', ?, ?, 'default')")
            .bind(&pipe_id)
            .bind(&user_id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO threads (id, pipe_id, session_id, user_id, status, thread_kind) VALUES (?, ?, 'session', ?, 'active', 'conversation')")
            .bind(&thread_id)
            .bind(&pipe_id)
            .bind(&user_id)
            .execute(&db)
            .await
            .unwrap();
        (db, user_id, thread_id)
    }

    async fn seed_run(db: &SqlitePool, thread_id: &str, user_id: &str, status: &str) {
        sqlx::query("INSERT INTO conversation_runs (id, thread_id, user_id, client_message_id, status) VALUES (?, ?, ?, ?, ?)")
            .bind(format!("run-{status}"))
            .bind(thread_id)
            .bind(user_id)
            .bind(format!("client-{status}"))
            .bind(status)
            .execute(db)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn reconciliation_interrupts_only_active_runs_and_is_idempotent() {
        let (db, user_id, thread_id) = setup_db().await;
        let active = ["queued", "running", "waiting_for_approval", "cancelling"];
        let terminal = ["completed", "failed", "cancelled", "interrupted"];
        for status in active.into_iter().chain(terminal) {
            seed_run(&db, &thread_id, &user_id, status).await;
        }

        assert_eq!(reconcile_orphaned_runs(&db).await.unwrap(), 4);
        assert_eq!(reconcile_orphaned_runs(&db).await.unwrap(), 0);

        for status in active {
            let row = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
                "SELECT status, error_code, completed_at FROM conversation_runs WHERE id = ?",
            )
            .bind(format!("run-{status}"))
            .fetch_one(&db)
            .await
            .unwrap();
            assert_eq!(row.0, "interrupted");
            assert_eq!(row.1.as_deref(), Some(INTERRUPTED_ON_RESTART));
            assert!(row.2.is_some());
        }
        for status in terminal {
            let persisted: String =
                sqlx::query_scalar("SELECT status FROM conversation_runs WHERE id = ?")
                    .bind(format!("run-{status}"))
                    .fetch_one(&db)
                    .await
                    .unwrap();
            assert_eq!(persisted, status);
        }

        let events = sqlx::query_as::<_, (String, String)>(
            "SELECT run_id, payload_json FROM conversation_events WHERE event_type = 'run.updated' ORDER BY run_id",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(events.len(), 4);
        for (run_id, payload_json) in events {
            let payload: serde_json::Value = serde_json::from_str(&payload_json).unwrap();
            assert_eq!(payload["run_id"], run_id);
            assert_eq!(payload["status"], "interrupted");
            assert_eq!(payload["error_code"], INTERRUPTED_ON_RESTART);
        }

        let active_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_runs WHERE thread_id = ? AND status IN ('queued','running','waiting_for_approval','cancelling')",
        )
        .bind(&thread_id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(active_count, 0);
        assert_eq!(
            get_conversation(&db, &user_id, &thread_id)
                .await
                .unwrap()
                .unwrap()
                .active_run_id,
            None
        );
        assert_eq!(
            list_conversations(&db, &user_id, 10, None)
                .await
                .unwrap()
                .conversations[0]
                .active_run_id,
            None
        );

        delete_conversation(&db, &user_id, &thread_id)
            .await
            .unwrap();
        assert!(
            get_conversation(&db, &user_id, &thread_id)
                .await
                .unwrap()
                .is_none()
        );
    }
}
