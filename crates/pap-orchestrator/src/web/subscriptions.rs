use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;
use tracing::error;
use uuid::Uuid;

use crate::state::AppState;
use crate::web::auth::AuthUser;

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SubView {
    pub id: String,
    pub user_id: String,
    pub username: String,
    pub event_type: String,
    pub reaction_type: String,
    pub reaction_config: String,
    pub filter_expression: String,
    pub active: bool,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct UserOption {
    pub id: String,
    pub display: String,
}

// ── Template ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "subscriptions.html")]
struct SubscriptionsTemplate {
    active_nav: &'static str,
    subs: Vec<SubView>,
    users: Vec<UserOption>,
}

// ── Form struct ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateSubForm {
    pub user_id: String,
    pub event_type: String,
    pub reaction_type: String,
    pub reaction_config: String,
    pub filter_expression: Option<String>,
    pub filter_description: Option<String>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn subscriptions_index(_user: AuthUser, State(state): State<AppState>) -> Response {
    let rows = sqlx::query!(
        r#"SELECT es.id, es.user_id, u.username,
                  es.event_type, es.reaction_type, es.reaction_config_json,
                  es.filter_expression, es.active, es.created_at
           FROM event_subscriptions es
           LEFT JOIN users u ON es.user_id = u.id
           ORDER BY es.created_at DESC"#
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let subs: Vec<SubView> = rows
        .into_iter()
        .filter_map(|r| {
            let id = r.id?;
            let user_id = r.user_id;
            let username = r.username.unwrap_or_else(|| user_id.clone());
            Some(SubView {
                id,
                user_id,
                username,
                event_type: r.event_type,
                reaction_type: r.reaction_type,
                reaction_config: r.reaction_config_json,
                filter_expression: r.filter_expression.unwrap_or_default(),
                active: r.active != 0,
                created_at: r.created_at,
            })
        })
        .collect();

    let users = sqlx::query!("SELECT id, username, display_name FROM users ORDER BY username")
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| UserOption {
            id: r.id.unwrap_or_default(),
            display: format!("{} ({})", r.display_name, r.username),
        })
        .collect();

    match (SubscriptionsTemplate { active_nav: "subscriptions", subs, users }).render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn subscriptions_create(
    _user: AuthUser,
    State(state): State<AppState>,
    Form(form): Form<CreateSubForm>,
) -> Response {
    if form.user_id.is_empty()
        || form.event_type.trim().is_empty()
        || form.reaction_type.is_empty()
    {
        return StatusCode::BAD_REQUEST.into_response();
    }

    if form.reaction_type != "pipe_notification" && form.reaction_type != "agent_invocation" {
        return StatusCode::BAD_REQUEST.into_response();
    }

    // Validate and re-serialize reaction_config JSON
    let config_val: serde_json::Value = match serde_json::from_str(&form.reaction_config) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let config_json = config_val.to_string();

    let filter_description = form.filter_description.filter(|v| !v.is_empty());

    // If a natural language description is provided but no CEL expression,
    // attempt NL-to-CEL translation via the LLM.
    let filter_expression = match (
        form.filter_expression.filter(|v| !v.is_empty()),
        &filter_description,
    ) {
        (Some(expr), _) => Some(expr), // User provided an explicit CEL expression
        (None, Some(desc)) => {
            // Try to translate NL description to CEL
            let cfg = &state.config;
            if !cfg.embedding.api_key.is_empty() {
                match crate::events::nl_to_cel::translate(
                    desc,
                    &cfg.embedding.api_key,
                    &cfg.embedding.base_url,
                    "gpt-4o-mini", // Use a fast/cheap model for translation
                ).await {
                    Ok(cel) if !cel.is_empty() => Some(cel),
                    Ok(_) => None,
                    Err(e) => {
                        tracing::warn!("NL-to-CEL translation failed: {e}");
                        None
                    }
                }
            } else {
                None
            }
        }
        (None, None) => None,
    };

    let id = Uuid::new_v4().to_string();
    let result = sqlx::query!(
        r#"INSERT INTO event_subscriptions
           (id, user_id, event_type, reaction_type, reaction_config_json,
            filter_expression, filter_description)
           VALUES (?, ?, ?, ?, ?, ?, ?)"#,
        id,
        form.user_id,
        form.event_type,
        form.reaction_type,
        config_json,
        filter_expression,
        filter_description,
    )
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Redirect::to("/subscriptions").into_response(),
        Err(e) => {
            error!("Failed to create subscription: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn subscriptions_delete(
    _user: AuthUser,
    Path(sub_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if let Err(e) = sqlx::query!(
        "UPDATE event_subscriptions SET active = 0 WHERE id = ?",
        sub_id
    )
    .execute(&state.db)
    .await
    {
        error!("Failed to deactivate subscription {sub_id}: {e}");
    }
    Html("").into_response()
}
