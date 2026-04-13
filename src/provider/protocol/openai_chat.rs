/// OpenAI-compatible chat completions provider with SSE streaming.
use crate::core::provider::{
    Provider, StopReason, StreamRequest, StreamResponse, ThinkingCapabilities,
};
use crate::core::types::{ContentBlock, Message, Role, ThinkingLevel, ToolSchema, Usage};
use crate::event::Event;
use crate::event_bus::Sender as EventSender;
use crate::provider::sse::{SseEventStream, post_sse};
use anyhow::Result;
use std::collections::HashMap;

const BASE_URL: &str = "https://api.openai.com/v1";

/// OpenAI chat completions provider (also works with Codex).
pub struct OpenAIChatRuntime {
    model: String,
    max_tokens: u32,
    base_url: String,
    api_key: String,
    account_label: String,
}

impl OpenAIChatRuntime {
    /// Create from model name, API key, and pool account label.
    pub fn new(model: &str, api_key: &str, account_label: &str) -> Self {
        Self {
            model: model.to_owned(),
            max_tokens: crate::provider::protocol::anthropic::DEFAULT_MAX_TOKENS,
            base_url: BASE_URL.to_owned(),
            api_key: api_key.to_owned(),
            account_label: account_label.to_owned(),
        }
    }

    /// Build the OpenAI Chat Completions request body. Pure.
    fn build_request_body(
        &self,
        messages: &[crate::core::types::Message],
        tools: &[crate::core::types::ToolSchema],
        server_tools: &[serde_json::Value],
        resolve_image: &crate::core::provider::ImageResolver,
        effective_max_tokens: u32,
    ) -> serde_json::Value {
        let api_messages = to_api_messages(messages, resolve_image);
        let mut api_tools = to_api_tools(tools);
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
        body
    }
}

impl Provider for OpenAIChatRuntime {
    fn name(&self) -> &str {
        "openai"
    }

    fn thinking_capabilities(&self) -> ThinkingCapabilities {
        ThinkingCapabilities::off_only()
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
            let body = self.build_request_body(
                messages,
                tools,
                server_tools,
                resolve_image,
                effective_max_tokens,
            );

            let auth_header = format!("Bearer {}", self.api_key);
            let headers = [("Authorization", auth_header.as_str())];

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

            consume_chat_stream(&mut stream, &tx, &self.model).await
        })
    }
}

/// Drain an OpenAI Chat Completions SSE stream into a `StreamResponse`.
async fn consume_chat_stream(
    stream: &mut SseEventStream,
    tx: &EventSender,
    model: &str,
) -> Result<StreamResponse> {
    let mut text = String::new();
    let mut tool_map: HashMap<u64, (String, String, String)> = HashMap::new();
    let mut usage = Usage::default();
    let mut finish_reason = String::new();

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

    // Build the assistant message content in a single ordered array:
    // leading text (if any), then tool_use blocks by ascending index.
    // OpenAI Chat Completions streams text and tool_calls in parallel,
    // but the unified `Vec<ContentBlock>` model only stores document
    // order — we don't have per-token interleaving info, so text
    // first then tools is the closest faithful reconstruction.
    let mut content: Vec<ContentBlock> = Vec::new();
    if !text.is_empty() {
        content.push(ContentBlock::Text { text });
    }
    let mut sorted_tools: Vec<_> = tool_map.into_iter().collect();
    sorted_tools.sort_by_key(|(idx, _)| *idx);
    for (_, (id, name, args)) in sorted_tools {
        let input: serde_json::Value = if args.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&args).unwrap_or_else(|_| serde_json::json!({}))
        };
        content.push(ContentBlock::ToolUse { id, name, input });
    }

    Ok(StreamResponse {
        message: Message {
            role: Role::Assistant,
            content,
            origin: Some(crate::core::types::MessageOrigin {
                provider: "openai".into(),
                model: Some(model.to_owned()),
            }),
        },
        usage,
        stop_reason: parse_finish_reason(&finish_reason),
    })
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
    let mut out = Vec::new();
    for msg in messages {
        match msg.role {
            Role::System => {
                out.push(serde_json::json!({"role": "system", "content": msg.text()}));
            }
            Role::User => {
                // Tool results on user messages → one `{role:"tool"}` entry
                // per block (OpenAI wire format — no nesting).
                let mut had_tool_result = false;
                for block in &msg.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                    {
                        out.push(serde_json::json!({
                            "role": "tool",
                            "content": content,
                            "tool_call_id": tool_use_id,
                        }));
                        had_tool_result = true;
                    }
                }
                if had_tool_result {
                    continue;
                }

                // Plain user message — text + images.
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
                                    "image_url": {
                                        "url": format!("data:{media_type};base64,{data}")
                                    }
                                }))
                            }
                            _ => None,
                        })
                        .collect();
                    out.push(serde_json::json!({"role": "user", "content": content}));
                } else {
                    out.push(serde_json::json!({"role": "user", "content": msg.text()}));
                }
            }
            Role::Assistant => {
                // Flatten: text blocks concatenated, tool_use blocks mapped
                // to the OpenAI `tool_calls` array. Thinking / redacted
                // thinking are intentionally dropped — OpenAI chat
                // completions doesn't understand them.
                let text = msg.text();
                let tool_calls: Vec<serde_json::Value> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, input } => Some(serde_json::json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(input).unwrap_or_default(),
                            }
                        })),
                        _ => None,
                    })
                    .collect();

                let mut v = serde_json::json!({"role": "assistant", "content": text});
                if !tool_calls.is_empty() {
                    v["tool_calls"] = tool_calls.into();
                }
                out.push(v);
            }
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Planner smooshes `<system-reminder>` evidence chunks into the last
    /// `tool_result.content` (see RFC §9.1). This test pins that the
    /// OpenAI adapter (a) forwards that content verbatim on the wire and
    /// (b) drops the internal `evidence_id` marker so it never reaches
    /// the model.
    #[test]
    fn tool_result_passes_smooshed_content_through_and_drops_evidence_id() {
        let smooshed = "original tool output\n\n<system-reminder>\n# Retrieved evidence: ev_1 (src/x.rs)\n\nfn main() {}\n</system-reminder>";
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: smooshed.into(),
                is_error: false,
                evidence_id: Some("ev_1".into()),
            }],
            origin: None,
        }];

        let api = to_api_messages(&messages, &|_| String::new());

        assert_eq!(api.len(), 1);
        assert_eq!(api[0]["role"], "tool");
        assert_eq!(api[0]["tool_call_id"], "call_1");
        assert_eq!(api[0]["content"].as_str(), Some(smooshed));
        // `evidence_id` is internal; the wire payload must not carry it.
        assert!(
            api[0].get("evidence_id").is_none(),
            "evidence_id leaked to OpenAI wire: {}",
            api[0]
        );
        assert!(
            !api[0].to_string().contains("evidence_id"),
            "evidence_id string leaked anywhere in payload: {}",
            api[0]
        );
    }
}
