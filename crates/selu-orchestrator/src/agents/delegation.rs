/// Built-in `delegate_to_agent` tool that lets the orchestrator (default agent)
/// hand off a request to a specialised agent and return the result.
use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::sync::Arc;

use crate::agents::loader::AgentDefinition;
use crate::llm::provider::ToolSpec;

/// The tool name used in tool specs and dispatcher matching.
pub const TOOL_NAME: &str = "delegate_to_agent";

/// Build the `ToolSpec` for the delegation tool.
///
/// `available_agents` is used to populate the enum of valid agent IDs so the
/// LLM knows which agents it can delegate to. The current agent's own ID is
/// excluded to prevent self-delegation loops.
pub fn tool_spec(
    current_agent_id: &str,
    available_agents: &HashMap<String, Arc<AgentDefinition>>,
) -> ToolSpec {
    let agent_ids: Vec<&str> = available_agents
        .keys()
        .filter(|id| id.as_str() != current_agent_id)
        .map(String::as_str)
        .collect();

    ToolSpec {
        name: TOOL_NAME.to_string(),
        description: "Delegate a user request to a specialised agent. Use this when the user's \
             request is better handled by one of the available specialist agents. \
             The specialist will process the request and return its response to you."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "enum": agent_ids,
                    "description": "The ID of the specialist agent to delegate to"
                },
                "message": {
                    "type": "string",
                    "description": "The message to send to the specialist agent. \
                                    Pass the user's request as-is or rephrase it \
                                    for clarity if needed."
                }
            },
            "required": ["agent_id", "message"]
        }),
    }
}

/// Build a summary of available agents for injection into the system prompt.
///
/// This gives the orchestrator agent knowledge of what specialists exist and
/// what they can do, so it can make informed delegation decisions.
pub fn agent_registry_prompt(
    current_agent_id: &str,
    available_agents: &HashMap<String, Arc<AgentDefinition>>,
) -> String {
    let mut lines = Vec::new();
    lines.push("## Available specialist agents\n".to_string());
    lines.push(
        "You can delegate requests to these agents using the `delegate_to_agent` tool. \
         Delegate when a user's request clearly falls within a specialist's domain. \
         For general questions, conversation, or anything not covered by a specialist, \
         handle it yourself. Do not tell the user to use @mentions — handle the routing yourself.\n"
            .to_string(),
    );

    let mut agents: Vec<&AgentDefinition> = available_agents
        .values()
        .filter(|a| a.id != current_agent_id)
        .map(|a| a.as_ref())
        .collect();
    agents.sort_by_key(|a| &a.id);

    for agent in agents {
        lines.push(format!("- **{}** (`{}`)", agent.name, agent.id));

        // Summarise capabilities if any
        if !agent.capability_manifests.is_empty() {
            let cap_names: Vec<&str> = agent
                .capability_manifests
                .values()
                .flat_map(|m| m.tools.iter().map(|t| t.name.as_str()))
                .collect();
            if !cap_names.is_empty() {
                lines.push(format!("  Tools: {}", cap_names.join(", ")));
            }
        }

        // Include a brief hint from the agent's system prompt (first line)
        let hint = agent
            .system_prompt
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        if !hint.is_empty() {
            lines.push(format!("  Description: {}", hint));
        }
    }

    lines.join("\n")
}

/// Parse and validate the `delegate_to_agent` tool call arguments.
pub fn parse_args(args: &serde_json::Value) -> Result<(String, String)> {
    let agent_id = args["agent_id"]
        .as_str()
        .context("delegate_to_agent: missing 'agent_id' string")?
        .to_string();

    let message = args["message"]
        .as_str()
        .context("delegate_to_agent: missing 'message' string")?
        .to_string();

    Ok((agent_id, message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::loader::AgentDefinition;

    fn make_agent(id: &str, name: &str, prompt: &str) -> Arc<AgentDefinition> {
        Arc::new(AgentDefinition {
            id: id.to_string(),
            name: name.to_string(),
            model: None,
            routing: Default::default(),
            session: Default::default(),
            system_prompt: prompt.to_string(),
            capabilities: vec![],
            capability_manifests: HashMap::new(),
            install_steps: vec![],
        })
    }

    #[test]
    fn tool_spec_excludes_current_agent() {
        let mut agents = HashMap::new();
        agents.insert(
            "default".to_string(),
            make_agent("default", "Selu", "General assistant"),
        );
        agents.insert(
            "weather".to_string(),
            make_agent("weather", "Weather", "Weather forecasts"),
        );
        agents.insert(
            "homekit".to_string(),
            make_agent("homekit", "HomeKit", "Smart home control"),
        );

        let spec = tool_spec("default", &agents);
        assert_eq!(spec.name, TOOL_NAME);

        let enum_vals = spec.parameters["properties"]["agent_id"]["enum"]
            .as_array()
            .unwrap();
        let ids: Vec<&str> = enum_vals.iter().map(|v| v.as_str().unwrap()).collect();

        assert!(!ids.contains(&"default"), "should not include self");
        assert!(ids.contains(&"weather"));
        assert!(ids.contains(&"homekit"));
    }

    #[test]
    fn agent_registry_prompt_lists_specialists() {
        let mut agents = HashMap::new();
        agents.insert(
            "default".to_string(),
            make_agent("default", "Selu", "General assistant"),
        );
        agents.insert(
            "weather".to_string(),
            make_agent("weather", "Weather", "Live weather forecasts"),
        );

        let prompt = agent_registry_prompt("default", &agents);
        assert!(prompt.contains("Weather"));
        assert!(prompt.contains("weather"));
        assert!(prompt.contains("Live weather forecasts"));
        assert!(!prompt.contains("Selu")); // should not include self
    }

    #[test]
    fn parse_args_ok() {
        let args = serde_json::json!({
            "agent_id": "weather",
            "message": "What's the weather in Berlin?"
        });
        let (id, msg) = parse_args(&args).unwrap();
        assert_eq!(id, "weather");
        assert_eq!(msg, "What's the weather in Berlin?");
    }

    #[test]
    fn parse_args_missing_fields() {
        assert!(parse_args(&serde_json::json!({})).is_err());
        assert!(parse_args(&serde_json::json!({"agent_id": "x"})).is_err());
    }
}
