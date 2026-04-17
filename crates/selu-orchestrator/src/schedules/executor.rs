/// Schedule executor: background task that fires due schedules.
///
/// Runs every 30 seconds, finds schedules where `next_run_at <= now()`,
/// creates a new thread for each target pipe, and runs the schedule's
/// prompt through the agent engine.  The agent's reply is then delivered
/// to the pipe via the channel registry (iMessage, webhooks, etc.).
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::agents::engine::{ChannelKind, TurnParams, noop_sender, run_turn};
use crate::agents::thread as thread_mgr;
use crate::channels::ChannelMessage;
use crate::state::AppState;

/// Check for and execute all due schedules.
///
/// Called by the background interval task in main.rs.
/// Also cleans up fired one-shot reminders older than 7 days.
pub async fn tick(state: &AppState) {
    // Clean up stale fired reminders (cheap no-op most of the time)
    super::cleanup_fired_reminders(&state.db).await;

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
            schedule.one_shot,
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
            let schedule_agent_id = schedule.agent_id.clone();
            let prompt = schedule.prompt.clone();
            let one_shot = schedule.one_shot;

            tokio::spawn(async move {
                execute_on_pipe(
                    &state,
                    &pipe_id,
                    &user_id,
                    schedule_agent_id.as_deref(),
                    &prompt,
                    one_shot,
                )
                .await;
            });
        }
    }
}

/// Execute a schedule's prompt on a single pipe.
///
/// Creates a new thread, persists a status note (not a user bubble),
/// runs the prompt through the standard agent engine, and delivers
/// the reply via the channel registry (iMessage, webhook, etc.).
async fn execute_on_pipe(
    state: &AppState,
    pipe_id: &str,
    user_id: &str,
    schedule_agent_id: Option<&str>,
    prompt: &str,
    one_shot: bool,
) {
    // Look up pipe's default agent
    let pipe_default_agent_id =
        sqlx::query_scalar!("SELECT default_agent_id FROM pipes WHERE id = ?", pipe_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .flatten();

    let agent_id = resolve_target_agent_id(schedule_agent_id, pipe_default_agent_id);
    let force_new_session = state
        .agents
        .load()
        .get(&agent_id)
        .map(|a| a.session.requires_thread_isolation())
        .unwrap_or(false);

    // Create a new thread for this schedule run
    let thread = match thread_mgr::create_thread(
        &state.db,
        pipe_id,
        user_id,
        &agent_id,
        force_new_session,
        None,
    )
    .await
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
        let key = if one_shot {
            "schedule.reminder_label"
        } else {
            "schedule.run_label"
        };
        crate::i18n::t(&lang, key).to_string()
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

    // Determine the channel kind: if a sender is registered for this pipe
    // (e.g. BlueBubbles/iMessage), use ThreadedNonInteractive so that
    // "Ask" tool policies trigger async approval prompts. Otherwise fall
    // back to NonInteractive (Ask → Block).
    let has_sender = state.channel_registry.has_sender(pipe_id).await;
    let channel_kind = if has_sender {
        ChannelKind::ThreadedNonInteractive {
            pipe_id: pipe_id.to_string(),
            thread_id: thread_id.clone(),
        }
    } else {
        ChannelKind::NonInteractive
    };

    // Determine the user's preferred language so we can instruct the agent
    // to respond in that language even though the prompt may have been stored
    // in a different language (e.g. English).
    let user_lang = crate::i18n::user_language(&state.db, user_id).await;
    let lang_instruction = match user_lang.as_str() {
        "de" => "Antworte auf Deutsch. ",
        "en" => "",
        _ => "Antworte auf Deutsch. ",
    };
    // Tell the agent that the output will be delivered to the pipe automatically.
    // Without this, words like "schicke" in schedule prompts cause the agent to
    // delegate to email instead of simply replying.
    let delivery_hint = match user_lang.as_str() {
        "de" => "Deine Antwort wird automatisch in diesem Kanal zugestellt. \
                 Antworte direkt mit dem Text — sende KEINE E-Mail, \
                 es sei denn, der Prompt verlangt es ausdrücklich. ",
        _ => "Your reply will be delivered to this channel automatically. \
              Reply with the text directly — do NOT send email \
              unless the prompt explicitly asks for it. ",
    };
    let full_prompt = format!("{}{}{}", lang_instruction, delivery_hint, prompt);

    let params = TurnParams {
        pipe_id: pipe_id.to_string(),
        user_id: user_id.to_string(),
        agent_id: Some(agent_id),
        message: full_prompt,
        thread_id: Some(thread_id.clone()),
        chain_depth: 0,
        channel_kind,
        skip_user_persist: false,
        enable_streaming: true,
        inbound_attachments: Vec::new(),
        delegation_trace: Vec::new(),
        location_context: None,
    };

    let output = match run_turn(state, params, noop_sender()).await {
        Ok(t) if !t.reply_text.is_empty() || !t.attachments.is_empty() => t,
        Ok(_) => {
            warn!(pipe_id = %pipe_id, "Schedule agent turn returned empty reply");
            return;
        }
        Err(e) => {
            error!("Schedule execution failed on pipe {}: {}", pipe_id, e);
            let _ = thread_mgr::fail_thread(&state.db, &thread_id).await;
            return;
        }
    };

    debug!(
        pipe_id = %pipe_id,
        reply_len = output.reply_text.len(),
        "Schedule executed, delivering reply"
    );

    let attachments = crate::agents::artifacts::to_outbound_attachments(
        &state.artifacts,
        &output.attachments,
        user_id,
        &state.public_base_url(),
        &state.config.encryption_key,
        true,
    )
    .await;
    let outbound_msg = ChannelMessage {
        text: output.reply_text,
        attachments,
    };

    // Deliver the reply via the channel registry.
    // Bot-initiated messages don't reply to any prior external message,
    // so reply_to_guid is None — this starts a new conversation thread
    // in the external channel (e.g. a new iMessage).
    match state
        .channel_registry
        .send(pipe_id, &thread_id, &outbound_msg, None)
        .await
    {
        Ok(sent_guid) => {
            info!(pipe_id = %pipe_id, "Schedule reply delivered");
            // Store the outbound GUID so future user replies are matched
            // back to this thread.
            if let Some(ref guid) = sent_guid {
                let _ = thread_mgr::update_reply_guid(&state.db, &thread_id, pipe_id, guid).await;
            }
        }
        Err(e) => {
            // No sender registered (e.g. web-only pipe) — the reply is
            // already persisted in the messages table by run_turn, so
            // it will be visible in the web UI.  This is not an error.
            debug!(
                pipe_id = %pipe_id,
                error = %e,
                "No channel sender for pipe, reply visible in web UI only"
            );
        }
    }

    // Notify the schedule owner's mobile device(s) via push (if enabled).
    if crate::web::system_updates::push_notifications_enabled(&state).await {
        let instance_id = match crate::persistence::db::get_instance_id(&state.db).await {
            Ok(id) => id,
            Err(e) => {
                warn!(error = %e, "Failed to load instance_id for schedule push");
                return;
            }
        };

        let tokens: Vec<String> = sqlx::query_scalar::<_, String>(
            "SELECT device_token FROM mobile_device_tokens WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

        if tokens.is_empty() {
            debug!(user_id = %user_id, "No device tokens for schedule push");
            return;
        }

        let push_body = if user_lang.starts_with("de") {
            "Zeitplan abgeschlossen"
        } else {
            "Schedule completed"
        };
        let client = reqwest::Client::new();
        for token in &tokens {
            let payload = serde_json::json!({
                "instance_id": instance_id,
                "device_token": token,
                "pipe_id": pipe_id,
                "thread_id": thread_id,
                "title": "selu",
                "body": push_body,
            });
            match client
                .post("https://selu.bot/api/relay/push-device")
                .header("X-Instance-Id", &instance_id)
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) => info!(status = %resp.status(), "Schedule push sent to device"),
                Err(e) => warn!(error = %e, "Schedule push to device failed"),
            }
        }
    }
}

fn resolve_target_agent_id(
    schedule_agent_id: Option<&str>,
    pipe_default_agent_id: Option<String>,
) -> String {
    schedule_agent_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
        .or(pipe_default_agent_id)
        .unwrap_or_else(|| "default".to_string())
}

#[cfg(test)]
mod tests {
    use super::resolve_target_agent_id;

    #[test]
    fn pinned_schedule_agent_wins() {
        let resolved = resolve_target_agent_id(Some("polymarket"), Some("default".to_string()));
        assert_eq!(resolved, "polymarket");
    }

    #[test]
    fn falls_back_to_pipe_default_for_legacy_schedule() {
        let resolved = resolve_target_agent_id(None, Some("weather".to_string()));
        assert_eq!(resolved, "weather");
    }

    #[test]
    fn falls_back_to_default_agent_when_none_available() {
        let resolved = resolve_target_agent_id(None, None);
        assert_eq!(resolved, "default");
    }
}
