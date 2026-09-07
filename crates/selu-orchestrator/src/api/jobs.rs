use axum::{
    Json, Router,
    extract::{FromRef, Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::{
    api::auth::ApiPrincipal,
    services::jobs::{Job, JobActor, JobService, JobServiceError, JobStatus},
};

#[derive(Debug, Serialize)]
pub struct JobResponse {
    pub id: String,
    pub owner_user_id: Option<String>,
    pub kind: String,
    pub resource_id: String,
    pub status: JobStatus,
    pub progress: i64,
    pub message_code: Option<String>,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub created_at: String,
    pub updated_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

impl From<Job> for JobResponse {
    fn from(job: Job) -> Self {
        Self {
            id: job.id,
            owner_user_id: job.owner_user_id,
            kind: job.kind,
            resource_id: job.resource_id,
            status: job.status,
            progress: job.progress,
            message_code: job.message_code,
            result: job.result,
            error: job.error,
            created_at: job.created_at,
            updated_at: job.updated_at,
            started_at: job.started_at,
            completed_at: job.completed_at,
        }
    }
}

pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    SqlitePool: FromRef<S>,
{
    Router::new()
        .route("/api/v1/jobs/{id}", get(get_job))
        .route("/api/v1/jobs/{id}/cancellation", post(cancel_job))
}

async fn get_job(
    principal: ApiPrincipal,
    State(db): State<SqlitePool>,
    Path(job_id): Path<String>,
) -> Response {
    let actor = JobActor::new(&principal.user_id, principal.is_admin);
    match JobService::new(db).get(actor, &job_id).await {
        Ok(job) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(JobResponse::from(job)),
        )
            .into_response(),
        Err(error) => error_response(error),
    }
}

async fn cancel_job(
    principal: ApiPrincipal,
    State(db): State<SqlitePool>,
    Path(job_id): Path<String>,
) -> Response {
    let actor = JobActor::new(&principal.user_id, principal.is_admin);
    match JobService::new(db).request_cancel(actor, &job_id).await {
        Ok(job) => accepted(job),
        Err(error) => error_response(error),
    }
}

/// Standard response for asynchronous creation/cancellation helpers: clients
/// receive the durable representation and canonical polling URL immediately.
pub fn accepted(job: Job) -> Response {
    let location = format!("/api/v1/jobs/{}", job.id);
    (
        StatusCode::ACCEPTED,
        [
            (header::LOCATION, location),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        Json(JobResponse::from(job)),
    )
        .into_response()
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
}

fn error_response(error: JobServiceError) -> Response {
    let (status, code, message) = match error {
        JobServiceError::NotFound => (
            StatusCode::NOT_FOUND,
            "job_not_found",
            "The job was not found.",
        ),
        JobServiceError::Forbidden => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "You do not have permission to do that.",
        ),
        JobServiceError::InvalidInput(_) => (
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "The job request is invalid.",
        ),
        JobServiceError::InvalidTransition { .. } | JobServiceError::Conflict => (
            StatusCode::CONFLICT,
            "job_conflict",
            "The job state changed and the request could not be applied.",
        ),
        error => {
            tracing::error!("Jobs API failed: {error:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "The request could not be completed.",
            )
        }
    };
    (
        status,
        Json(ErrorEnvelope {
            error: ErrorBody { code, message },
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{
        auth::{self, SESSION_COOKIE},
        jobs::NewJob,
    };
    use axum::body::Body;
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    async fn setup() -> (SqlitePool, String, String, String) {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        sqlx::query!(
            "INSERT INTO users (id, username, display_name, password_hash, is_admin, language) VALUES ('owner', 'owner', 'Owner', 'x', 0, 'en'), ('other', 'other', 'Other', 'x', 0, 'en'), ('admin', 'admin', 'Admin', 'x', 1, 'en')"
        )
        .execute(&db)
        .await
        .unwrap();
        let owner_session = auth::create_session(&db, "owner").await.unwrap();
        let other_session = auth::create_session(&db, "other").await.unwrap();
        let admin_session = auth::create_session(&db, "admin").await.unwrap();
        (db, owner_session, other_session, admin_session)
    }

    fn request(uri: &str, method: &str, session: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", format!("{SESSION_COOKIE}={session}"))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn endpoints_hide_cross_user_jobs_and_return_location_on_cancellation() {
        let (db, owner_session, other_session, admin_session) = setup().await;
        let job = JobService::new(db.clone())
            .create(
                JobActor::new("owner", false),
                NewJob {
                    owner_user_id: Some("owner"),
                    kind: "test",
                    resource_id: "resource",
                    idempotency_key: None,
                    message_code: None,
                },
            )
            .await
            .unwrap()
            .job;
        let app = router().with_state(db);
        let uri = format!("/api/v1/jobs/{}", job.id);

        let own = app
            .clone()
            .oneshot(request(&uri, "GET", &owner_session))
            .await
            .unwrap();
        assert_eq!(own.status(), StatusCode::OK);

        let hidden = app
            .clone()
            .oneshot(request(&uri, "GET", &other_session))
            .await
            .unwrap();
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);

        let hidden_cancel = app
            .clone()
            .oneshot(request(
                &format!("{uri}/cancellation"),
                "POST",
                &other_session,
            ))
            .await
            .unwrap();
        assert_eq!(hidden_cancel.status(), StatusCode::NOT_FOUND);

        let admin_cancel = app
            .oneshot(request(
                &format!("{uri}/cancellation"),
                "POST",
                &admin_session,
            ))
            .await
            .unwrap();
        assert_eq!(admin_cancel.status(), StatusCode::ACCEPTED);
        assert_eq!(admin_cancel.headers()[header::LOCATION], uri);
    }
}
