/// Claude provider — Anthropic Messages API with SSE streaming.
use crate::core::provider::Provider;
use crate::core::types::{
    ContentBlock, Message, Role, ThinkingLevel, ToolCall, ToolCallFunction, ToolSchema, Usage,
};
use crate::event::Event;
use crate::provider::sse::{SseEvent, post_sse};
use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const BASE_URL: &str = "https://api.anthropic.com";

/// Anthropic Claude provider.
pub struct ClaudeProvider {
    model: String,
    max_tokens: u32,
    base_url: String,
    api_key: String,
    is_oauth: bool,
    thinking: ThinkingLevel,
}

impl ClaudeProvider {
    /// Create from token. Set `is_oauth` true for OAuth tokens, false for raw API keys.
    pub fn new(model: &str, api_key: &str, is_oauth: bool) -> Self {
        Self {
            model: model.to_owned(),
            max_tokens: 8192,
            base_url: BASE_URL.to_owned(),
            api_key: api_key.to_owned(),
            is_oauth,
            thinking: ThinkingLevel::Off,
        }
    }
}

impl Provider for ClaudeProvider {
    fn name(&self) -> &str {
        "claude"
    }
    fn thinking(&self) -> ThinkingLevel {
        self.thinking
    }
    fn set_thinking(&mut self, level: ThinkingLevel) {
        self.thinking = level;
    }

    fn server_tool_schemas(&self, capabilities: &[String]) -> Vec<serde_json::Value> {
        capabilities.iter().filter_map(|cap| if cap == "web_search" {
            Some(serde_json::json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 5}))
        } else { None }).collect()
    }

    fn stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolSchema],
        server_tools: &'a [serde_json::Value],
        resolve_image: &'a crate::core::provider::ImageResolver,
        tx: mpsc::Sender<Event>,
        cancel: CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(Message, Usage)>> + Send + 'a>>
    {
        Box::pin(async move {
            let system_text = extract_system(messages);
            let api_messages = to_api_messages(messages, resolve_image);
            let mut api_tools = to_api_tools(tools);

            // Append server-side tools (e.g. web search)
            for st in server_tools {
                api_tools.push(st.clone());
            }

            // Prompt caching: single cache_control breakpoint on last message
            let mut api_messages = api_messages;
            if !api_messages.is_empty() {
                let last = api_messages.len() - 1;
                if let Some(content) = api_messages[last]["content"].as_array_mut() {
                    if let Some(last_block) = content.last_mut() {
                        last_block["cache_control"] = serde_json::json!({"type": "ephemeral"});
                    }
                } else {
                    let text_val = api_messages[last]["content"].take();
                    api_messages[last]["content"] = serde_json::json!([{
                        "type": "text",
                        "text": text_val,
                        "cache_control": {"type": "ephemeral"}
                    }]);
                }
            }

            let mut body = serde_json::json!({
                "model": self.model,
                "max_tokens": self.max_tokens,
                "messages": api_messages,
                "stream": true,
            });

            let budget = self.thinking.budget();
            if budget > 0 {
                body["thinking"] = serde_json::json!({"type": "enabled", "budget_tokens": budget});
            }

            if self.is_oauth {
                let first_user_text = messages
                    .iter()
                    .find(|m| m.role == Role::User)
                    .map(|m| m.text())
                    .unwrap_or_default();
                body["system"] = build_oauth_system(&system_text, &first_user_text);
                if api_tools.is_empty() {
                    api_tools.push(serde_json::json!({
                        "name": "mcp_noop", "description": "No-op",
                        "input_schema": {"type": "object", "properties": {}}
                    }));
                }
            } else if !system_text.is_empty() {
                body["system"] = system_text.into();
            }

            if !api_tools.is_empty() {
                body["tools"] = api_tools.into();
            }

            let auth_header = if self.is_oauth {
                format!("Bearer {}", self.api_key)
            } else {
                self.api_key.clone()
            };
            let auth_key = if self.is_oauth {
                "Authorization"
            } else {
                "x-api-key"
            };

            let mut header_vec: Vec<(&str, String)> = vec![
                (auth_key, auth_header),
                ("anthropic-version", "2023-06-01".into()),
            ];
            if self.is_oauth {
                let betas = build_betas(&self.model);
                header_vec.push(("anthropic-beta", betas));
                header_vec.push(("Anthropic-Dangerous-Direct-Browser-Access", "true".into()));
                header_vec.push(("User-Agent", "claude-cli/1.0.0 (external, cli)".into()));
                header_vec.push(("x-app", "cli".into()));
            }
            let headers: Vec<(&str, &str)> =
                header_vec.iter().map(|(k, v)| (*k, v.as_str())).collect();

            let mut text = String::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            let mut current_id = String::new();
            let mut current_name = String::new();
            let mut current_args = String::new();
            let mut streaming_content = false;
            let mut server_tool_json = String::new();
            let mut in_server_tool = false;
            let mut usage = Usage::default();
            let mut saw_message_stop = false;

            let tx_ref = &tx;
            let usage_ref = &mut usage;
            post_sse(
                "claude",
                &format!("{}/v1/messages", self.base_url),
                &headers,
                &body,
                &tx,
                &cancel,
                |event: SseEvent| {
                    let data = &event.data;

                    if data["type"] == "content_block_start" {
                        let block = &data["content_block"];
                        let block_type = block["type"].as_str().unwrap_or("");
                        match block_type {
                            "tool_use" => {
                                current_id = block["id"].as_str().unwrap_or("").to_owned();
                                current_name = block["name"].as_str().unwrap_or("").to_owned();
                                current_args.clear();
                                streaming_content = false;
                            }
                            "server_tool_use" if block["name"] == "web_search" => {
                                in_server_tool = true;
                                server_tool_json.clear();
                            }
                            "web_search_tool_result" => {
                                let content_len =
                                    block["content"].as_array().map(|a| a.len()).unwrap_or(0);
                                crate::dbg_log!(
                                    "claude web_search_tool_result: {} items in content",
                                    content_len
                                );
                                let mut hits = Vec::new();
                                if let Some(results) = block["content"].as_array() {
                                    for r in results {
                                        let title = r["title"].as_str().unwrap_or("").to_owned();
                                        let url = r["url"].as_str().unwrap_or("").to_owned();
                                        if !url.is_empty() {
                                            hits.push(crate::event::SearchHit {
                                                title,
                                                url,
                                                snippet: String::new(),
                                            });
                                        }
                                    }
                                }
                                let _ = tx_ref.try_send(Event::WebSearchDone {
                                    query: String::new(),
                                    results: hits,
                                });
                            }
                            _ => {}
                        }
                    }

                    if data["type"] == "content_block_delta" {
                        let delta = &data["delta"];
                        if delta["type"] == "thinking_delta" {
                            if let Some(t) = delta["thinking"].as_str() {
                                let _ = tx_ref.try_send(Event::Thinking(t.to_owned()));
                            }
                        } else if delta["type"] == "text_delta" {
                            if let Some(t) = delta["text"].as_str() {
                                text.push_str(t);
                                let _ = tx_ref.try_send(Event::Token(t.to_owned()));
                            }
                        } else if delta["type"] == "input_json_delta"
                            && let Some(j) = delta["partial_json"].as_str()
                        {
                            // Server tool (web search): accumulate JSON, extract query
                            if in_server_tool {
                                server_tool_json.push_str(j);
                                if let Some(start) = server_tool_json.find("\"query\"")
                                    && let Some(q) =
                                        extract_json_string_value(&server_tool_json[start..])
                                {
                                    let _ = tx_ref.try_send(Event::WebSearchStart { query: q });
                                    in_server_tool = false;
                                }
                            }
                            current_args.push_str(j);
                            // Stream content preview for write/edit tools
                            if is_streamable_tool(&current_name) {
                                if streaming_content {
                                    let _ = tx_ref.try_send(Event::ToolInput {
                                        name: current_name.clone(),
                                        chunk: unescape_json_chunk(j),
                                    });
                                } else if has_content_key(&current_args) {
                                    streaming_content = true;
                                    if let Some(initial) = extract_content_value(&current_args)
                                        && !initial.is_empty()
                                    {
                                        let _ = tx_ref.try_send(Event::ToolInput {
                                            name: current_name.clone(),
                                            chunk: initial,
                                        });
                                    }
                                }
                            }
                        }
                    }

                    if data["type"] == "content_block_stop" && !current_id.is_empty() {
                        tool_calls.push(ToolCall {
                            id: std::mem::take(&mut current_id),
                            r#type: "function".into(),
                            function: ToolCallFunction {
                                name: std::mem::take(&mut current_name),
                                arguments: std::mem::take(&mut current_args),
                            },
                        });
                    }

                    if data["type"] == "message_start"
                        && let Some(u) = data["message"]["usage"].as_object()
                    {
                        let u_data = Usage {
                            input_tokens: u
                                .get("input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                            output_tokens: u
                                .get("output_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                            cache_read: u.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
                            cache_write: u
                                .get("cache_creation_input_tokens")
                                .and_then(|v| v.as_u64()),
                        };
                        *usage_ref = u_data.clone();
                        let _ = tx_ref.try_send(Event::Usage(u_data));
                    }

                    // message_delta carries final output_tokens
                    if data["type"] == "message_delta"
                        && let Some(u) = data["usage"].as_object()
                        && let Some(out) = u.get("output_tokens").and_then(|v| v.as_u64())
                    {
                        usage_ref.output_tokens = out;
                        // Send without cache values — they were already reported in message_start
                        let _ = tx_ref.try_send(Event::Usage(Usage {
                            input_tokens: usage_ref.input_tokens,
                            output_tokens: out,
                            cache_read: None,
                            cache_write: None,
                        }));
                    }

                    if data["type"] == "message_stop" {
                        saw_message_stop = true;
                    }
                },
            )
            .await?;

            // Stream ended without message_stop — network cut, server error, etc.
            // Return partial content so the turn is not silently lost.
            if !saw_message_stop && text.is_empty() && tool_calls.is_empty() {
                anyhow::bail!("Claude stream ended with no content (missing message_stop)");
            }

            let mut msg = Message::assistant(text);
            if !tool_calls.is_empty() {
                msg.tool_calls = Some(tool_calls);
            }
            Ok((msg, usage))
        })
    }
}

const CLI_VERSION: &str = "1.0.0";
const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

fn build_oauth_system(user_system: &str, first_user_content: &str) -> serde_json::Value {
    let cch = compute_cch(first_user_content);
    let billing = format!(
        "x-anthropic-billing-header: cc_version={CLI_VERSION}; cc_entrypoint=cli; cch={cch};"
    );
    let mut blocks = vec![
        serde_json::json!({"type": "text", "text": billing, "cache_control": {"type": "ephemeral", "ttl": "1h"}}),
        serde_json::json!({"type": "text", "text": IDENTITY, "cache_control": {"type": "ephemeral", "ttl": "1h"}}),
    ];
    if !user_system.is_empty() {
        blocks.push(serde_json::json!({"type": "text", "text": user_system}));
    }
    serde_json::Value::Array(blocks)
}

fn compute_cch(first_user_content: &str) -> String {
    use sha2::{Digest, Sha256};
    let salt = "59cf53e54c78";
    let positions = [4, 7, 20];
    let chars: String = positions
        .iter()
        .map(|&p| first_user_content.chars().nth(p).unwrap_or('0'))
        .collect();
    let input = format!("{salt}{chars}{CLI_VERSION}");
    let hash = Sha256::digest(input.as_bytes());
    format!("{:x}", hash)[..5].to_owned()
}

fn build_betas(model: &str) -> String {
    let m = model.to_lowercase();
    let is_haiku = m.contains("haiku");
    let mut betas = Vec::new();
    if !is_haiku {
        betas.push("claude-code-20250219");
    }
    betas.push("oauth-2025-04-20");
    if !is_haiku && !m.contains("claude-3-") {
        betas.push("interleaved-thinking-2025-05-14");
    }
    betas.push("prompt-caching-scope-2026-01-05");
    betas.join(",")
}

fn extract_system(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Convert content blocks to Anthropic API format.
fn content_blocks_to_api(
    blocks: &[ContentBlock],
    resolve: &crate::core::provider::ImageResolver,
) -> Vec<serde_json::Value> {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } | ContentBlock::Paste { text } if !text.is_empty() => {
                Some(serde_json::json!({"type": "text", "text": text}))
            }
            ContentBlock::Image { media_type, id } => {
                let data = resolve(id);
                if data.is_empty() {
                    return None;
                }
                Some(serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": data,
                    }
                }))
            }
            _ => None,
        })
        .collect()
}

fn to_api_messages(
    messages: &[Message],
    resolve: &crate::core::provider::ImageResolver,
) -> Vec<serde_json::Value> {
    let mut result = Vec::new();
    for msg in messages {
        if msg.role == Role::System {
            continue;
        }
        match msg.role {
            Role::User => {
                let api_content = content_blocks_to_api(&msg.content, resolve);
                if api_content.len() == 1 && !msg.has_images() {
                    result.push(serde_json::json!({"role": "user", "content": msg.text()}));
                } else {
                    result.push(serde_json::json!({"role": "user", "content": api_content}));
                }
            }
            Role::Assistant => {
                let mut content = Vec::new();
                let text = msg.text();
                if !text.is_empty() {
                    content.push(serde_json::json!({"type": "text", "text": text}));
                }
                if let Some(tcs) = &msg.tool_calls {
                    for tc in tcs {
                        let input: serde_json::Value =
                            serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                        content.push(serde_json::json!({
                            "type": "tool_use", "id": tc.id,
                            "name": tc.function.name, "input": input
                        }));
                    }
                }
                result.push(serde_json::json!({"role": "assistant", "content": content}));
            }
            Role::Tool => {
                let tool_result = serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": msg.tool_call_id.as_deref().unwrap_or(""),
                    "content": msg.text()
                });
                if let Some(last) = result.last_mut()
                    && last["role"] == "user"
                    && last["content"].is_array()
                    && let Some(content_array) = last["content"].as_array_mut()
                {
                    content_array.push(tool_result);
                    continue;
                }
                result.push(serde_json::json!({"role": "user", "content": [tool_result]}));
            }
            _ => {}
        }
    }
    result
}

fn to_api_tools(tools: &[ToolSchema]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        })
        .collect()
}

/// Tools whose content field should be streamed to UI as preview.
const STREAMABLE_TOOLS: &[&str] = &["Write", "Edit"];

fn is_streamable_tool(name: &str) -> bool {
    STREAMABLE_TOOLS.contains(&name)
}

/// Content field keys in streaming JSON args.
const CONTENT_KEYS: &[&str] = &[
    "\"content\": \"",
    "\"content\":\"",
    "\"new_string\": \"",
    "\"new_string\":\"",
];

fn has_content_key(args: &str) -> bool {
    CONTENT_KEYS.iter().any(|k| args.contains(k))
}

/// Extract text after the content key's opening quote.
/// Extract a JSON string value from partial JSON like `"query": "rust async"`.
fn extract_json_string_value(s: &str) -> Option<String> {
    // Find pattern: "key": "value"
    let colon = s.find(':')?;
    let rest = s[colon + 1..].trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    let inner = &rest[1..];
    // Find closing quote (handle escapes)
    let mut end = 0;
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
            end += 2;
            continue;
        }
        if c == '"' {
            return Some(unescape_json_chunk(&inner[..end]));
        }
        end += c.len_utf8();
    }
    None // incomplete JSON, query not fully received yet
}

fn extract_content_value(args: &str) -> Option<String> {
    let mut best_pos = None;
    let mut best_key_len = 0;
    for key in CONTENT_KEYS {
        if let Some(pos) = args.rfind(key)
            && best_pos.is_none_or(|bp| pos > bp)
        {
            best_pos = Some(pos);
            best_key_len = key.len();
        }
    }
    best_pos.map(|pos| unescape_json_chunk(&args[pos + best_key_len..]))
}

/// Unescape JSON string escapes in a streaming chunk.
fn unescape_json_chunk(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}
