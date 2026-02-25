use crate::agents::loader::AgentDefinition;
use std::collections::HashMap;

/// Parse an @mention from the start of a message.
/// Returns (agent_id, remaining_text_without_mention)
pub fn parse_mention(text: &str) -> Option<(String, String)> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('@')?;
    // Agent ID is the first word after @
    let agent_id = rest.split_whitespace().next()?;
    if agent_id.is_empty() {
        return None;
    }
    // Remaining text is everything after "@agent_id "
    let message_start = trimmed[1 + agent_id.len()..].trim_start();
    Some((agent_id.to_lowercase(), message_start.to_string()))
}

/// Determine which agent should handle a message.
/// Returns (agent_id, effective_message_text)
pub fn route(
    text: &str,
    default_agent_id: Option<&str>,
    available_agents: &HashMap<String, AgentDefinition>,
) -> (String, String) {
    if let Some((agent_id, remaining)) = parse_mention(text) {
        if available_agents.contains_key(&agent_id) {
            return (agent_id, remaining);
        }
        // Unknown @mention — fall through to default with original text
    }

    let default_id = default_agent_id
        .filter(|id| available_agents.contains_key(*id))
        .unwrap_or("default");

    (default_id.to_string(), text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mention() {
        assert_eq!(
            parse_mention("@dev fix the tests"),
            Some(("dev".to_string(), "fix the tests".to_string()))
        );
        assert_eq!(
            parse_mention("@pim what's on my calendar?"),
            Some(("pim".to_string(), "what's on my calendar?".to_string()))
        );
        assert_eq!(parse_mention("hello there"), None);
        assert_eq!(
            parse_mention("  @home turn off lights  "),
            Some(("home".to_string(), "turn off lights".to_string()))
        );
        // @mention with no message body
        assert_eq!(
            parse_mention("@dev"),
            Some(("dev".to_string(), "".to_string()))
        );
    }

    #[test]
    fn test_route_with_mention() {
        let mut agents = HashMap::new();
        agents.insert(
            "dev".to_string(),
            AgentDefinition {
                id: "dev".to_string(),
                name: "Dev".to_string(),
                model: crate::agents::loader::ModelConfig {
                    provider: "anthropic".to_string(),
                    model_id: "claude-opus-4-5".to_string(),
                    temperature: 0.7,
                },
                session: Default::default(),
                memory: Default::default(),
                system_prompt: String::new(),
                capabilities: vec![],
                capability_manifests: std::collections::HashMap::new(),
            },
        );

        let (agent_id, text) = route("@dev fix the tests", Some("default"), &agents);
        assert_eq!(agent_id, "dev");
        assert_eq!(text, "fix the tests");
    }

    #[test]
    fn test_route_unknown_mention_falls_to_default() {
        let agents = HashMap::new(); // empty — "default" not in agents
        let (agent_id, text) = route("@unknown hello", Some("default"), &agents);
        assert_eq!(agent_id, "default");
        assert_eq!(text, "@unknown hello");
    }
}
