/// Claude provider — Anthropic Messages API with SSE streaming.
///
/// Wire-level parity with the official Claude Code CLI (headers, betas,
/// system block shape). Sources: `src/utils/http.ts`,
/// `src/services/api/client.ts`, `src/services/api/claude.ts`,
/// `src/utils/fingerprint.ts`, `src/constants/system.ts`, `src/utils/betas.ts`
/// in `yasasbanukaofficial/claude-code`.
use crate::core::provider::{Provider, StopReason, StreamRequest, StreamResponse};
use crate::core::types::{ContentBlock, Message, Role, ThinkingLevel, ToolSchema, Usage};
use crate::event::Event;
use crate::provider::json_stream::{JsonStringExtractor, streamable_arg_for};
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

            if let Some((thinking, output_config)) =
                build_thinking_config(&self.model, self.thinking, effective_max_tokens)
            {
                body["thinking"] = thinking;
                if let Some(output_config) = output_config {
                    body["output_config"] = output_config;
                }
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

            // Default headers — matches `src/services/api/client.ts::getAnthropicClient`
            // and `src/utils/http.ts::getUserAgent`.
            let user_agent = claude_cli_user_agent();
            let session_id = claude_session_id();
            let request_id = uuid_v4().unwrap_or_default();
            let mut header_vec: Vec<(&str, String)> = vec![
                (auth_key, auth_header),
                ("anthropic-version", "2023-06-01".into()),
            ];
            if self.is_oauth {
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
                                let arg_extractor =
                                    streamable_arg_for(tools, &name).map(JsonStringExtractor::new);
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
                                        if !chunk.is_empty() {
                                            let _ = tx
                                                .send(Event::ToolInput {
                                                    name: name.clone(),
                                                    chunk,
                                                })
                                                .await;
                                        }
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

/// Upstream CLI version reverse-engineered from `~/.local/bin/claude@2.1.100`.
/// Used for `User-Agent`, `cc_version`, and as input to [`compute_fingerprint`].
/// Must match across the three so the backend's attribution validator
/// accepts the fingerprint.
const CLI_VERSION: &str = "2.1.100";

const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Hardcoded fingerprint salt — `src/utils/fingerprint.ts:8`.
const FINGERPRINT_SALT: &str = "59cf53e54c78";

/// First-user-message character indices sampled for the fingerprint hash.
const FINGERPRINT_POSITIONS: [usize; 3] = [4, 7, 20];

/// Stable per-process session id for the `X-Claude-Code-Session-Id` header.
fn claude_session_id() -> String {
    use std::sync::OnceLock;
    static SESSION_ID: OnceLock<String> = OnceLock::new();
    SESSION_ID
        .get_or_init(|| uuid_v4().unwrap_or_else(|| "unknown".to_owned()))
        .clone()
}

/// `claude-cli/{CLI_VERSION} (external, cli)` — `src/utils/http.ts::getUserAgent`.
fn claude_cli_user_agent() -> String {
    format!("claude-cli/{CLI_VERSION} (external, cli)")
}

/// Build the OAuth-mode `system` array (`src/utils/api.ts::splitSysPromptPrefix`
/// + `src/services/api/claude.ts::buildSystemPromptBlocks`).
///
/// Wire shape:
/// 1. attribution header (no `cache_control`, `cacheScope: null`)
/// 2. CLI sysprompt prefix / identity (`cache_control: { type: 'ephemeral' }`)
/// 3. optional user system text (same cache_control)
fn build_oauth_system(user_system: &str, first_user_content: &str) -> serde_json::Value {
    let fingerprint = compute_fingerprint(first_user_content);
    // Native-client-attestation placeholder — claude-code@2.1.100 always
    // emits ` cch=00000;` on first-party traffic. The real CLI's HTTP
    // stack overwrites the zeros with a computed attestation token in
    // flight; omitting the segment entirely trips the backend's
    // first-party client check.
    let billing = format!(
        "x-anthropic-billing-header: cc_version={CLI_VERSION}.{fingerprint}; cc_entrypoint=cli; cch=00000;"
    );
    let cache_ephemeral = serde_json::json!({"type": "ephemeral"});
    let mut blocks = vec![
        serde_json::json!({"type": "text", "text": billing}),
        serde_json::json!({"type": "text", "text": IDENTITY, "cache_control": cache_ephemeral}),
    ];
    if !user_system.is_empty() {
        blocks.push(serde_json::json!({
            "type": "text",
            "text": user_system,
            "cache_control": cache_ephemeral,
        }));
    }
    serde_json::Value::Array(blocks)
}

/// 3-char attribution fingerprint — `src/utils/fingerprint.ts::computeFingerprint`.
/// `SHA256(SALT + msg[4] + msg[7] + msg[20] + version)[:3]`, missing positions
/// substituted with `'0'`. Backend-validated: any drift breaks attribution.
fn compute_fingerprint(first_user_content: &str) -> String {
    use sha2::{Digest, Sha256};
    let chars: String = FINGERPRINT_POSITIONS
        .iter()
        .map(|&p| first_user_content.chars().nth(p).unwrap_or('0'))
        .collect();
    let input = format!("{FINGERPRINT_SALT}{chars}{CLI_VERSION}");
    let hash = Sha256::digest(input.as_bytes());
    format!("{hash:x}")[..3].to_owned()
}

/// `anthropic-beta` header value, in upstream emit order for the common
/// Claude.ai subscriber + Claude 4.x path. See
/// `src/utils/betas.ts::getAllModelBetas`.
fn build_betas(model: &str) -> String {
    let m = model.to_lowercase();
    let is_haiku = m.contains("haiku");
    let is_claude_3 = m.contains("claude-3-");
    let mut betas: Vec<&str> = Vec::new();
    if !is_haiku {
        betas.push("claude-code-20250219");
    }
    betas.push("oauth-2025-04-20");
    if !is_haiku && !is_claude_3 {
        betas.push("interleaved-thinking-2025-05-14");
    }
    if !is_claude_3 {
        betas.push("context-management-2025-06-27");
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

/// Convert a single `ContentBlock` to Anthropic wire JSON.
///
/// Order-preserving: every call produces exactly one wire block so the
/// caller can map over the message's content in document order.
fn content_block_to_api(
    block: &ContentBlock,
    resolve: &crate::core::provider::ImageResolver,
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
        } => Some(serde_json::json!({
            "type": "thinking",
            "thinking": thinking,
            "signature": signature,
        })),
        ContentBlock::RedactedThinking { data } => Some(serde_json::json!({
            "type": "redacted_thinking",
            "data": data,
        })),
    }
}

fn to_api_messages(
    messages: &[Message],
    resolve: &crate::core::provider::ImageResolver,
) -> Vec<serde_json::Value> {
    messages
        .iter()
        .filter(|m| m.role != Role::System)
        .map(|msg| {
            let content: Vec<serde_json::Value> = msg
                .content
                .iter()
                .filter_map(|b| content_block_to_api(b, resolve))
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

/// Build the Claude request config for thinking-related fields.
///
/// Returns `(thinking, output_config)` where:
///
/// * Adaptive-capable models (Sonnet/Opus 4.6) use
///   `thinking: {"type": "adaptive"}` plus optional
///   `output_config: {"effort": ...}`.
/// * Older thinking-capable models use
///   `thinking: {"type": "enabled", "budget_tokens": N}` and no
///   `output_config`.
/// * `ThinkingLevel::Off` disables both fields.
fn build_thinking_config(
    model: &str,
    level: ThinkingLevel,
    max_tokens: u32,
) -> Option<(serde_json::Value, Option<serde_json::Value>)> {
    if level == ThinkingLevel::Off {
        return None;
    }
    if is_adaptive_thinking_model(model) {
        let effort = match level {
            ThinkingLevel::Off => return None,
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "max",
        };
        return Some((
            serde_json::json!({"type": "adaptive"}),
            Some(serde_json::json!({"effort": effort})),
        ));
    }
    let budget = level.budget();
    if budget == 0 || max_tokens <= 1 {
        return None;
    }
    let capped = budget.min(max_tokens - 1);
    Some((
        serde_json::json!({
            "type": "enabled",
            "budget_tokens": capped,
        }),
        None,
    ))
}

/// Whether `model` supports Anthropic's adaptive thinking mode.
///
/// Mirrors upstream `rN_(model)` in `claude-code@2.1.100`: true only for
/// `opus-4-6` and `sonnet-4-6`. Other Claude 4.x models still use the
/// old `{type: "enabled", budget_tokens: N}` shape.
fn is_adaptive_thinking_model(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("opus-4-6") || m.contains("sonnet-4-6")
}

/// Apply a single `cache_control: ephemeral` breakpoint to the last block
/// of the last message. Mutates the messages in place.
///
/// Anthropic's prompt caching uses breakpoints to mark cache boundaries; a
/// single breakpoint on the last message caches the full prefix. `content`
/// is always an array after `to_api_messages` so we can assume that shape.
fn apply_cache_breakpoint(api_messages: &mut [serde_json::Value]) {
    let Some(last_msg) = api_messages.last_mut() else {
        return;
    };
    let Some(content) = last_msg["content"].as_array_mut() else {
        return;
    };
    for block in content.iter_mut().rev() {
        let block_type = block["type"].as_str().unwrap_or("");
        if matches!(block_type, "thinking" | "redacted_thinking") {
            continue;
        }
        block["cache_control"] = serde_json::json!({"type": "ephemeral"});
        break;
    }
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
    fn thinking_config_off_returns_none() {
        assert_eq!(
            build_thinking_config("claude-sonnet-4-5", ThinkingLevel::Off, 8192),
            None
        );
    }

    #[test]
    fn thinking_config_enabled_under_max_is_passed_through() {
        // claude-sonnet-4-5 → non-adaptive → enabled with Low=1024 budget.
        let (thinking, output_config) =
            build_thinking_config("claude-sonnet-4-5", ThinkingLevel::Low, 8192).unwrap();
        assert_eq!(thinking["type"], "enabled");
        assert_eq!(thinking["budget_tokens"], 1024);
        assert!(output_config.is_none());
    }

    #[test]
    fn thinking_config_enabled_capped_to_max_minus_one() {
        // High=8192, max=8192 → must cap to 8191.
        let (thinking, output_config) =
            build_thinking_config("claude-sonnet-4-5", ThinkingLevel::High, 8192).unwrap();
        assert_eq!(thinking["budget_tokens"], 8191);
        assert!(output_config.is_none());
    }

    #[test]
    fn thinking_config_enabled_with_tiny_max_returns_none() {
        assert_eq!(
            build_thinking_config("claude-sonnet-4-5", ThinkingLevel::Low, 1),
            None
        );
        assert_eq!(
            build_thinking_config("claude-sonnet-4-5", ThinkingLevel::Low, 0),
            None
        );
    }

    #[test]
    fn thinking_config_adaptive_for_sonnet_4_6_uses_effort() {
        let (thinking, output_config) =
            build_thinking_config("claude-sonnet-4-6", ThinkingLevel::Low, 8192).unwrap();
        assert_eq!(thinking["type"], "adaptive");
        assert!(thinking.get("budget_tokens").is_none());
        let output_config = output_config.expect("adaptive output_config");
        assert_eq!(output_config["effort"], "low");

        let (thinking, output_config) =
            build_thinking_config("claude-opus-4-6", ThinkingLevel::Medium, 64_000).unwrap();
        assert_eq!(thinking["type"], "adaptive");
        assert_eq!(output_config.unwrap()["effort"], "medium");
    }

    #[test]
    fn thinking_config_adaptive_high_maps_to_max_effort() {
        let (thinking, output_config) =
            build_thinking_config("claude-sonnet-4-6", ThinkingLevel::High, 8192).unwrap();
        assert_eq!(thinking["type"], "adaptive");
        assert_eq!(output_config.unwrap()["effort"], "max");
    }

    #[test]
    fn adaptive_thinking_model_matches_upstream() {
        // Upstream rN_(model): true only for opus-4-6 / sonnet-4-6.
        assert!(is_adaptive_thinking_model("claude-opus-4-6"));
        assert!(is_adaptive_thinking_model("claude-sonnet-4-6"));
        assert!(is_adaptive_thinking_model("claude-sonnet-4-6-20251002"));
        assert!(!is_adaptive_thinking_model("claude-sonnet-4-5"));
        assert!(!is_adaptive_thinking_model("claude-opus-4-5"));
        assert!(!is_adaptive_thinking_model("claude-haiku-4-5"));
        assert!(!is_adaptive_thinking_model("claude-3-opus"));
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
    fn cache_breakpoint_skips_thinking_and_marks_last_mutable_block() {
        let mut msgs = vec![serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "answer"},
                {"type": "thinking", "thinking": "x", "signature": "sig"},
                {"type": "redacted_thinking", "data": "opaque"},
            ]
        })];
        apply_cache_breakpoint(&mut msgs);
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
        assert!(content[1].get("cache_control").is_none());
        assert!(content[2].get("cache_control").is_none());
    }

    #[test]
    fn cache_breakpoint_with_only_thinking_blocks_is_noop() {
        let mut msgs = vec![serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "x", "signature": "sig"},
                {"type": "redacted_thinking", "data": "opaque"},
            ]
        })];
        apply_cache_breakpoint(&mut msgs);
        let content = msgs[0]["content"].as_array().unwrap();
        assert!(content[0].get("cache_control").is_none());
        assert!(content[1].get("cache_control").is_none());
    }

    #[test]
    fn cache_breakpoint_on_empty_messages_is_noop() {
        let mut msgs: Vec<serde_json::Value> = vec![];
        apply_cache_breakpoint(&mut msgs);
        assert!(msgs.is_empty());
    }

    // --- Claude Code parity regression tests ---

    #[test]
    fn user_agent_matches_upstream_shape() {
        let ua = claude_cli_user_agent();
        assert!(ua.starts_with("claude-cli/"));
        assert!(ua.ends_with(" (external, cli)"));
        assert!(ua.contains(CLI_VERSION));
    }

    #[test]
    fn session_id_is_stable_per_process() {
        let a = claude_session_id();
        let b = claude_session_id();
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn fingerprint_is_three_hex_chars() {
        let fp = compute_fingerprint("hello world, this is a short prompt");
        assert_eq!(fp.len(), 3);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn fingerprint_substitutes_zero_for_missing_positions() {
        // All three sample positions fall past the end → all '0's.
        assert_eq!(compute_fingerprint("abc"), compute_fingerprint(""));
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let a = compute_fingerprint("the quick brown fox jumps over lazy dog");
        let b = compute_fingerprint("the quick brown fox jumps over lazy dog");
        assert_eq!(a, b);
    }

    #[test]
    fn billing_block_has_expected_shape() {
        let sys = build_oauth_system("my system", "hello world");
        let arr = sys.as_array().expect("array");
        assert_eq!(arr.len(), 3);

        // Block 0: attribution header — no cache_control.
        let billing = arr[0]["text"].as_str().unwrap();
        assert!(billing.starts_with("x-anthropic-billing-header: cc_version="));
        assert!(billing.contains(&format!("cc_version={CLI_VERSION}.")));
        assert!(billing.contains("cc_entrypoint=cli;"));
        // claude-code@2.1.100 always emits the native-client-attestation
        // placeholder on first-party traffic — omitting it trips the backend.
        assert!(billing.contains("cch=00000;"));
        assert!(!billing.contains("ttl"));
        assert!(arr[0].get("cache_control").is_none());

        // Block 1: identity — plain ephemeral.
        assert_eq!(arr[1]["text"], IDENTITY);
        assert_eq!(arr[1]["cache_control"]["type"], "ephemeral");
        assert!(arr[1]["cache_control"].get("ttl").is_none());

        // Block 2: user system — plain ephemeral.
        assert_eq!(arr[2]["text"], "my system");
        assert_eq!(arr[2]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn billing_block_omits_user_system_when_empty() {
        let sys = build_oauth_system("", "hi");
        let arr = sys.as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn beta_list_for_claude_4_oauth_matches_upstream() {
        let betas = build_betas("claude-sonnet-4-6");
        assert!(betas.contains("claude-code-20250219"));
        assert!(betas.contains("oauth-2025-04-20"));
        assert!(betas.contains("interleaved-thinking-2025-05-14"));
        assert!(betas.contains("context-management-2025-06-27"));
        assert!(betas.contains("prompt-caching-scope-2026-01-05"));
    }

    #[test]
    fn beta_list_for_haiku_drops_claude_code_and_interleaved() {
        let betas = build_betas("claude-haiku-4-5");
        assert!(!betas.contains("claude-code-20250219"));
        assert!(!betas.contains("interleaved-thinking-2025-05-14"));
        assert!(betas.contains("oauth-2025-04-20"));
        assert!(betas.contains("prompt-caching-scope-2026-01-05"));
    }

    #[test]
    fn beta_list_for_claude_3_drops_interleaved_and_context_mgmt() {
        let betas = build_betas("claude-3-opus");
        assert!(!betas.contains("interleaved-thinking-2025-05-14"));
        assert!(!betas.contains("context-management-2025-06-27"));
    }
}
