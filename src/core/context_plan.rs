//! Context planner — build the message sequence sent to the provider.
//!
//! The planner is the single choke point between `Session.messages` (the
//! canonical transcript) and `provider.stream()`. It runs once per turn
//! iteration and owns:
//!
//! * which messages to include,
//! * whether to inject evidence blobs pulled from the evidence store, and
//! * how to shape the prelude (system prompt, handoff snapshot) so the
//!   cache prefix stays stable across turns.
//!
//! This file scaffolds the entry point only. The current implementation
//! is a faithful passthrough so we can land the wiring without changing
//! model behavior. Subsequent commits add evidence dedup / selection;
//! every rule lands with a regression test (RFC §9, §14).
use crate::core::types::Message;

/// Build the prepared message sequence for a single provider call.
///
/// Contract: the returned `Vec<Message>` is what the provider will see,
/// in order. Passthrough today; tomorrow this is where evidence
/// injection and transcript trimming live.
///
/// Allocates once per turn iteration. For a 60-message session this is
/// a single clone of ~60 `Message` values and is not on the hot path
/// compared to the network round trip that follows.
pub fn build_prepared_messages(transcript: &[Message]) -> Vec<Message> {
    transcript.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{ContentBlock, Message, Role};

    #[test]
    fn passthrough_preserves_order_and_count() {
        let msgs = vec![
            Message::system("sys"),
            Message::user("hi"),
            Message::assistant("hello"),
        ];
        let out = build_prepared_messages(&msgs);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].role, Role::System);
        assert_eq!(out[1].role, Role::User);
        assert_eq!(out[2].role, Role::Assistant);
    }

    #[test]
    fn passthrough_empty_input() {
        let out = build_prepared_messages(&[]);
        assert!(out.is_empty());
    }

    #[test]
    fn passthrough_preserves_tool_result_with_evidence_ref() {
        let msg = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tc_1".into(),
                content: "src/main.rs (520 lines, stored as evidence)".into(),
                is_error: false,
                evidence_id: Some("ev_abc".into()),
            }],
            origin: None,
        };
        let out = build_prepared_messages(&[msg]);
        assert_eq!(out.len(), 1);
        match &out[0].content[0] {
            ContentBlock::ToolResult {
                evidence_id,
                content,
                ..
            } => {
                assert_eq!(evidence_id.as_deref(), Some("ev_abc"));
                assert!(content.contains("520 lines"));
            }
            _ => panic!("tool_result block lost in passthrough"),
        }
    }
}
