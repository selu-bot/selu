/// Bundled default agent that is always available regardless of what is installed.
///
/// The default agent's definition is embedded into the binary at compile time
/// using `include_str!()`, so it works even with an empty installed agents
/// directory.
use crate::agents::loader::AgentDefinition;

const DEFAULT_AGENT_YAML: &str = include_str!("../../../../agents/default/agent.yaml");
const DEFAULT_AGENT_MD: &str = include_str!("../../../../agents/default/agent.md");

/// Build the bundled default agent definition.
///
/// This agent cannot be uninstalled. It is always injected into the in-memory
/// agents map at startup, overriding any installed agent with id "default".
pub fn default_agent() -> AgentDefinition {
    let mut agent: AgentDefinition = serde_yaml::from_str(DEFAULT_AGENT_YAML)
        .expect("bundled default agent.yaml is invalid — this is a build error");
    agent.system_prompt = DEFAULT_AGENT_MD.to_string();
    agent
}

/// Ensure the default agent has a row in the `agents` DB table.
///
/// This is called at startup to make sure the DB knows about the bundled
/// default agent (for model assignment queries etc.).
pub async fn ensure_db_row(db: &sqlx::SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO agents (id, display_name, version, is_bundled, setup_complete) \
         VALUES ('default', 'PAP Assistant', '0.1.0', 1, 1) \
         ON CONFLICT(id) DO UPDATE SET is_bundled = 1",
    )
    .execute(db)
    .await?;

    Ok(())
}
