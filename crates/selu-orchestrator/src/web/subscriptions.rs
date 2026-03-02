use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;
use tracing::error;
use uuid::Uuid;

use crate::state::AppState;
use crate::web::auth::AuthUser;
use crate::web::prefixed_redirect;

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
    is_admin: bool,
    base_path: String,
    subs: Vec<SubView>,
    users: Vec<UserOption>,
    error: Option<String>,
    success: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SubsQuery {
    pub error: Option<String>,
    pub success: Option<String>,
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

pub async fn subscriptions_index(user: AuthUser, Query(q): Query<SubsQuery>, State(state): State<AppState>) -> Response {
    if !user.is_admin {
        return prefixed_redirect(&state, "/chat").into_response();
    }
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

    match (SubscriptionsTemplate { active_nav: "subscriptions", is_admin: user.is_admin, base_path: state.base_path.clone(), subs, users, error: q.error, success: q.success }).render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn subscriptions_create(
    user: AuthUser,
    State(state): State<AppState>,
    Form(form): Form<CreateSubForm>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    if form.user_id.is_empty()
        || form.event_type.trim().is_empty()
        || form.reaction_type.is_empty()
    {
        return Redirect::to(&format!("{}/subscriptions?error=User%2C+event+type%2C+and+reaction+type+are+required.", state.base_path)).into_response();
    }

    if form.reaction_type != "pipe_notification" && form.reaction_type != "agent_invocation" {
        return Redirect::to(&format!("{}/subscriptions?error=Invalid+reaction+type.", state.base_path)).into_response();
    }

    // Validate and re-serialize reaction_config JSON
    let config_val: serde_json::Value = match serde_json::from_str(&form.reaction_config) {
        Ok(v) => v,
        Err(_) => return Redirect::to(&format!("{}/subscriptions?error=Reaction+config+must+be+valid+JSON.", state.base_path)).into_response(),
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
            // Try to translate NL description to CEL using the default LLM provider
            match crate::agents::model::resolve_model(&state.db, "default").await {
                Ok(resolved) => {
                    match crate::llm::registry::load_provider(
                        &state.db, &resolved.provider_id, &resolved.model_id, &state.credentials,
                    ).await {
                        Ok(provider) => {
                            let msgs = vec![
                                crate::llm::provider::ChatMessage::system(
                                    "You are a CEL (Common Expression Language) expert. Translate the user's filter description into a valid CEL expression. \
                                     Available variables: event_type (string), event (map). Return ONLY the CEL expression, nothing else."
                                ),
                                crate::llm::provider::ChatMessage::user(
                                    format!("Translate this to a CEL expression: {}", desc)
                                ),
                            ];
                            match provider.chat(&msgs, &[], 0.0).await {
                                Ok(crate::llm::provider::LlmResponse::Text(cel)) => {
                                    let cel = cel.trim().to_string();
                                    if !cel.is_empty() { Some(cel) } else { None }
                                }
                                _ => None,
                            }
                        }
                        Err(e) => {
                            tracing::warn!("NL-to-CEL: failed to load provider: {e}");
                            None
                        }
                    }
                }
                Err(_) => None,
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
        Ok(_) => Redirect::to(&format!("{}/subscriptions?success=Subscription+created.", state.base_path)).into_response(),
        Err(e) => {
            error!("Failed to create subscription: {e}");
            Redirect::to(&format!("{}/subscriptions?error=Failed+to+create+subscription.+Please+try+again.", state.base_path)).into_response()
        }
    }
}

pub async fn subscriptions_delete(
    user: AuthUser,
    Path(sub_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if !user.is_admin {
        return StatusCode::FORBIDDEN.into_response();
    }
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
