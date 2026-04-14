//! Thinking-config quirk for Anthropic Messages.
//!
//! Maps a unified `ThinkingLevel` into Anthropic's wire shape, which
//! differs across models:
//!
//! * Adaptive-capable models (Sonnet/Opus 4.6) use
//!   `thinking: {"type": "adaptive"}` plus optional
//!   `output_config: {"effort": ...}`.
//! * Older thinking-capable models use
//!   `thinking: {"type": "enabled", "budget_tokens": N}` and no
//!   `output_config`.
//! * `ThinkingLevel::Off` disables both fields.

use crate::core::types::ThinkingLevel;

/// Build `(thinking, output_config)` for the Anthropic request body.
pub fn build_thinking_config(
    model: &str,
    level: ThinkingLevel,
    max_tokens: u32,
) -> Option<(serde_json::Value, Option<serde_json::Value>)> {
    if level == ThinkingLevel::Off {
        return None;
    }
    if is_adaptive_thinking_model(model) {
        let effort = match level {
            ThinkingLevel::Off => unreachable!("Off short-circuits above"),
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Max => "max",
        };
        return Some((
            serde_json::json!({"type": "adaptive"}),
            Some(serde_json::json!({"effort": effort})),
        ));
    }
    let budget = level.budget();
    if budget == 0 || max_tokens <= 1 {
        return None;
    }
    let capped = budget.min(max_tokens - 1);
    Some((
        serde_json::json!({
            "type": "enabled",
            "budget_tokens": capped,
        }),
        None,
    ))
}

/// Whether `model` supports Anthropic's adaptive thinking mode.
///
/// Mirrors upstream `rN_(model)` in `claude-code@2.1.100`: true only for
/// `opus-4-6` and `sonnet-4-6`. Other Claude 4.x models still use the
/// old `{type: "enabled", budget_tokens: N}` shape.
pub fn is_adaptive_thinking_model(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("opus-4-6") || m.contains("sonnet-4-6")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_config_off_returns_none() {
        assert_eq!(
            build_thinking_config("claude-sonnet-4-5", ThinkingLevel::Off, 8192),
            None
        );
    }

    #[test]
    fn thinking_config_enabled_under_max_is_passed_through() {
        let (thinking, output_config) =
            build_thinking_config("claude-sonnet-4-5", ThinkingLevel::Low, 8192).unwrap();
        assert_eq!(thinking["type"], "enabled");
        assert_eq!(thinking["budget_tokens"], 1024);
        assert!(output_config.is_none());
    }

    #[test]
    fn thinking_config_enabled_capped_to_max_minus_one() {
        let (thinking, output_config) =
            build_thinking_config("claude-sonnet-4-5", ThinkingLevel::High, 8192).unwrap();
        assert_eq!(thinking["budget_tokens"], 8191);
        assert!(output_config.is_none());
    }

    #[test]
    fn thinking_config_enabled_with_tiny_max_returns_none() {
        assert_eq!(
            build_thinking_config("claude-sonnet-4-5", ThinkingLevel::Low, 1),
            None
        );
        assert_eq!(
            build_thinking_config("claude-sonnet-4-5", ThinkingLevel::Low, 0),
            None
        );
    }

    #[test]
    fn thinking_config_adaptive_for_sonnet_4_6_uses_effort() {
        let (thinking, output_config) =
            build_thinking_config("claude-sonnet-4-6", ThinkingLevel::Low, 8192).unwrap();
        assert_eq!(thinking["type"], "adaptive");
        assert!(thinking.get("budget_tokens").is_none());
        let output_config = output_config.expect("adaptive output_config");
        assert_eq!(output_config["effort"], "low");

        let (thinking, output_config) =
            build_thinking_config("claude-opus-4-6", ThinkingLevel::Medium, 64_000).unwrap();
        assert_eq!(thinking["type"], "adaptive");
        assert_eq!(output_config.unwrap()["effort"], "medium");
    }

    #[test]
    fn thinking_config_adaptive_high_maps_to_high_effort() {
        let (thinking, output_config) =
            build_thinking_config("claude-sonnet-4-6", ThinkingLevel::High, 8192).unwrap();
        assert_eq!(thinking["type"], "adaptive");
        assert_eq!(output_config.unwrap()["effort"], "high");
    }

    #[test]
    fn thinking_config_adaptive_max_maps_to_max_effort() {
        let (thinking, output_config) =
            build_thinking_config("claude-sonnet-4-6", ThinkingLevel::Max, 8192).unwrap();
        assert_eq!(thinking["type"], "adaptive");
        assert_eq!(output_config.unwrap()["effort"], "max");
    }

    #[test]
    fn adaptive_thinking_model_matches_upstream() {
        assert!(is_adaptive_thinking_model("claude-opus-4-6"));
        assert!(is_adaptive_thinking_model("claude-sonnet-4-6"));
        assert!(is_adaptive_thinking_model("claude-sonnet-4-6-20251002"));
        assert!(!is_adaptive_thinking_model("claude-sonnet-4-5"));
        assert!(!is_adaptive_thinking_model("claude-opus-4-5"));
        assert!(!is_adaptive_thinking_model("claude-haiku-4-5"));
        assert!(!is_adaptive_thinking_model("claude-3-opus"));
    }
}
