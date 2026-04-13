//! OAuth-mode system block rewrite + `anthropic-beta` header.
//!
//! Claude Code's OAuth path prepends a billing attribution block and
//! swaps in a fixed identity string ahead of the user system prompt.
//! The backend rejects traffic missing either the billing header shape
//! or the beta list, so the exact wire form must match upstream.
//!
//! Upstream references: `src/utils/api.ts::splitSysPromptPrefix`,
//! `src/services/api/claude.ts::buildSystemPromptBlocks`,
//! `src/utils/betas.ts::getAllModelBetas`.
//!
//! Extracted from `provider::claude` by RFC 0002.

use crate::provider::quirks::claude_identity::{CLI_VERSION, compute_fingerprint};

pub const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Build the OAuth-mode `system` array.
///
/// Wire shape:
/// 1. attribution header (no `cache_control`, `cacheScope: null`)
/// 2. CLI sysprompt prefix / identity (`cache_control: { type: 'ephemeral' }`)
/// 3. optional user system text (same cache_control)
pub fn build_oauth_system(user_system: &str, first_user_content: &str) -> serde_json::Value {
    let fingerprint = compute_fingerprint(first_user_content);
    // Native-client-attestation placeholder — claude-code@2.1.100 always
    // emits ` cch=00000;` on first-party traffic. The real CLI's HTTP
    // stack overwrites the zeros with a computed attestation token in
    // flight; omitting the segment entirely trips the backend's
    // first-party client check.
    let billing = format!(
        "x-anthropic-billing-header: cc_version={CLI_VERSION}.{fingerprint}; cc_entrypoint=cli; cch=00000;"
    );
    let cache_ephemeral = serde_json::json!({"type": "ephemeral"});
    let mut blocks = vec![
        serde_json::json!({"type": "text", "text": billing}),
        serde_json::json!({"type": "text", "text": IDENTITY, "cache_control": cache_ephemeral}),
    ];
    if !user_system.is_empty() {
        blocks.push(serde_json::json!({
            "type": "text",
            "text": user_system,
            "cache_control": cache_ephemeral,
        }));
    }
    serde_json::Value::Array(blocks)
}

/// `anthropic-beta` header value, in upstream emit order for the common
/// Claude.ai subscriber + Claude 4.x path.
pub fn build_betas(model: &str) -> String {
    let m = model.to_lowercase();
    let is_haiku = m.contains("haiku");
    let is_claude_3 = m.contains("claude-3-");
    let mut betas: Vec<&str> = Vec::new();
    if !is_haiku {
        betas.push("claude-code-20250219");
    }
    betas.push("oauth-2025-04-20");
    if !is_haiku && !is_claude_3 {
        betas.push("interleaved-thinking-2025-05-14");
    }
    if !is_claude_3 {
        betas.push("context-management-2025-06-27");
    }
    betas.push("prompt-caching-scope-2026-01-05");
    betas.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billing_block_has_expected_shape() {
        let sys = build_oauth_system("my system", "hello world");
        let arr = sys.as_array().expect("array");
        assert_eq!(arr.len(), 3);

        let billing = arr[0]["text"].as_str().unwrap();
        assert!(billing.starts_with("x-anthropic-billing-header: cc_version="));
        assert!(billing.contains(&format!("cc_version={CLI_VERSION}.")));
        assert!(billing.contains("cc_entrypoint=cli;"));
        assert!(billing.contains("cch=00000;"));
        assert!(!billing.contains("ttl"));
        assert!(arr[0].get("cache_control").is_none());

        assert_eq!(arr[1]["text"], IDENTITY);
        assert_eq!(arr[1]["cache_control"]["type"], "ephemeral");
        assert!(arr[1]["cache_control"].get("ttl").is_none());

        assert_eq!(arr[2]["text"], "my system");
        assert_eq!(arr[2]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn billing_block_omits_user_system_when_empty() {
        let sys = build_oauth_system("", "hi");
        let arr = sys.as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn beta_list_for_claude_4_oauth_matches_upstream() {
        let betas = build_betas("claude-sonnet-4-6");
        assert!(betas.contains("claude-code-20250219"));
        assert!(betas.contains("oauth-2025-04-20"));
        assert!(betas.contains("interleaved-thinking-2025-05-14"));
        assert!(betas.contains("context-management-2025-06-27"));
        assert!(betas.contains("prompt-caching-scope-2026-01-05"));
    }

    #[test]
    fn beta_list_for_haiku_drops_claude_code_and_interleaved() {
        let betas = build_betas("claude-haiku-4-5");
        assert!(!betas.contains("claude-code-20250219"));
        assert!(!betas.contains("interleaved-thinking-2025-05-14"));
        assert!(betas.contains("oauth-2025-04-20"));
        assert!(betas.contains("prompt-caching-scope-2026-01-05"));
    }

    #[test]
    fn beta_list_for_claude_3_drops_interleaved_and_context_mgmt() {
        let betas = build_betas("claude-3-opus");
        assert!(!betas.contains("interleaved-thinking-2025-05-14"));
        assert!(!betas.contains("context-management-2025-06-27"));
    }
}
