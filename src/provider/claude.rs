/// Claude provider — Anthropic Messages API with SSE streaming.
use crate::core::provider::{Provider, StopReason, StreamRequest, StreamResponse};
use crate::core::types::{
    ContentBlock, Message, Role, ThinkingLevel, ToolCall, ToolCallFunction, ToolSchema, Usage,
};
use crate::event::Event;
use crate::provider::json_stream::{JsonStringExtractor, streamable_arg_for};
use crate::provider::sse::post_sse;
use anyhow::Result;

const BASE_URL: &str = "https://api.anthropic.com";

/// Default output token cap, matching claude-code's capped default.
/// Caller can escalate to [`ESCALATED_MAX_TOKENS`] on first `max_tokens` hit.
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Escalation cap used after hitting `max_tokens` once. Claude 4.x native limit.
pub const ESCALATED_MAX_TOKENS: u32 = 64_000;

/// Anthropic Claude provider.
pub struct ClaudeProvider {
    model: String,
    max_tokens: u32,
    base_url: String,
    api_key: String,
    is_oauth: bool,
    thinking: ThinkingLevel,
    account_label: String,
}

impl ClaudeProvider {
    /// Create from token. Set `is_oauth` true for OAuth tokens, false for raw API keys.
    /// `account_label` is the pool entry name used for rate-limit / usage accounting.
    pub fn new(model: &str, api_key: &str, is_oauth: bool, account_label: &str) -> Self {
        Self {
            max_tokens: DEFAULT_MAX_TOKENS,
            model: model.to_owned(),
            base_url: BASE_URL.to_owned(),
            api_key: api_key.to_owned(),
            is_oauth,
            thinking: ThinkingLevel::Off,
            account_label: account_label.to_owned(),
        }
    }
}

impl Provider for ClaudeProvider {
    fn name(&self) -> &str {
        "claude"
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
        req: StreamRequest<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<StreamResponse>> + Send + 'a>>
    {
        Box::pin(async move {
            let StreamRequest {
                messages,
                tools,
                server_tools,
                resolve_image,
                max_tokens_override,
                tx,
                cancel,
            } = req;
            let effective_max_tokens = max_tokens_override.unwrap_or(self.max_tokens);
            let system_text = extract_system(messages);
            let mut api_messages = to_api_messages(messages, resolve_image);
            let mut api_tools = to_api_tools(tools);

            // Append server-side tools (e.g. web search)
            for st in server_tools {
                api_tools.push(st.clone());
            }

            apply_cache_breakpoint(&mut api_messages);

            let mut body = serde_json::json!({
                "model": self.model,
                "max_tokens": effective_max_tokens,
                "messages": api_messages,
                "stream": true,
            });

            if let Some(budget) = resolve_thinking_budget(self.thinking, effective_max_tokens) {
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
            // JSON string extractor for the streamable arg of the current tool.
            // `None` when the current tool has no streamable_arg, or between tool blocks.
            let mut arg_extractor: Option<JsonStringExtractor> = None;
            // Extractor for the `query` field of a web_search server tool call.
            let mut web_search_extractor: Option<JsonStringExtractor> = None;
            let mut web_search_query_sent = false;
            let mut usage = Usage::default();
            let mut saw_message_stop = false;
            let mut stop_reason = String::new();

            let mut stream = post_sse(
                "claude",
                &self.account_label,
                &format!("{}/v1/messages", self.base_url),
                &headers,
                &body,
                &tx,
                &cancel,
            )
            .await?;

            while let Some(event_result) = stream.next().await {
                let event = event_result?;
                let data = &event.data;

                if data["type"] == "content_block_start" {
                    let block = &data["content_block"];
                    let block_type = block["type"].as_str().unwrap_or("");
                    match block_type {
                        "tool_use" => {
                            current_id = block["id"].as_str().unwrap_or("").to_owned();
                            current_name = block["name"].as_str().unwrap_or("").to_owned();
                            current_args.clear();
                            arg_extractor = streamable_arg_for(tools, &current_name)
                                .map(JsonStringExtractor::new);
                            // Signal block creation immediately so the UI shows
                            // a pending card during the gap between tool_use
                            // start and the first streamable-arg delta
                            // (Anthropic frequently pauses ~10s before emitting
                            // the `content` / `new_string` field).
                            if !current_name.is_empty() {
                                let _ = tx
                                    .send(Event::ToolSelected {
                                        name: current_name.clone(),
                                    })
                                    .await;
                            }
                        }
                        "server_tool_use" if block["name"] == "web_search" => {
                            web_search_extractor = Some(JsonStringExtractor::new("query"));
                            web_search_query_sent = false;
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
                            let _ = tx
                                .send(Event::WebSearchDone {
                                    query: String::new(),
                                    results: hits,
                                })
                                .await;
                        }
                        _ => {}
                    }
                }

                if data["type"] == "content_block_delta" {
                    let delta = &data["delta"];
                    if delta["type"] == "thinking_delta" {
                        if let Some(t) = delta["thinking"].as_str() {
                            let _ = tx.send(Event::Thinking(t.to_owned())).await;
                        }
                    } else if delta["type"] == "text_delta" {
                        if let Some(t) = delta["text"].as_str() {
                            text.push_str(t);
                            let _ = tx.send(Event::Token(t.to_owned())).await;
                        }
                    } else if delta["type"] == "input_json_delta"
                        && let Some(j) = delta["partial_json"].as_str()
                    {
                        current_args.push_str(j);

                        // Web search query: feed server-tool extractor.
                        if let Some(ex) = web_search_extractor.as_mut() {
                            let chunk = ex.feed(j);
                            if !chunk.is_empty() && !web_search_query_sent {
                                let _ = tx.send(Event::WebSearchStart { query: chunk }).await;
                                web_search_query_sent = true;
                            }
                        }

                        // Tool arg preview: feed the per-tool extractor and
                        // forward any unescaped characters that became
                        // available as a ToolInput event.
                        if let Some(ex) = arg_extractor.as_mut() {
                            let chunk = ex.feed(j);
                            if !chunk.is_empty() {
                                let _ = tx
                                    .send(Event::ToolInput {
                                        name: current_name.clone(),
                                        chunk,
                                    })
                                    .await;
                            }
                        }
                    }
                }

                if data["type"] == "content_block_stop" {
                    if !current_id.is_empty() {
                        tool_calls.push(ToolCall {
                            id: std::mem::take(&mut current_id),
                            r#type: "function".into(),
                            function: ToolCallFunction {
                                name: std::mem::take(&mut current_name),
                                arguments: std::mem::take(&mut current_args),
                            },
                        });
                    }
                    // Reset per-block extractors regardless of whether this
                    // was a tool_use, server_tool_use, or text block.
                    arg_extractor = None;
                    web_search_extractor = None;
                }

                if data["type"] == "message_start"
                    && let Some(u) = data["message"]["usage"].as_object()
                {
                    let u_data = Usage {
                        input_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                        output_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                        cache_read: u.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
                        cache_write: u
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_u64()),
                    };
                    usage = u_data.clone();
                    let _ = tx.send(Event::Usage(u_data)).await;
                }

                // message_delta carries final output_tokens and stop_reason
                if data["type"] == "message_delta" {
                    if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                        stop_reason = reason.to_owned();
                    }
                    if let Some(u) = data["usage"].as_object()
                        && let Some(out) = u.get("output_tokens").and_then(|v| v.as_u64())
                    {
                        usage.output_tokens = out;
                        // Send without cache values — they were already reported in message_start
                        let _ = tx
                            .send(Event::Usage(Usage {
                                input_tokens: usage.input_tokens,
                                output_tokens: out,
                                cache_read: None,
                                cache_write: None,
                            }))
                            .await;
                    }
                }

                if data["type"] == "message_stop" {
                    saw_message_stop = true;
                }
            }

            // Stream ended without message_stop AND without content → truly
            // interrupted (network cut, proxy failure). Retryable.
            //
            // Match claude-code behavior: a valid stop_reason with empty content
            // is a legitimate turn (e.g. structured output tool call on turn 1,
            // then end_turn on turn 2 with no text). Do NOT error — let the
            // caller decide what to do based on stop_reason.
            let is_empty = text.is_empty() && tool_calls.is_empty();
            if !saw_message_stop && is_empty {
                return Err(crate::provider::sse::StreamInterrupted(
                    "Claude stream ended with no content".into(),
                )
                .into());
            }

            let mut msg = Message::assistant(text);
            if !tool_calls.is_empty() {
                msg.tool_calls = Some(tool_calls);
            }
            Ok(StreamResponse {
                message: msg,
                usage,
                stop_reason: parse_stop_reason(&stop_reason),
            })
        })
    }
}

/// Map Anthropic stop_reason string to the unified [`StopReason`] enum.
fn parse_stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
        "tool_use" => StopReason::ToolUse,
        _ => StopReason::Other,
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

/// Resolve the effective `budget_tokens` for Anthropic's thinking parameter.
///
/// API invariant: `budget_tokens < max_tokens`. Returns `None` when thinking
/// is disabled (so the caller can skip the field entirely) or when
/// `max_tokens <= 1` (no room for any budget).
fn resolve_thinking_budget(level: ThinkingLevel, max_tokens: u32) -> Option<u32> {
    let budget = level.budget();
    if budget == 0 || max_tokens <= 1 {
        return None;
    }
    Some(budget.min(max_tokens - 1))
}

/// Apply a single `cache_control: ephemeral` breakpoint to the last block of
/// the last message. Mutates the messages in place.
///
/// Anthropic's prompt caching uses breakpoints to mark cache boundaries; a
/// single breakpoint on the last message caches the full prefix.
fn apply_cache_breakpoint(api_messages: &mut [serde_json::Value]) {
    let Some(last_msg) = api_messages.last_mut() else {
        return;
    };
    if let Some(content) = last_msg["content"].as_array_mut() {
        if let Some(last_block) = content.last_mut() {
            last_block["cache_control"] = serde_json::json!({"type": "ephemeral"});
        }
        return;
    }
    // Content is a bare string — lift it into an array with a cache_control
    // annotation.
    let text_val = last_msg["content"].take();
    last_msg["content"] = serde_json::json!([{
        "type": "text",
        "text": text_val,
        "cache_control": {"type": "ephemeral"}
    }]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stop_reason_known_values() {
        assert_eq!(parse_stop_reason("end_turn"), StopReason::EndTurn);
        assert_eq!(parse_stop_reason("max_tokens"), StopReason::MaxTokens);
        assert_eq!(
            parse_stop_reason("model_context_window_exceeded"),
            StopReason::MaxTokens,
        );
        assert_eq!(parse_stop_reason("tool_use"), StopReason::ToolUse);
        assert_eq!(parse_stop_reason("refusal"), StopReason::Other);
        assert_eq!(parse_stop_reason(""), StopReason::Other);
    }

    #[test]
    fn thinking_budget_off_returns_none() {
        assert_eq!(resolve_thinking_budget(ThinkingLevel::Off, 8192), None);
    }

    #[test]
    fn thinking_budget_under_max_is_passed_through() {
        // Low = 1024, max = 8192 → 1024 fits.
        assert_eq!(
            resolve_thinking_budget(ThinkingLevel::Low, 8192),
            Some(1024)
        );
    }

    #[test]
    fn thinking_budget_capped_to_max_minus_one() {
        // High = 8192, max = 8192 → must cap to 8191 (invariant budget < max).
        assert_eq!(
            resolve_thinking_budget(ThinkingLevel::High, 8192),
            Some(8191)
        );
    }

    #[test]
    fn thinking_budget_with_tiny_max_returns_none() {
        assert_eq!(resolve_thinking_budget(ThinkingLevel::Low, 1), None);
        assert_eq!(resolve_thinking_budget(ThinkingLevel::Low, 0), None);
    }

    #[test]
    fn cache_breakpoint_on_string_content() {
        let mut msgs = vec![serde_json::json!({"role": "user", "content": "hi"})];
        apply_cache_breakpoint(&mut msgs);
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "hi");
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
    }

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
    fn cache_breakpoint_on_empty_messages_is_noop() {
        let mut msgs: Vec<serde_json::Value> = vec![];
        apply_cache_breakpoint(&mut msgs);
        assert!(msgs.is_empty());
    }
}
