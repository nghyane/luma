/// Core types shared across agent, provider, and tool modules.
use serde::{Deserialize, Serialize};

/// A content block within a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { media_type: String, id: String },
    Paste { text: String },
}

/// A chat message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

impl Message {
    /// Concatenate all text + paste blocks into a single string.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            let t = match block {
                ContentBlock::Text { text } => text.as_str(),
                ContentBlock::Paste { text } => text.as_str(),
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
                ContentBlock::Text { text } => text.as_str(),
                ContentBlock::Paste { text } => text.as_str(),
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

    /// Whether there is any text content.
    pub fn has_text(&self) -> bool {
        self.content.iter().any(|b| match b {
            ContentBlock::Text { text } | ContentBlock::Paste { text } => !text.is_empty(),
            _ => false,
        })
    }

    /// Whether message contains image blocks.
    pub fn has_images(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Image { .. }))
    }

    /// Create a text-only message.
    fn text_msg(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
            tool_call_id: None,
            tool_calls: None,
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

    /// Create an assistant message from text.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::text_msg(Role::Assistant, text)
    }

    /// Create a tool result message.
    pub fn tool(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: vec![ContentBlock::Text { text: text.into() }],
            tool_call_id: Some(id.into()),
            tool_calls: None,
        }
    }
}

/// Message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub r#type: String,
    pub function: ToolCallFunction,
}

/// The function name and arguments of a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
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
}

impl ThinkingLevel {
    /// Budget in tokens for this thinking level.
    pub const fn budget(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Low => 1024,
            Self::Medium => 4096,
            Self::High => 8192,
        }
    }

    /// Cycle to next level.
    pub const fn next(self) -> Self {
        match self {
            Self::Off => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Off,
        }
    }

    /// Display label — consistent with `AgentMode::as_str` convention.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
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
    fn thinking_level_cycle() {
        assert_eq!(ThinkingLevel::Off.next(), ThinkingLevel::Low);
        assert_eq!(ThinkingLevel::Low.next(), ThinkingLevel::Medium);
        assert_eq!(ThinkingLevel::Medium.next(), ThinkingLevel::High);
        assert_eq!(ThinkingLevel::High.next(), ThinkingLevel::Off);
    }

    #[test]
    fn thinking_level_budget() {
        assert_eq!(ThinkingLevel::Off.budget(), 0);
        assert_eq!(ThinkingLevel::High.budget(), 8192);
    }

    #[test]
    fn message_serializes() {
        let msg = Message::user("hello");
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(!json.contains("tool_call_id"));
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
            tool_call_id: None,
            tool_calls: None,
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

        let t = Message::tool("tc_1", "result");
        assert_eq!(t.role, Role::Tool);
        assert_eq!(t.tool_call_id.as_deref(), Some("tc_1"));
    }

    #[test]
    fn usage_default() {
        let u = Usage::default();
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 0);
    }
}
