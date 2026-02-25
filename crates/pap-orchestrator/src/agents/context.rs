use anyhow::Result;
use sqlx::SqlitePool;
use std::collections::HashMap;
use tracing::debug;

use crate::agents::delegation;
use crate::agents::loader::AgentDefinition;
use crate::agents::memory::MemoryStore;
use crate::llm::provider::ChatMessage;

/// Builds the LLM context window for a turn:
///   [system prompt + capability prompts + agent registry]
///   [semantically relevant messages from memory (if enabled)]
///   [recent N messages from this thread (or session/pipe if no thread)]
pub async fn build(
    db: &SqlitePool,
    agent: &AgentDefinition,
    pipe_id: &str,
    session_id: &str,
    thread_id: Option<&str>,
    memory_store: Option<&MemoryStore>,
    latest_message: &str,
    all_agents: Option<&HashMap<String, AgentDefinition>>,
) -> Result<Vec<ChatMessage>> {
    let mut messages = Vec::new();

    // ── System prompt ─────────────────────────────────────────────────────────
    let mut system = agent.system_prompt.clone();

    // Append each capability's prompt.md (if non-empty) so the LLM knows
    // what tools are available and how to use them.
    for manifest in agent.capability_manifests.values() {
        if !manifest.prompt.is_empty() {
            system.push_str("\n\n## Capability: ");
            system.push_str(&manifest.id);
            system.push('\n');
            system.push_str(&manifest.prompt);
        }
    }

    // Append agent registry so the orchestrator knows which specialists exist
    // and can use the delegate_to_agent tool effectively.
    if let Some(agents) = all_agents {
        let has_delegates = agents.keys().any(|id| id != &agent.id);
        if has_delegates {
            system.push_str("\n\n");
            system.push_str(&delegation::agent_registry_prompt(&agent.id, agents));
        }
    }

    messages.push(ChatMessage::system(system));

    // ── Semantic memory retrieval ─────────────────────────────────────────────
    // If the agent has memory policy "retrieval" and a memory store is available,
    // retrieve relevant past messages and inject them as context.
    // Memory is shared across all threads in the session (threads share context).
    if agent.memory.policy == "retrieval" {
        if let Some(store) = memory_store {
            match store.retrieve(latest_message, session_id, agent.memory.top_k).await {
                Ok(relevant) if !relevant.is_empty() => {
                    debug!(
                        count = relevant.len(),
                        top_k = agent.memory.top_k,
                        "Retrieved relevant messages from memory"
                    );
                    let mut memory_context = String::from(
                        "The following are relevant messages from earlier in this conversation:\n\n"
                    );
                    for (_, content, score) in &relevant {
                        memory_context.push_str(&format!("- [relevance: {:.2}] {}\n", score, content));
                    }
                    messages.push(ChatMessage::system(memory_context));
                }
                Ok(_) => {} // No relevant messages found
                Err(e) => {
                    debug!("Memory retrieval failed (non-fatal): {e}");
                }
            }
        }
    }

    // ── Conversation history ──────────────────────────────────────────────────
    // If a thread_id is provided, pull messages for that specific thread.
    // This gives each thread its own isolated conversation history while
    // sharing semantic memory across the session.
    // If no thread_id (web chat, delegation), fall back to pipe+session history.
    if let Some(tid) = thread_id {
        let history = sqlx::query!(
            r#"SELECT role, content FROM messages
               WHERE thread_id = ?
               ORDER BY created_at DESC
               LIMIT 30"#,
            tid,
        )
        .fetch_all(db)
        .await?;

        for row in history.into_iter().rev() {
            match row.role.as_str() {
                "user"      => messages.push(ChatMessage::user(&row.content)),
                "assistant" => messages.push(ChatMessage::assistant(&row.content)),
                _ => {}
            }
        }
    } else {
        let history = sqlx::query!(
            r#"SELECT role, content FROM messages
               WHERE pipe_id = ? AND (session_id = ? OR session_id IS NULL)
                 AND thread_id IS NULL
               ORDER BY created_at DESC
               LIMIT 30"#,
            pipe_id,
            session_id,
        )
        .fetch_all(db)
        .await?;

        for row in history.into_iter().rev() {
            match row.role.as_str() {
                "user"      => messages.push(ChatMessage::user(&row.content)),
                "assistant" => messages.push(ChatMessage::assistant(&row.content)),
                _ => {}
            }
        }
    };

    Ok(messages)
}
