/// Core types shared across agent, provider, and tool modules.
use serde::{Deserialize, Serialize};

/// A content block within a message.
///
/// Single heterogeneous array that matches Anthropic's wire format
/// byte-for-byte (`text | image | tool_use | tool_result | thinking |
/// redacted_thinking`). Order is preserved — required for interleaved
/// thinking signature validation. Providers that speak a different wire
/// shape (OpenAI, Codex) reconstruct their format from this one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Model-generated or user-provided text.
    Text { text: String },
    /// Pasted text — rendered like `Text` but kept distinct so the TUI
    /// can collapse long pastes without rewriting the original bytes.
    Paste { text: String },
    /// Image attachment. `id` resolves to base64 data at send time via
    /// the provider's `ImageResolver`.
    Image { media_type: String, id: String },
    /// Tool invocation requested by the model.
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    /// Tool execution result sent back to the model.
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
        /// Structured file-change artifact from write/edit/apply_patch tools.
        /// Persisted so session resume can render diffs identically.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact: Option<FileChangeArtifact>,
    },
    /// Extended-thinking block with signature. Both fields must round-trip
    /// verbatim on subsequent turns or the Anthropic backend rejects the
    /// request with a signature-mismatch 400.
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: String,
    },
    /// Opaque thinking block redacted by the backend's safety layer.
    /// Must still be echoed back on later turns.
    RedactedThinking { data: String },
}

/// A chat message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
}

/// Provider/model provenance for assistant messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageOrigin {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Message {
    /// Concatenate all text + paste blocks into a single string.
    ///
    /// Used for display previews, token counting, and legacy consumers
    /// that want a flat string view. Non-text blocks are skipped.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            let t = match block {
                ContentBlock::Text { text } | ContentBlock::Paste { text } => text.as_str(),
                _ => continue,
            };
            if !out.is_empty() && !t.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
        out
    }

    /// Concatenate text from standalone content blocks (no Message needed).
    pub fn content_text(blocks: &[ContentBlock]) -> String {
        let mut out = String::new();
        for b in blocks {
            let t = match b {
                ContentBlock::Text { text } | ContentBlock::Paste { text } => text.as_str(),
                _ => continue,
            };
            if !out.is_empty() && !t.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
        out
    }

    /// First text block only — for display (excludes file attachments).
    pub fn display_text(&self) -> &str {
        self.content
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("")
    }

    /// Whether any text block is non-empty.
    pub fn has_text(&self) -> bool {
        self.content.iter().any(|b| match b {
            ContentBlock::Text { text } | ContentBlock::Paste { text } => !text.is_empty(),
            _ => false,
        })
    }

    /// Whether the message carries content visible to the user.
    ///
    /// Tool-result and thinking blocks are internal plumbing — they exist
    /// in the history for the provider but are never shown as standalone
    /// conversation turns. This is the single source of truth for both
    /// stream replay and history rendering.
    pub fn has_visible_content(&self) -> bool {
        self.content.iter().any(|b| match b {
            ContentBlock::Text { text } | ContentBlock::Paste { text } => !text.is_empty(),
            ContentBlock::Image { .. } | ContentBlock::ToolUse { .. } => true,
            ContentBlock::ToolResult { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::RedactedThinking { .. } => false,
        })
    }

    /// Whether message contains image blocks.
    pub fn has_images(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Image { .. }))
    }

    /// Whether the message contains at least one `ToolUse` block.
    pub fn has_tool_use(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
    }

    /// Iterate over `ToolUse` blocks.
    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &serde_json::Value)> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }

    /// Create a text-only message.
    fn text_msg(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
            origin: None,
        }
    }

    /// Create a user message from text.
    #[cfg(test)]
    pub fn user(text: impl Into<String>) -> Self {
        Self::text_msg(Role::User, text)
    }

    /// Create a system message.
    pub fn system(text: impl Into<String>) -> Self {
        Self::text_msg(Role::System, text)
    }

    /// Create an assistant message from text. Test-only — the stream
    /// layer builds assistant messages from `ContentBlock` vecs directly.
    #[cfg(test)]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::text_msg(Role::Assistant, text)
    }

    /// Create a user message carrying a single tool_result block.
    ///
    /// Tool results always ride on user messages in the Anthropic wire
    /// format; `Role::Tool` is gone from the unified schema.
    pub fn tool_result(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: content.into(),
                is_error: false,
                artifact: None,
            }],
            origin: None,
        }
    }
}

/// Message role. `Tool` is gone — tool results are user messages carrying
/// `ContentBlock::ToolResult` blocks, matching Anthropic's wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// JSON schema for tool parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    /// Name of a string argument whose value should be streamed to the UI as
    /// the model writes it (e.g. `"content"` for Write, `"new_string"` for
    /// Edit). `None` disables the preview for this tool.
    ///
    /// Streaming is opt-in so tools own the decision: which field is a large
    /// user-visible payload vs control flags. Provider layer must not
    /// hardcode tool names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streamable_arg: Option<String>,
}

/// File operation captured by a tool artifact.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FileOp {
    Add,
    Update,
    Delete,
    Move { from: String },
}

/// Lifecycle state for a structured tool artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolStatus {
    Streaming,
    Done,
    Failed,
}

/// Structured per-file artifact for file-changing tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileArtifact {
    pub path: String,
    pub operation: FileOp,
    pub diff: Option<String>,
    pub preview: Option<String>,
}

/// Shared artifact for write/edit/apply_patch style tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChangeArtifact {
    pub files: Vec<FileArtifact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub status: ToolStatus,
}

/// Thinking budget level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingLevel {
    Off,
    Low,
    Medium,
    High,
    Max,
}

impl ThinkingLevel {
    /// Budget in tokens for this thinking level.
    pub const fn budget(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Low => 1024,
            Self::Medium => 4096,
            Self::High | Self::Max => 8192,
        }
    }

    /// Ordering rank used when degrading unsupported levels.
    pub const fn rank(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
            Self::Max => 4,
        }
    }
}

/// Token usage from a provider response.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_level_rank_orders_levels() {
        assert!(ThinkingLevel::Off.rank() < ThinkingLevel::Low.rank());
        assert!(ThinkingLevel::Low.rank() < ThinkingLevel::Medium.rank());
        assert!(ThinkingLevel::Medium.rank() < ThinkingLevel::High.rank());
        assert!(ThinkingLevel::High.rank() < ThinkingLevel::Max.rank());
    }

    #[test]
    fn thinking_level_budget() {
        assert_eq!(ThinkingLevel::Off.budget(), 0);
        assert_eq!(ThinkingLevel::High.budget(), 8192);
        assert_eq!(ThinkingLevel::Max.budget(), 8192);
    }

    #[test]
    fn message_serializes() {
        let msg = Message::user("hello");
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(!json.contains("tool_call_id"));
        assert!(!json.contains("tool_calls"));
    }

    #[test]
    fn message_text_helper() {
        let msg = Message::user("hello");
        assert_eq!(msg.text(), "hello");
    }

    #[test]
    fn message_multiblock_text() {
        let msg = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "hello".into(),
                },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    id: "img_1".into(),
                },
                ContentBlock::Text {
                    text: "world".into(),
                },
            ],
            origin: None,
        };
        assert_eq!(msg.text(), "hello\nworld");
        assert!(msg.has_images());
        assert!(msg.has_text());
    }

    #[test]
    fn deserialize_content_blocks() {
        let json = r#"{"role":"user","content":[{"type":"text","text":"hi"},{"type":"image","media_type":"image/png","id":"img_1"}]}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        assert_eq!(msg.text(), "hi");
        assert!(msg.has_images());
        assert_eq!(msg.content.len(), 2);
    }

    #[test]
    fn message_constructors() {
        let u = Message::user("test");
        assert_eq!(u.role, Role::User);
        assert_eq!(u.text(), "test");

        let s = Message::system("sys");
        assert_eq!(s.role, Role::System);

        let a = Message::assistant("reply");
        assert_eq!(a.role, Role::Assistant);

        let t = Message::tool_result("tc_1", "result");
        assert_eq!(t.role, Role::User);
        match &t.content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                artifact,
            } => {
                assert_eq!(tool_use_id, "tc_1");
                assert_eq!(content, "result");
                assert!(!is_error);
                assert!(artifact.is_none());
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn tool_use_blocks_roundtrip() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "calling tool".into(),
                },
                ContentBlock::ToolUse {
                    id: "tc_1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "/tmp/x"}),
                },
            ],
            origin: None,
        };
        assert!(msg.has_tool_use());
        let uses: Vec<_> = msg.tool_uses().collect();
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].0, "tc_1");
        assert_eq!(uses[0].1, "read");

        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.content.len(), 2);
        assert!(back.has_tool_use());
    }

    #[test]
    fn thinking_block_roundtrip() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "reasoning...".into(),
                    signature: "sig_abc".into(),
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
            origin: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"thinking\":\"reasoning...\""));
        assert!(json.contains("\"signature\":\"sig_abc\""));
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.content.len(), 2);
        match &back.content[0] {
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                assert_eq!(thinking, "reasoning...");
                assert_eq!(signature, "sig_abc");
            }
            _ => panic!("expected Thinking"),
        }
    }

    #[test]
    fn usage_default() {
        let u = Usage::default();
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 0);
    }

    #[test]
    fn has_visible_content_text() {
        let msg = Message::user("hello");
        assert!(msg.has_visible_content());
    }

    #[test]
    fn has_visible_content_tool_result_only() {
        let msg = Message::tool_result("tc_1", "result");
        assert!(!msg.has_visible_content());
    }

    #[test]
    fn has_visible_content_thinking_only() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: "hmm".into(),
                signature: "sig".into(),
            }],
            origin: None,
        };
        assert!(!msg.has_visible_content());
    }

    #[test]
    fn has_visible_content_tool_use() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tc_1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            }],
            origin: None,
        };
        assert!(msg.has_visible_content());
    }
}
