/// Parses incoming message text to determine routing:
/// - @mention -> named agent session
/// - no mention -> default agent for the pipe
///
/// Note: This is a utility kept for direct use; the main routing logic
/// is in agents::router which calls this internally.
#[allow(dead_code)]
pub fn parse_mention(text: &str) -> Option<String> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('@') {
        let agent_id = rest.split_whitespace().next()?;
        if !agent_id.is_empty() {
            return Some(agent_id.to_lowercase());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mention() {
        assert_eq!(parse_mention("@dev fix the tests"), Some("dev".to_string()));
        assert_eq!(
            parse_mention("@pim what's on my calendar?"),
            Some("pim".to_string())
        );
        assert_eq!(parse_mention("hello there"), None);
        assert_eq!(
            parse_mention("  @home turn off lights  "),
            Some("home".to_string())
        );
    }
}
