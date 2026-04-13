//! Cache-breakpoint quirk for Anthropic Messages.
//!
//! Anthropic's prompt caching requires an `"cache_control":
//! {"type":"ephemeral"}` marker on the last mutable block of the most
//! recent message. Thinking and redacted_thinking blocks are skipped
//! because they round-trip verbatim and their signatures break if
//! annotated.
//!
//! Extracted from `provider::claude` by RFC 0002. Still invoked directly
//! from `ClaudeProvider::build_request_body`; the quirks-middleware
//! cutover happens in the final commit of PR1.

/// Annotate the last mutable content block of the last message with an
/// ephemeral cache breakpoint. No-op on empty input or when the last
/// message contains only thinking blocks.
pub fn apply_cache_breakpoint(api_messages: &mut [serde_json::Value]) {
    let Some(last_msg) = api_messages.last_mut() else {
        return;
    };
    let Some(content) = last_msg["content"].as_array_mut() else {
        return;
    };
    for block in content.iter_mut().rev() {
        let block_type = block["type"].as_str().unwrap_or("");
        if matches!(block_type, "thinking" | "redacted_thinking") {
            continue;
        }
        block["cache_control"] = serde_json::json!({"type": "ephemeral"});
        break;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_breakpoint_on_array_content() {
        let mut msgs = vec![serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "a"},
                {"type": "text", "text": "b"},
            ]
        })];
        apply_cache_breakpoint(&mut msgs);
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert!(content[0].get("cache_control").is_none());
        assert_eq!(content[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_breakpoint_skips_thinking_and_marks_last_mutable_block() {
        let mut msgs = vec![serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "answer"},
                {"type": "thinking", "thinking": "x", "signature": "sig"},
                {"type": "redacted_thinking", "data": "opaque"},
            ]
        })];
        apply_cache_breakpoint(&mut msgs);
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
        assert!(content[1].get("cache_control").is_none());
        assert!(content[2].get("cache_control").is_none());
    }

    #[test]
    fn cache_breakpoint_with_only_thinking_blocks_is_noop() {
        let mut msgs = vec![serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "x", "signature": "sig"},
                {"type": "redacted_thinking", "data": "opaque"},
            ]
        })];
        apply_cache_breakpoint(&mut msgs);
        let content = msgs[0]["content"].as_array().unwrap();
        assert!(content[0].get("cache_control").is_none());
        assert!(content[1].get("cache_control").is_none());
    }

    #[test]
    fn cache_breakpoint_on_empty_messages_is_noop() {
        let mut msgs: Vec<serde_json::Value> = vec![];
        apply_cache_breakpoint(&mut msgs);
        assert!(msgs.is_empty());
    }
}
