//! Versioned conversation API shared by the web SPA and native clients.

use std::{collections::VecDeque, convert::Infallible, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response, Sse},
    routing::{get, post},
};
use futures::stream;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{
    agents::{
        access,
        engine::{ChannelKind, TurnParams, run_turn},
        router as agent_router,
    },
    llm::tool_loop::LoopEvent,
    services::conversations::{self, ConversationRun},
    state::AppState,
    web::auth::AuthUser,
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
            post(send_message),
        )
        .route("/api/v1/events", get(events))
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
}

async fn session(user: AuthUser) -> Json<SessionResponse> {
    Json(SessionResponse {
        display_name: user.display_name,
        is_admin: user.is_admin,
        language: user.language,
    })
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
    /// Opaque keyset cursor returned as `next_cursor` by the previous page.
    before: Option<String>,
}

#[derive(Serialize)]
struct ListResponse {
    conversations: Vec<conversations::ConversationSummary>,
    /// Present when an older page exists. Pass it back as `?before=`.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

async fn list(
    user: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Response {
    let cursor = match query.before.as_deref().map(conversations::ListCursor::decode) {
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
}

/// Rename a conversation. Titles are user-facing labels only; the agent never
/// reads them, so any non-empty text is accepted.
async fn update(
    user: AuthUser,
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
    Json(request): Json<UpdateConversationRequest>,
) -> Response {
    if !owns_conversation(&state, &user, &conversation_id).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    let title: String = request
        .title
        .unwrap_or_default()
        .trim()
        .chars()
        .take(120)
        .collect();
    if title.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "code": "conversation.invalid_title" })),
        )
            .into_response();
    }
    if let Err(error) = sqlx::query("UPDATE threads SET title = ? WHERE id = ? AND user_id = ?")
        .bind(&title)
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
            serde_json::json!({ "title": title }),
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
    user: AuthUser,
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
    let artifacts = match conversations::delete_conversation(&state.db, &user.user_id, &conversation_id).await {
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
    let _ = state
        .conversation_events
        .publish_transient(
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
    user: AuthUser,
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
    /// Last durable user-scoped event the client may use as an SSE cursor.
    event_cursor: i64,
}

async fn snapshot(
    user: AuthUser,
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

    let (messages, runs, latest) = tokio::join!(
        conversations::list_messages(&state.db, &conversation_id, 200),
        conversations::list_runs(&state.db, &conversation_id),
        sqlx::query_scalar::<_, Option<i64>>(
            "SELECT MAX(id) FROM conversation_events WHERE user_id = ?"
        )
        .bind(&user.user_id)
        .fetch_one(&state.db),
    );
    match (messages, runs, latest) {
        (Ok(messages), Ok(runs), Ok(event_cursor)) => {
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
                event_cursor: event_cursor.unwrap_or(0),
            })
            .into_response()
        }
        (Err(error), _, _) | (_, Err(error), _) => internal_error(error),
        (_, _, Err(error)) => internal_error(error.into()),
    }
}

#[derive(Deserialize)]
struct SendMessageRequest {
    text: String,
    client_message_id: String,
}

#[derive(Serialize)]
struct SendMessageResponse {
    run: ConversationRun,
}

/// Accept a user message and start exactly one durable run. The response is
/// intentionally asynchronous; clients learn every transition via `/events`.
async fn send_message(
    user: AuthUser,
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
    Json(request): Json<SendMessageRequest>,
) -> Response {
    let text = request.text.trim().to_owned();
    if text.is_empty() || Uuid::parse_str(&request.client_message_id).is_err() {
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
        return Json(SendMessageResponse { run: ConversationRun {
            id: row.0, client_message_id: request.client_message_id, status: row.1, error_code: row.2,
            created_at: row.3, started_at: row.4, completed_at: row.5,
        }}).into_response();
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
    if let Err(error) = sqlx::query(
        "INSERT INTO conversation_runs (id, thread_id, user_id, client_message_id, status) VALUES (?, ?, ?, ?, 'queued')",
    ).bind(&run_id).bind(&conversation_id).bind(&user.user_id).bind(&request.client_message_id).execute(&state.db).await {
        return internal_error(error.into());
    }
    let created_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
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

    // The engine persists this message shortly after the run begins. Publish
    // the accepted logical message now so every subscribed device shows the
    // same optimistic state, keyed by the stable client-generated ID.
    let message = conversations::ConversationMessage {
        id: request.client_message_id.clone(),
        role: "user".to_owned(),
        content: text.clone(),
        created_at,
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

    tokio::spawn(execute_run(
        state,
        user.user_id,
        pipe_id,
        conversation_id,
        run_id,
        request.client_message_id,
        text,
    ));
    Json(SendMessageResponse { run }).into_response()
}

async fn execute_run(
    state: AppState,
    user_id: String,
    pipe_id: String,
    conversation_id: String,
    run_id: String,
    client_message_id: String,
    text: String,
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
            inbound_attachments: Vec::new(),
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
                if let Err(error) = result { terminal_error = Some(error.to_string()); }
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
    if !crate::web::system_updates::push_notifications_enabled(state).await {
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
    if !crate::web::system_updates::push_notifications_enabled(state).await {
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

async fn handle_engine_event(
    state: &AppState,
    user_id: &str,
    conversation_id: &str,
    run_id: &str,
    event: LoopEvent,
) {
    let (event_type, payload) = match event {
        LoopEvent::Token(text) => ("message.text_delta", serde_json::json!({ "text": text })),
        LoopEvent::AssistantPartFinished => ("message.part_finished", serde_json::json!({})),
        LoopEvent::CapabilityStatus(label) => {
            ("run.progress", serde_json::json!({ "label": label }))
        }
        LoopEvent::Artifacts(artifacts) => (
            "message.artifacts",
            serde_json::json!({ "artifacts": artifacts }),
        ),
        LoopEvent::Done => ("run.output_finished", serde_json::json!({})),
        LoopEvent::Error(_) => (
            "run.error",
            serde_json::json!({ "code": "conversation.agent_failed" }),
        ),
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
            approval_id,
            ..
        } => (
            "approval.queued",
            serde_json::json!({ "approval_id": approval_id, "tool_name": tool_display_name }),
        ),
        LoopEvent::ToolMessage(_) => ("conversation.changed", serde_json::json!({})),
    };
    if let Err(error) = state
        .conversation_events
        .publish(
            &state.db,
            user_id,
            conversation_id,
            Some(run_id),
            event_type,
            Some(run_id),
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
    user: AuthUser,
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
    let user_id = user.user_id;
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
    user: AuthUser,
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

async fn owns_conversation(state: &AppState, user: &AuthUser, conversation_id: &str) -> bool {
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
