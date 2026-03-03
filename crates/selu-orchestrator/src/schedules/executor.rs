/// Schedule executor: background task that fires due schedules.
///
/// Runs every 30 seconds, finds schedules where `next_run_at <= now()`,
/// creates a new thread for each target pipe, and runs the schedule's
/// prompt through the agent engine.
use tracing::{error, info};
use uuid::Uuid;

use crate::agents::engine::{ChannelKind, TurnParams, noop_sender, run_turn};
use crate::agents::thread as thread_mgr;
use crate::state::AppState;

/// Check for and execute all due schedules.
///
/// Called by the background interval task in main.rs.
pub async fn tick(state: &AppState) {
    let due = match super::fetch_due_schedules(&state.db).await {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to fetch due schedules: {e}");
            return;
        }
    };

    if due.is_empty() {
        return;
    }

    info!("Executing {} due schedule(s)", due.len());

    for schedule in due {
        // Advance the schedule immediately so it won't re-fire if execution is slow
        super::mark_executed(
            &state.db,
            &schedule.id,
            &schedule.timezone,
            &schedule.cron_expression,
        )
        .await;

        if schedule.pipe_ids.is_empty() {
            error!("Schedule {} has no pipes assigned, skipping", schedule.id);
            continue;
        }

        for pipe_id in &schedule.pipe_ids {
            let state = state.clone();
            let pipe_id = pipe_id.clone();
            let user_id = schedule.user_id.clone();
            let prompt = schedule.prompt.clone();

            tokio::spawn(async move {
                execute_on_pipe(&state, &pipe_id, &user_id, &prompt).await;
            });
        }
    }
}

/// Execute a schedule's prompt on a single pipe.
///
/// Creates a new thread, persists a status note (not a user bubble),
/// and runs the prompt through the standard agent engine.
async fn execute_on_pipe(state: &AppState, pipe_id: &str, user_id: &str, prompt: &str) {
    // Look up pipe's default agent
    let default_agent_id =
        sqlx::query_scalar!("SELECT default_agent_id FROM pipes WHERE id = ?", pipe_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .flatten();

    let agent_id = default_agent_id.unwrap_or_else(|| "default".to_string());

    // Create a new thread for this schedule run
    let thread = match thread_mgr::create_thread(&state.db, pipe_id, user_id, &agent_id, None).await
    {
        Ok(t) => t,
        Err(e) => {
            error!(
                "Failed to create thread for schedule on pipe {}: {}",
                pipe_id, e
            );
            return;
        }
    };

    let thread_id = thread.id.to_string();

    // Persist a "status" label so the chat UI shows this is a scheduled run.
    // The actual prompt is still persisted as a normal "user" message by
    // run_turn — the LLM requires the conversation to start with a user message.
    let status_id = Uuid::new_v4().to_string();
    let status_label = {
        let lang = crate::i18n::user_language(&state.db, user_id).await;
        crate::i18n::t(&lang, "schedule.run_label").to_string()
    };
    let _ = sqlx::query!(
        "INSERT INTO messages (id, pipe_id, session_id, thread_id, role, content) VALUES (?, ?, '', ?, 'status', ?)",
        status_id,
        pipe_id,
        thread_id,
        status_label,
    )
    .execute(&state.db)
    .await;

    let params = TurnParams {
        pipe_id: pipe_id.to_string(),
        user_id: user_id.to_string(),
        agent_id: Some(agent_id),
        message: prompt.to_string(),
        thread_id: Some(thread_id),
        chain_depth: 0,
        channel_kind: ChannelKind::NonInteractive,
        skip_user_persist: false,
    };

    match run_turn(state, params, noop_sender()).await {
        Ok(reply) => {
            tracing::debug!(
                pipe_id = pipe_id,
                reply_len = reply.len(),
                "Schedule executed successfully"
            );
        }
        Err(e) => {
            error!("Schedule execution failed on pipe {}: {}", pipe_id, e);
        }
    }
}
