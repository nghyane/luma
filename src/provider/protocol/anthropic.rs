/// Claude provider — Anthropic Messages API with SSE streaming.
///
/// Wire-level parity with the official Claude Code CLI (headers, betas,
/// system block shape). Sources: `src/utils/http.ts`,
/// `src/services/api/client.ts`, `src/services/api/claude.ts`,
/// `src/utils/fingerprint.ts`, `src/constants/system.ts`, `src/utils/betas.ts`
/// in `yasasbanukaofficial/claude-code`.
use crate::core::provider::{
    DEFAULT_MAX_TOKENS, Provider, StopReason, StreamEvent, StreamRequest, StreamResponse,
};
use crate::core::types::{
    ContentBlock, Message, Role, ThinkingLevel, ToolResultBody, ToolResultItem, ToolSchema, Usage,
};
use crate::event::Event;
use crate::provider::json_stream::{JsonStringExtractor, streamable_arg_for};
use crate::provider::quirks::QuirkSet;
use crate::provider::quirks::adaptive_thinking::build_thinking_config;
use crate::provider::quirks::cache_breakpoint::apply_cache_breakpoint;
use crate::provider::quirks::claude_identity::{claude_cli_user_agent, claude_session_id};
use crate::provider::quirks::oauth_system_rewrite::{build_betas, build_oauth_system};
use crate::provider::sse::{SseEventStream, post_sse};
use crate::util::uuid_v4;
use anyhow::Result;
use futures::stream::{BoxStream, StreamExt};
use std::collections::VecDeque;

/// Anthropic Claude provider.
pub struct AnthropicRuntime {
    model: String,
    max_tokens: u32,
    base_url: String,
    api_key: String,
    is_oauth: bool,
    quirks: QuirkSet,
    thinking: ThinkingLevel,
    account_label: String,
}

impl AnthropicRuntime {
    /// Create from a credential token, the OAuth-vs-api-key bit, and the
    /// set of quirks the gateway needs applied. `base_url` is the
    /// gateway's scheme+host with no trailing slash.
    pub fn new(
        model: &str,
        base_url: &str,
        api_key: &str,
        is_oauth: bool,
        quirks: QuirkSet,
        account_label: &str,
    ) -> Self {
        Self {
            max_tokens: DEFAULT_MAX_TOKENS,
            model: model.to_owned(),
            base_url: base_url.to_owned(),
            api_key: api_key.to_owned(),
            is_oauth,
            quirks,
            thinking: ThinkingLevel::Off,
            account_label: account_label.to_owned(),
        }
    }

    /// Build the full Anthropic Messages request body. Pure.
    ///
    /// Delegates Claude-specific concerns (cache breakpoint, OAuth system
    /// rewrite, mcp_noop tool injection, thinking config) to helpers in
    /// `provider::quirks`.
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

        if self.quirks.contains(QuirkSet::CACHE_BREAKPOINT) {
            apply_cache_breakpoint(&mut api_messages);
        }

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": effective_max_tokens,
            "messages": api_messages,
            "stream": true,
        });

        if self.quirks.contains(QuirkSet::ADAPTIVE_THINKING)
            && let Some((thinking, output_config)) =
                build_thinking_config(&self.model, self.thinking, effective_max_tokens)
        {
            body["thinking"] = thinking;
            if let Some(output_config) = output_config {
                body["output_config"] = output_config;
            }
        }

        if self.quirks.contains(QuirkSet::OAUTH_SYSTEM_REWRITE) {
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
                tool_use_tx,
            } = req;
            let effective_max_tokens = max_tokens_override.unwrap_or(self.max_tokens);
            let body = self.build_request_body(
                messages,
                tools,
                server_tools,
                resolve_image,
                effective_max_tokens,
            );

            let (auth_key, auth_header) = if self.is_oauth {
                ("Authorization", format!("Bearer {}", self.api_key))
            } else {
                ("x-api-key", self.api_key.clone())
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
            if self.quirks.contains(QuirkSet::ANTHROPIC_BETAS) {
                let betas = build_betas(&self.model);
                header_vec.push(("anthropic-beta", betas));
            }
            if self.quirks.contains(QuirkSet::CLAUDE_IDENTITY) {
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
            // Decoder converts Anthropic's wire SSE into normalized
            // `StreamEvent`s (see `decode_anthropic_sse`). The consumer
            // loop below drains that stream, forwards UI events to
            // `tx`, and accumulates the final message.
            let sse = post_sse(
                "claude",
                &self.account_label,
                &format!("{}/v1/messages", self.base_url),
                &headers,
                &body,
                &tx,
                &cancel,
            )
            .await?;

            let mut events = decode_anthropic_sse(sse, tools.to_vec());
            let mut blocks: Vec<ContentBlock> = Vec::new();
            let mut usage = Usage::default();
            let mut stop_reason = StopReason::default();
            let mut saw_done = false;

            loop {
                let evt = tokio::select! {
                    _ = cancel.cancelled() => break,
                    evt = events.next() => evt,
                };
                let Some(evt) = evt else {
                    break;
                };
                match evt? {
                    StreamEvent::TextDelta(t) => {
                        tx.send_or_log(Event::Token(t)).await;
                    }
                    StreamEvent::ThinkingDelta(t) => {
                        tx.send_or_log(Event::Thinking(t)).await;
                    }
                    StreamEvent::ToolSelected { name } => {
                        tx.send_or_log(Event::ToolSelected { name }).await;
                    }
                    StreamEvent::ToolInput { name, chunk } => {
                        tx.send_or_log(Event::ToolInput { name, chunk }).await;
                    }
                    StreamEvent::WebSearchStart { query } => {
                        tx.send_or_log(Event::WebSearchStart { query }).await;
                    }
                    StreamEvent::WebSearchDone { results } => {
                        tx
                            .send_or_log(Event::WebSearchDone {
                                query: String::new(),
                                results,
                            })
                            .await;
                    }
                    StreamEvent::UsageUpdate(u) => {
                        usage = u.clone();
                        tx.send_or_log(Event::Usage(u)).await;
                    }
                    StreamEvent::BlockComplete(b) => {
                        if let Some(ref tu_tx) = tool_use_tx {
                            if matches!(&b, ContentBlock::ToolUse { .. }) {
                                let _ = tu_tx.send(b.clone()).await;
                            }
                        }
                        blocks.push(b);
                    }
                    StreamEvent::Done { stop } => {
                        stop_reason = stop;
                        saw_done = true;
                        break;
                    }
                }
            }

            // Stream ended without a terminal Done event AND no content →
            // truly interrupted (network cut, proxy failure). Retryable.
            // A valid stop_reason with empty content is a legitimate turn
            // (e.g. structured output tool call on turn 1, then end_turn
            // on turn 2 with no text) — don't error.
            if !saw_done && blocks.is_empty() {
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
                stop_reason,
            })
        })
    }
}

/// Decoder state held across SSE frames. Pure — no I/O, no UI.
struct AnthropicDecoder {
    tools: Vec<ToolSchema>,
    pending: Option<PendingBlock>,
    web_search_query_sent: bool,
    /// Usage accumulator. `message_start` populates input/cache; `message_delta`
    /// overwrites output. Emitted as `UsageUpdate` at both boundaries.
    usage: Usage,
    /// Anthropic stop_reason string, resolved at `message_delta`. Converted
    /// to `StopReason` only when we emit `Done`.
    stop_reason_raw: String,
    /// Drained by the outer Stream on each poll.
    out: VecDeque<StreamEvent>,
    saw_message_stop: bool,
}

impl AnthropicDecoder {
    fn new(tools: Vec<ToolSchema>) -> Self {
        Self {
            tools,
            pending: None,
            web_search_query_sent: false,
            usage: Usage::default(),
            stop_reason_raw: String::new(),
            out: VecDeque::new(),
            saw_message_stop: false,
        }
    }

    /// Feed one Anthropic SSE frame; append zero or more `StreamEvent`s to
    /// the output queue. Never blocks; never touches the network.
    fn feed(&mut self, data: &serde_json::Value) {
        match data["type"].as_str().unwrap_or("") {
            "content_block_start" => self.on_block_start(&data["content_block"]),
            "content_block_delta" => self.on_block_delta(&data["delta"]),
            "content_block_stop" => self.on_block_stop(),
            "message_start" => {
                if let Some(u) = data["message"]["usage"].as_object() {
                    self.usage = Usage {
                        input_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                        output_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                        cache_read: u.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
                        cache_write: u
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_u64()),
                    };
                    self.out
                        .push_back(StreamEvent::UsageUpdate(self.usage.clone()));
                }
            }
            "message_delta" => {
                if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                    self.stop_reason_raw = reason.to_owned();
                }
                if let Some(u) = data["usage"].as_object()
                    && let Some(out) = u.get("output_tokens").and_then(|v| v.as_u64())
                {
                    self.usage.output_tokens = out;
                    // Cache values were already reported in message_start —
                    // emit only the new output count to avoid double-counting.
                    self.out.push_back(StreamEvent::UsageUpdate(Usage {
                        input_tokens: self.usage.input_tokens,
                        output_tokens: out,
                        cache_read: None,
                        cache_write: None,
                    }));
                }
            }
            "message_stop" => {
                self.saw_message_stop = true;
            }
            _ => {}
        }
    }

    fn on_block_start(&mut self, block: &serde_json::Value) {
        match block["type"].as_str().unwrap_or("") {
            "text" => {
                self.pending = Some(PendingBlock::Text {
                    text: String::new(),
                });
            }
            "thinking" => {
                self.pending = Some(PendingBlock::Thinking {
                    thinking: block["thinking"].as_str().unwrap_or("").to_owned(),
                    signature: block["signature"].as_str().unwrap_or("").to_owned(),
                });
            }
            "redacted_thinking" => {
                self.pending = Some(PendingBlock::RedactedThinking {
                    data: block["data"].as_str().unwrap_or("").to_owned(),
                });
            }
            "tool_use" => {
                let id = block["id"].as_str().unwrap_or("").to_owned();
                let name = block["name"].as_str().unwrap_or("").to_owned();
                if !name.is_empty() {
                    self.out
                        .push_back(StreamEvent::ToolSelected { name: name.clone() });
                }
                let streamable = streamable_arg_for(&self.tools, &name);
                let arg_extractor = streamable.map(JsonStringExtractor::new);
                self.pending = Some(PendingBlock::ToolUse {
                    id,
                    name,
                    args_buffer: String::new(),
                    arg_extractor,
                });
            }
            "server_tool_use" if block["name"] == "web_search" => {
                self.pending = Some(PendingBlock::WebSearch {
                    query_extractor: JsonStringExtractor::new("query"),
                });
                self.web_search_query_sent = false;
            }
            "web_search_tool_result" => {
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
                self.out
                    .push_back(StreamEvent::WebSearchDone { results: hits });
                // web_search_tool_result is terminal and has no deltas —
                // no PendingBlock is opened, nothing to commit on stop.
            }
            _ => {}
        }
    }

    fn on_block_delta(&mut self, delta: &serde_json::Value) {
        let delta_type = delta["type"].as_str().unwrap_or("");
        match (delta_type, self.pending.as_mut()) {
            ("text_delta", Some(PendingBlock::Text { text })) => {
                if let Some(t) = delta["text"].as_str() {
                    text.push_str(t);
                    self.out.push_back(StreamEvent::TextDelta(t.to_owned()));
                }
            }
            ("thinking_delta", Some(PendingBlock::Thinking { thinking, .. })) => {
                if let Some(t) = delta["thinking"].as_str() {
                    thinking.push_str(t);
                    self.out.push_back(StreamEvent::ThinkingDelta(t.to_owned()));
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
                        if !chunk.is_empty() {
                            self.out.push_back(StreamEvent::ToolInput {
                                name: name.clone(),
                                chunk,
                            });
                        }
                    }
                }
            }
            ("input_json_delta", Some(PendingBlock::WebSearch { query_extractor })) => {
                if let Some(j) = delta["partial_json"].as_str() {
                    let chunk = query_extractor.feed(j);
                    if !chunk.is_empty() && !self.web_search_query_sent {
                        self.out
                            .push_back(StreamEvent::WebSearchStart { query: chunk });
                        self.web_search_query_sent = true;
                    }
                }
            }
            _ => {}
        }
    }

    fn on_block_stop(&mut self) {
        if let Some(pb) = self.pending.take()
            && let Some(committed) = pb.commit()
        {
            self.out.push_back(StreamEvent::BlockComplete(committed));
        }
    }
}

/// Convert an Anthropic SSE stream into a pull-based `StreamEvent` stream.
///
/// Pure with respect to the outside world: no UI events, no filesystem.
/// The caller drives consumption; dropping the returned stream aborts the
/// underlying HTTP reader task (via `SseEventStream`'s drop).
fn decode_anthropic_sse(
    sse: SseEventStream,
    tools: Vec<ToolSchema>,
) -> BoxStream<'static, Result<StreamEvent>> {
    let decoder = AnthropicDecoder::new(tools);
    futures::stream::unfold(
        (sse, decoder, false),
        |(mut sse, mut decoder, mut done_emitted)| async move {
            // Drain any buffered events first.
            if let Some(evt) = decoder.out.pop_front() {
                return Some((Ok(evt), (sse, decoder, done_emitted)));
            }
            if done_emitted {
                return None;
            }
            loop {
                match sse.next().await {
                    Some(Ok(frame)) => {
                        decoder.feed(&frame.data);
                        if let Some(evt) = decoder.out.pop_front() {
                            return Some((Ok(evt), (sse, decoder, done_emitted)));
                        }
                    }
                    Some(Err(e)) => return Some((Err(e), (sse, decoder, true))),
                    None => {
                        // SSE exhausted. If we saw the terminal
                        // `message_stop` frame, emit `Done` and let the
                        // consumer treat this as a clean close. Otherwise
                        // end the stream without `Done` so the consumer
                        // can classify it as a network interruption.
                        done_emitted = true;
                        if decoder.saw_message_stop {
                            let stop = parse_stop_reason(&decoder.stop_reason_raw);
                            return Some((
                                Ok(StreamEvent::Done { stop }),
                                (sse, decoder, done_emitted),
                            ));
                        }
                        return None;
                    }
                }
            }
        },
    )
    .boxed()
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

/// Render `tool_result.content` for Anthropic wire.
///
/// `ToolResultBody::Text` → plain string (backward-compat shape used by all
/// pre-multimodal tools). `ToolResultBody::Items` → array of typed blocks so
/// the model can see images alongside text. Unresolvable image ids are
/// dropped so the request body stays valid — tool text portion is still
/// delivered, model sees a stripped-down result rather than a 400.
fn tool_result_content_anthropic(
    body: &ToolResultBody,
    resolve: &crate::core::provider::ImageResolver,
) -> serde_json::Value {
    match body {
        ToolResultBody::Text(s) => serde_json::json!(s),
        ToolResultBody::Items(items) => {
            let blocks: Vec<serde_json::Value> = items
                .iter()
                .filter_map(|item| match item {
                    ToolResultItem::Text { text } if !text.is_empty() => {
                        Some(serde_json::json!({"type": "text", "text": text}))
                    }
                    ToolResultItem::Text { .. } => None,
                    ToolResultItem::Image { media_type, id } => {
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
                })
                .collect();
            serde_json::json!(blocks)
        }
    }
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
            let content_json = tool_result_content_anthropic(content, resolve);
            let mut v = serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content_json,
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

    fn feed_frames(decoder: &mut AnthropicDecoder, frames: &[serde_json::Value]) {
        for f in frames {
            decoder.feed(f);
        }
    }

    fn drain(decoder: &mut AnthropicDecoder) -> Vec<StreamEvent> {
        decoder.out.drain(..).collect()
    }

    #[test]
    fn decoder_emits_text_delta_and_block_complete_for_text_block() {
        let mut d = AnthropicDecoder::new(vec![]);
        feed_frames(
            &mut d,
            &[
                serde_json::json!({
                    "type": "content_block_start",
                    "content_block": {"type": "text"},
                }),
                serde_json::json!({
                    "type": "content_block_delta",
                    "delta": {"type": "text_delta", "text": "hi"},
                }),
                serde_json::json!({
                    "type": "content_block_delta",
                    "delta": {"type": "text_delta", "text": " there"},
                }),
                serde_json::json!({"type": "content_block_stop"}),
            ],
        );
        let events = drain(&mut d);
        match &events[..] {
            [
                StreamEvent::TextDelta(a),
                StreamEvent::TextDelta(b),
                StreamEvent::BlockComplete(ContentBlock::Text { text }),
            ] => {
                assert_eq!(a, "hi");
                assert_eq!(b, " there");
                assert_eq!(text, "hi there");
            }
            _ => panic!("unexpected: {events:?}"),
        }
    }

    #[test]
    fn decoder_emits_tool_selected_and_input_with_extractor() {
        let tool = ToolSchema {
            name: "write".into(),
            description: String::new(),
            parameters: serde_json::json!({}),
            streamable_arg: Some("content".into()),
        };
        let mut d = AnthropicDecoder::new(vec![tool]);
        feed_frames(
            &mut d,
            &[
                serde_json::json!({
                    "type": "content_block_start",
                    "content_block": {"type": "tool_use", "id": "tc_1", "name": "write"},
                }),
                serde_json::json!({
                    "type": "content_block_delta",
                    "delta": {"type": "input_json_delta",
                              "partial_json": "{\"content\":\"hello\""},
                }),
                serde_json::json!({
                    "type": "content_block_delta",
                    "delta": {"type": "input_json_delta",
                              "partial_json": ",\"path\":\"/x\"}"},
                }),
                serde_json::json!({"type": "content_block_stop"}),
            ],
        );
        let events = drain(&mut d);
        let names: Vec<_> = events
            .iter()
            .map(|e| match e {
                StreamEvent::ToolSelected { name } => format!("selected:{name}"),
                StreamEvent::ToolInput { chunk, .. } => format!("input:{chunk}"),
                StreamEvent::BlockComplete(ContentBlock::ToolUse { name, input, .. }) => {
                    format!("done:{name}:{}", input["content"].as_str().unwrap_or(""))
                }
                _ => "other".into(),
            })
            .collect();
        assert_eq!(
            names,
            vec!["selected:write", "input:hello", "done:write:hello"]
        );
    }

    #[test]
    fn decoder_tracks_usage_and_stop_reason() {
        let mut d = AnthropicDecoder::new(vec![]);
        feed_frames(
            &mut d,
            &[
                serde_json::json!({
                    "type": "message_start",
                    "message": {"usage": {
                        "input_tokens": 10,
                        "output_tokens": 0,
                        "cache_read_input_tokens": 3,
                    }},
                }),
                serde_json::json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn"},
                    "usage": {"output_tokens": 42},
                }),
                serde_json::json!({"type": "message_stop"}),
            ],
        );
        let events = drain(&mut d);
        let usages: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::UsageUpdate(u) => Some(u.output_tokens),
                _ => None,
            })
            .collect();
        assert_eq!(usages, vec![0, 42]);
        assert!(d.saw_message_stop);
        assert_eq!(parse_stop_reason(&d.stop_reason_raw), StopReason::EndTurn);
    }

    #[test]
    fn decoder_emits_web_search_done_with_hits() {
        let mut d = AnthropicDecoder::new(vec![]);
        feed_frames(
            &mut d,
            &[serde_json::json!({
                "type": "content_block_start",
                "content_block": {
                    "type": "web_search_tool_result",
                    "content": [
                        {"title": "Rust", "url": "https://rust-lang.org"},
                        {"title": "No URL"},
                    ],
                },
            })],
        );
        match drain(&mut d).as_slice() {
            [StreamEvent::WebSearchDone { results }] => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].url, "https://rust-lang.org");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn tool_result_text_body_serializes_as_string_content() {
        use crate::core::types::{ContentBlock, Message, ToolResultBody};

        let msg = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tc_1".into(),
                content: ToolResultBody::Text("ok".into()),
                is_error: false,
                evidence_id: None,
            }],
            origin: None,
        };
        let api = to_api_messages(&[msg], &|_| String::new());
        let tr = &api[0]["content"][0];
        assert_eq!(tr["type"], "tool_result");
        // Text body MUST round-trip to a JSON string (not an array),
        // preserving wire compatibility with pre-multimodal callers.
        assert_eq!(tr["content"], serde_json::json!("ok"));
    }

    #[test]
    fn tool_result_items_body_serializes_as_block_array_with_image() {
        use crate::core::types::{ContentBlock, Message, ToolResultBody, ToolResultItem};

        let msg = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tc_1".into(),
                content: ToolResultBody::Items(vec![
                    ToolResultItem::Text {
                        text: "PNG 512x512".into(),
                    },
                    ToolResultItem::Image {
                        media_type: "image/png".into(),
                        id: "img_abc".into(),
                    },
                ]),
                is_error: false,
                evidence_id: None,
            }],
            origin: None,
        };
        let api = to_api_messages(&[msg], &|id| {
            assert_eq!(id, "img_abc");
            "BASE64DATA".into()
        });
        let content = api[0]["content"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "PNG 512x512");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "BASE64DATA");
    }

    #[test]
    fn tool_result_items_body_drops_unresolvable_images() {
        // Resolver returns empty string for unknown ids. Serializing an
        // unresolved image would produce `"data": ""` and a 400 response;
        // the adapter MUST skip it so the text portion still lands.
        use crate::core::types::{ContentBlock, Message, ToolResultBody, ToolResultItem};

        let msg = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tc_1".into(),
                content: ToolResultBody::Items(vec![
                    ToolResultItem::Text { text: "txt".into() },
                    ToolResultItem::Image {
                        media_type: "image/png".into(),
                        id: "missing".into(),
                    },
                ]),
                is_error: false,
                evidence_id: None,
            }],
            origin: None,
        };
        let api = to_api_messages(&[msg], &|_| String::new());
        let content = api[0]["content"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
    }
}
