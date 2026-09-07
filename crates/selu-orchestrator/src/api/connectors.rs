use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::{
    api::auth::ApiAdmin,
    state::AppState,
    web::{BasePath, ExternalOrigin},
};

pub(crate) mod domain;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/connectors", get(list).post(create_simple))
        .route("/api/v1/connectors/{pipe_id}", delete(remove))
        .route("/api/v1/connectors/imessage", post(create_imessage))
        .route("/api/v1/connectors/imessage/chats", post(imessage_chats))
        .route("/api/v1/connectors/telegram", post(create_telegram))
        .route("/api/v1/connectors/telegram/chats", post(telegram_chats))
        .route(
            "/api/v1/connectors/{pipe_id}/telegram/webhook",
            get(telegram_webhook).post(refresh_telegram_webhook),
        )
        .route("/api/v1/connectors/whatsapp", post(create_whatsapp))
        .route("/api/v1/connectors/whatsapp/chats", get(whatsapp_chats))
        .route("/api/v1/connectors/whatsapp/status", get(whatsapp_status))
        .route("/api/v1/connectors/{pipe_id}/people", post(add_person))
        .route(
            "/api/v1/connectors/{pipe_id}/people/{ref_id}",
            delete(remove_person),
        )
}

#[derive(Debug, Serialize)]
struct ConnectorsResponse {
    connectors: Vec<ConnectorResponse>,
    users: Vec<UserResponse>,
    telegram_https_ready: bool,
}

#[derive(Debug, Serialize)]
struct ConnectorResponse {
    pipe_id: String,
    config_id: Option<String>,
    kind: String,
    name: String,
    owner_user_id: String,
    owner_name: String,
    active: bool,
    callback_url: Option<String>,
    server_url: Option<String>,
    chat_ref: Option<String>,
    people: Vec<PersonResponse>,
    created_at: String,
}

#[derive(Debug, Serialize)]
struct UserResponse {
    id: String,
    display_name: String,
    username: String,
}

#[derive(Debug, Serialize)]
struct PersonResponse {
    ref_id: String,
    user_id: String,
    display_name: String,
    username: String,
    sender_ref: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SimpleConnectorKind {
    Web,
    Webhook,
}

#[derive(Debug, Deserialize)]
struct CreateSimpleRequest {
    kind: SimpleConnectorKind,
    owner_user_id: String,
    name: String,
    callback_url: Option<String>,
    callback_authorization: Option<String>,
}

#[derive(Debug, Serialize)]
struct CreateSimpleResponse {
    pipe_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    inbound_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inbound_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PersonInput {
    user_id: String,
    sender_ref: String,
}

#[derive(Debug, Deserialize)]
struct CreateImessageRequest {
    name: String,
    server_url: String,
    server_password: String,
    chat_guid: String,
    people: Vec<PersonInput>,
    callback_base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateTelegramRequest {
    name: String,
    bot_token: String,
    chat_id: String,
    people: Vec<PersonInput>,
}

#[derive(Debug, Deserialize)]
struct CreateWhatsappRequest {
    name: String,
    callback_authorization: Option<String>,
    people: Vec<PersonInput>,
}

#[derive(Debug, Deserialize)]
struct ImessageChatsRequest {
    server_url: String,
    server_password: String,
}

#[derive(Debug, Deserialize)]
struct TelegramChatsRequest {
    bot_token: String,
}

#[derive(Debug, Deserialize)]
struct WhatsappChatsQuery {
    q: Option<String>,
}

#[derive(Debug, Serialize)]
struct ImessageChatResponse {
    guid: String,
    display_name: String,
    participants: Vec<String>,
    is_group: bool,
    last_message: String,
}

#[derive(Debug, Serialize)]
struct ImessageChatsResponse {
    ok: bool,
    chats: Vec<ImessageChatResponse>,
}

#[derive(Debug, Serialize)]
struct TelegramChatResponse {
    chat_id: String,
    display_name: String,
    chat_type: String,
    last_message: String,
}

#[derive(Debug, Serialize)]
struct TelegramChatsResponse {
    ok: bool,
    bot_username: String,
    chats: Vec<TelegramChatResponse>,
}

#[derive(Debug, Serialize)]
struct WhatsappChatResponse {
    sender_ref: String,
    label: String,
}

#[derive(Debug, Serialize)]
struct WhatsappChatsResponse {
    ok: bool,
    running: bool,
    connection_state: Option<String>,
    message: Option<String>,
    chats: Vec<WhatsappChatResponse>,
}

#[derive(Debug, Serialize)]
struct OperationResponse {
    ok: bool,
}

#[derive(Debug, Serialize)]
struct TelegramWebhookResponse {
    registered: bool,
    url: String,
    pending_updates: i64,
    last_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct WhatsappStatusResponse {
    running: bool,
    connection_state: Option<String>,
    requires_qr: bool,
    qr_data_url: Option<String>,
    jid: Option<String>,
    last_error: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
}

fn error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (
        status,
        Json(ErrorEnvelope {
            error: ErrorBody { code, message },
        }),
    )
        .into_response()
}

fn connector_error(operation: &'static str, failure: domain::ConnectorError) -> Response {
    use domain::ConnectorError;
    match failure {
        ConnectorError::Validation(code) => match code {
            "connector_owner_not_found" => error(
                StatusCode::BAD_REQUEST,
                "connector_owner_not_found",
                "Choose an existing person as the owner.",
            ),
            "person_fields_required" => error(
                StatusCode::BAD_REQUEST,
                "person_fields_required",
                "Choose a person and add the matching sender.",
            ),
            _ => error(
                StatusCode::BAD_REQUEST,
                "connector_fields_required",
                "Fill in all required connection details.",
            ),
        },
        ConnectorError::Conflict(code) => match code {
            "telegram_https_required" => error(
                StatusCode::CONFLICT,
                "telegram_https_required",
                "Telegram needs Selu to use a secure https web address.",
            ),
            "sender_already_added" => error(
                StatusCode::CONFLICT,
                "sender_already_added",
                "That sender is already connected.",
            ),
            _ => error(
                StatusCode::CONFLICT,
                "connector_already_configured",
                "That connection is already set up.",
            ),
        },
        ConnectorError::NotFound => error(
            StatusCode::NOT_FOUND,
            "connector_not_found",
            "That connection or person no longer exists.",
        ),
        ConnectorError::External(source) => {
            tracing::warn!(error = %source, operation, "Connector dependency failed");
            error(
                StatusCode::BAD_GATEWAY,
                "connector_dependency_failed",
                "The connection service could not complete the request.",
            )
        }
        ConnectorError::Internal(source) => {
            tracing::error!(error = %source, operation, "Connector operation failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "connector_operation_failed",
                "The connection could not be changed.",
            )
        }
    }
}

async fn list(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    ExternalOrigin(external_origin): ExternalOrigin,
) -> Response {
    let users =
        match sqlx::query("SELECT id, display_name, username FROM users ORDER BY display_name")
            .fetch_all(&state.db)
            .await
        {
            Ok(rows) => rows
                .into_iter()
                .map(|row| UserResponse {
                    id: row.try_get("id").unwrap_or_default(),
                    display_name: row.try_get("display_name").unwrap_or_default(),
                    username: row.try_get("username").unwrap_or_default(),
                })
                .collect::<Vec<_>>(),
            Err(db_error) => {
                tracing::error!(error = %db_error, "Failed to load connector users");
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "connectors_unavailable",
                    "Connections could not be loaded.",
                );
            }
        };

    let rows = match sqlx::query(
        "SELECT p.id, p.user_id, p.name, p.transport, p.outbound_url, p.active, p.created_at, \
                u.display_name AS owner_name, \
                bc.id AS imessage_config_id, bc.name AS imessage_name, bc.server_url, bc.chat_guid, \
                tc.id AS telegram_config_id, tc.name AS telegram_name, tc.chat_id, \
                wc.id AS whatsapp_config_id, wc.name AS whatsapp_name \
         FROM pipes p \
         JOIN users u ON u.id = p.user_id \
         LEFT JOIN bluebubbles_configs bc ON bc.pipe_id = p.id AND bc.active = 1 \
         LEFT JOIN telegram_configs tc ON tc.pipe_id = p.id AND tc.active = 1 \
         LEFT JOIN whatsapp_configs wc ON wc.pipe_id = p.id AND wc.active = 1 \
         WHERE p.active = 1 \
         ORDER BY p.created_at DESC",
    )
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(db_error) => {
            tracing::error!(error = %db_error, "Failed to list connectors");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "connectors_unavailable",
                "Connections could not be loaded.",
            );
        }
    };

    let mut connectors = Vec::with_capacity(rows.len());
    for row in rows {
        let pipe_id: String = row.try_get("id").unwrap_or_default();
        let people = load_people(&state, &pipe_id).await;
        let imessage_id = row
            .try_get::<Option<String>, _>("imessage_config_id")
            .ok()
            .flatten();
        let telegram_id = row
            .try_get::<Option<String>, _>("telegram_config_id")
            .ok()
            .flatten();
        let whatsapp_id = row
            .try_get::<Option<String>, _>("whatsapp_config_id")
            .ok()
            .flatten();
        let transport: String = row.try_get("transport").unwrap_or_default();
        let outbound_url: String = row.try_get("outbound_url").unwrap_or_default();

        let (kind, config_id, name, callback_url, server_url, chat_ref) =
            if let Some(config_id) = imessage_id {
                (
                    "imessage".to_string(),
                    Some(config_id),
                    row.try_get("imessage_name").unwrap_or_default(),
                    None,
                    row.try_get::<Option<String>, _>("server_url")
                        .ok()
                        .flatten(),
                    row.try_get::<Option<String>, _>("chat_guid").ok().flatten(),
                )
            } else if let Some(config_id) = telegram_id {
                (
                    "telegram".to_string(),
                    Some(config_id),
                    row.try_get("telegram_name").unwrap_or_default(),
                    None,
                    None,
                    row.try_get::<Option<String>, _>("chat_id").ok().flatten(),
                )
            } else if let Some(config_id) = whatsapp_id {
                (
                    "whatsapp".to_string(),
                    Some(config_id),
                    row.try_get("whatsapp_name").unwrap_or_default(),
                    None,
                    None,
                    None,
                )
            } else if transport == "web" {
                (
                    "web".to_string(),
                    None,
                    row.try_get("name").unwrap_or_default(),
                    None,
                    None,
                    None,
                )
            } else {
                (
                    "webhook".to_string(),
                    None,
                    row.try_get("name").unwrap_or_default(),
                    Some(outbound_url),
                    None,
                    None,
                )
            };

        connectors.push(ConnectorResponse {
            pipe_id,
            config_id,
            kind,
            name,
            owner_user_id: row.try_get("user_id").unwrap_or_default(),
            owner_name: row.try_get("owner_name").unwrap_or_default(),
            active: row.try_get::<i64, _>("active").unwrap_or(0) != 0,
            callback_url,
            server_url,
            chat_ref,
            people,
            created_at: row.try_get("created_at").unwrap_or_default(),
        });
    }

    Json(ConnectorsResponse {
        connectors,
        users,
        telegram_https_ready: external_origin.starts_with("https://"),
    })
    .into_response()
}

async fn create_simple(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    ExternalOrigin(external_origin): ExternalOrigin,
    Json(request): Json<CreateSimpleRequest>,
) -> Response {
    let (transport, outbound_url, reveals_inbound) = match request.kind {
        SimpleConnectorKind::Web => ("web", "internal://web".to_string(), false),
        SimpleConnectorKind::Webhook => {
            let Some(callback) = request
                .callback_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                return error(
                    StatusCode::BAD_REQUEST,
                    "callback_url_required",
                    "Add the web address that should receive replies.",
                );
            };
            let Some(parsed) = parse_http_url(callback) else {
                return error(
                    StatusCode::BAD_REQUEST,
                    "invalid_callback_url",
                    "Enter a full http or https web address.",
                );
            };
            ("webhook", parsed.to_string(), true)
        }
    };

    let created = match domain::create_simple(
        &state,
        domain::SimpleConnectorInput {
            owner_user_id: request.owner_user_id,
            name: request.name,
            transport,
            outbound_url,
            outbound_auth: request.callback_authorization,
        },
    )
    .await
    {
        Ok(created) => created,
        Err(failure) => return connector_error("create simple connector", failure),
    };
    let inbound_url = reveals_inbound.then(|| {
        format!(
            "{}/api/pipes/{}/inbound",
            external_origin.trim_end_matches('/'),
            created.pipe_id
        )
    });
    let inbound_token = reveals_inbound.then_some(created.inbound_token);
    (
        StatusCode::CREATED,
        [
            (
                header::LOCATION,
                format!("{base_path}/api/v1/connectors/{}", created.pipe_id),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Json(CreateSimpleResponse {
            pipe_id: created.pipe_id,
            inbound_url,
            inbound_token,
        }),
    )
        .into_response()
}

async fn create_imessage(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Json(request): Json<CreateImessageRequest>,
) -> Response {
    let callback_is_valid = request
        .callback_base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none_or(|value| parse_http_url(value).is_some());
    if parse_http_url(&request.server_url).is_none() || !callback_is_valid {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_connector_url",
            "Enter a full http or https web address.",
        );
    }
    match domain::create_imessage(
        &state,
        domain::ImessageConnectorInput {
            name: request.name,
            server_url: request.server_url,
            server_password: request.server_password,
            chat_guid: request.chat_guid,
            callback_base_url: request.callback_base_url,
            people: domain_people(request.people),
        },
    )
    .await
    {
        Ok(_) => (StatusCode::CREATED, Json(OperationResponse { ok: true })).into_response(),
        Err(failure) => connector_error("create iMessage connector", failure),
    }
}

async fn create_telegram(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    ExternalOrigin(origin): ExternalOrigin,
    Json(request): Json<CreateTelegramRequest>,
) -> Response {
    match domain::create_telegram(
        &state,
        &origin,
        domain::TelegramConnectorInput {
            name: request.name,
            bot_token: request.bot_token,
            chat_id: request.chat_id,
            people: domain_people(request.people),
        },
    )
    .await
    {
        Ok(_) => (StatusCode::CREATED, Json(OperationResponse { ok: true })).into_response(),
        Err(failure) => connector_error("create Telegram connector", failure),
    }
}

async fn create_whatsapp(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Json(request): Json<CreateWhatsappRequest>,
) -> Response {
    match domain::create_whatsapp(
        &state,
        domain::WhatsappConnectorInput {
            name: request.name,
            outbound_auth: request.callback_authorization,
            people: domain_people(request.people),
        },
    )
    .await
    {
        Ok(_) => (StatusCode::CREATED, Json(OperationResponse { ok: true })).into_response(),
        Err(failure) => connector_error("create WhatsApp connector", failure),
    }
}

async fn imessage_chats(_admin: ApiAdmin, Json(request): Json<ImessageChatsRequest>) -> Response {
    if parse_http_url(&request.server_url).is_none() {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_connector_url",
            "Enter a full http or https web address.",
        );
    }
    match domain::discover_imessage_chats(&request.server_url, &request.server_password).await {
        Ok(chats) => Json(ImessageChatsResponse {
            ok: true,
            chats: chats
                .into_iter()
                .map(|chat| ImessageChatResponse {
                    guid: chat.guid,
                    display_name: chat.display_name,
                    participants: chat.participants,
                    is_group: chat.is_group,
                    last_message: chat.last_message,
                })
                .collect(),
        })
        .into_response(),
        Err(failure) => {
            tracing::warn!(error = %failure, "BlueBubbles chat discovery failed");
            error(
                StatusCode::BAD_GATEWAY,
                "imessage_discovery_failed",
                "Chats could not be loaded from BlueBubbles.",
            )
        }
    }
}

async fn telegram_chats(_admin: ApiAdmin, Json(request): Json<TelegramChatsRequest>) -> Response {
    match domain::discover_telegram_chats(&request.bot_token).await {
        Ok(result) => Json(TelegramChatsResponse {
            ok: true,
            bot_username: result.bot_username,
            chats: result
                .chats
                .into_iter()
                .map(|chat| TelegramChatResponse {
                    chat_id: chat.chat_id,
                    display_name: chat.display_name,
                    chat_type: chat.chat_type,
                    last_message: chat.last_message,
                })
                .collect(),
        })
        .into_response(),
        Err(failure) => {
            tracing::warn!(error = %failure, "Telegram chat discovery failed");
            error(
                StatusCode::BAD_GATEWAY,
                "telegram_discovery_failed",
                "Chats could not be loaded from Telegram.",
            )
        }
    }
}

async fn whatsapp_chats(
    _admin: ApiAdmin,
    State(state): State<AppState>,
    Query(query): Query<WhatsappChatsQuery>,
) -> Response {
    match domain::whatsapp_chats(&state, query.q.as_deref().unwrap_or_default()).await {
        Ok(result) => Json(WhatsappChatsResponse {
            ok: true,
            running: result.running,
            connection_state: result.connection_state,
            message: result.message,
            chats: result
                .chats
                .into_iter()
                .map(|chat| WhatsappChatResponse {
                    sender_ref: chat.sender_ref,
                    label: chat.label,
                })
                .collect(),
        })
        .into_response(),
        Err(failure) => {
            tracing::warn!(error = %failure, "WhatsApp chat discovery failed");
            error(
                StatusCode::BAD_GATEWAY,
                "whatsapp_bridge_unavailable",
                "The WhatsApp connection service could not be reached.",
            )
        }
    }
}

async fn whatsapp_status(_admin: ApiAdmin, State(state): State<AppState>) -> Response {
    match domain::whatsapp_status(&state).await {
        Ok(status) => Json(WhatsappStatusResponse {
            running: status.running,
            connection_state: status.connection_state,
            requires_qr: status.requires_qr,
            qr_data_url: status.qr_data_url,
            jid: status.jid,
            last_error: status.last_error,
            message: status.message,
        })
        .into_response(),
        Err(failure) => {
            tracing::warn!(error = %failure, "Failed to load WhatsApp bridge status");
            error(
                StatusCode::BAD_GATEWAY,
                "whatsapp_bridge_unavailable",
                "The WhatsApp connection service could not be reached.",
            )
        }
    }
}

async fn add_person(
    _admin: ApiAdmin,
    Path(pipe_id): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<PersonInput>,
) -> Response {
    match domain::add_person(
        &state,
        &pipe_id,
        domain::PersonInput {
            user_id: request.user_id,
            sender_ref: request.sender_ref,
        },
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(failure) => connector_error("add connector person", failure),
    }
}

async fn remove_person(
    _admin: ApiAdmin,
    Path((pipe_id, ref_id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    match domain::remove_person(&state, &pipe_id, &ref_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(failure) => connector_error("remove connector person", failure),
    }
}

async fn telegram_webhook(
    _admin: ApiAdmin,
    Path(pipe_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    let token = match domain::telegram_token_for_pipe(&state, &pipe_id).await {
        Ok(token) => token,
        Err(failure) => return connector_error("load Telegram webhook", failure),
    };
    match crate::telegram::adapter::get_webhook_info(&token).await {
        Ok(info) => Json(TelegramWebhookResponse {
            registered: !info.url.is_empty(),
            url: info.url,
            pending_updates: info.pending_update_count,
            last_error: info.last_error_message,
        })
        .into_response(),
        Err(check_error) => {
            tracing::warn!(error = %check_error, "Telegram webhook check failed");
            error(
                StatusCode::BAD_GATEWAY,
                "telegram_check_failed",
                "The Telegram connection could not be checked.",
            )
        }
    }
}

async fn refresh_telegram_webhook(
    _admin: ApiAdmin,
    Path(pipe_id): Path<String>,
    State(state): State<AppState>,
    ExternalOrigin(origin): ExternalOrigin,
) -> Response {
    if !origin.starts_with("https://") {
        return error(
            StatusCode::CONFLICT,
            "telegram_https_required",
            "Telegram needs Selu to use a secure https web address.",
        );
    }
    let config_id = match domain::telegram_config_id_for_pipe(&state, &pipe_id).await {
        Ok(config_id) => config_id,
        Err(failure) => return connector_error("resolve Telegram connector", failure),
    };
    match crate::telegram::adapter::reregister_webhook(&state, &config_id, &origin).await {
        Ok(()) => Json(OperationResponse { ok: true }).into_response(),
        Err(refresh_error) => {
            tracing::warn!(error = %refresh_error, "Telegram webhook refresh failed");
            error(
                StatusCode::BAD_GATEWAY,
                "telegram_refresh_failed",
                "The Telegram connection could not be refreshed.",
            )
        }
    }
}

async fn remove(
    _admin: ApiAdmin,
    Path(pipe_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    match domain::remove_connector(&state, &pipe_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(failure) => connector_error("remove connector", failure),
    }
}

fn domain_people(people: Vec<PersonInput>) -> Vec<domain::PersonInput> {
    people
        .into_iter()
        .map(|person| domain::PersonInput {
            user_id: person.user_id,
            sender_ref: person.sender_ref,
        })
        .collect()
}

fn valid_http_url(url: &reqwest::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

fn parse_http_url(value: &str) -> Option<reqwest::Url> {
    reqwest::Url::parse(value.trim())
        .ok()
        .filter(valid_http_url)
}

async fn load_people(state: &AppState, pipe_id: &str) -> Vec<PersonResponse> {
    sqlx::query(
        "SELECT sr.id, sr.user_id, sr.sender_ref, u.display_name, u.username \
         FROM user_sender_refs sr JOIN users u ON u.id = sr.user_id \
         WHERE sr.pipe_id = ? ORDER BY u.display_name",
    )
    .bind(pipe_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|row| PersonResponse {
        ref_id: row.try_get("id").unwrap_or_default(),
        user_id: row.try_get("user_id").unwrap_or_default(),
        display_name: row.try_get("display_name").unwrap_or_default(),
        username: row.try_get("username").unwrap_or_default(),
        sender_ref: row.try_get("sender_ref").unwrap_or_default(),
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn people_are_converted_for_the_domain_service() {
        let value = domain_people(vec![PersonInput {
            user_id: "user-1".to_string(),
            sender_ref: "+49123".to_string(),
        }]);
        assert_eq!(value.len(), 1);
        assert_eq!(value[0].sender_ref, "+49123");
    }

    #[test]
    fn connector_urls_require_plain_http_or_https() {
        assert!(parse_http_url("https://example.test/callback?source=selu").is_some());
        assert!(parse_http_url("http://bluebubbles.local:1234").is_some());
        assert!(parse_http_url("javascript:alert(1)").is_none());
        assert!(parse_http_url("https://user:secret@example.test").is_none());
        assert!(parse_http_url("https://example.test/#secret").is_none());
    }
}
