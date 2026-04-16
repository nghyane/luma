pub mod binding;
pub mod gateway;
pub mod gateways;
pub mod json_stream;
pub mod protocol;
pub mod quirks;
pub mod retry;
pub mod sse;
pub mod stream_io;

/// Estimate total context chars matching Kiro CLI's algorithm:
/// text + tool_use input + tool_result content + tool spec JSON.
/// tokens = (chars / 4 + 5) / 10 * 10
pub fn estimate_context_chars(
    messages: &[crate::core::types::Message],
    tool_schemas: &[crate::core::types::ToolSchema],
) -> usize {
    use crate::core::types::{ContentBlock, ToolResultBody, ToolResultItem};

    fn json_char_count(v: &serde_json::Value) -> usize {
        match v {
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 1,
            serde_json::Value::String(s) => s.len(),
            serde_json::Value::Array(a) => a.iter().map(json_char_count).sum(),
            serde_json::Value::Object(m) => m.values().map(json_char_count).sum(),
        }
    }

    let mut chars: usize = 0;
    for msg in messages {
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } | ContentBlock::Paste { text } => chars += text.len(),
                ContentBlock::ToolUse { input, .. } => chars += json_char_count(input),
                ContentBlock::ToolResult { content, .. } => match content {
                    ToolResultBody::Text(s) => chars += s.len(),
                    ToolResultBody::Items(items) => {
                        for item in items {
                            if let ToolResultItem::Text { text } = item {
                                chars += text.len();
                            }
                        }
                    }
                },
                ContentBlock::Thinking { thinking, .. } => chars += thinking.len(),
                _ => {}
            }
        }
    }
    for schema in tool_schemas {
        if let Ok(s) = serde_json::to_string(schema) {
            chars += s.len();
        }
    }
    chars
}
