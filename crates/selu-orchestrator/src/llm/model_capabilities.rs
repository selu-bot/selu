/// Model-specific request capabilities that differ across provider generations.
///
/// Sampling fields are optional in every provider payload Selu builds. Newer
/// reasoning models increasingly require provider-managed sampling and reject
/// a caller-supplied temperature, even when older models on the same API accept
/// it. Keep those compatibility rules in one place so direct and Bedrock-backed
/// models behave consistently.
pub fn supports_configurable_temperature(provider_id: &str, model_id: &str) -> bool {
    let provider = provider_id.to_ascii_lowercase();
    let model = model_id.to_ascii_lowercase();

    match provider.as_str() {
        "openai" => !openai_uses_managed_temperature(&model),
        "anthropic" => !anthropic_uses_managed_temperature(&model),
        "bedrock" => {
            !openai_uses_managed_temperature(&model) && !anthropic_uses_managed_temperature(&model)
        }
        _ => true,
    }
}

fn openai_uses_managed_temperature(model: &str) -> bool {
    model.starts_with("gpt-5")
        || model.contains(".gpt-5")
        || ["o1", "o3", "o4"].iter().any(|family| {
            model == *family
                || model.starts_with(&format!("{family}-"))
                || model.contains(&format!(".{family}-"))
        })
}

fn anthropic_uses_managed_temperature(model: &str) -> bool {
    let is_claude_5 =
        model.contains("claude-") && model.split(['-', '.']).any(|segment| segment == "5");

    is_claude_5
        || [
            "claude-fable-",
            "claude-mythos-",
            "claude-opus-4-7",
            "claude-opus-4-8",
        ]
        .iter()
        .any(|family| model.contains(family))
}

#[cfg(test)]
mod tests {
    use super::supports_configurable_temperature;

    #[test]
    fn openai_reasoning_models_use_managed_temperature() {
        assert!(!supports_configurable_temperature("openai", "gpt-5.6"));
        assert!(!supports_configurable_temperature("openai", "o4-mini"));
        assert!(supports_configurable_temperature("openai", "gpt-4o"));
    }

    #[test]
    fn modern_anthropic_models_use_managed_temperature() {
        assert!(!supports_configurable_temperature(
            "anthropic",
            "claude-fable-5-1"
        ));
        assert!(!supports_configurable_temperature(
            "anthropic",
            "claude-opus-4-8"
        ));
        assert!(supports_configurable_temperature(
            "anthropic",
            "claude-sonnet-4-20250514"
        ));
    }

    #[test]
    fn bedrock_inference_profile_prefixes_are_supported() {
        assert!(!supports_configurable_temperature(
            "bedrock",
            "global.anthropic.claude-fable-5-1"
        ));
        assert!(!supports_configurable_temperature(
            "bedrock",
            "us.openai.gpt-5.6-sol"
        ));
        assert!(supports_configurable_temperature(
            "bedrock",
            "us.anthropic.claude-sonnet-4-20250514-v1:0"
        ));
    }
}
