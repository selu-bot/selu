use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    WaitingForInput,
    Succeeded,
    Failed,
    Cancelling,
    Cancelled,
    Interrupted,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::WaitingForInput => "waiting_for_input",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelling => "cancelling",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    fn parse(value: &str) -> Result<Self, JobServiceError> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "waiting_for_input" => Ok(Self::WaitingForInput),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelling" => Ok(Self::Cancelling),
            "cancelled" => Ok(Self::Cancelled),
            "interrupted" => Ok(Self::Interrupted),
            _ => Err(JobServiceError::CorruptStatus(value.to_owned())),
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: String,
    pub owner_user_id: Option<String>,
    pub kind: String,
    pub resource_id: String,
    pub status: JobStatus,
    pub progress: i64,
    pub message_code: Option<String>,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub idempotency_key: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct JobActor<'a> {
    pub user_id: &'a str,
    pub is_admin: bool,
}

impl<'a> JobActor<'a> {
    pub fn new(user_id: &'a str, is_admin: bool) -> Self {
        Self { user_id, is_admin }
    }
}

/// Input used by internal long-running job producers. The public Jobs API only
/// exposes status and cancellation; producers call this service directly.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone)]
pub struct NewJob<'a> {
    pub owner_user_id: Option<&'a str>,
    pub kind: &'a str,
    pub resource_id: &'a str,
    pub idempotency_key: Option<&'a str>,
    pub message_code: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct JobUpdate<'a> {
    pub status: JobStatus,
    pub message_code: Option<&'a str>,
}

/// Returned to internal job producers so idempotent retries can distinguish a
/// reused job from a newly created one.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone)]
pub struct CreateJobOutcome {
    pub job: Job,
    pub created: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum JobServiceError {
    #[error("job not found")]
    NotFound,
    /// Internal producers may only create jobs for themselves unless acting as
    /// an administrator. The public Jobs API never accepts job creation input.
    #[cfg_attr(not(test), allow(dead_code))]
    #[error("job operation is forbidden")]
    Forbidden,
    #[error("invalid job input: {0}")]
    InvalidInput(&'static str),
    #[error("invalid job state transition from {from:?} to {to:?}")]
    InvalidTransition { from: JobStatus, to: JobStatus },
    #[error("concurrent job update")]
    Conflict,
    #[error("invalid persisted job status: {0}")]
    CorruptStatus(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Clone)]
pub struct JobService {
    db: SqlitePool,
}

// Creation and lifecycle transitions are called by internal long-running job
// producers, not by the read/cancel-only HTTP API.
#[cfg_attr(not(test), allow(dead_code))]
impl JobService {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    pub async fn create(
        &self,
        actor: JobActor<'_>,
        input: NewJob<'_>,
    ) -> Result<CreateJobOutcome, JobServiceError> {
        validate_identifier(input.kind, "kind")?;
        validate_identifier(input.resource_id, "resource_id")?;
        validate_optional_code(input.message_code)?;
        validate_optional_idempotency_key(input.idempotency_key)?;
        if !actor.is_admin && input.owner_user_id != Some(actor.user_id) {
            return Err(JobServiceError::Forbidden);
        }

        if let Some(key) = input.idempotency_key
            && let Some(job) = self
                .find_idempotent(actor, input.owner_user_id, input.kind, key)
                .await?
        {
            return Ok(CreateJobOutcome {
                job,
                created: false,
            });
        }

        let id = Uuid::new_v4().to_string();
        let insert = sqlx::query!(
            r#"INSERT INTO jobs
               (id, owner_user_id, kind, resource_id, status, message_code, idempotency_key)
               VALUES (?, ?, ?, ?, 'queued', ?, ?)"#,
            id,
            input.owner_user_id,
            input.kind,
            input.resource_id,
            input.message_code,
            input.idempotency_key,
        )
        .execute(&self.db)
        .await;

        if let Err(error) = insert {
            if input.idempotency_key.is_some()
                && error
                    .as_database_error()
                    .is_some_and(|database| database.is_unique_violation())
                && let Some(job) = self
                    .find_idempotent(
                        actor,
                        input.owner_user_id,
                        input.kind,
                        input.idempotency_key.unwrap_or_default(),
                    )
                    .await?
            {
                return Ok(CreateJobOutcome {
                    job,
                    created: false,
                });
            }
            return Err(error.into());
        }

        Ok(CreateJobOutcome {
            job: self.get(actor, &id).await?,
            created: true,
        })
    }

    pub async fn get(&self, actor: JobActor<'_>, job_id: &str) -> Result<Job, JobServiceError> {
        let admin = i64::from(actor.is_admin);
        let row = sqlx::query!(
            r#"SELECT id AS "id!", owner_user_id, kind, resource_id, status, progress,
                      message_code, result_json, error_json, idempotency_key,
                      created_at, updated_at, started_at, completed_at
               FROM jobs
               WHERE id = ? AND (? = 1 OR owner_user_id = ?)"#,
            job_id,
            admin,
            actor.user_id,
        )
        .fetch_optional(&self.db)
        .await?;

        let row = row.ok_or(JobServiceError::NotFound)?;
        row_to_job(
            row.id,
            row.owner_user_id,
            row.kind,
            row.resource_id,
            row.status,
            row.progress,
            row.message_code,
            row.result_json,
            row.error_json,
            row.idempotency_key,
            row.created_at,
            row.updated_at,
            row.started_at,
            row.completed_at,
        )
    }

    pub async fn update(
        &self,
        actor: JobActor<'_>,
        job_id: &str,
        update: JobUpdate<'_>,
    ) -> Result<Job, JobServiceError> {
        validate_optional_code(update.message_code)?;
        let current = self.get(actor, job_id).await?;
        if current.status != update.status && !can_transition(current.status, update.status) {
            return Err(JobServiceError::InvalidTransition {
                from: current.status,
                to: update.status,
            });
        }
        let admin = i64::from(actor.is_admin);
        let status = update.status.as_str();
        let current_status = current.status.as_str();
        let terminal = i64::from(update.status.is_terminal());
        let running = i64::from(update.status == JobStatus::Running);
        let result = sqlx::query!(
            r#"UPDATE jobs
               SET status = ?, message_code = ?,
                   started_at = CASE WHEN ? = 1 THEN COALESCE(started_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) ELSE started_at END,
                   completed_at = CASE WHEN ? = 1 THEN COALESCE(completed_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) ELSE NULL END,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? AND status = ? AND (? = 1 OR owner_user_id = ?)"#,
            status,
            update.message_code,
            running,
            terminal,
            job_id,
            current_status,
            admin,
            actor.user_id,
        )
        .execute(&self.db)
        .await?;
        if result.rows_affected() != 1 {
            return Err(JobServiceError::Conflict);
        }
        self.get(actor, job_id).await
    }

    pub async fn progress(
        &self,
        actor: JobActor<'_>,
        job_id: &str,
        progress: i64,
        message_code: Option<&str>,
    ) -> Result<Job, JobServiceError> {
        if !(0..=100).contains(&progress) {
            return Err(JobServiceError::InvalidInput(
                "progress must be from 0 to 100",
            ));
        }
        validate_optional_code(message_code)?;
        let current = self.get(actor, job_id).await?;
        if current.status.is_terminal() {
            return Err(JobServiceError::InvalidTransition {
                from: current.status,
                to: current.status,
            });
        }
        let admin = i64::from(actor.is_admin);
        let status = current.status.as_str();
        let result = sqlx::query!(
            r#"UPDATE jobs
               SET progress = ?, message_code = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? AND status = ? AND (? = 1 OR owner_user_id = ?)"#,
            progress,
            message_code,
            job_id,
            status,
            admin,
            actor.user_id,
        )
        .execute(&self.db)
        .await?;
        if result.rows_affected() != 1 {
            return Err(JobServiceError::Conflict);
        }
        self.get(actor, job_id).await
    }

    pub async fn complete(
        &self,
        actor: JobActor<'_>,
        job_id: &str,
        result: Value,
    ) -> Result<Job, JobServiceError> {
        let current = self.get(actor, job_id).await?;
        if !matches!(
            current.status,
            JobStatus::Running | JobStatus::WaitingForInput
        ) {
            return Err(JobServiceError::InvalidTransition {
                from: current.status,
                to: JobStatus::Succeeded,
            });
        }
        let result_json = serde_json::to_string(&sanitize_json(result))
            .context("serialize sanitized job result")?;
        let admin = i64::from(actor.is_admin);
        let status = current.status.as_str();
        let changed = sqlx::query!(
            r#"UPDATE jobs
               SET status = 'succeeded', progress = 100, result_json = ?, error_json = NULL,
                   completed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? AND status = ? AND (? = 1 OR owner_user_id = ?)"#,
            result_json,
            job_id,
            status,
            admin,
            actor.user_id,
        )
        .execute(&self.db)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(JobServiceError::Conflict);
        }
        self.get(actor, job_id).await
    }

    pub async fn fail(
        &self,
        actor: JobActor<'_>,
        job_id: &str,
        error: Value,
    ) -> Result<Job, JobServiceError> {
        let current = self.get(actor, job_id).await?;
        if current.status.is_terminal() {
            return Err(JobServiceError::InvalidTransition {
                from: current.status,
                to: JobStatus::Failed,
            });
        }
        let error_json = serde_json::to_string(&sanitize_json(error))
            .context("serialize sanitized job error")?;
        self.finish_with_payload(actor, job_id, current.status, "failed", error_json)
            .await
    }

    pub async fn request_cancel(
        &self,
        actor: JobActor<'_>,
        job_id: &str,
    ) -> Result<Job, JobServiceError> {
        let current = self.get(actor, job_id).await?;
        if current.status.is_terminal() || current.status == JobStatus::Cancelling {
            return Ok(current);
        }
        let next = if matches!(
            current.status,
            JobStatus::Queued | JobStatus::WaitingForInput
        ) {
            JobStatus::Cancelled
        } else {
            JobStatus::Cancelling
        };
        self.update(
            actor,
            job_id,
            JobUpdate {
                status: next,
                message_code: Some("job.cancellation_requested"),
            },
        )
        .await
    }

    pub async fn mark_interrupted(
        &self,
        actor: JobActor<'_>,
        job_id: &str,
    ) -> Result<Job, JobServiceError> {
        let current = self.get(actor, job_id).await?;
        if current.status.is_terminal() {
            return Ok(current);
        }
        self.update(
            actor,
            job_id,
            JobUpdate {
                status: JobStatus::Interrupted,
                message_code: Some("job.interrupted"),
            },
        )
        .await
    }

    /// Reconcile work that could have been executing when the process stopped.
    /// Queued and waiting jobs remain durable and resumable.
    pub async fn reconcile_after_restart(&self) -> Result<u64, JobServiceError> {
        let changed = sqlx::query!(
            r#"UPDATE jobs
               SET status = 'interrupted', message_code = 'job.interrupted.restart',
                   completed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE status IN ('running', 'cancelling')"#,
        )
        .execute(&self.db)
        .await?;
        Ok(changed.rows_affected())
    }

    async fn finish_with_payload(
        &self,
        actor: JobActor<'_>,
        job_id: &str,
        current: JobStatus,
        target: &str,
        payload_json: String,
    ) -> Result<Job, JobServiceError> {
        let admin = i64::from(actor.is_admin);
        let current_status = current.as_str();
        let changed = sqlx::query!(
            r#"UPDATE jobs
               SET status = ?, error_json = ?, result_json = NULL,
                   completed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                   updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
               WHERE id = ? AND status = ? AND (? = 1 OR owner_user_id = ?)"#,
            target,
            payload_json,
            job_id,
            current_status,
            admin,
            actor.user_id,
        )
        .execute(&self.db)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(JobServiceError::Conflict);
        }
        self.get(actor, job_id).await
    }

    async fn find_idempotent(
        &self,
        actor: JobActor<'_>,
        owner_user_id: Option<&str>,
        kind: &str,
        key: &str,
    ) -> Result<Option<Job>, JobServiceError> {
        let row = sqlx::query_scalar!(
            "SELECT id AS \"id!\" FROM jobs WHERE owner_user_id IS ? AND kind = ? AND idempotency_key = ?",
            owner_user_id,
            kind,
            key,
        )
        .fetch_optional(&self.db)
        .await?;
        match row {
            Some(id) => self.get(actor, &id).await.map(Some),
            None => Ok(None),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn row_to_job(
    id: String,
    owner_user_id: Option<String>,
    kind: String,
    resource_id: String,
    status: String,
    progress: i64,
    message_code: Option<String>,
    result_json: Option<String>,
    error_json: Option<String>,
    idempotency_key: Option<String>,
    created_at: String,
    updated_at: String,
    started_at: Option<String>,
    completed_at: Option<String>,
) -> Result<Job, JobServiceError> {
    let parse_json = |raw: Option<String>| -> Result<Option<Value>, JobServiceError> {
        raw.map(|value| {
            serde_json::from_str(&value)
                .context("decode persisted job payload")
                .map(sanitize_json)
                .map_err(JobServiceError::from)
        })
        .transpose()
    };
    Ok(Job {
        id,
        owner_user_id,
        kind,
        resource_id,
        status: JobStatus::parse(&status)?,
        progress,
        message_code,
        result: parse_json(result_json)?,
        error: parse_json(error_json)?,
        idempotency_key,
        created_at,
        updated_at,
        started_at,
        completed_at,
    })
}

fn can_transition(from: JobStatus, to: JobStatus) -> bool {
    match from {
        JobStatus::Queued => matches!(
            to,
            JobStatus::Running
                | JobStatus::WaitingForInput
                | JobStatus::Failed
                | JobStatus::Cancelled
                | JobStatus::Interrupted
        ),
        JobStatus::Running => matches!(
            to,
            JobStatus::WaitingForInput
                | JobStatus::Succeeded
                | JobStatus::Failed
                | JobStatus::Cancelling
                | JobStatus::Interrupted
        ),
        JobStatus::WaitingForInput => matches!(
            to,
            JobStatus::Running
                | JobStatus::Succeeded
                | JobStatus::Failed
                | JobStatus::Cancelled
                | JobStatus::Interrupted
        ),
        JobStatus::Cancelling => matches!(
            to,
            JobStatus::Failed | JobStatus::Cancelled | JobStatus::Interrupted
        ),
        JobStatus::Succeeded
        | JobStatus::Failed
        | JobStatus::Cancelled
        | JobStatus::Interrupted => false,
    }
}

fn validate_identifier(value: &str, field: &'static str) -> Result<(), JobServiceError> {
    if value.is_empty() || value.len() > 255 {
        return Err(JobServiceError::InvalidInput(field));
    }
    Ok(())
}

fn validate_optional_idempotency_key(value: Option<&str>) -> Result<(), JobServiceError> {
    if value.is_some_and(|value| value.is_empty() || value.len() > 255) {
        return Err(JobServiceError::InvalidInput("idempotency_key"));
    }
    Ok(())
}

fn validate_optional_code(value: Option<&str>) -> Result<(), JobServiceError> {
    if value.is_some_and(|value| {
        value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    }) {
        return Err(JobServiceError::InvalidInput("message_code"));
    }
    Ok(())
}

fn sanitize_json(mut value: Value) -> Value {
    fn is_secret_key(key: &str) -> bool {
        let normalized: String = key
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        [
            "secret",
            "password",
            "token",
            "authorization",
            "cookie",
            "credential",
            "apikey",
            "privatekey",
        ]
        .iter()
        .any(|needle| normalized.contains(needle))
    }

    match &mut value {
        Value::Object(object) => {
            for (key, nested) in object {
                if is_secret_key(key) {
                    *nested = Value::String("[redacted]".to_owned());
                } else {
                    *nested = sanitize_json(nested.take());
                }
            }
        }
        Value::Array(array) => {
            for nested in array {
                *nested = sanitize_json(nested.take());
            }
        }
        _ => {}
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn setup_db() -> SqlitePool {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        sqlx::query!(
            "INSERT INTO users (id, username, display_name, password_hash, is_admin, language) VALUES ('u1', 'one', 'One', 'x', 0, 'en'), ('u2', 'two', 'Two', 'x', 0, 'en'), ('admin', 'admin', 'Admin', 'x', 1, 'en')"
        )
        .execute(&db)
        .await
        .unwrap();
        db
    }

    fn input<'a>(owner: &'a str, key: Option<&'a str>) -> NewJob<'a> {
        NewJob {
            owner_user_id: Some(owner),
            kind: "test.export",
            resource_id: "resource-1",
            idempotency_key: key,
            message_code: Some("job.queued"),
        }
    }

    #[tokio::test]
    async fn lifecycle_is_durable_and_idempotent() {
        let db = setup_db().await;
        let service = JobService::new(db);
        let actor = JobActor::new("u1", false);
        let created = service
            .create(actor, input("u1", Some("request-1")))
            .await
            .unwrap();
        assert!(created.created);
        let replay = service
            .create(actor, input("u1", Some("request-1")))
            .await
            .unwrap();
        assert!(!replay.created);
        assert_eq!(replay.job.id, created.job.id);

        let running = service
            .update(
                actor,
                &created.job.id,
                JobUpdate {
                    status: JobStatus::Running,
                    message_code: Some("job.running"),
                },
            )
            .await
            .unwrap();
        assert!(running.started_at.is_some());
        let progressed = service
            .progress(actor, &running.id, 42, Some("job.processing"))
            .await
            .unwrap();
        assert_eq!(progressed.progress, 42);
        let completed = service
            .complete(
                actor,
                &running.id,
                json!({
                    "artifact_id": "a1",
                    "api_token": "do-not-store",
                    "nested": {"clientSecret": "also-do-not-store"}
                }),
            )
            .await
            .unwrap();
        assert_eq!(completed.status, JobStatus::Succeeded);
        assert_eq!(completed.progress, 100);
        let result = completed.result.unwrap();
        assert_eq!(result["api_token"], "[redacted]");
        assert_eq!(result["nested"]["clientSecret"], "[redacted]");
        assert!(completed.completed_at.is_some());

        let interrupted = service.create(actor, input("u1", None)).await.unwrap().job;
        let interrupted = service
            .mark_interrupted(actor, &interrupted.id)
            .await
            .unwrap();
        assert_eq!(interrupted.status, JobStatus::Interrupted);
    }

    #[tokio::test]
    async fn ownership_is_hidden_while_admin_can_manage_any_job() {
        let db = setup_db().await;
        let service = JobService::new(db);
        let owner = JobActor::new("u1", false);
        let job = service.create(owner, input("u1", None)).await.unwrap().job;

        assert!(matches!(
            service.get(JobActor::new("u2", false), &job.id).await,
            Err(JobServiceError::NotFound)
        ));
        assert!(matches!(
            service
                .create(JobActor::new("u2", false), input("u1", None))
                .await,
            Err(JobServiceError::Forbidden)
        ));
        let admin_job = service
            .get(JobActor::new("admin", true), &job.id)
            .await
            .unwrap();
        assert_eq!(admin_job.owner_user_id.as_deref(), Some("u1"));
    }

    #[tokio::test]
    async fn cancellation_and_restart_reconciliation_preserve_resumable_jobs() {
        let db = setup_db().await;
        let service = JobService::new(db);
        let actor = JobActor::new("u1", false);
        let queued = service.create(actor, input("u1", None)).await.unwrap().job;
        let cancelled = service.request_cancel(actor, &queued.id).await.unwrap();
        assert_eq!(cancelled.status, JobStatus::Cancelled);

        let running = service.create(actor, input("u1", None)).await.unwrap().job;
        service
            .update(
                actor,
                &running.id,
                JobUpdate {
                    status: JobStatus::Running,
                    message_code: None,
                },
            )
            .await
            .unwrap();
        let waiting = service.create(actor, input("u1", None)).await.unwrap().job;
        service
            .update(
                actor,
                &waiting.id,
                JobUpdate {
                    status: JobStatus::WaitingForInput,
                    message_code: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(service.reconcile_after_restart().await.unwrap(), 1);
        assert_eq!(
            service.get(actor, &running.id).await.unwrap().status,
            JobStatus::Interrupted
        );
        assert_eq!(
            service.get(actor, &waiting.id).await.unwrap().status,
            JobStatus::WaitingForInput
        );
    }

    #[tokio::test]
    async fn failures_are_sanitized_and_terminal_transitions_are_rejected() {
        let db = setup_db().await;
        let service = JobService::new(db);
        let actor = JobActor::new("u1", false);
        let job = service.create(actor, input("u1", None)).await.unwrap().job;
        let failed = service
            .fail(
                actor,
                &job.id,
                json!({"code": "upstream", "details": {"password": "bad"}}),
            )
            .await
            .unwrap();
        assert_eq!(failed.error.unwrap()["details"]["password"], "[redacted]");
        assert!(matches!(
            service.progress(actor, &job.id, 50, None).await,
            Err(JobServiceError::InvalidTransition { .. })
        ));
    }
}
