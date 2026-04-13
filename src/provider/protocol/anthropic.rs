/// Claude provider — Anthropic Messages API with SSE streaming.
///
/// Wire-level parity with the official Claude Code CLI (headers, betas,
/// system block shape). Sources: `src/utils/http.ts`,
/// `src/services/api/client.ts`, `src/services/api/claude.ts`,
/// `src/utils/fingerprint.ts`, `src/constants/system.ts`, `src/utils/betas.ts`
/// in `yasasbanukaofficial/claude-code`.
use crate::config::auth::AuthKind;
use crate::core::provider::{
    Provider, StopReason, StreamRequest, StreamResponse, ThinkingCapabilities, ThinkingOption,
};
use crate::core::types::{ContentBlock, Message, Role, ThinkingLevel, ToolSchema, Usage};
use crate::event::Event;
use crate::provider::json_stream::{JsonStringExtractor, streamable_arg_for};
use crate::provider::quirks::adaptive_thinking::{
    build_thinking_config, is_adaptive_thinking_model,
};
use crate::provider::quirks::cache_breakpoint::apply_cache_breakpoint;
use crate::provider::quirks::claude_identity::{claude_cli_user_agent, claude_session_id};
use crate::provider::quirks::oauth_system_rewrite::{build_betas, build_oauth_system};
use crate::provider::sse::post_sse;
use crate::util::uuid_v4;
use anyhow::Result;

const BASE_URL: &str = "https://api.anthropic.com";

/// Default output token cap, matching claude-code's capped default.
/// Caller can escalate to [`ESCALATED_MAX_TOKENS`] on first `max_tokens` hit.
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Escalation cap used after hitting `max_tokens` once. Claude 4.x native limit.
pub const ESCALATED_MAX_TOKENS: u32 = 64_000;

/// Anthropic Claude provider.
pub struct AnthropicRuntime {
    model: String,
    max_tokens: u32,
    base_url: String,
    api_key: String,
    auth_kind: AuthKind,
    thinking: ThinkingLevel,
    account_label: String,
}

impl AnthropicRuntime {
    /// Create from a credential token and its wire-level auth kind.
    /// `account_label` is the pool entry name used for rate-limit / usage accounting.
    pub fn new(model: &str, api_key: &str, auth_kind: AuthKind, account_label: &str) -> Self {
        Self {
            max_tokens: DEFAULT_MAX_TOKENS,
            model: model.to_owned(),
            base_url: BASE_URL.to_owned(),
            api_key: api_key.to_owned(),
            auth_kind,
            thinking: ThinkingLevel::Off,
            account_label: account_label.to_owned(),
        }
    }

    /// Build the full Anthropic Messages request body.
    ///
    /// Pure function of provider config + request inputs. Mixes in
    /// Claude-specific quirks (cache breakpoint, OAuth system rewrite,
    /// mcp_noop tool injection, thinking config) which RFC 0002 will
    /// later extract into middleware. For now this is an in-place
    /// refactor to make the wire-body building testable in isolation.
    fn build_request_body(
        &self,
        messages: &[Message],
        tools: &[crate::core::types::ToolSchema],
        server_tools: &[serde_json::Value],
        resolve_image: &crate::core::provider::ImageResolver,
        effective_max_tokens: u32,
    ) -> serde_json::Value {
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

        if let Some((thinking, output_config)) =
            build_thinking_config(&self.model, self.thinking, effective_max_tokens)
        {
            body["thinking"] = thinking;
            if let Some(output_config) = output_config {
                body["output_config"] = output_config;
            }
        }

        if matches!(self.auth_kind, AuthKind::OAuthBearer) {
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

        body
    }
}

impl Provider for AnthropicRuntime {
    fn name(&self) -> &str {
        "claude"
    }

    fn thinking_capabilities(&self) -> ThinkingCapabilities {
        if is_adaptive_thinking_model(&self.model) {
            ThinkingCapabilities::new(vec![
                ThinkingOption {
                    level: ThinkingLevel::Off,
                    label: "off",
                },
                ThinkingOption {
                    level: ThinkingLevel::Low,
                    label: "low",
                },
                ThinkingOption {
                    level: ThinkingLevel::Medium,
                    label: "medium",
                },
                ThinkingOption {
                    level: ThinkingLevel::High,
                    label: "high",
                },
                ThinkingOption {
                    level: ThinkingLevel::Max,
                    label: "max",
                },
            ])
        } else {
            ThinkingCapabilities::standard()
        }
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
            let body = self.build_request_body(
                messages,
                tools,
                server_tools,
                resolve_image,
                effective_max_tokens,
            );

            let (auth_key, auth_header) = match self.auth_kind {
                AuthKind::OAuthBearer | AuthKind::CodexSession => {
                    ("Authorization", format!("Bearer {}", self.api_key))
                }
                AuthKind::ApiKey => ("x-api-key", self.api_key.clone()),
            };

            // Default headers — matches `src/services/api/client.ts::getAnthropicClient`
            // and `src/utils/http.ts::getUserAgent`.
            let user_agent = claude_cli_user_agent();
            let session_id = claude_session_id();
            let request_id = uuid_v4().unwrap_or_default();
            let mut header_vec: Vec<(&str, String)> = vec![
                (auth_key, auth_header),
                ("anthropic-version", "2023-06-01".into()),
            ];
            if matches!(self.auth_kind, AuthKind::OAuthBearer) {
                let betas = build_betas(&self.model);
                header_vec.push(("anthropic-beta", betas));
                header_vec.push(("x-app", "cli".into()));
                header_vec.push(("User-Agent", user_agent));
                header_vec.push(("X-Claude-Code-Session-Id", session_id));
                if !request_id.is_empty() {
                    header_vec.push(("x-client-request-id", request_id));
                }
            }
            let headers: Vec<(&str, &str)> =
                header_vec.iter().map(|(k, v)| (*k, v.as_str())).collect();

            // --- Stream state ---------------------------------------------
            //
            // `blocks` is the authoritative ordered content for the assistant
            // message being assembled. Each Anthropic SSE `content_block_start`
            // opens a pending block; deltas append to it; `content_block_stop`
            // commits it into `blocks` in document order. This preserves the
            // interleaving required for thinking signature validation.
            //
            // `PendingBlock` carries mutable state that's only valid between
            // open and close of a single block — after commit, the data
            // becomes immutable inside `blocks`.
            let mut blocks: Vec<ContentBlock> = Vec::new();
            let mut pending: Option<PendingBlock> = None;
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

                match data["type"].as_str().unwrap_or("") {
                    "content_block_start" => {
                        let block = &data["content_block"];
                        let block_type = block["type"].as_str().unwrap_or("");
                        match block_type {
                            "text" => {
                                pending = Some(PendingBlock::Text {
                                    text: String::new(),
                                });
                            }
                            "thinking" => {
                                pending = Some(PendingBlock::Thinking {
                                    thinking: block["thinking"].as_str().unwrap_or("").to_owned(),
                                    signature: block["signature"].as_str().unwrap_or("").to_owned(),
                                });
                            }
                            "redacted_thinking" => {
                                pending = Some(PendingBlock::RedactedThinking {
                                    data: block["data"].as_str().unwrap_or("").to_owned(),
                                });
                            }
                            "tool_use" => {
                                let id = block["id"].as_str().unwrap_or("").to_owned();
                                let name = block["name"].as_str().unwrap_or("").to_owned();
                                if !name.is_empty() {
                                    let _ =
                                        tx.send(Event::ToolSelected { name: name.clone() }).await;
                                }
                                let streamable = streamable_arg_for(tools, &name);
                                crate::dbg_log!(
                                    "claude tool_use block_start: name={name:?} streamable_arg={streamable:?}"
                                );
                                let arg_extractor = streamable.map(JsonStringExtractor::new);
                                pending = Some(PendingBlock::ToolUse {
                                    id,
                                    name,
                                    args_buffer: String::new(),
                                    arg_extractor,
                                });
                            }
                            "server_tool_use" if block["name"] == "web_search" => {
                                pending = Some(PendingBlock::WebSearch {
                                    query_extractor: JsonStringExtractor::new("query"),
                                });
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
                                // web_search_tool_result is a terminal block
                                // with no deltas — nothing to commit on stop.
                            }
                            _ => {}
                        }
                    }

                    "content_block_delta" => {
                        let delta = &data["delta"];
                        let delta_type = delta["type"].as_str().unwrap_or("");
                        match (delta_type, pending.as_mut()) {
                            ("text_delta", Some(PendingBlock::Text { text })) => {
                                if let Some(t) = delta["text"].as_str() {
                                    text.push_str(t);
                                    let _ = tx.send(Event::Token(t.to_owned())).await;
                                }
                            }
                            ("thinking_delta", Some(PendingBlock::Thinking { thinking, .. })) => {
                                if let Some(t) = delta["thinking"].as_str() {
                                    thinking.push_str(t);
                                    let _ = tx.send(Event::Thinking(t.to_owned())).await;
                                }
                            }
                            ("signature_delta", Some(PendingBlock::Thinking { signature, .. })) => {
                                if let Some(s) = delta["signature"].as_str() {
                                    signature.push_str(s);
                                }
                            }
                            (
                                "input_json_delta",
                                Some(PendingBlock::ToolUse {
                                    name,
                                    args_buffer,
                                    arg_extractor,
                                    ..
                                }),
                            ) => {
                                if let Some(j) = delta["partial_json"].as_str() {
                                    args_buffer.push_str(j);
                                    if let Some(ex) = arg_extractor.as_mut() {
                                        let chunk = ex.feed(j);
                                        crate::dbg_log!(
                                            "claude input_json_delta tool={name} delta_bytes={} extracted={}",
                                            j.len(),
                                            chunk.len()
                                        );
                                        if !chunk.is_empty() {
                                            let _ = tx
                                                .send(Event::ToolInput {
                                                    name: name.clone(),
                                                    chunk,
                                                })
                                                .await;
                                        }
                                    } else {
                                        crate::dbg_log!(
                                            "claude input_json_delta tool={name} NO EXTRACTOR (streamable_arg not set or tool not found)"
                                        );
                                    }
                                }
                            }
                            (
                                "input_json_delta",
                                Some(PendingBlock::WebSearch { query_extractor }),
                            ) => {
                                if let Some(j) = delta["partial_json"].as_str() {
                                    let chunk = query_extractor.feed(j);
                                    if !chunk.is_empty() && !web_search_query_sent {
                                        let _ =
                                            tx.send(Event::WebSearchStart { query: chunk }).await;
                                        web_search_query_sent = true;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }

                    "content_block_stop" => {
                        if let Some(pb) = pending.take()
                            && let Some(committed) = pb.commit()
                        {
                            blocks.push(committed);
                        }
                    }

                    "message_start" => {
                        if let Some(u) = data["message"]["usage"].as_object() {
                            let u_data = Usage {
                                input_tokens: u
                                    .get("input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0),
                                output_tokens: u
                                    .get("output_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0),
                                cache_read: u
                                    .get("cache_read_input_tokens")
                                    .and_then(|v| v.as_u64()),
                                cache_write: u
                                    .get("cache_creation_input_tokens")
                                    .and_then(|v| v.as_u64()),
                            };
                            usage = u_data.clone();
                            let _ = tx.send(Event::Usage(u_data)).await;
                        }
                    }

                    "message_delta" => {
                        if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                            stop_reason = reason.to_owned();
                        }
                        if let Some(u) = data["usage"].as_object()
                            && let Some(out) = u.get("output_tokens").and_then(|v| v.as_u64())
                        {
                            usage.output_tokens = out;
                            // Cache values were already reported in message_start.
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

                    "message_stop" => {
                        saw_message_stop = true;
                    }

                    _ => {}
                }
            }

            // Stream ended without message_stop AND no content → truly
            // interrupted (network cut, proxy failure). Retryable.
            //
            // A valid stop_reason with empty content is a legitimate turn
            // (e.g. structured output tool call on turn 1, then end_turn on
            // turn 2 with no text). Do NOT error — let the caller decide
            // based on stop_reason.
            if !saw_message_stop && blocks.is_empty() {
                return Err(crate::provider::sse::StreamInterrupted(
                    "Claude stream ended with no content".into(),
                )
                .into());
            }

            Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: blocks,
                    origin: Some(crate::core::types::MessageOrigin {
                        provider: "anthropic".into(),
                        model: Some(self.model.clone()),
                    }),
                },
                usage,
                stop_reason: parse_stop_reason(&stop_reason),
            })
        })
    }
}

/// Pending block under construction during SSE streaming. Exactly one
/// variant is live between `content_block_start` and `content_block_stop`.
enum PendingBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        /// Raw accumulated `partial_json` fragments — parsed once on commit.
        args_buffer: String,
        /// Incremental extractor for the streamable arg (Write `content`,
        /// Edit `new_string`, etc.) — `None` when the tool opts out.
        arg_extractor: Option<JsonStringExtractor>,
    },
    /// Anthropic server_tool_use for web_search. Emits `WebSearchStart`
    /// via `query_extractor` but never materializes a wire block on our
    /// side — the Anthropic backend handles the search; we only display.
    WebSearch {
        query_extractor: JsonStringExtractor,
    },
}

impl PendingBlock {
    /// Finalize the pending block into a persistent `ContentBlock`, or
    /// `None` for blocks we don't retain (web_search server tool).
    fn commit(self) -> Option<ContentBlock> {
        match self {
            Self::Text { text } if !text.is_empty() => Some(ContentBlock::Text { text }),
            Self::Text { .. } => None,
            Self::Thinking {
                thinking,
                signature,
            } => Some(ContentBlock::Thinking {
                thinking,
                signature,
            }),
            Self::RedactedThinking { data } => Some(ContentBlock::RedactedThinking { data }),
            Self::ToolUse {
                id,
                name,
                args_buffer,
                ..
            } => {
                let input: serde_json::Value = if args_buffer.is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&args_buffer).unwrap_or_else(|_| serde_json::json!({}))
                };
                Some(ContentBlock::ToolUse { id, name, input })
            }
            Self::WebSearch { .. } => None,
        }
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

fn extract_system(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Convert a single `ContentBlock` to Anthropic wire JSON.
///
/// Order-preserving: every call produces exactly one wire block so the
/// caller can map over the message's content in document order.
fn content_block_to_api(
    block: &ContentBlock,
    resolve: &crate::core::provider::ImageResolver,
    include_provider_state: bool,
) -> Option<serde_json::Value> {
    match block {
        ContentBlock::Text { text } | ContentBlock::Paste { text } if !text.is_empty() => {
            Some(serde_json::json!({"type": "text", "text": text}))
        }
        ContentBlock::Text { .. } | ContentBlock::Paste { .. } => None,
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
        ContentBlock::ToolUse { id, name, input } => Some(serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        })),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => {
            let mut v = serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
            });
            if *is_error {
                v["is_error"] = serde_json::json!(true);
            }
            Some(v)
        }
        ContentBlock::Thinking {
            thinking,
            signature,
        } if include_provider_state => Some(serde_json::json!({
            "type": "thinking",
            "thinking": thinking,
            "signature": signature,
        })),
        ContentBlock::RedactedThinking { data } if include_provider_state => {
            Some(serde_json::json!({
                "type": "redacted_thinking",
                "data": data,
            }))
        }
        ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => None,
    }
}

fn should_roundtrip_claude_thinking(msg: &Message, is_latest_assistant: bool) -> bool {
    is_latest_assistant
        && msg.role == Role::Assistant
        && msg.has_tool_use()
        && msg
            .origin
            .as_ref()
            .is_some_and(|origin| origin.provider == "anthropic")
}

fn to_api_messages(
    messages: &[Message],
    resolve: &crate::core::provider::ImageResolver,
) -> Vec<serde_json::Value> {
    let latest_assistant_idx = messages.iter().rposition(|m| m.role == Role::Assistant);
    messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role != Role::System)
        .map(|(idx, msg)| {
            let include_provider_state =
                should_roundtrip_claude_thinking(msg, Some(idx) == latest_assistant_idx);
            let content: Vec<serde_json::Value> = msg
                .content
                .iter()
                .filter_map(|b| content_block_to_api(b, resolve, include_provider_state))
                .collect();
            let role = match msg.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => unreachable!(),
            };
            serde_json::json!({"role": role, "content": content})
        })
        .collect()
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
    fn adaptive_thinking_capabilities_include_max() {
        let provider = AnthropicRuntime::new(
            "claude-sonnet-4-6",
            "key",
            crate::config::auth::AuthKind::ApiKey,
            "acc",
        );
        let labels: Vec<_> = provider
            .thinking_capabilities()
            .options()
            .iter()
            .map(|o| o.label)
            .collect();
        assert_eq!(labels, ["off", "low", "medium", "high", "max"]);
    }

    #[test]
    fn non_adaptive_thinking_capabilities_stop_at_high() {
        let provider = AnthropicRuntime::new(
            "claude-sonnet-4-5",
            "key",
            crate::config::auth::AuthKind::ApiKey,
            "acc",
        );
        let labels: Vec<_> = provider
            .thinking_capabilities()
            .options()
            .iter()
            .map(|o| o.label)
            .collect();
        assert_eq!(labels, ["off", "low", "medium", "high"]);
    }

    #[test]
    fn strips_thinking_blocks_from_non_claude_history() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "reasoning".into(),
                    signature: "sig".into(),
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
            origin: Some(crate::core::types::MessageOrigin {
                provider: "codex".into(),
                model: Some("gpt-5.4".into()),
            }),
        }];
        let api = to_api_messages(&messages, &|_| String::new());
        let content = api[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn strips_thinking_blocks_from_claude_non_tool_turns() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "reasoning".into(),
                    signature: "sig".into(),
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
            origin: Some(crate::core::types::MessageOrigin {
                provider: "anthropic".into(),
                model: Some("claude-sonnet-4-6".into()),
            }),
        }];
        let api = to_api_messages(&messages, &|_| String::new());
        let content = api[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn preserves_thinking_blocks_for_latest_claude_tool_turn() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "old".into(),
                        signature: "old-sig".into(),
                    },
                    ContentBlock::Text {
                        text: "old answer".into(),
                    },
                ],
                origin: Some(crate::core::types::MessageOrigin {
                    provider: "anthropic".into(),
                    model: Some("claude-sonnet-4-6".into()),
                }),
            },
            Message::tool_result("tc_prev", "done"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "reasoning".into(),
                        signature: "sig".into(),
                    },
                    ContentBlock::Text {
                        text: "calling tool".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "tc_1".into(),
                        name: "read".into(),
                        input: serde_json::json!({"path": "/tmp/x"}),
                    },
                ],
                origin: Some(crate::core::types::MessageOrigin {
                    provider: "anthropic".into(),
                    model: Some("claude-sonnet-4-6".into()),
                }),
            },
            Message::tool_result("tc_1", "file"),
        ];
        let api = to_api_messages(&messages, &|_| String::new());
        let content = api[2]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[2]["type"], "tool_use");
        let old_content = api[0]["content"].as_array().unwrap();
        assert_eq!(old_content.len(), 1);
        assert_eq!(old_content[0]["type"], "text");
    }

    // --- Claude Code parity regression tests ---
}
