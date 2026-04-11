/// OpenAI-compatible chat completions provider with SSE streaming.
use crate::core::provider::{Provider, StopReason, StreamRequest, StreamResponse};
use crate::core::types::{
    ContentBlock, Message, Role, ThinkingLevel, ToolCall, ToolCallFunction, ToolSchema, Usage,
};
use crate::event::Event;
use crate::provider::sse::post_sse;
use anyhow::Result;
use std::collections::HashMap;

const BASE_URL: &str = "https://api.openai.com/v1";

/// OpenAI chat completions provider (also works with Codex).
pub struct OpenAIProvider {
    model: String,
    max_tokens: u32,
    base_url: String,
    api_key: String,
    account_label: String,
}

impl OpenAIProvider {
    /// Create from model name, API key, and pool account label.
    pub fn new(model: &str, api_key: &str, account_label: &str) -> Self {
        Self {
            model: model.to_owned(),
            max_tokens: crate::provider::claude::DEFAULT_MAX_TOKENS,
            base_url: BASE_URL.to_owned(),
            api_key: api_key.to_owned(),
            account_label: account_label.to_owned(),
        }
    }
}

impl Provider for OpenAIProvider {
    fn name(&self) -> &str {
        "openai"
    }
    fn set_thinking(&mut self, _level: ThinkingLevel) {
        // OpenAI Chat Completions has no reasoning/thinking parameter.
    }

    fn server_tool_schemas(&self, _capabilities: &[String]) -> Vec<serde_json::Value> {
        // Chat Completions API does not support web_search tool type.
        // Web search requires search-specific models (gpt-4o-search-preview).
        // Client-side WebSearchTool fallback handles this.
        vec![]
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
            let api_messages = to_api_messages(messages, resolve_image);
            let mut api_tools = to_api_tools(tools);

            // Append server-side tools
            for st in server_tools {
                api_tools.push(st.clone());
            }

            let mut body = serde_json::json!({
                "model": self.model,
                "max_tokens": effective_max_tokens,
                "messages": api_messages,
                "stream": true,
            });

            if !api_tools.is_empty() {
                body["tools"] = api_tools.into();
            }

            let auth_header = format!("Bearer {}", self.api_key);
            let headers = [("Authorization", auth_header.as_str())];

            let mut text = String::new();
            let mut tool_map: HashMap<u64, (String, String, String)> = HashMap::new();
            let mut usage = Usage::default();
            let mut finish_reason = String::new();

            let mut stream = post_sse(
                "openai",
                &self.account_label,
                &format!("{}/chat/completions", self.base_url),
                &headers,
                &body,
                &tx,
                &cancel,
            )
            .await?;

            while let Some(event_result) = stream.next().await {
                let event = event_result?;
                let choice = &event.data["choices"][0];
                if let Some(r) = choice["finish_reason"].as_str()
                    && !r.is_empty()
                {
                    finish_reason = r.to_owned();
                }
                let delta = &choice["delta"];

                if !delta.is_null() {
                    if let Some(t) = delta["reasoning_content"].as_str()
                        && !t.is_empty()
                    {
                        let _ = tx.send(Event::Thinking(t.to_owned())).await;
                    }

                    if let Some(t) = delta["content"].as_str()
                        && !t.is_empty()
                    {
                        text.push_str(t);
                        let _ = tx.send(Event::Token(t.to_owned())).await;
                    }

                    if let Some(tcs) = delta["tool_calls"].as_array() {
                        for tc in tcs {
                            let idx = tc["index"].as_u64().unwrap_or(0);
                            let entry = tool_map
                                .entry(idx)
                                .or_insert_with(|| (String::new(), String::new(), String::new()));
                            if let Some(id) = tc["id"].as_str() {
                                entry.0 = id.to_owned();
                            }
                            if let Some(name) = tc["function"]["name"].as_str() {
                                entry.1 = name.to_owned();
                            }
                            if let Some(args) = tc["function"]["arguments"].as_str() {
                                entry.2.push_str(args);
                            }
                        }
                    }
                }

                if let Some(u) = event.data["usage"].as_object() {
                    let cached = u
                        .get("prompt_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(|v| v.as_u64());
                    let prompt = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    // OpenAI prompt_tokens includes cached — subtract to match Claude semantics
                    let non_cached = prompt.saturating_sub(cached.unwrap_or(0));
                    let u_data = Usage {
                        input_tokens: non_cached,
                        output_tokens: u
                            .get("completion_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0),
                        cache_read: cached,
                        cache_write: None,
                    };
                    usage = u_data.clone();
                    let _ = tx.send(Event::Usage(u_data)).await;
                }
            }

            // Stream ended without [DONE] AND without content → truly
            // interrupted. Retryable. Valid finish_reason + empty is legitimate.
            let is_empty = text.is_empty() && tool_map.is_empty();
            if !stream.saw_done() && is_empty {
                return Err(crate::provider::sse::StreamInterrupted(
                    "OpenAI stream ended with no content".into(),
                )
                .into());
            }

            let tool_calls: Vec<ToolCall> = tool_map
                .into_values()
                .map(|(id, name, args)| ToolCall {
                    id,
                    r#type: "function".into(),
                    function: ToolCallFunction {
                        name,
                        arguments: args,
                    },
                })
                .collect();

            let mut msg = Message::assistant(text);
            if !tool_calls.is_empty() {
                msg.tool_calls = Some(tool_calls);
            }
            Ok(StreamResponse {
                message: msg,
                usage,
                stop_reason: parse_finish_reason(&finish_reason),
            })
        })
    }
}

/// Map OpenAI finish_reason string to unified [`StopReason`].
fn parse_finish_reason(s: &str) -> StopReason {
    match s {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" => StopReason::ToolUse,
        _ => StopReason::Other,
    }
}

fn to_api_messages(
    messages: &[Message],
    resolve: &crate::core::provider::ImageResolver,
) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|msg| match msg.role {
            Role::System => serde_json::json!({"role": "system", "content": msg.text()}),
            Role::User => {
                if msg.has_images() {
                    let content: Vec<serde_json::Value> = msg
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } | ContentBlock::Paste { text }
                                if !text.is_empty() =>
                            {
                                Some(serde_json::json!({"type": "text", "text": text}))
                            }
                            ContentBlock::Image { media_type, id } => {
                                let data = resolve(id);
                                if data.is_empty() {
                                    return None;
                                }
                                Some(serde_json::json!({
                                    "type": "image_url",
                                    "image_url": {"url": format!("data:{media_type};base64,{data}")}
                                }))
                            }
                            _ => None,
                        })
                        .collect();
                    serde_json::json!({"role": "user", "content": content})
                } else {
                    serde_json::json!({"role": "user", "content": msg.text()})
                }
            }
            Role::Assistant => {
                let mut v = serde_json::json!({"role": "assistant", "content": msg.text()});
                if let Some(tcs) = &msg.tool_calls {
                    let api_tcs: Vec<_> = tcs.iter().map(|tc| serde_json::json!({
                        "id": tc.id, "type": "function",
                        "function": {"name": tc.function.name, "arguments": tc.function.arguments}
                    })).collect();
                    v["tool_calls"] = api_tcs.into();
                }
                v
            }
            Role::Tool => serde_json::json!({
                "role": "tool",
                "content": msg.text(),
                "tool_call_id": msg.tool_call_id.as_deref().unwrap_or("")
            }),
        })
        .collect()
}

fn to_api_tools(tools: &[ToolSchema]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
}
