use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};
use tracing::{info, warn};
use uuid::Uuid;

use crate::permissions::CredentialStore;
use crate::state::AppState;
use crate::updater::client::SidecarUpdaterClient;
use crate::updater::types::{
    SidecarEnsureWhatsappBridgeRequest, SidecarStopWhatsappBridgeRequest,
    SidecarWhatsappBridgeChatsResponse, SidecarWhatsappBridgeStatusResponse,
};

const DEFAULT_BRIDGE_OUTBOUND_URL_DOCKER: &str =
    "http://selu-whatsapp-bridge:3200/webhooks/selu/outbound";
const DEFAULT_BRIDGE_OUTBOUND_URL_LOCAL: &str = "http://127.0.0.1:3200/webhooks/selu/outbound";
const DEFAULT_SELU_INBOUND_ORIGIN_DOCKER: &str = "http://selu";
const DEFAULT_SELU_INBOUND_ORIGIN_LOCAL: &str = "http://host.docker.internal";

#[derive(Debug, thiserror::Error)]
pub enum ConnectorError {
    #[error("{0}")]
    Validation(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("connector not found")]
    NotFound,
    #[error("connector dependency failed")]
    External(#[source] anyhow::Error),
    #[error("connector persistence failed")]
    Internal(#[source] anyhow::Error),
}

impl ConnectorError {
    fn internal(error: impl Into<anyhow::Error>) -> Self {
        Self::Internal(error.into())
    }

    fn external(error: impl Into<anyhow::Error>) -> Self {
        Self::External(error.into())
    }
}

#[derive(Debug, Clone)]
pub struct PersonInput {
    pub user_id: String,
    pub sender_ref: String,
}

#[derive(Debug)]
pub struct SimpleConnectorInput {
    pub owner_user_id: String,
    pub name: String,
    pub transport: &'static str,
    pub outbound_url: String,
    pub outbound_auth: Option<String>,
}

#[derive(Debug)]
pub struct ImessageConnectorInput {
    pub name: String,
    pub server_url: String,
    pub server_password: String,
    pub chat_guid: String,
    pub callback_base_url: Option<String>,
    pub people: Vec<PersonInput>,
}

#[derive(Debug)]
pub struct TelegramConnectorInput {
    pub name: String,
    pub bot_token: String,
    pub chat_id: String,
    pub people: Vec<PersonInput>,
}

#[derive(Debug)]
pub struct WhatsappConnectorInput {
    pub name: String,
    pub outbound_auth: Option<String>,
    pub people: Vec<PersonInput>,
}

#[derive(Debug)]
pub struct CreatedConnector {
    pub pipe_id: String,
    pub inbound_token: String,
}

#[derive(Debug, Clone)]
pub struct ImessageChat {
    pub guid: String,
    pub display_name: String,
    pub participants: Vec<String>,
    pub is_group: bool,
    pub last_message: String,
}

#[derive(Debug)]
pub struct TelegramChats {
    pub bot_username: String,
    pub chats: Vec<crate::telegram::adapter::TgRecentChat>,
}

#[derive(Debug, Deserialize)]
struct BbApiChatResponse {
    data: Vec<BbApiChat>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BbApiChat {
    guid: Option<String>,
    display_name: Option<String>,
    #[serde(default)]
    participants: Option<Vec<BbApiHandle>>,
    #[serde(default)]
    group_id: Option<String>,
    #[serde(default)]
    last_message: Option<BbApiLastMessage>,
}

#[derive(Debug, Deserialize)]
struct BbApiHandle {
    #[serde(default)]
    address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BbApiLastMessage {
    #[serde(default)]
    text: Option<String>,
}

pub async fn create_simple(
    state: &AppState,
    input: SimpleConnectorInput,
) -> Result<CreatedConnector, ConnectorError> {
    let name = input.name.trim();
    if name.is_empty() || input.owner_user_id.trim().is_empty() {
        return Err(ConnectorError::Validation("connector_fields_required"));
    }
    ensure_user(&state.db, input.owner_user_id.trim()).await?;

    let pipe_id = Uuid::new_v4().to_string();
    let inbound_token = Uuid::new_v4().simple().to_string();
    let inbound_encrypted = encrypt(&state.credentials, &inbound_token)?;
    let outbound_auth_encrypted = input
        .outbound_auth
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| encrypt(&state.credentials, value))
        .transpose()?;

    sqlx::query(
        "INSERT INTO pipes \
         (id, user_id, name, transport, inbound_token, inbound_token_encrypted, \
          outbound_url, outbound_auth, outbound_auth_encrypted, default_agent_id) \
         VALUES (?, ?, ?, ?, '', ?, ?, NULL, ?, NULL)",
    )
    .bind(&pipe_id)
    .bind(input.owner_user_id.trim())
    .bind(name)
    .bind(input.transport)
    .bind(inbound_encrypted)
    .bind(input.outbound_url)
    .bind(outbound_auth_encrypted)
    .execute(&state.db)
    .await
    .context("create simple connector pipe")
    .map_err(ConnectorError::internal)?;

    Ok(CreatedConnector {
        pipe_id,
        inbound_token,
    })
}

pub async fn create_imessage(
    state: &AppState,
    input: ImessageConnectorInput,
) -> Result<CreatedConnector, ConnectorError> {
    if input.name.trim().is_empty()
        || input.server_url.trim().is_empty()
        || input.server_password.is_empty()
        || input.chat_guid.trim().is_empty()
    {
        return Err(ConnectorError::Validation("connector_fields_required"));
    }
    if active_config_exists(&state.db, "bluebubbles_configs").await? {
        return Err(ConnectorError::Conflict("imessage_already_configured"));
    }
    let owner_user_id = resolve_owner(&state.db, &input.people).await?;
    validate_people(&state.db, &input.people).await?;

    let pipe_id = Uuid::new_v4().to_string();
    let config_id = Uuid::new_v4().to_string();
    let inbound_token = Uuid::new_v4().simple().to_string();
    let inbound_encrypted = encrypt(&state.credentials, &inbound_token)?;
    let password_encrypted = encrypt(&state.credentials, &input.server_password)?;
    let callback_base_url = input
        .callback_base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut tx = state
        .db
        .begin()
        .await
        .context("begin iMessage connector transaction")
        .map_err(ConnectorError::internal)?;
    sqlx::query(
        "INSERT INTO pipes \
         (id, user_id, name, transport, inbound_token, inbound_token_encrypted, \
          outbound_url, default_agent_id) \
         VALUES (?, ?, ?, 'webhook', '', ?, 'internal://bluebubbles', NULL)",
    )
    .bind(&pipe_id)
    .bind(&owner_user_id)
    .bind(format!("iMessage: {}", input.name.trim()))
    .bind(inbound_encrypted)
    .execute(&mut *tx)
    .await
    .context("create iMessage pipe")
    .map_err(ConnectorError::internal)?;
    sqlx::query(
        "INSERT INTO bluebubbles_configs \
         (id, name, server_url, server_password, server_password_encrypted, chat_guid, \
          pipe_id, callback_base_url) \
         VALUES (?, ?, ?, '', ?, ?, ?, ?)",
    )
    .bind(&config_id)
    .bind(input.name.trim())
    .bind(input.server_url.trim().trim_end_matches('/'))
    .bind(password_encrypted)
    .bind(input.chat_guid.trim())
    .bind(&pipe_id)
    .bind(callback_base_url)
    .execute(&mut *tx)
    .await
    .context("create BlueBubbles config")
    .map_err(ConnectorError::internal)?;
    insert_people(&mut tx, &pipe_id, &input.people).await?;
    tx.commit()
        .await
        .context("commit iMessage connector transaction")
        .map_err(ConnectorError::internal)?;

    if let Err(error) = crate::bluebubbles::adapter::start_one(state.clone(), &config_id).await {
        rollback_created_connector(state, &pipe_id, Some(("bluebubbles_configs", &config_id)))
            .await;
        return Err(ConnectorError::external(
            error.context("register BlueBubbles adapter for new connector"),
        ));
    }

    Ok(CreatedConnector {
        pipe_id,
        inbound_token,
    })
}

pub async fn create_telegram(
    state: &AppState,
    external_origin: &str,
    input: TelegramConnectorInput,
) -> Result<CreatedConnector, ConnectorError> {
    if !external_origin.starts_with("https://") {
        return Err(ConnectorError::Conflict("telegram_https_required"));
    }
    if input.name.trim().is_empty()
        || input.bot_token.trim().is_empty()
        || input.chat_id.trim().is_empty()
    {
        return Err(ConnectorError::Validation("connector_fields_required"));
    }
    if active_config_exists(&state.db, "telegram_configs").await? {
        return Err(ConnectorError::Conflict("telegram_already_configured"));
    }
    let owner_user_id = resolve_owner(&state.db, &input.people).await?;
    validate_people(&state.db, &input.people).await?;

    let pipe_id = Uuid::new_v4().to_string();
    let config_id = Uuid::new_v4().to_string();
    let inbound_token = Uuid::new_v4().simple().to_string();
    let inbound_encrypted = encrypt(&state.credentials, &inbound_token)?;
    let bot_token_encrypted = encrypt(&state.credentials, input.bot_token.trim())?;

    let mut tx = state
        .db
        .begin()
        .await
        .context("begin Telegram connector transaction")
        .map_err(ConnectorError::internal)?;
    sqlx::query(
        "INSERT INTO pipes \
         (id, user_id, name, transport, inbound_token, inbound_token_encrypted, \
          outbound_url, default_agent_id) \
         VALUES (?, ?, ?, 'webhook', '', ?, 'internal://telegram', NULL)",
    )
    .bind(&pipe_id)
    .bind(&owner_user_id)
    .bind(format!("Telegram: {}", input.name.trim()))
    .bind(inbound_encrypted)
    .execute(&mut *tx)
    .await
    .context("create Telegram pipe")
    .map_err(ConnectorError::internal)?;
    sqlx::query(
        "INSERT INTO telegram_configs \
         (id, name, bot_token, bot_token_encrypted, chat_id, pipe_id) \
         VALUES (?, ?, '', ?, ?, ?)",
    )
    .bind(&config_id)
    .bind(input.name.trim())
    .bind(bot_token_encrypted)
    .bind(input.chat_id.trim())
    .bind(&pipe_id)
    .execute(&mut *tx)
    .await
    .context("create Telegram config")
    .map_err(ConnectorError::internal)?;
    insert_people(&mut tx, &pipe_id, &input.people).await?;
    tx.commit()
        .await
        .context("commit Telegram connector transaction")
        .map_err(ConnectorError::internal)?;

    if let Err(error) =
        crate::telegram::adapter::start_one(state.clone(), &config_id, Some(external_origin)).await
    {
        rollback_created_connector(state, &pipe_id, Some(("telegram_configs", &config_id))).await;
        return Err(ConnectorError::external(
            error.context("register Telegram webhook for new connector"),
        ));
    }

    Ok(CreatedConnector {
        pipe_id,
        inbound_token,
    })
}

pub async fn create_whatsapp(
    state: &AppState,
    input: WhatsappConnectorInput,
) -> Result<CreatedConnector, ConnectorError> {
    if input.name.trim().is_empty() {
        return Err(ConnectorError::Validation("connector_fields_required"));
    }
    if active_config_exists(&state.db, "whatsapp_configs").await? {
        return Err(ConnectorError::Conflict("whatsapp_already_configured"));
    }
    let owner_user_id = resolve_owner(&state.db, &input.people).await?;
    validate_people(&state.db, &input.people).await?;

    let pipe_id = Uuid::new_v4().to_string();
    let config_id = Uuid::new_v4().to_string();
    let inbound_token = Uuid::new_v4().simple().to_string();
    let inbound_encrypted = encrypt(&state.credentials, &inbound_token)?;
    let outbound_auth = input
        .outbound_auth
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let outbound_auth_encrypted = outbound_auth
        .map(|value| encrypt(&state.credentials, value))
        .transpose()?;
    let bridge_url = managed_bridge_outbound_url(state);
    let inbound_url = managed_bridge_inbound_url(state, &pipe_id);
    let client = SidecarUpdaterClient::from_config(&state.config)
        .context("create updater client for WhatsApp setup")
        .map_err(ConnectorError::external)?;
    let channel = crate::services::system_updates::release_channel(state).await;
    let maintenance_lease = state.docker_storage.shared_lease().await;
    let ensure = client
        .ensure_whatsapp_bridge(&SidecarEnsureWhatsappBridgeRequest {
            request_id: Uuid::new_v4().to_string(),
            channel: channel.clone(),
            inbound_url,
            inbound_token: inbound_token.clone(),
            outbound_auth: outbound_auth.unwrap_or_default().to_string(),
        })
        .await
        .context("start WhatsApp bridge")
        .map_err(ConnectorError::external)?;
    drop(maintenance_lease);
    if !ensure.accepted {
        return Err(ConnectorError::external(anyhow!(
            "{}",
            ensure
                .message
                .unwrap_or_else(|| "updates service rejected WhatsApp bridge startup".to_string())
        )));
    }

    let persisted = persist_whatsapp(
        state,
        &pipe_id,
        &config_id,
        &owner_user_id,
        input.name.trim(),
        &bridge_url,
        &inbound_encrypted,
        outbound_auth_encrypted.as_deref(),
        &input.people,
    )
    .await;
    if let Err(error) = persisted {
        if let Err(cleanup_error) = stop_whatsapp_bridge(state, &client, &channel).await {
            warn!(error = %cleanup_error, "WhatsApp setup rollback could not stop sidecar");
        }
        return Err(error);
    }

    Ok(CreatedConnector {
        pipe_id,
        inbound_token,
    })
}

#[allow(clippy::too_many_arguments)]
async fn persist_whatsapp(
    state: &AppState,
    pipe_id: &str,
    config_id: &str,
    owner_user_id: &str,
    name: &str,
    bridge_url: &str,
    inbound_encrypted: &str,
    outbound_auth_encrypted: Option<&str>,
    people: &[PersonInput],
) -> Result<(), ConnectorError> {
    let mut tx = state
        .db
        .begin()
        .await
        .context("begin WhatsApp connector transaction")
        .map_err(ConnectorError::internal)?;
    sqlx::query(
        "INSERT INTO pipes \
         (id, user_id, name, transport, inbound_token, inbound_token_encrypted, \
          outbound_url, outbound_auth, outbound_auth_encrypted, default_agent_id) \
         VALUES (?, ?, ?, 'webhook', '', ?, ?, NULL, ?, NULL)",
    )
    .bind(pipe_id)
    .bind(owner_user_id)
    .bind(format!("WhatsApp: {name}"))
    .bind(inbound_encrypted)
    .bind(bridge_url)
    .bind(outbound_auth_encrypted)
    .execute(&mut *tx)
    .await
    .context("create WhatsApp pipe")
    .map_err(ConnectorError::internal)?;
    sqlx::query("INSERT INTO whatsapp_configs (id, name, bridge_url, pipe_id) VALUES (?, ?, ?, ?)")
        .bind(config_id)
        .bind(name)
        .bind(bridge_url)
        .bind(pipe_id)
        .execute(&mut *tx)
        .await
        .context("create WhatsApp config")
        .map_err(ConnectorError::internal)?;
    insert_people(&mut tx, pipe_id, people).await?;
    tx.commit()
        .await
        .context("commit WhatsApp connector transaction")
        .map_err(ConnectorError::internal)
}

pub async fn add_person(
    state: &AppState,
    pipe_id: &str,
    person: PersonInput,
) -> Result<(), ConnectorError> {
    if person.user_id.trim().is_empty() || person.sender_ref.trim().is_empty() {
        return Err(ConnectorError::Validation("person_fields_required"));
    }
    ensure_user(&state.db, person.user_id.trim()).await?;
    ensure_active_pipe(&state.db, pipe_id).await?;
    sqlx::query(
        "INSERT INTO user_sender_refs (id, user_id, pipe_id, sender_ref) VALUES (?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(person.user_id.trim())
    .bind(pipe_id)
    .bind(person.sender_ref.trim())
    .execute(&state.db)
    .await
    .map_err(|error| {
        if error
            .as_database_error()
            .is_some_and(|db_error| db_error.is_unique_violation())
        {
            ConnectorError::Conflict("sender_already_added")
        } else {
            ConnectorError::internal(error)
        }
    })?;
    Ok(())
}

pub async fn remove_person(
    state: &AppState,
    pipe_id: &str,
    ref_id: &str,
) -> Result<(), ConnectorError> {
    let result = sqlx::query("DELETE FROM user_sender_refs WHERE id = ? AND pipe_id = ?")
        .bind(ref_id)
        .bind(pipe_id)
        .execute(&state.db)
        .await
        .context("remove connector person")
        .map_err(ConnectorError::internal)?;
    if result.rows_affected() == 0 {
        return Err(ConnectorError::NotFound);
    }
    Ok(())
}

pub async fn remove_connector(state: &AppState, pipe_id: &str) -> Result<(), ConnectorError> {
    let row = sqlx::query(
        "SELECT bc.id AS imessage_id, bc.server_url, bc.server_password_encrypted, \
                bc.bb_webhook_id, tc.id AS telegram_id, tc.bot_token_encrypted, \
                wc.id AS whatsapp_id \
         FROM pipes p \
         LEFT JOIN bluebubbles_configs bc ON bc.pipe_id = p.id AND bc.active = 1 \
         LEFT JOIN telegram_configs tc ON tc.pipe_id = p.id AND tc.active = 1 \
         LEFT JOIN whatsapp_configs wc ON wc.pipe_id = p.id AND wc.active = 1 \
         WHERE p.id = ? AND p.active = 1",
    )
    .bind(pipe_id)
    .fetch_optional(&state.db)
    .await
    .context("load connector for removal")
    .map_err(ConnectorError::internal)?
    .ok_or(ConnectorError::NotFound)?;

    let imessage_id: Option<String> = row.try_get("imessage_id").ok().flatten();
    let telegram_id: Option<String> = row.try_get("telegram_id").ok().flatten();
    let whatsapp_id: Option<String> = row.try_get("whatsapp_id").ok().flatten();

    if let Some(config_id) = imessage_id.as_deref() {
        let server_url: String = row.try_get("server_url").unwrap_or_default();
        let password_blob: Option<String> = row.try_get("server_password_encrypted").ok().flatten();
        let webhook_id: Option<String> = row.try_get("bb_webhook_id").ok().flatten();
        if let (Some(password), Some(webhook_id)) = (
            decrypt_optional(&state.credentials, password_blob.as_deref())
                .map_err(ConnectorError::internal)?,
            webhook_id,
        ) && let Err(error) = crate::bluebubbles::adapter::deregister_webhook_from_bb(
            &server_url,
            &password,
            &webhook_id,
        )
        .await
        {
            warn!(config_id, error = %error, "Failed to deregister BlueBubbles webhook");
        }
        deactivate_connector(state, pipe_id, Some(("bluebubbles_configs", config_id))).await?;
        crate::bluebubbles::adapter::stop_one(state, config_id, pipe_id).await;
        return Ok(());
    }

    if let Some(config_id) = telegram_id.as_deref() {
        let token_blob: Option<String> = row.try_get("bot_token_encrypted").ok().flatten();
        if let Some(token) = decrypt_optional(&state.credentials, token_blob.as_deref())
            .map_err(ConnectorError::internal)?
            && let Err(error) = crate::telegram::adapter::delete_webhook(&token).await
        {
            warn!(config_id, error = %error, "Failed to delete Telegram webhook");
        }
        deactivate_connector(state, pipe_id, Some(("telegram_configs", config_id))).await?;
        crate::telegram::adapter::stop_one(state, config_id, pipe_id).await;
        return Ok(());
    }

    if let Some(config_id) = whatsapp_id.as_deref() {
        deactivate_connector(state, pipe_id, Some(("whatsapp_configs", config_id))).await?;
        stop_bridge_if_no_active_connector(state)
            .await
            .map_err(ConnectorError::external)?;
        return Ok(());
    }

    deactivate_connector(state, pipe_id, None).await
}

async fn deactivate_connector(
    state: &AppState,
    pipe_id: &str,
    config: Option<(&str, &str)>,
) -> Result<(), ConnectorError> {
    let mut tx = state
        .db
        .begin()
        .await
        .context("begin connector removal transaction")
        .map_err(ConnectorError::internal)?;
    if let Some((table, config_id)) = config {
        let sql = match table {
            "bluebubbles_configs" => "UPDATE bluebubbles_configs SET active = 0 WHERE id = ?",
            "telegram_configs" => "UPDATE telegram_configs SET active = 0 WHERE id = ?",
            "whatsapp_configs" => "UPDATE whatsapp_configs SET active = 0 WHERE id = ?",
            _ => return Err(ConnectorError::internal(anyhow!("unknown connector table"))),
        };
        sqlx::query(sql)
            .bind(config_id)
            .execute(&mut *tx)
            .await
            .context("deactivate connector config")
            .map_err(ConnectorError::internal)?;
    }
    let result = sqlx::query("UPDATE pipes SET active = 0 WHERE id = ? AND active = 1")
        .bind(pipe_id)
        .execute(&mut *tx)
        .await
        .context("deactivate connector pipe")
        .map_err(ConnectorError::internal)?;
    if result.rows_affected() == 0 {
        return Err(ConnectorError::NotFound);
    }
    tx.commit()
        .await
        .context("commit connector removal")
        .map_err(ConnectorError::internal)
}

pub async fn telegram_token_for_pipe(
    state: &AppState,
    pipe_id: &str,
) -> Result<String, ConnectorError> {
    let blob = sqlx::query_scalar::<_, String>(
        "SELECT tc.bot_token_encrypted FROM telegram_configs tc \
         JOIN pipes p ON p.id = tc.pipe_id \
         WHERE tc.pipe_id = ? AND tc.active = 1 AND p.active = 1",
    )
    .bind(pipe_id)
    .fetch_optional(&state.db)
    .await
    .context("load Telegram connector secret")
    .map_err(ConnectorError::internal)?
    .ok_or(ConnectorError::NotFound)?;
    decrypt_required(&state.credentials, &blob).map_err(ConnectorError::internal)
}

pub async fn telegram_config_id_for_pipe(
    state: &AppState,
    pipe_id: &str,
) -> Result<String, ConnectorError> {
    sqlx::query_scalar::<_, String>(
        "SELECT tc.id FROM telegram_configs tc \
         JOIN pipes p ON p.id = tc.pipe_id \
         WHERE tc.pipe_id = ? AND tc.active = 1 AND p.active = 1",
    )
    .bind(pipe_id)
    .fetch_optional(&state.db)
    .await
    .context("load Telegram connector")
    .map_err(ConnectorError::internal)?
    .ok_or(ConnectorError::NotFound)
}

pub async fn discover_telegram_chats(bot_token: &str) -> Result<TelegramChats> {
    let token = bot_token.trim();
    let bot_username = crate::telegram::adapter::verify_bot_token(token).await?;
    let chats = crate::telegram::adapter::get_recent_chats(token)
        .await
        .unwrap_or_default();
    Ok(TelegramChats {
        bot_username,
        chats,
    })
}

pub async fn discover_imessage_chats(
    server_url: &str,
    server_password: &str,
) -> Result<Vec<ImessageChat>> {
    let server_url = server_url.trim().trim_end_matches('/');
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build BlueBubbles discovery client")?;
    let response = http
        .post(format!("{server_url}/api/v1/chat/query"))
        .query(&[("password", server_password)])
        .json(&serde_json::json!({
            "with": ["lastMessage", "participants"],
            "limit": 50,
            "sort": "lastmessage",
        }))
        .send()
        .await
        .context("reach BlueBubbles server")?;
    let status = response.status();
    let raw_body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("BlueBubbles returned {status}"));
    }
    let parsed: BbApiChatResponse =
        serde_json::from_str(&raw_body).context("parse BlueBubbles response")?;
    Ok(parsed
        .data
        .into_iter()
        .filter_map(|chat| {
            let guid = chat.guid?;
            let participants = chat
                .participants
                .unwrap_or_default()
                .into_iter()
                .filter_map(|participant| participant.address)
                .collect::<Vec<_>>();
            let display_name = chat
                .display_name
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| {
                    if participants.is_empty() {
                        "Unknown".to_string()
                    } else {
                        participants.join(", ")
                    }
                });
            let is_group = chat.group_id.is_some() || participants.len() > 1;
            let last_message = truncate_utf8(
                &chat
                    .last_message
                    .and_then(|message| message.text)
                    .unwrap_or_default(),
                80,
            );
            Some(ImessageChat {
                guid,
                display_name,
                participants,
                is_group,
                last_message,
            })
        })
        .collect())
}

pub async fn whatsapp_chats(
    state: &AppState,
    query: &str,
) -> Result<SidecarWhatsappBridgeChatsResponse> {
    SidecarUpdaterClient::from_config(&state.config)?
        .whatsapp_bridge_chats(query)
        .await
}

pub async fn whatsapp_status(state: &AppState) -> Result<SidecarWhatsappBridgeStatusResponse> {
    SidecarUpdaterClient::from_config(&state.config)?
        .whatsapp_bridge_status()
        .await
}

pub async fn ensure_whatsapp_bridge_for_active_pipe(state: &AppState) {
    let row = match sqlx::query(
        "SELECT wc.id AS config_id, wc.pipe_id, p.inbound_token_encrypted, \
                p.outbound_auth_encrypted \
         FROM whatsapp_configs wc \
         JOIN pipes p ON p.id = wc.pipe_id \
         WHERE wc.active = 1 AND p.active = 1 \
         ORDER BY wc.created_at DESC LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await
    {
        Ok(row) => row,
        Err(error) => {
            warn!(error = %error, "Failed to inspect active WhatsApp connector");
            return;
        }
    };
    let Some(row) = row else {
        if let Err(error) = stop_bridge_if_no_active_connector(state).await {
            warn!(error = %error, "Failed to clean up inactive WhatsApp bridge");
        }
        return;
    };
    let config_id: String = row.try_get("config_id").unwrap_or_default();
    let pipe_id: String = row.try_get("pipe_id").unwrap_or_default();
    let inbound_blob: String = row.try_get("inbound_token_encrypted").unwrap_or_default();
    let outbound_blob: Option<String> = row.try_get("outbound_auth_encrypted").ok().flatten();
    let inbound_token = match decrypt_required(&state.credentials, &inbound_blob) {
        Ok(value) => value,
        Err(error) => {
            warn!(config_id, error = %error, "Failed to decrypt WhatsApp inbound token");
            return;
        }
    };
    let outbound_auth = match decrypt_optional(&state.credentials, outbound_blob.as_deref()) {
        Ok(value) => value.unwrap_or_default(),
        Err(error) => {
            warn!(config_id, error = %error, "Failed to decrypt WhatsApp callback authorization");
            return;
        }
    };
    let client = match SidecarUpdaterClient::from_config(&state.config) {
        Ok(client) => client,
        Err(error) => {
            warn!(config_id, error = %error, "Failed to create WhatsApp updater client");
            return;
        }
    };
    let _maintenance_lease = state.docker_storage.shared_lease().await;
    match client
        .ensure_whatsapp_bridge(&SidecarEnsureWhatsappBridgeRequest {
            request_id: Uuid::new_v4().to_string(),
            channel: crate::services::system_updates::release_channel(state).await,
            inbound_url: managed_bridge_inbound_url(state, &pipe_id),
            inbound_token,
            outbound_auth,
        })
        .await
    {
        Ok(response) if response.accepted => {
            info!(config_id, "Ensured WhatsApp bridge for active connector");
        }
        Ok(response) => {
            warn!(config_id, message = ?response.message, "Updater rejected WhatsApp bridge startup")
        }
        Err(error) => warn!(config_id, error = %error, "Failed to ensure WhatsApp bridge"),
    }
}

pub async fn stop_bridge_if_no_active_connector(state: &AppState) -> Result<()> {
    if active_config_exists(&state.db, "whatsapp_configs")
        .await
        .map_err(|error| anyhow!(error))?
    {
        return Ok(());
    }
    let client = SidecarUpdaterClient::from_config(&state.config)?;
    let channel = crate::services::system_updates::release_channel(state).await;
    stop_whatsapp_bridge(state, &client, &channel).await
}

async fn stop_whatsapp_bridge(
    state: &AppState,
    client: &SidecarUpdaterClient,
    channel: &str,
) -> Result<()> {
    let _maintenance_lease = state.docker_storage.shared_lease().await;
    let response = client
        .stop_whatsapp_bridge(&SidecarStopWhatsappBridgeRequest {
            request_id: Uuid::new_v4().to_string(),
            channel: channel.to_string(),
        })
        .await
        .context("stop WhatsApp bridge")?;
    if response.accepted {
        Ok(())
    } else {
        Err(anyhow!(
            "{}",
            response
                .message
                .unwrap_or_else(|| "updates service rejected WhatsApp bridge cleanup".to_string())
        ))
    }
}

pub async fn backfill_connector_secrets(
    db: &SqlitePool,
    credentials: &CredentialStore,
) -> Result<usize> {
    let mut tx = db
        .begin()
        .await
        .context("begin connector secret backfill")?;
    let mut migrated = 0_usize;

    let pipes = sqlx::query(
        "SELECT id, inbound_token, inbound_token_encrypted, outbound_auth, \
                outbound_auth_encrypted FROM pipes",
    )
    .fetch_all(&mut *tx)
    .await
    .context("load legacy pipe secrets")?;
    for row in pipes {
        let id: String = row.try_get("id")?;
        let inbound: String = row.try_get("inbound_token")?;
        let inbound_blob: Option<String> = row.try_get("inbound_token_encrypted")?;
        let outbound: Option<String> = row.try_get("outbound_auth")?;
        let outbound_blob: Option<String> = row.try_get("outbound_auth_encrypted")?;
        let new_inbound = if inbound_blob
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        {
            inbound_blob
        } else if inbound.is_empty() {
            None
        } else {
            migrated += 1;
            Some(credentials.encrypt_raw(inbound.as_bytes())?)
        };
        let new_outbound = if outbound_blob
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        {
            outbound_blob
        } else if let Some(value) = outbound.as_deref().filter(|value| !value.is_empty()) {
            migrated += 1;
            Some(credentials.encrypt_raw(value.as_bytes())?)
        } else {
            None
        };
        if !inbound.is_empty() || outbound.is_some() {
            sqlx::query(
                "UPDATE pipes SET inbound_token = '', inbound_token_encrypted = ?, \
                 outbound_auth = NULL, outbound_auth_encrypted = ? WHERE id = ?",
            )
            .bind(new_inbound)
            .bind(new_outbound)
            .bind(id)
            .execute(&mut *tx)
            .await
            .context("backfill pipe secrets")?;
        }
    }

    let telegram = sqlx::query("SELECT id, bot_token, bot_token_encrypted FROM telegram_configs")
        .fetch_all(&mut *tx)
        .await
        .context("load legacy Telegram secrets")?;
    for row in telegram {
        let id: String = row.try_get("id")?;
        let value: String = row.try_get("bot_token")?;
        let encrypted: Option<String> = row.try_get("bot_token_encrypted")?;
        if !value.is_empty() {
            let encrypted = if encrypted.as_deref().is_some_and(|blob| !blob.is_empty()) {
                encrypted
            } else {
                migrated += 1;
                Some(credentials.encrypt_raw(value.as_bytes())?)
            };
            sqlx::query(
                "UPDATE telegram_configs SET bot_token = '', bot_token_encrypted = ? WHERE id = ?",
            )
            .bind(encrypted)
            .bind(id)
            .execute(&mut *tx)
            .await
            .context("backfill Telegram token")?;
        }
    }

    let bluebubbles = sqlx::query(
        "SELECT id, server_password, server_password_encrypted FROM bluebubbles_configs",
    )
    .fetch_all(&mut *tx)
    .await
    .context("load legacy BlueBubbles secrets")?;
    for row in bluebubbles {
        let id: String = row.try_get("id")?;
        let value: String = row.try_get("server_password")?;
        let encrypted: Option<String> = row.try_get("server_password_encrypted")?;
        if !value.is_empty() {
            let encrypted = if encrypted.as_deref().is_some_and(|blob| !blob.is_empty()) {
                encrypted
            } else {
                migrated += 1;
                Some(credentials.encrypt_raw(value.as_bytes())?)
            };
            sqlx::query(
                "UPDATE bluebubbles_configs SET server_password = '', \
                 server_password_encrypted = ? WHERE id = ?",
            )
            .bind(encrypted)
            .bind(id)
            .execute(&mut *tx)
            .await
            .context("backfill BlueBubbles password")?;
        }
    }

    tx.commit()
        .await
        .context("commit connector secret backfill")?;
    Ok(migrated)
}

pub fn decrypt_required(credentials: &CredentialStore, encrypted: &str) -> Result<String> {
    if encrypted.trim().is_empty() {
        return Err(anyhow!("encrypted connector secret is missing"));
    }
    String::from_utf8(credentials.decrypt_raw(encrypted)?)
        .context("decrypted connector secret is not UTF-8")
}

pub fn decrypt_optional(
    credentials: &CredentialStore,
    encrypted: Option<&str>,
) -> Result<Option<String>> {
    encrypted
        .filter(|value| !value.trim().is_empty())
        .map(|value| decrypt_required(credentials, value))
        .transpose()
}

pub fn truncate_utf8(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn encrypt(credentials: &CredentialStore, value: &str) -> Result<String, ConnectorError> {
    credentials
        .encrypt_raw(value.as_bytes())
        .context("encrypt connector secret")
        .map_err(ConnectorError::internal)
}

async fn insert_people(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    pipe_id: &str,
    people: &[PersonInput],
) -> Result<(), ConnectorError> {
    let mut seen = HashSet::new();
    for person in people {
        let user_id = person.user_id.trim();
        let sender_ref = person.sender_ref.trim();
        if user_id.is_empty() || sender_ref.is_empty() || !seen.insert(sender_ref.to_string()) {
            continue;
        }
        sqlx::query(
            "INSERT INTO user_sender_refs (id, user_id, pipe_id, sender_ref) VALUES (?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(user_id)
        .bind(pipe_id)
        .bind(sender_ref)
        .execute(&mut **tx)
        .await
        .context("create connector sender mapping")
        .map_err(ConnectorError::internal)?;
    }
    Ok(())
}

async fn validate_people(db: &SqlitePool, people: &[PersonInput]) -> Result<(), ConnectorError> {
    for user_id in people
        .iter()
        .map(|person| person.user_id.trim())
        .filter(|user_id| !user_id.is_empty())
    {
        ensure_user(db, user_id).await?;
    }
    Ok(())
}

async fn resolve_owner(db: &SqlitePool, people: &[PersonInput]) -> Result<String, ConnectorError> {
    if let Some(user_id) = people
        .iter()
        .map(|person| person.user_id.trim())
        .find(|user_id| !user_id.is_empty())
    {
        ensure_user(db, user_id).await?;
        return Ok(user_id.to_string());
    }
    sqlx::query_scalar::<_, String>("SELECT id FROM users ORDER BY created_at LIMIT 1")
        .fetch_optional(db)
        .await
        .context("load connector owner")
        .map_err(ConnectorError::internal)?
        .ok_or(ConnectorError::Validation("connector_owner_not_found"))
}

async fn ensure_user(db: &SqlitePool, user_id: &str) -> Result<(), ConnectorError> {
    let exists = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_one(db)
        .await
        .context("check connector owner")
        .map_err(ConnectorError::internal)?;
    if exists == 0 {
        Err(ConnectorError::Validation("connector_owner_not_found"))
    } else {
        Ok(())
    }
}

async fn ensure_active_pipe(db: &SqlitePool, pipe_id: &str) -> Result<(), ConnectorError> {
    let exists =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pipes WHERE id = ? AND active = 1")
            .bind(pipe_id)
            .fetch_one(db)
            .await
            .context("check connector pipe")
            .map_err(ConnectorError::internal)?;
    if exists == 0 {
        Err(ConnectorError::NotFound)
    } else {
        Ok(())
    }
}

async fn active_config_exists(db: &SqlitePool, table: &str) -> Result<bool, ConnectorError> {
    let sql = match table {
        "bluebubbles_configs" => {
            "SELECT COUNT(*) FROM bluebubbles_configs c JOIN pipes p ON p.id = c.pipe_id \
             WHERE c.active = 1 AND p.active = 1"
        }
        "telegram_configs" => {
            "SELECT COUNT(*) FROM telegram_configs c JOIN pipes p ON p.id = c.pipe_id \
             WHERE c.active = 1 AND p.active = 1"
        }
        "whatsapp_configs" => {
            "SELECT COUNT(*) FROM whatsapp_configs c JOIN pipes p ON p.id = c.pipe_id \
             WHERE c.active = 1 AND p.active = 1"
        }
        _ => return Err(ConnectorError::internal(anyhow!("unknown connector table"))),
    };
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(db)
        .await
        .context("check active connector")
        .map(|count| count > 0)
        .map_err(ConnectorError::internal)
}

async fn rollback_created_connector(state: &AppState, pipe_id: &str, config: Option<(&str, &str)>) {
    let result = async {
        let mut tx = state.db.begin().await?;
        sqlx::query("DELETE FROM user_sender_refs WHERE pipe_id = ?")
            .bind(pipe_id)
            .execute(&mut *tx)
            .await?;
        if let Some((table, config_id)) = config {
            let sql = match table {
                "bluebubbles_configs" => "DELETE FROM bluebubbles_configs WHERE id = ?",
                "telegram_configs" => "DELETE FROM telegram_configs WHERE id = ?",
                _ => return Err(anyhow!("unknown rollback table")),
            };
            sqlx::query(sql).bind(config_id).execute(&mut *tx).await?;
        }
        sqlx::query("DELETE FROM pipes WHERE id = ?")
            .bind(pipe_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        warn!(pipe_id, error = %error, "Failed to roll back connector setup");
    }
}

fn managed_bridge_outbound_url(state: &AppState) -> String {
    let updater = state.config.updater_url.to_ascii_lowercase();
    if updater.contains("localhost") || updater.contains("127.0.0.1") {
        DEFAULT_BRIDGE_OUTBOUND_URL_LOCAL.to_string()
    } else {
        DEFAULT_BRIDGE_OUTBOUND_URL_DOCKER.to_string()
    }
}

fn managed_bridge_inbound_url(state: &AppState, pipe_id: &str) -> String {
    format!(
        "{}/api/pipes/{pipe_id}/inbound",
        managed_bridge_inbound_origin(state)
    )
}

fn managed_bridge_inbound_origin(state: &AppState) -> String {
    let external = state.public_base_url();
    let lower = external.to_ascii_lowercase();
    if !external.is_empty() && !lower.contains("localhost") && !lower.contains("127.0.0.1") {
        return external.trim_end_matches('/').to_string();
    }
    let updater = state.config.updater_url.to_ascii_lowercase();
    let origin = if updater.contains("localhost") || updater.contains("127.0.0.1") {
        format!(
            "{}:{}",
            DEFAULT_SELU_INBOUND_ORIGIN_LOCAL, state.config.server.port
        )
    } else {
        format!(
            "{}:{}",
            DEFAULT_SELU_INBOUND_ORIGIN_DOCKER, state.config.server.port
        )
    };
    if state.config.base_path().is_empty() {
        origin
    } else {
        format!("{}{}", origin, state.config.base_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    fn credentials(db: SqlitePool) -> CredentialStore {
        CredentialStore::new(db, [7_u8; 32])
    }

    #[test]
    fn truncate_utf8_never_slices_inside_a_character() {
        assert_eq!(truncate_utf8("abcd", 3), "abc…");
        assert_eq!(truncate_utf8("Grüße 👋", 7), "Grüße 👋");
        assert_eq!(truncate_utf8("Grüße 👋!", 7), "Grüße 👋…");
    }

    #[tokio::test]
    async fn legacy_connector_secrets_are_encrypted_and_plaintext_is_cleared() {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        sqlx::query("INSERT INTO users (id, username, display_name, password_hash) VALUES ('u', 'u', 'U', 'x')")
            .execute(&db).await.unwrap();
        sqlx::query("INSERT INTO pipes (id, user_id, name, transport, inbound_token, outbound_url, outbound_auth) VALUES ('p', 'u', 'P', 'webhook', 'incoming', 'https://example.test', 'Bearer old')")
            .execute(&db).await.unwrap();
        sqlx::query("INSERT INTO telegram_configs (id, name, bot_token, chat_id, pipe_id) VALUES ('t', 'T', 'telegram-secret', '1', 'p')")
            .execute(&db).await.unwrap();
        sqlx::query("INSERT INTO bluebubbles_configs (id, name, server_url, server_password, chat_guid, pipe_id) VALUES ('b', 'B', 'http://bb.test', 'bb-secret', 'chat', 'p')")
            .execute(&db).await.unwrap();
        let store = credentials(db.clone());

        let migrated = backfill_connector_secrets(&db, &store).await.unwrap();
        assert_eq!(migrated, 4);

        let pipe = sqlx::query("SELECT inbound_token, inbound_token_encrypted, outbound_auth, outbound_auth_encrypted FROM pipes WHERE id = 'p'")
            .fetch_one(&db).await.unwrap();
        assert_eq!(pipe.get::<String, _>("inbound_token"), "");
        assert!(pipe.get::<Option<String>, _>("outbound_auth").is_none());
        assert_eq!(
            decrypt_required(&store, &pipe.get::<String, _>("inbound_token_encrypted")).unwrap(),
            "incoming"
        );
        assert_eq!(
            decrypt_required(&store, &pipe.get::<String, _>("outbound_auth_encrypted")).unwrap(),
            "Bearer old"
        );

        let telegram: (String, String) = sqlx::query_as(
            "SELECT bot_token, bot_token_encrypted FROM telegram_configs WHERE id = 't'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(telegram.0, "");
        assert_eq!(
            decrypt_required(&store, &telegram.1).unwrap(),
            "telegram-secret"
        );

        let bluebubbles: (String, String) = sqlx::query_as("SELECT server_password, server_password_encrypted FROM bluebubbles_configs WHERE id = 'b'")
            .fetch_one(&db).await.unwrap();
        assert_eq!(bluebubbles.0, "");
        assert_eq!(
            decrypt_required(&store, &bluebubbles.1).unwrap(),
            "bb-secret"
        );
    }

    #[tokio::test]
    async fn failed_sender_mapping_rolls_back_setup_transaction() {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, display_name, password_hash) \
             VALUES ('u', 'u', 'U', 'x')",
        )
        .execute(&db)
        .await
        .unwrap();

        let mut transaction = db.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO pipes \
             (id, user_id, name, transport, inbound_token, outbound_url) \
             VALUES ('p', 'u', 'P', 'webhook', '', 'internal://telegram')",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
        let people = vec![
            PersonInput {
                user_id: "u".to_string(),
                sender_ref: "sender-ok".to_string(),
            },
            PersonInput {
                user_id: "missing".to_string(),
                sender_ref: "sender-invalid".to_string(),
            },
        ];
        assert!(insert_people(&mut transaction, "p", &people).await.is_err());
        transaction.rollback().await.unwrap();

        let pipe_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pipes WHERE id = 'p'")
            .fetch_one(&db)
            .await
            .unwrap();
        let sender_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM user_sender_refs WHERE pipe_id = 'p'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(pipe_count, 0);
        assert_eq!(sender_count, 0);
    }
}
