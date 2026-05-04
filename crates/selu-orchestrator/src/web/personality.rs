use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use tracing::error;

use crate::agents::profile;
use crate::state::AppState;
use crate::web::BasePath;
use crate::web::auth::AuthUser;

// ── View structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FactRow {
    pub id: String,
    pub category: String,
    pub fact: String,
    pub source: String,
    pub created_at: String,
}

/// A group of facts under one category heading.
#[derive(Debug, Clone)]
pub struct FactGroup {
    pub category: String,
    pub label_en: &'static str,
    pub facts: Vec<FactRow>,
}

// ── Templates ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "personality.html")]
struct PersonalityTemplate {
    active_nav: &'static str,
    is_admin: bool,
    base_path: String,
    groups: Vec<FactGroup>,
    error: Option<String>,
    success: Option<String>,
}

#[derive(Template)]
#[template(path = "personality_fact_row.html")]
struct FactRowFragment {
    fact: FactRow,
    base_path: String,
}

#[derive(Template)]
#[template(path = "personality_edit_row.html")]
struct EditRowFragment {
    fact: FactRow,
    base_path: String,
}

// ── Query / Form structs ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PersonalityQuery {
    pub error: Option<String>,
    pub success: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddFactForm {
    pub category: String,
    pub fact: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateFactForm {
    pub category: String,
    pub fact: String,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn category_label(cat: &str) -> &'static str {
    match cat {
        "personal" => "Personal",
        "preferences" => "Preferences",
        "location" => "Location",
        "work" => "Work & Skills",
        _ => "Other",
    }
}

fn group_facts(facts: Vec<profile::ProfileFact>) -> Vec<FactGroup> {
    let categories = ["personal", "preferences", "location", "work", "other"];
    let mut groups = Vec::new();

    for cat in &categories {
        let cat_facts: Vec<FactRow> = facts
            .iter()
            .filter(|f| {
                if f.category.is_empty() {
                    *cat == "other"
                } else {
                    f.category == *cat
                }
            })
            .map(|f| FactRow {
                id: f.id.clone(),
                category: if f.category.is_empty() {
                    "other".to_string()
                } else {
                    f.category.clone()
                },
                fact: f.fact.clone(),
                source: f.source.clone(),
                created_at: f.created_at.clone(),
            })
            .collect();

        groups.push(FactGroup {
            category: cat.to_string(),
            label_en: category_label(cat),
            facts: cat_facts,
        });
    }

    groups
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /personality — list all memory entries grouped by category
pub async fn personality_index(
    user: AuthUser,
    Query(q): Query<PersonalityQuery>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    let facts = profile::list_facts(&state.db, &user.user_id, 200)
        .await
        .unwrap_or_default();

    let groups = group_facts(facts);

    match (PersonalityTemplate {
        active_nav: "personality",
        is_admin: user.is_admin,
        base_path,
        groups,
        error: q.error,
        success: q.success,
    })
    .render()
    {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// POST /personality — add a new memory manually
pub async fn personality_add(
    user: AuthUser,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<AddFactForm>,
) -> Response {
    if form.fact.trim().is_empty() {
        return Redirect::to(&format!("{}/personality?error=empty", base_path)).into_response();
    }

    let valid_categories = ["personal", "preferences", "location", "work", "other"];
    let category = if valid_categories.contains(&form.category.as_str()) {
        &form.category
    } else {
        "other"
    };

    match profile::add_fact(
        &state.db,
        &user.user_id,
        form.fact.trim(),
        category,
        "manual",
        "system",
    )
    .await
    {
        Ok(_) => Redirect::to(&format!("{}/personality?success=added", base_path)).into_response(),
        Err(e) => {
            error!("Failed to add profile fact: {e}");
            Redirect::to(&format!("{}/personality?error=add_failed", base_path)).into_response()
        }
    }
}

/// DELETE /personality/{id} — remove a memory (HTMX)
pub async fn personality_delete(
    user: AuthUser,
    Path(memory_id): Path<String>,
    State(state): State<AppState>,
) -> Response {
    if let Err(e) = profile::delete_fact(&state.db, &user.user_id, &memory_id).await {
        error!("Failed to delete profile fact {memory_id}: {e}");
    }
    Html("").into_response()
}

/// GET /personality/{id}/edit — return the edit form fragment (HTMX)
pub async fn personality_edit_form(
    _user: AuthUser,
    Path(memory_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    let row = sqlx::query_as::<_, (String, String, String, String, String)>(
        "SELECT id, category, fact_text, source, created_at FROM user_profile WHERE id = ?",
    )
    .bind(&memory_id)
    .fetch_optional(&state.db)
    .await;

    match row {
        Ok(Some((id, category, fact, source, created_at))) => {
            let row = FactRow {
                id,
                category: if category.is_empty() {
                    "other".to_string()
                } else {
                    category
                },
                fact,
                source,
                created_at,
            };
            match (EditRowFragment {
                fact: row,
                base_path,
            })
            .render()
            {
                Ok(html) => Html(html).into_response(),
                Err(e) => {
                    error!("Template render error: {e}");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// PUT /personality/{id} — update a profile fact (HTMX)
pub async fn personality_update(
    _user: AuthUser,
    Path(memory_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
    Form(form): Form<UpdateFactForm>,
) -> Response {
    let valid_categories = ["personal", "preferences", "location", "work", "other"];
    let category = if valid_categories.contains(&form.category.as_str()) {
        &form.category
    } else {
        "other"
    };

    if let Err(e) = profile::update_fact(&state.db, &memory_id, form.fact.trim(), category).await {
        error!("Failed to update profile fact {memory_id}: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Return the updated row fragment
    let source = sqlx::query_as::<_, (String,)>("SELECT source FROM user_profile WHERE id = ?")
        .bind(&memory_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .map(|(s,)| s)
        .unwrap_or_else(|| "manual".to_string());

    let row = FactRow {
        id: memory_id,
        category: category.to_string(),
        fact: form.fact.trim().to_string(),
        source,
        created_at: String::new(),
    };
    match (FactRowFragment {
        fact: row,
        base_path,
    })
    .render()
    {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// GET /personality/{id}/row — return the read-only row fragment (HTMX, for cancel)
pub async fn personality_row(
    _user: AuthUser,
    Path(memory_id): Path<String>,
    State(state): State<AppState>,
    BasePath(base_path): BasePath,
) -> Response {
    let row = sqlx::query_as::<_, (String, String, String, String, String)>(
        "SELECT id, category, fact_text, source, created_at FROM user_profile WHERE id = ?",
    )
    .bind(&memory_id)
    .fetch_optional(&state.db)
    .await;

    match row {
        Ok(Some((id, category, fact, source, created_at))) => {
            let row = FactRow {
                id,
                category: if category.is_empty() {
                    "other".to_string()
                } else {
                    category
                },
                fact,
                source,
                created_at,
            };
            match (FactRowFragment {
                fact: row,
                base_path,
            })
            .render()
            {
                Ok(html) => Html(html).into_response(),
                Err(e) => {
                    error!("Template render error: {e}");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}
