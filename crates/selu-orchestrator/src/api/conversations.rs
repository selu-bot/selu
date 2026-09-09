//! Versioned conversation API shared by the web SPA and native clients.

use std::{collections::VecDeque, convert::Infallible, time::Duration};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response, Sse},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::stream;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{
    agents::{
        access,
        engine::{ChannelKind, InboundAttachmentInput, TurnParams, run_turn},
        improvement, router as agent_router,
    },
    api::auth::ApiPrincipal,
    commands,
    llm::tool_loop::LoopEvent,
    services::conversations::{self, ConversationRun},
    state::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/session", get(session))
        .route("/api/v1/conversations", get(list))
        .route("/api/v1/conversations", post(create))
        .route(
            "/api/v1/conversations/{conversation_id}",
            get(snapshot).patch(update).delete(delete),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/messages",
            post(send_message).layer(DefaultBodyLimit::max(18 * 1024 * 1024)),
        )
        .route(
            "/api/v1/conversations/{conversation_id}/feedback",
            post(rate_latest_turn),
        )
        .route("/api/v1/commands", get(list_commands))
        .route("/api/v1/events", get(events))
        .route(
            "/api/v1/artifacts/{artifact_id}",
            get(crate::api::artifacts::download_authenticated),
        )
        .route(
            "/api/v1/approvals/{approval_id}/decision",
            post(decide_approval),
        )
}

#[derive(Serialize)]
struct SessionResponse {
    display_name: String,
    is_admin: bool,
    language: String,
    timezone: String,
    supports_photo_uploads: bool,
}

async fn session(user: ApiPrincipal) -> Json<SessionResponse> {
    Json(SessionResponse {
        display_name: user.display_name.clone(),
        is_admin: user.is_admin,
        language: user.language.clone(),
        timezone: user.timezone.clone(),
        supports_photo_uploads: true,
    })
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
    /// Opaque keyset cursor returned as `next_cursor` by the previous page.
    before: Option<String>,
    /// When present, return only saved or unsaved conversations.
    saved: Option<bool>,
}

#[derive(Serialize)]
struct ListResponse {
    conversations: Vec<conversations::ConversationSummary>,
    /// Present when an older page exists. Pass it back as `?before=`.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

async fn list(
    user: ApiPrincipal,
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Response {
    let cursor = match query
        .before
        .as_deref()
        .map(conversations::ListCursor::decode)
    {
        Some(Ok(cursor)) => Some(cursor),
        Some(Err(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "code": "conversation.invalid_cursor" })),
            )
                .into_response();
        }
        None => None,
    };
    match conversations::list_conversations(
        &state.db,
        &user.user_id,
        query.limit.unwrap_or(50),
        cursor.as_ref(),
        query.saved,
    )
    .await
    {
        Ok(page) => Json(ListResponse {
            conversations: page.conversations,
            next_cursor: page.next_cursor.map(|cursor| cursor.encode()),
        })
        .into_response(),
        Err(error) => internal_error(error),
    }
}

#[derive(Deserialize)]
struct UpdateConversationRequest {
    title: Option<String>,
    saved: Option<bool>,
}

/// Update user-facing conversation metadata. Saving is reversible and only
/// applies to ordinary conversations; schedule threads remain automation-owned.
async fn update(
    user: ApiPrincipal,
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
    Json(request): Json<UpdateConversationRequest>,
) -> Response {
    if !owns_conversation(&state, &user, &conversation_id).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    let title = match request.title {
        Some(value) => {
            let value: String = value.trim().chars().take(120).collect();
            if value.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "code": "conversation.invalid_title" })),
                )
                    .into_response();
            }
            Some(value)
        }
        None => None,
    };
    if title.is_none() && request.saved.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "code": "conversation.no_changes" })),
        )
            .into_response();
    }
    if request.saved.is_some() {
        let kind = match sqlx::query_scalar::<_, String>(
            "SELECT thread_kind FROM threads WHERE id = ? AND user_id = ?",
        )
        .bind(&conversation_id)
        .bind(&user.user_id)
        .fetch_optional(&state.db)
        .await
        {
            Ok(Some(kind)) => kind,
            Ok(None) => return StatusCode::NOT_FOUND.into_response(),
            Err(error) => return internal_error(error.into()),
        };
        if kind == "schedule" {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "code": "conversation.schedule_cannot_be_saved"
                })),
            )
                .into_response();
        }
    }
    let saved = request.saved.map(i64::from);
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if let Err(error) = sqlx::query(
        r#"UPDATE threads
           SET title = COALESCE(?, title),
               saved_at = CASE
                   WHEN ? IS NULL THEN saved_at
                   WHEN ? = 1 THEN COALESCE(saved_at, ?)
                   ELSE NULL
               END
           WHERE id = ? AND user_id = ?"#,
    )
    .bind(&title)
    .bind(saved)
    .bind(saved)
    .bind(&now)
    .bind(&conversation_id)
    .bind(&user.user_id)
    .execute(&state.db)
    .await
    {
        return internal_error(error.into());
    }
    if let Err(error) = state
        .conversation_events
        .publish(
            &state.db,
            &user.user_id,
            &conversation_id,
            None,
            "conversation.changed",
            Some(&conversation_id),
            serde_json::json!({ "title": title, "saved": request.saved }),
        )
        .await
    {
        return internal_error(error);
    }
    match conversations::get_conversation(&state.db, &user.user_id, &conversation_id).await {
        Ok(Some(conversation)) => Json(conversation).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => internal_error(error),
    }
}

/// Delete a conversation together with its messages, runs, events and
/// persisted artifacts. A conversation with an active run is refused so the
/// running engine never writes into a thread that no longer exists.
async fn delete(
    user: ApiPrincipal,
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
) -> Response {
    if !owns_conversation(&state, &user, &conversation_id).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    let active: i64 = match sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_runs WHERE thread_id = ? AND status IN ('queued','running','waiting_for_approval','cancelling')",
    ).bind(&conversation_id).fetch_one(&state.db).await {
        Ok(value) => value,
        Err(error) => return internal_error(error.into()),
    };
    if active > 0 {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "code": "conversation.run_in_progress" })),
        )
            .into_response();
    }
    let artifacts = match conversations::delete_conversation(
        &state.db,
        &user.user_id,
        &conversation_id,
    )
    .await
    {
        Ok(artifacts) => artifacts,
        Err(error) => return internal_error(error),
    };
    if !artifacts.is_empty() {
        let mut store = state.artifacts.write().await;
        for artifact in &artifacts {
            store.remove(&artifact.artifact_id);
        }
    }
    for artifact in &artifacts {
        match tokio::fs::remove_file(&artifact.file_path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                artifact_id = %artifact.artifact_id,
                file_path = %artifact.file_path,
                "Could not delete artifact file of deleted conversation: {error}"
            ),
        }
    }
    // The thread row is gone, so this event is not attached to a run and is
    // only delivered live; a reconnecting client simply no longer sees the
    // conversation in its list.
    let _ = state.conversation_events.publish_transient(
        &user.user_id,
        &conversation_id,
        "conversation.deleted",
        serde_json::json!({ "conversation_id": conversation_id }),
    );
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct CreateConversationRequest {
    pipe_id: Option<String>,
}

/// Create an empty conversation on a caller-owned pipe. Creation belongs to
/// the shared contract rather than either client so both surfaces navigate to
/// the exact same conversation identity.
async fn create(
    user: ApiPrincipal,
    State(state): State<AppState>,
    Json(request): Json<CreateConversationRequest>,
) -> Response {
    let pipe = match sqlx::query_as::<_, (String, Option<String>)>(
        r#"SELECT id, default_agent_id FROM pipes
           WHERE user_id = ? AND active = 1 AND (? IS NULL OR id = ?)
           ORDER BY CASE WHEN transport = 'web' THEN 0 ELSE 1 END, created_at ASC
           LIMIT 1"#,
    )
    .bind(&user.user_id)
    .bind(&request.pipe_id)
    .bind(&request.pipe_id)
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(pipe)) => pipe,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error(error.into()),
    };
    let (pipe_id, default_agent_id) = (pipe.0, pipe.1.unwrap_or_else(|| "default".to_owned()));
    let force_new = state
        .agents
        .load()
        .get(&default_agent_id)
        .map(|agent| agent.session.requires_thread_isolation())
        .unwrap_or(false);
    let thread = match crate::agents::thread::create_thread(
        &state.db,
        &pipe_id,
        &user.user_id,
        &default_agent_id,
        force_new,
        None,
    )
    .await
    {
        Ok(thread) => thread,
        Err(error) => return internal_error(error),
    };
    match conversations::get_conversation(&state.db, &user.user_id, &thread.id.to_string()).await {
        Ok(Some(conversation)) => Json(conversation).into_response(),
        Ok(None) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Err(error) => internal_error(error),
    }
}

#[derive(Serialize)]
struct SnapshotResponse {
    conversation: conversations::ConversationSummary,
    messages: Vec<conversations::ConversationMessage>,
    runs: Vec<ConversationRun>,
    pending_approval: Option<serde_json::Value>,
    /// Thumbs rating (1 or -1) the user gave the most recent reply, if any.
    latest_turn_rating: Option<i64>,
    /// Last durable user-scoped event the client may use as an SSE cursor.
    event_cursor: i64,
}

async fn snapshot(
    user: ApiPrincipal,
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
) -> Response {
    if !owns_conversation(&state, &user, &conversation_id).await {
        return StatusCode::NOT_FOUND.into_response();
    }

    let conversation =
        match conversations::get_conversation(&state.db, &user.user_id, &conversation_id).await {
            Ok(conversation) => conversation,
            Err(error) => return internal_error(error),
        };
    let Some(conversation) = conversation else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // Establish the replay boundary before reading the snapshot. Reading the
    // cursor concurrently with messages can skip an event committed between them.
    let event_cursor = match sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MAX(id) FROM conversation_events WHERE user_id = ?",
    )
    .bind(&user.user_id)
    .fetch_one(&state.db)
    .await
    {
        Ok(value) => value.unwrap_or(0),
        Err(error) => return internal_error(error.into()),
    };
    let (messages, runs, latest_turn_rating) = tokio::join!(
        conversations::list_messages(&state.db, &conversation_id, 200),
        conversations::list_runs(&state.db, &conversation_id),
        improvement::latest_turn_rating(&state.db, &user.user_id, &conversation_id),
    );
    // A missing rating must never block the conversation from loading.
    let latest_turn_rating = latest_turn_rating.unwrap_or_else(|error| {
        tracing::warn!(conversation_id, "Could not load turn rating: {error:#}");
        None
    });
    match (messages, runs) {
        (Ok(messages), Ok(runs)) => {
            let pending_approval = if let Some(run) =
                runs.iter().find(|run| run.status == "waiting_for_approval")
            {
                let candidate = sqlx::query_scalar::<_, String>(
                    "SELECT payload_json FROM conversation_events WHERE user_id = ? AND thread_id = ? AND run_id = ? AND event_type = 'approval.requested' ORDER BY id DESC LIMIT 1",
                )
                .bind(&user.user_id)
                .bind(&conversation_id)
                .bind(&run.id)
                .fetch_optional(&state.db)
                .await
                .ok()
                .flatten()
                .and_then(|payload| serde_json::from_str::<serde_json::Value>(&payload).ok());
                if let Some(approval_id) = candidate
                    .as_ref()
                    .and_then(|approval| approval.get("approval_id"))
                    .and_then(|id| id.as_str())
                {
                    if state
                        .pending_confirmations
                        .lock()
                        .await
                        .contains_key(approval_id)
                    {
                        candidate
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
            Json(SnapshotResponse {
                conversation,
                messages,
                runs,
                pending_approval,
                latest_turn_rating,
                event_cursor,
            })
            .into_response()
        }
        (Err(error), _) | (_, Err(error)) => internal_error(error),
    }
}

#[derive(Deserialize)]
struct CommandsQuery {
    /// Client UI language; defaults to the user's profile language.
    lang: Option<String>,
}

#[derive(Serialize)]
struct CommandsResponse {
    commands: Vec<commands::CommandInfo>,
}

/// The slash commands a client may offer in its composer. Descriptions are
/// localized so the picker needs no client-side strings per command.
async fn list_commands(
    user: ApiPrincipal,
    Query(query): Query<CommandsQuery>,
) -> Json<CommandsResponse> {
    let lang = query
        .lang
        .as_deref()
        .filter(|lang| matches!(*lang, "en" | "de"))
        .unwrap_or(&user.language);
    Json(CommandsResponse {
        commands: commands::catalog(lang),
    })
}

#[derive(Deserialize)]
struct SendMessageRequest {
    text: String,
    client_message_id: String,
    #[serde(default)]
    attachments: Vec<PhotoUpload>,
}

#[derive(Deserialize)]
struct PhotoUpload {
    filename: String,
    mime_type: String,
    data_base64: String,
}

/// Bound both encoded and decoded sizes before allocating or starting a run.
/// Native clients normalize photos to JPEG; PNG, GIF and WebP are also accepted.
fn decode_photos(photos: Vec<PhotoUpload>) -> Result<Vec<InboundAttachmentInput>, &'static str> {
    const MAX_PHOTO: usize = 2 * 1024 * 1024;
    const MAX_TOTAL: usize = 12 * 1024 * 1024;
    if photos.len() > 10 {
        return Err("conversation.too_many_photos");
    }
    let mut total = 0;
    photos
        .into_iter()
        .map(|photo| {
            if photo.data_base64.len() > MAX_PHOTO.div_ceil(3) * 4 {
                return Err("conversation.photo_too_large");
            }
            let data = STANDARD
                .decode(&photo.data_base64)
                .map_err(|_| "conversation.invalid_photo")?;
            total += data.len();
            if data.len() > MAX_PHOTO || total > MAX_TOTAL {
                return Err("conversation.photo_too_large");
            }
            let valid = match photo.mime_type.as_str() {
                "image/jpeg" => data.starts_with(&[0xff, 0xd8, 0xff]),
                "image/png" => data.starts_with(b"\x89PNG\r\n\x1a\n"),
                "image/gif" => data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a"),
                "image/webp" => data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP"),
                _ => false,
            };
            if !valid {
                return Err("conversation.invalid_photo");
            }
            // A filename is a display label, never a filesystem path.
            let filename: String = photo
                .filename
                .chars()
                .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
                .take(120)
                .collect();
            Ok(InboundAttachmentInput {
                filename: if filename.is_empty() {
                    "photo".to_owned()
                } else {
                    filename
                },
                mime_type: photo.mime_type,
                data,
            })
        })
        .collect()
}

fn validate_photo_command(
    text: &str,
    attachments: &[InboundAttachmentInput],
) -> Result<(), &'static str> {
    if !attachments.is_empty() && commands::is_command(text) {
        return Err("conversation.photo_command");
    }
    Ok(())
}

#[derive(Serialize)]
struct SendMessageResponse {
    run: ConversationRun,
}

/// Accept a user message and start exactly one durable run. The response is
/// intentionally asynchronous; clients learn every transition via `/events`.
async fn send_message(
    user: ApiPrincipal,
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
    Json(request): Json<SendMessageRequest>,
) -> Response {
    let text = request.text.trim().to_owned();
    if (text.is_empty() && request.attachments.is_empty())
        || Uuid::parse_str(&request.client_message_id).is_err()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "code": "conversation.invalid_message",
            })),
        )
            .into_response();
    }
    let Some((owner_id, pipe_id)) =
        (match conversations::conversation_owner(&state.db, &conversation_id).await {
            Ok(owner) => owner,
            Err(error) => return internal_error(error),
        })
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if owner_id != user.user_id {
        return StatusCode::NOT_FOUND.into_response();
    }

    // An idempotent retry wins over the active-run guard.
    if let Ok(Some(row)) = sqlx::query_as::<_, (String, String, Option<String>, String, Option<String>, Option<String>)>(
        "SELECT id, status, error_code, created_at, started_at, completed_at FROM conversation_runs WHERE user_id = ? AND client_message_id = ?",
    ).bind(&user.user_id).bind(&request.client_message_id).fetch_optional(&state.db).await {
        let run = ConversationRun {
            id: row.0, client_message_id: request.client_message_id, status: row.1, error_code: row.2,
            created_at: row.3, started_at: row.4, completed_at: row.5,
        };
        return match run.canonicalized() {
            Ok(run) => Json(SendMessageResponse { run }).into_response(),
            Err(error) => internal_error(error),
        };
    }
    let attachments = match decode_photos(request.attachments) {
        Ok(attachments) => attachments,
        Err(code) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "code": code })),
            )
                .into_response();
        }
    };
    // Commands don't accept images; never silently discard a photo.
    if let Err(code) = validate_photo_command(&text, &attachments) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "code": code })),
        )
            .into_response();
    }
    let active: i64 = match sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_runs WHERE thread_id = ? AND status IN ('queued','running','waiting_for_approval','cancelling')",
    ).bind(&conversation_id).fetch_one(&state.db).await {
        Ok(value) => value,
        Err(error) => return internal_error(error.into()),
    };
    if active > 0 {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "code": "conversation.run_in_progress",
            })),
        )
            .into_response();
    }

    let run_id = Uuid::new_v4().to_string();
    let created_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut tx = match state.db.begin().await {
        Ok(tx) => tx,
        Err(error) => return internal_error(error.into()),
    };
    if let Err(error) = sqlx::query(
        "UPDATE threads SET started_at = COALESCE(started_at, ?) WHERE id = ? AND user_id = ?",
    )
    .bind(&created_at)
    .bind(&conversation_id)
    .bind(&user.user_id)
    .execute(&mut *tx)
    .await
    {
        return internal_error(error.into());
    }
    if let Err(error) = sqlx::query(
        "INSERT INTO conversation_runs (id, thread_id, user_id, client_message_id, status) VALUES (?, ?, ?, ?, 'queued')",
    )
    .bind(&run_id)
    .bind(&conversation_id)
    .bind(&user.user_id)
    .bind(&request.client_message_id)
    .execute(&mut *tx)
    .await
    {
        return internal_error(error.into());
    }
    if let Err(error) = tx.commit().await {
        return internal_error(error.into());
    }
    let run = ConversationRun {
        id: run_id.clone(),
        client_message_id: request.client_message_id.clone(),
        status: "queued".to_owned(),
        error_code: None,
        created_at: created_at.clone(),
        started_at: None,
        completed_at: None,
    };
    if let Err(error) = state
        .conversation_events
        .publish(
            &state.db,
            &user.user_id,
            &conversation_id,
            Some(&run_id),
            "run.created",
            Some(&run_id),
            serde_json::json!({ "run": run }),
        )
        .await
    {
        return internal_error(error);
    }

    // Text-only messages have no artifact persistence dependency and keep the
    // immediate event timing older clients expect. Photo messages are emitted
    // by the engine once their stable artifact references are durable.
    if attachments.is_empty() {
        let message = conversations::ConversationMessage {
            id: request.client_message_id.clone(),
            role: "user".to_owned(),
            content: text.clone(),
            created_at: created_at.clone(),
            tool_calls: None,
            tool_call_id: None,
            attachments: None,
            compacted: false,
        };
        if let Err(error) = state
            .conversation_events
            .publish(
                &state.db,
                &user.user_id,
                &conversation_id,
                Some(&run_id),
                "message.created",
                Some(&request.client_message_id),
                serde_json::json!({ "message": message }),
            )
            .await
        {
            return internal_error(error);
        }
    }

    if commands::is_command(&text) {
        tokio::spawn(execute_command_run(
            state,
            user.user_id.clone(),
            conversation_id,
            run_id,
            request.client_message_id,
            text,
        ));
    } else {
        tokio::spawn(execute_run(
            state,
            user.user_id.clone(),
            pipe_id,
            conversation_id,
            run_id,
            request.client_message_id,
            text,
            attachments,
        ));
    }
    Json(SendMessageResponse { run }).into_response()
}

/// A slash command is a run like any other, so clients see the reply through
/// the same events, but it completes without the agent engine and without
/// Live Activity notifications.
async fn execute_command_run(
    state: AppState,
    user_id: String,
    conversation_id: String,
    run_id: String,
    client_message_id: String,
    text: String,
) {
    if let Err(error) =
        set_run_status(&state, &user_id, &conversation_id, &run_id, "running", None).await
    {
        tracing::error!(run_id, "Could not start command run: {error:#}");
        return;
    }
    let reply = commands::try_handle_message(
        &state,
        &user_id,
        &conversation_id,
        &text,
        Some(&client_message_id),
    )
    .await;
    let Some(reply) = reply else {
        tracing::error!(
            run_id,
            "Slash command could not be handled for this conversation"
        );
        let _ = set_run_status(
            &state,
            &user_id,
            &conversation_id,
            &run_id,
            "failed",
            Some("conversation.agent_failed"),
        )
        .await;
        return;
    };
    let message = conversations::ConversationMessage {
        id: reply.reply_message_id.clone(),
        role: "assistant".to_owned(),
        content: reply.text,
        created_at: reply.created_at,
        tool_calls: None,
        tool_call_id: None,
        attachments: None,
        compacted: false,
    };
    if let Err(error) = state
        .conversation_events
        .publish(
            &state.db,
            &user_id,
            &conversation_id,
            Some(&run_id),
            "message.created",
            Some(&reply.reply_message_id),
            serde_json::json!({ "message": message }),
        )
        .await
    {
        tracing::warn!(run_id, "Could not publish command reply: {error:#}");
    }
    if let Err(error) = set_run_status(
        &state,
        &user_id,
        &conversation_id,
        &run_id,
        "completed",
        None,
    )
    .await
    {
        tracing::error!(run_id, "Could not finish command run: {error:#}");
    }
}

#[derive(Deserialize)]
struct TurnFeedbackRequest {
    /// `1` for a helpful reply, `-1` for an unhelpful one.
    rating: i64,
}

/// Record thumbs up/down for the most recent reply in a conversation. The
/// rating lands on the latest turn signal, which feeds the agent's behavioral
/// lessons. Rating while Selu is still working is refused so the rating cannot
/// attach to a reply the user has not seen yet.
async fn rate_latest_turn(
    user: ApiPrincipal,
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
    Json(request): Json<TurnFeedbackRequest>,
) -> Response {
    if !matches!(request.rating, 1 | -1) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "code": "conversation.invalid_feedback" })),
        )
            .into_response();
    }
    if !owns_conversation(&state, &user, &conversation_id).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    let active: i64 = match sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_runs WHERE thread_id = ? AND status IN ('queued','running','waiting_for_approval','cancelling')",
    ).bind(&conversation_id).fetch_one(&state.db).await {
        Ok(value) => value,
        Err(error) => return internal_error(error.into()),
    };
    if active > 0 {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "code": "conversation.run_in_progress" })),
        )
            .into_response();
    }
    match improvement::rate_turn(&state.db, &user.user_id, &conversation_id, request.rating).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        // The turn signal is written shortly after a reply completes; a
        // conversation without one has nothing the rating could apply to.
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "code": "conversation.feedback_unavailable" })),
        )
            .into_response(),
        Err(error) => internal_error(error),
    }
}

async fn execute_run(
    state: AppState,
    user_id: String,
    pipe_id: String,
    conversation_id: String,
    run_id: String,
    client_message_id: String,
    text: String,
    attachments: Vec<InboundAttachmentInput>,
) {
    if let Err(error) =
        set_run_status(&state, &user_id, &conversation_id, &run_id, "running", None).await
    {
        tracing::error!(run_id, "Could not start conversation run: {error:#}");
        return;
    }
    notify_run_started(&state, &user_id, &pipe_id, &conversation_id, &run_id, &text).await;
    let default_agent_id =
        sqlx::query_scalar::<_, Option<String>>("SELECT default_agent_id FROM pipes WHERE id = ?")
            .bind(&pipe_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .flatten();
    let agents = state.agents.load();
    let visible_agents = access::visible_agents(&state.db, &user_id, &agents).await;
    let (agent_id, effective_text) =
        agent_router::route(&text, default_agent_id.as_deref(), &visible_agents);
    drop(agents);

    let (sender, mut receiver) = mpsc::channel::<LoopEvent>(128);
    let turn = run_turn(
        &state,
        TurnParams {
            pipe_id: pipe_id.clone(),
            user_id: user_id.clone(),
            agent_id: Some(agent_id),
            message: effective_text,
            thread_id: Some(conversation_id.clone()),
            chain_depth: 0,
            channel_kind: ChannelKind::Interactive,
            skip_user_persist: false,
            client_message_id: Some(client_message_id),
            enable_streaming: true,
            inbound_attachments: attachments,
            delegation_trace: Vec::new(),
            location_context: None,
        },
        sender,
    );
    tokio::pin!(turn);
    let mut terminal_error: Option<String> = None;
    let mut turn_finished = false;
    loop {
        tokio::select! {
            result = &mut turn, if !turn_finished => {
                if let Err(error) = result {
                    tracing::error!(run_id, "Conversation run failed: {error:#}");
                    terminal_error = Some(error.to_string());
                }
                turn_finished = true;
            }
            event = receiver.recv() => match event {
                Some(event) => handle_engine_event(&state, &user_id, &conversation_id, &run_id, event).await,
                // Every sender (the turn and its stream forwarder) has been
                // dropped, so the trailing `Done`/artifact events buffered
                // before the turn returned have all been published.
                None => break,
            },
            _ = tokio::time::sleep(Duration::from_secs(10)), if turn_finished => {
                tracing::warn!(run_id, "Engine stream stayed open after the turn finished; closing run");
                break;
            }
        }
    }
    let final_status = if terminal_error.is_some() {
        "failed"
    } else {
        "completed"
    };
    let error_code = terminal_error
        .as_deref()
        .map(|_| "conversation.agent_failed");
    if let Err(error) = set_run_status(
        &state,
        &user_id,
        &conversation_id,
        &run_id,
        final_status,
        error_code,
    )
    .await
    {
        tracing::error!(run_id, "Could not finish conversation run: {error:#}");
    }
    notify_run_finished(&state, &user_id, &pipe_id, &conversation_id, &run_id).await;
}

/// A Live Activity is a delivery hint. The run snapshot and event journal stay
/// authoritative when APNs is delayed, unavailable, or a device is offline.
async fn notify_run_started(
    state: &AppState,
    user_id: &str,
    pipe_id: &str,
    conversation_id: &str,
    run_id: &str,
    text: &str,
) {
    if !crate::services::system_updates::push_notifications_enabled(state).await {
        return;
    }
    let Ok(instance_id) = crate::persistence::db::get_instance_id(&state.db).await else {
        return;
    };
    let Ok(tokens) = sqlx::query_scalar::<_, String>(
        "SELECT device_token FROM mobile_device_tokens WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await
    else {
        return;
    };
    let title: String = text.chars().take(80).collect();
    let client = reqwest::Client::new();
    for device_token in tokens {
        let payload = serde_json::json!({
            "instance_id": instance_id,
            "pipe_id": pipe_id,
            "thread_id": conversation_id,
            "run_id": run_id,
            "thread_title": title,
            "device_token": device_token,
        });
        if let Err(error) = client
            .post("https://selu.bot/api/relay/start-activity")
            .header("X-Instance-Id", &instance_id)
            .json(&payload)
            .send()
            .await
        {
            tracing::warn!(run_id, error = %error, "Could not start Live Activity for conversation run");
        }
    }
}

async fn notify_run_finished(
    state: &AppState,
    user_id: &str,
    pipe_id: &str,
    conversation_id: &str,
    run_id: &str,
) {
    if !crate::services::system_updates::push_notifications_enabled(state).await {
        return;
    }
    let Ok(instance_id) = crate::persistence::db::get_instance_id(&state.db).await else {
        return;
    };
    let language = crate::i18n::user_language(&state.db, user_id).await;
    let body = if language.starts_with("de") {
        "Agent ist fertig"
    } else {
        "Agent completed"
    };
    let payload = serde_json::json!({
        "instance_id": instance_id,
        "pipe_id": pipe_id,
        "thread_id": conversation_id,
        "run_id": run_id,
        "event": "end",
        "title": "selu",
        "body": body,
    });
    if let Err(error) = reqwest::Client::new()
        .post("https://selu.bot/api/relay/push")
        .header("X-Instance-Id", &instance_id)
        .json(&payload)
        .send()
        .await
    {
        tracing::warn!(run_id, error = %error, "Could not finish Live Activity for conversation run");
    }
}

fn persisted_user_message(
    id: String,
    content: String,
    created_at: String,
    attachments: Vec<crate::agents::artifacts::ArtifactRef>,
) -> conversations::ConversationMessage {
    conversations::ConversationMessage {
        id,
        role: "user".to_owned(),
        content,
        created_at,
        tool_calls: None,
        tool_call_id: None,
        attachments: (!attachments.is_empty()).then(|| serde_json::json!(attachments)),
        compacted: false,
    }
}

async fn handle_engine_event(
    state: &AppState,
    user_id: &str,
    conversation_id: &str,
    run_id: &str,
    event: LoopEvent,
) {
    let (event_type, entity_id, payload) = match event {
        LoopEvent::UserMessagePersisted {
            id,
            content,
            created_at,
            attachments,
        } => {
            let entity_id = id.clone();
            let message = persisted_user_message(id, content, created_at, attachments);
            (
                "message.created",
                entity_id,
                serde_json::json!({ "message": message }),
            )
        }
        LoopEvent::Token(text) => (
            "message.text_delta",
            run_id.to_owned(),
            serde_json::json!({ "text": text }),
        ),
        LoopEvent::AssistantPartFinished => (
            "message.part_finished",
            run_id.to_owned(),
            serde_json::json!({}),
        ),
        LoopEvent::CapabilityStatus(label) => (
            "run.progress",
            run_id.to_owned(),
            serde_json::json!({ "label": label }),
        ),
        LoopEvent::Artifacts(artifacts) => (
            "message.artifacts",
            run_id.to_owned(),
            serde_json::json!({ "artifacts": artifacts }),
        ),
        LoopEvent::Done => (
            "run.output_finished",
            run_id.to_owned(),
            serde_json::json!({}),
        ),
        // Clients only get a stable code; the engine's message is for the log.
        LoopEvent::Error(message) => {
            tracing::warn!(run_id, "Agent reported an error during the run: {message}");
            (
                "run.error",
                run_id.to_owned(),
                serde_json::json!({ "code": "conversation.agent_failed" }),
            )
        }
        LoopEvent::ConfirmationRequired(request) => {
            let approval_id = Uuid::new_v4().to_string();
            state
                .pending_confirmations
                .lock()
                .await
                .insert(approval_id.clone(), request.reply);
            state.conversation_confirmation_owners.lock().await.insert(
                approval_id.clone(),
                (
                    user_id.to_owned(),
                    conversation_id.to_owned(),
                    run_id.to_owned(),
                ),
            );
            let _ = set_run_status(
                state,
                user_id,
                conversation_id,
                run_id,
                "waiting_for_approval",
                None,
            )
            .await;
            (
                "approval.requested",
                run_id.to_owned(),
                serde_json::json!({
                    "approval_id": approval_id,
                    "tool_name": request.tool_display_name,
                    "message": request.approval_message,
                    "arguments": request.arguments,
                }),
            )
        }
        LoopEvent::ApprovalQueued {
            tool_display_name,
            approval_message,
            approval_id,
        } => (
            "approval.queued",
            run_id.to_owned(),
            serde_json::json!({
                "approval_id": approval_id,
                "tool_name": tool_display_name,
                "message": approval_message,
            }),
        ),
        LoopEvent::ToolMessage(_) => (
            "conversation.changed",
            run_id.to_owned(),
            serde_json::json!({}),
        ),
    };
    if let Err(error) = state
        .conversation_events
        .publish(
            &state.db,
            user_id,
            conversation_id,
            Some(run_id),
            event_type,
            Some(&entity_id),
            payload,
        )
        .await
    {
        tracing::warn!(run_id, "Could not publish conversation event: {error:#}");
    }
}

async fn set_run_status(
    state: &AppState,
    user_id: &str,
    conversation_id: &str,
    run_id: &str,
    status: &str,
    error_code: Option<&str>,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    sqlx::query("UPDATE conversation_runs SET status = ?, error_code = ?, started_at = CASE WHEN ? = 'running' THEN COALESCE(started_at, ?) ELSE started_at END, completed_at = CASE WHEN ? IN ('completed','failed','cancelled','interrupted') THEN ? ELSE completed_at END WHERE id = ?")
        .bind(status).bind(error_code).bind(status).bind(&now).bind(status).bind(&now).bind(run_id)
        .execute(&state.db).await?;
    state
        .conversation_events
        .publish(
            &state.db,
            user_id,
            conversation_id,
            Some(run_id),
            "run.updated",
            Some(run_id),
            serde_json::json!({
                "run_id": run_id, "status": status, "error_code": error_code,
            }),
        )
        .await?;
    Ok(())
}

#[derive(Deserialize)]
struct EventQuery {
    after: Option<i64>,
    conversation_id: Option<String>,
}

async fn events(
    user: ApiPrincipal,
    State(state): State<AppState>,
    Query(query): Query<EventQuery>,
) -> Response {
    if let Some(ref conversation_id) = query.conversation_id {
        if !owns_conversation(&state, &user, conversation_id).await {
            return StatusCode::NOT_FOUND.into_response();
        }
    }
    // Subscribe before reading the durable replay window. Otherwise an event
    // committed between the query and subscription would be permanently lost
    // to this connection.
    let receiver = state.conversation_events.subscribe();
    let historical = match conversations::list_events(
        &state.db,
        &user.user_id,
        query.after.unwrap_or(0),
        query.conversation_id.as_deref(),
    )
    .await
    {
        Ok(events) => events,
        Err(error) => return internal_error(error),
    };
    let replay_cursor = historical
        .last()
        .map(|event| event.id)
        .unwrap_or_else(|| query.after.unwrap_or(0));
    let user_id = user.user_id.clone();
    let filter_conversation_id = query.conversation_id;
    let event_stream = stream::unfold(
        (VecDeque::from(historical), receiver, replay_cursor),
        move |(mut queued, mut receiver, mut cursor)| {
            let user_id = user_id.clone();
            let filter_conversation_id = filter_conversation_id.clone();
            async move {
                loop {
                    let (event, is_live) = match queued.pop_front() {
                        Some(event) => (event, false),
                        None => match receiver.recv().await {
                            Ok(event) => (event, true),
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                        },
                    };
                    // The receiver may contain events already supplied by the
                    // replay query because it was intentionally subscribed
                    // first. SSE IDs make this suppression deterministic.
                    // Transient events carry id 0 and are never journaled,
                    // so they bypass the replay de-duplication.
                    let journaled = event.id > 0;
                    if is_live && journaled && event.id <= cursor {
                        continue;
                    }
                    if event.user_id == user_id
                        && filter_conversation_id
                            .as_deref()
                            .is_none_or(|id| id == event.conversation_id)
                    {
                        let data =
                            serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_owned());
                        let mut sse_event = axum::response::sse::Event::default()
                            .event(event.event_type)
                            .data(data);
                        if journaled {
                            sse_event = sse_event.id(event.id.to_string());
                        }
                        if is_live && journaled {
                            cursor = event.id;
                        }
                        return Some((Ok::<_, Infallible>(sse_event), (queued, receiver, cursor)));
                    }
                }
            }
        },
    );
    Sse::new(event_stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("ping"),
        )
        .into_response()
}

#[derive(Deserialize)]
struct ApprovalDecision {
    approved: bool,
}

async fn decide_approval(
    user: ApiPrincipal,
    State(state): State<AppState>,
    Path(approval_id): Path<String>,
    Json(decision): Json<ApprovalDecision>,
) -> Response {
    // A confirmation sender is only created for an authenticated v1 run. The
    // pending map is process-local, so an expired/restarted approval is safely
    // rejected and clients rehydrate from the run snapshot.
    let context = {
        let mut owners = state.conversation_confirmation_owners.lock().await;
        if owners
            .get(&approval_id)
            .is_some_and(|context| context.0 == user.user_id)
        {
            owners.remove(&approval_id)
        } else {
            None
        }
    };
    let Some((_owner, conversation_id, run_id)) = context else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match state
        .pending_confirmations
        .lock()
        .await
        .remove(&approval_id)
    {
        Some(sender) => {
            let _ = sender.send(decision.approved);
            let _ = set_run_status(
                &state,
                &user.user_id,
                &conversation_id,
                &run_id,
                "running",
                None,
            )
            .await;
            StatusCode::NO_CONTENT.into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "code": "conversation.approval_expired" })),
        )
            .into_response(),
    }
}

async fn owns_conversation(state: &AppState, user: &ApiPrincipal, conversation_id: &str) -> bool {
    matches!(conversations::conversation_owner(&state.db, conversation_id).await, Ok(Some((owner, _))) if owner == user.user_id)
}

fn internal_error(error: anyhow::Error) -> Response {
    tracing::error!(error = %error, "Conversation API request failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "code": "conversation.unavailable" })),
    )
        .into_response()
}

#[cfg(test)]
mod photo_tests {
    use super::*;

    fn photo(mime: &str, bytes: &[u8]) -> PhotoUpload {
        PhotoUpload {
            filename: "photo.jpg".into(),
            mime_type: mime.into(),
            data_base64: STANDARD.encode(bytes),
        }
    }

    #[test]
    fn old_text_clients_need_no_attachment_field() {
        let value: SendMessageRequest = serde_json::from_value(serde_json::json!({
            "text": "Hello", "client_message_id": Uuid::new_v4().to_string()
        }))
        .unwrap();
        assert!(value.attachments.is_empty());
    }

    #[test]
    fn validates_photo_type_and_content() {
        assert!(decode_photos(vec![photo("image/jpeg", &[0xff, 0xd8, 0xff, 0xd9])]).is_ok());
        assert!(decode_photos(vec![photo("image/png", b"not a PNG")]).is_err());
        assert!(decode_photos(vec![photo("image/svg+xml", b"<svg/>")]).is_err());
        assert!(
            decode_photos(vec![PhotoUpload {
                filename: "p".into(),
                mime_type: "image/jpeg".into(),
                data_base64: "%%%".into()
            }])
            .is_err()
        );
    }

    #[test]
    fn enforces_photo_count_and_size_limits() {
        assert!(matches!(
            decode_photos(
                (0..11)
                    .map(|_| photo("image/jpeg", &[0xff, 0xd8, 0xff]))
                    .collect()
            ),
            Err("conversation.too_many_photos")
        ));
        assert!(matches!(
            decode_photos(vec![photo("image/jpeg", &vec![0xff; 2 * 1024 * 1024 + 1])]),
            Err("conversation.photo_too_large")
        ));
    }

    #[test]
    fn rejects_photos_with_slash_commands() {
        let attachments = decode_photos(vec![photo("image/jpeg", &[0xff, 0xd8, 0xff])]).unwrap();
        assert_eq!(
            validate_photo_command("/help", &attachments),
            Err("conversation.photo_command")
        );
        assert!(validate_photo_command("Help with 1/2 cup", &attachments).is_ok());
        assert!(validate_photo_command("/help", &[]).is_ok());
    }

    #[test]
    fn filename_is_only_a_display_label() {
        let mut input = photo("image/jpeg", &[0xff, 0xd8, 0xff]);
        input.filename = "../folder/\nphoto.jpg".into();
        let result = decode_photos(vec![input]).unwrap();
        assert!(!result[0].filename.contains('/'));
        assert!(!result[0].filename.contains('\n'));
    }

    #[test]
    fn persisted_photo_event_message_includes_artifact_references() {
        let message = persisted_user_message(
            "message-1".into(),
            "".into(),
            "2026-09-08T12:00:00.000".into(),
            vec![crate::agents::artifacts::ArtifactRef {
                artifact_id: "artifact-1".into(),
                filename: "garden.jpg".into(),
                mime_type: "image/jpeg".into(),
                size_bytes: 4,
            }],
        );
        let payload = serde_json::json!({ "message": message });

        assert_eq!(payload["message"]["id"], "message-1");
        assert_eq!(payload["message"]["content"], "");
        assert_eq!(
            payload["message"]["attachments"][0]["artifact_id"],
            "artifact-1"
        );
        assert_eq!(
            payload["message"]["attachments"][0]["filename"],
            "garden.jpg"
        );
        assert!(!payload["message"]["attachments"].is_null());
    }
}
