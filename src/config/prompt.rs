/// System prompt — composed from a behavior template (per agent mode) and
/// a tool-usage template (per provider tool style).
use crate::config::models::AgentMode;
use crate::tool::ToolStyle;

const RUSH: &str = include_str!("prompt/rush.md");
const SMART: &str = include_str!("prompt/smart.md");
const DEEP: &str = include_str!("prompt/deep.md");

const TOOLS_NATIVE: &str = include_str!("prompt/tools_native.md");
const TOOLS_PATCH: &str = include_str!("prompt/tools_patch.md");

/// Build the system prompt for the given agent mode and tool style.
///
/// Rush is intentionally minimal and does not include a tool-usage block;
/// the short inline hints in `rush.md` are enough.
pub fn build(mode: AgentMode, style: ToolStyle) -> String {
    let behavior = match mode {
        AgentMode::Rush => RUSH,
        AgentMode::Smart => SMART,
        AgentMode::Deep => DEEP,
    };
    if matches!(mode, AgentMode::Rush) {
        return behavior.to_owned();
    }
    let tools = match style {
        ToolStyle::Native => TOOLS_NATIVE,
        ToolStyle::Patch => TOOLS_PATCH,
    };
    format!("{behavior}\n{tools}")
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rush_is_short() {
        let p = build(AgentMode::Rush, ToolStyle::Native);
        assert!(p.len() < 900, "Rush too long: {}", p.len());
        // Rush is style-agnostic — same prompt for both styles.
        assert_eq!(p, build(AgentMode::Rush, ToolStyle::Patch));
    }

    #[test]
    fn smart_structure() {
        let p = build(AgentMode::Smart, ToolStyle::Native);
        assert!(p.contains("# Agency"));
        assert!(p.contains("dedicated tools"));
        assert!(p.contains("`MultiEdit`"));
        assert!(p.contains("# Git Safety"));
        assert!(p.contains("# Pragmatism"));
        assert!(p.contains("# Handling Ambiguity"));
        assert!(!p.contains("Autonomy"));
    }

    #[test]
    fn deep_native_structure() {
        let p = build(AgentMode::Deep, ToolStyle::Native);
        assert!(p.contains("pragmatic, effective software engineer"));
        assert!(p.contains("# Autonomy"));
        assert!(p.contains("`MultiEdit`"));
        assert!(p.contains("# Editing Constraints"));
        assert!(!p.contains("# Agency"));
    }

    #[test]
    fn deep_patch_structure() {
        let p = build(AgentMode::Deep, ToolStyle::Patch);
        assert!(p.contains("# Autonomy"));
        assert!(p.contains("apply_patch"));
        assert!(p.contains("exec_command"));
    }

    #[test]
    fn all_variants_have_git_safety() {
        for mode in &[AgentMode::Rush, AgentMode::Smart, AgentMode::Deep] {
            for style in &[ToolStyle::Native, ToolStyle::Patch] {
                let p = build(*mode, *style);
                assert!(
                    p.contains("reset --hard") || p.contains("destructive"),
                    "Missing git safety: {mode:?}/{style:?}"
                );
            }
        }
    }

    #[test]
    fn all_variants_have_emoji_rule() {
        for mode in &[AgentMode::Rush, AgentMode::Smart, AgentMode::Deep] {
            for style in &[ToolStyle::Native, ToolStyle::Patch] {
                let p = build(*mode, *style);
                assert!(p.contains("emoji"), "Missing emoji rule: {mode:?}/{style:?}");
            }
        }
    }
}
