//! Kiro (Amazon Q) protocol — AWS Event Stream binary framing.
//!
//! Endpoint: POST /generateAssistantResponse?origin=KIRO_CLI&profileArn=<arn>
//! Response: chunked AWS Event Stream frames. Each frame carries a JSON
//! payload plus an `:event-type` header. Relevant event types:
//!
//! * `assistantResponseEvent`: `{content: "<chunk>", modelId: ...}`
//! * `toolUseEvent`: `{name, toolUseId, input?, stop?}` — `input` is a
//!   raw argument-JSON fragment, concatenated across events and parsed
//!   at stop. A `stop: true` frame terminates the tool call.
//! * `meteringEvent`: credit usage (not token usage); ignored here.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
use serde_json::json;
use std::collections::BTreeMap;

use crate::core::provider::{Provider, StopReason, StreamRequest, StreamResponse};
use crate::core::types::{
    ContentBlock, Message, Role, ThinkingLevel, ToolResultBody, ToolResultItem, Usage,
};
use crate::event::Event;
use crate::event_bus::Sender as EventSender;
use crate::provider::json_stream::{JsonStringExtractor, streamable_arg_for};
use crate::provider::retry::ProviderUnauthorized;
use crate::provider::stream_io::next_chunk_or_cancel;
use crate::util::uuid_v4;

/// User-Agent Kiro CLI sends. Captured via mitmproxy from a real
/// kiro-cli session — the backend enforces a first-party-client
/// check against this prefix, so drift will start dropping requests.
const KIRO_USER_AGENT: &str = "aws-sdk-rust/1.3.14 ua/2.1 api/codewhispererstreaming/0.1.14474 os/macos lang/rust/1.92.0 app/AmazonQ-For-CLI";

pub struct KiroRuntime {
    model_id: String,
    base_url: String,
    token: String,
    profile_arn: Option<String>,
    /// Stable identifiers derived from the app session. Live on the
    /// runtime so every turn in the same session routes to the same
    /// server-side conversation — useful for Kiro portal observability
    /// even though the backend does not key its prompt cache on them.
    conversation_id: String,
    continuation_id: String,
}

impl KiroRuntime {
    /// Create from model, gateway base URL, credential token, optional
    /// profile ARN, and the app session ID. `base_url` is the gateway's
    /// scheme+host with no trailing slash; the runtime appends
    /// `/generateAssistantResponse`. `session_id` is any stable token
    /// (app session UUID); two UUIDs are derived from it per turn so
    /// server-side session logs group by one conversation.
    pub fn new(
        model_id: &str,
        base_url: &str,
        token: &str,
        profile_arn: Option<String>,
        session_id: &str,
    ) -> Self {
        let (conversation_id, continuation_id) = derive_session_uuids(session_id);
        Self {
            model_id: model_id.to_owned(),
            base_url: base_url.to_owned(),
            token: token.to_owned(),
            profile_arn,
            conversation_id,
            continuation_id,
        }
    }
}

impl Provider for KiroRuntime {
    fn name(&self) -> &str {
        "kiro"
    }

    fn set_thinking(&mut self, _level: ThinkingLevel) {}

    fn supports_max_tokens_override(&self) -> bool {
        false
    }

    fn tool_result_image_routing(&self) -> crate::core::provider::ToolResultImageRouting {
        crate::core::provider::ToolResultImageRouting::UserAttachment
    }
    fn stream<'a>(
        &'a self,
        req: StreamRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<StreamResponse>> + Send + 'a>> {
        Box::pin(async move { self.run(req).await })
    }
}

impl KiroRuntime {
    async fn run(&self, req: StreamRequest<'_>) -> Result<StreamResponse> {
        let profile_arn = self.profile_arn.as_deref().unwrap_or("");

        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| anyhow::anyhow!("Kiro client build: {e}"))?;

        // Kiro CLI uses the AWS Smithy "service" endpoint: root path, target
        // in `X-Amz-Target`, `application/x-amz-json-1.0` envelope. This
        // dispatches to AmazonCodeWhispererStreamingService and emits the
        // richer event-stream response that includes contextUsageEvent —
        // the legacy `/generateAssistantResponse` path only surfaces
        // assistantResponseEvent + meteringEvent.
        let body = build_request_body(
            req.messages,
            &self.model_id,
            profile_arn,
            req.tools,
            &self.conversation_id,
            &self.continuation_id,
            req.resolve_image,
        );

        let resp = client
            .post(&self.base_url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/x-amz-json-1.0")
            .header(
                "X-Amz-Target",
                "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
            )
            .header("User-Agent", KIRO_USER_AGENT)
            .header("X-Amz-User-Agent", KIRO_USER_AGENT)
            .header("X-Amzn-Codewhisperer-Optout", "false")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let snippet: String = text.chars().take(300).collect();
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(ProviderUnauthorized {
                    provider: "kiro".to_owned(),
                    status: status.as_u16(),
                    detail: if snippet.is_empty() {
                        "unauthorized".to_owned()
                    } else {
                        snippet
                    },
                }
                .into());
            }

            // Wrap as non-retryable so stream_with_retry doesn't loop.
            return Err(anyhow::anyhow!("Kiro HTTP {status}: {snippet}"));
        }

        // AWS Event Stream arrives over chunked transfer encoding. Decode
        // frames as they land so the UI sees real-time tokens instead of
        // a one-shot dump at end-of-body.
        let mut decoder = FrameDecoder::new(req.tools);
        let mut byte_stream = resp.bytes_stream();
        let chunk_timeout = std::time::Duration::from_secs(120);
        loop {
            let chunk_result = tokio::select! {
                c = next_chunk_or_cancel(&mut byte_stream, &req.cancel) => c,
                _ = tokio::time::sleep(chunk_timeout) => {
                    return Err(crate::provider::sse::StreamInterrupted(
                        "Kiro stream idle timeout — no data for 120s".into(),
                    ).into());
                }
            };
            let Some(chunk) = chunk_result.map_err(|e| anyhow::anyhow!("Kiro read error: {e}"))?
            else {
                break;
            };
            decoder.feed(&chunk);
            while let Some(frame) = decoder.pop_frame() {
                decoder
                    .handle_frame(&frame, &req.tx, &req.tool_use_tx)
                    .await;
            }
        }

        let mut response = decoder.finish();

        // Kiro server emits contextUsageEvent only on large-enough prompts.
        // When absent, estimate client-side using the same algorithm as
        // Kiro CLI: chars/4, rounded to nearest 10.
        if !response.context_usage_emitted {
            let est_chars = crate::provider::estimate_context_chars(req.messages, req.tools);
            let ctx_window = crate::config::models::context_window(&self.model_id);
            let est_tokens = ((est_chars / 4 + 5) / 10 * 10) as u64;
            let pct = ((est_tokens as f64 / ctx_window as f64) * 100.0).clamp(0.0, 100.0) as f32;
            req.tx.send_or_log(Event::ContextUsage(pct)).await;
            response.context_usage_emitted = true;
        }

        Ok(response)
    }
}

// =============================================================================
// Request builder
// =============================================================================

/// Derive two stable UUIDs from an app session ID. The app session ID is
/// itself a UUID, so reuse it verbatim for the conversation. The
/// continuation ID is a deterministic transform so both IDs are stable
/// across turns in the same session but never collide.
fn derive_session_uuids(session_id: &str) -> (String, String) {
    let fallback = "00000000-0000-4000-8000-000000000000".to_owned();
    let conversation_id = if is_uuid_shape(session_id) {
        session_id.to_owned()
    } else {
        uuid_v4().unwrap_or_else(|| fallback.clone())
    };
    // Continuation: rotate the last hex char of the trailing group so the
    // UUID stays valid but differs from conversation_id. If even that
    // trivial transform fails (string too short), fall back to a fresh v4.
    let continuation_id =
        rotate_last_hex(&conversation_id).unwrap_or_else(|| uuid_v4().unwrap_or(fallback));
    (conversation_id, continuation_id)
}

fn is_uuid_shape(s: &str) -> bool {
    // Minimal gate — Kiro rejects non-hex-and-dash cids with 400
    // "Improperly formed request." Don't ship a corrupt id to the wire.
    s.len() == 36
        && s.as_bytes()
            .iter()
            .all(|b| b.is_ascii_hexdigit() || *b == b'-')
}

fn rotate_last_hex(uuid: &str) -> Option<String> {
    let mut chars: Vec<char> = uuid.chars().collect();
    let last = chars.last_mut()?;
    let next = match *last {
        '0' => '1',
        '1' => '2',
        '2' => '3',
        '3' => '4',
        '4' => '5',
        '5' => '6',
        '6' => '7',
        '7' => '8',
        '8' => '9',
        '9' => 'a',
        'a' => 'b',
        'b' => 'c',
        'c' => 'd',
        'd' => 'e',
        'e' => 'f',
        'f' => '0',
        _ => return None,
    };
    *last = next;
    Some(chars.into_iter().collect())
}

fn build_request_body(
    messages: &[Message],
    model_id: &str,
    profile_arn: &str,
    tools: &[crate::core::types::ToolSchema],
    conversation_id: &str,
    continuation_id: &str,
    resolve: &crate::core::provider::ImageResolver,
) -> serde_json::Value {
    if messages.is_empty() {
        return json!({});
    }

    let (history_msgs, current_msg) = (
        &messages[..messages.len() - 1],
        &messages[messages.len() - 1],
    );

    let tool_specs = build_tool_specs(tools);
    let history = build_history(history_msgs, model_id, resolve);
    let current = build_current_message(current_msg, model_id, &tool_specs, resolve);

    json!({
        "conversationState": {
            "chatTriggerType": "MANUAL",
            "conversationId": conversation_id,
            "agentContinuationId": continuation_id,
            "agentTaskType": "vibe",
            "history": history,
            "currentMessage": current,
        },
        "profileArn": profile_arn,
    })
}

fn build_tool_specs(tools: &[crate::core::types::ToolSchema]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "toolSpecification": {
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": { "json": t.parameters }
                }
            })
        })
        .collect()
}

fn msg_text(msg: &Message) -> String {
    Message::content_text(&msg.content)
}

fn msg_images(
    msg: &Message,
    resolve: &crate::core::provider::ImageResolver,
) -> Vec<serde_json::Value> {
    msg.content
        .iter()
        .filter_map(|block| {
            let ContentBlock::Image { media_type, id } = block else {
                return None;
            };
            let data = resolve(id);
            if data.is_empty() {
                return None;
            }
            let format = match media_type.as_str() {
                "image/gif" => "gif",
                "image/jpeg" | "image/jpg" => "jpeg",
                "image/png" => "png",
                "image/webp" => "webp",
                _ => return None,
            };
            Some(json!({
                "format": format,
                "source": {
                    "bytes": data,
                }
            }))
        })
        .collect()
}

/// Kiro/Q Developer API has no `system` role. System messages are injected as
/// a synthetic user→assistant pair at the start of history, matching the
/// approach used by the official Q Developer CLI (`context_messages`).
const SYSTEM_ACK: &str = "I will fully incorporate this information when \
    generating my responses, and explicitly acknowledge relevant parts when \
    answering questions.";

fn build_history(
    messages: &[Message],
    model_id: &str,
    resolve: &crate::core::provider::ImageResolver,
) -> Vec<serde_json::Value> {
    let env_state = build_env_state();
    let mut result = Vec::new();
    for msg in messages {
        match msg.role {
            Role::System => {
                let content = msg_text(msg);
                if !content.is_empty() {
                    result.push(build_user_input_message(
                        &content,
                        model_id,
                        &env_state,
                        None,
                        None,
                        msg,
                        resolve,
                    ));
                    result.push(
                        json!({ "assistantResponseMessage": { "content": SYSTEM_ACK } }),
                    );
                }
            }
            Role::User => {
                let tool_results = extract_tool_results(msg);
                let content = if tool_results.is_empty() {
                    msg_text(msg)
                } else {
                    String::new()
                };
                result.push(build_user_input_message(
                    &content,
                    model_id,
                    &env_state,
                    None,
                    (!tool_results.is_empty()).then_some(tool_results.as_slice()),
                    msg,
                    resolve,
                ));
            }
            Role::Assistant => {
                let content = msg_text(msg);
                let tool_uses = extract_tool_uses(msg);
                let mut assistant_msg = json!({ "content": content });
                if !tool_uses.is_empty() {
                    assistant_msg["toolUses"] = json!(tool_uses);
                }
                result.push(json!({ "assistantResponseMessage": assistant_msg }));
            }
        }
    }
    result
}

fn build_current_message(
    msg: &Message,
    model_id: &str,
    tool_specs: &[serde_json::Value],
    resolve: &crate::core::provider::ImageResolver,
) -> serde_json::Value {
    let env_state = build_env_state();
    let tool_results = extract_tool_results(msg);
    let content = if tool_results.is_empty() {
        msg_text(msg)
    } else {
        String::new()
    };
    build_user_input_message(
        &content,
        model_id,
        &env_state,
        Some(tool_specs),
        (!tool_results.is_empty()).then_some(tool_results.as_slice()),
        msg,
        resolve,
    )
}

fn build_user_input_message(
    content: &str,
    model_id: &str,
    env_state: &serde_json::Value,
    tools: Option<&[serde_json::Value]>,
    tool_results: Option<&[serde_json::Value]>,
    msg: &Message,
    resolve: &crate::core::provider::ImageResolver,
) -> serde_json::Value {
    let mut ctx = serde_json::json!({
        "envState": env_state.clone(),
    });
    if let Some(tools) = tools {
        ctx["tools"] = json!(tools);
    }
    if let Some(tool_results) = tool_results {
        ctx["toolResults"] = json!(tool_results);
    }

    let mut user_msg = json!({
        "userInputMessage": {
            "content": content,
            "origin": "KIRO_CLI",
            "modelId": model_id,
            "userInputMessageContext": ctx,
        }
    });
    let images = msg_images(msg, resolve);
    if !images.is_empty() {
        user_msg["userInputMessage"]["images"] = json!(images);
    }
    user_msg
}

/// `envState` payload Q Developer CLI ships on every `userInputMessage`.
/// Captured via mitmproxy: just `{operatingSystem, currentWorkingDirectory}`.
/// Resolved per call so the server sees the current shell cwd if the user
/// `cd`s mid-session; a stable string (not a random fallback) keeps the
/// request bytes identical across turns at the same cwd for prompt cache.
fn build_env_state() -> serde_json::Value {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    };
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_default();
    json!({
        "operatingSystem": os,
        "currentWorkingDirectory": cwd,
    })
}

fn extract_tool_uses(msg: &Message) -> Vec<serde_json::Value> {
    msg.content
        .iter()
        .filter_map(|b| {
            if let ContentBlock::ToolUse { id, name, input } = b {
                Some(json!({ "toolUseId": id, "name": name, "input": input }))
            } else {
                None
            }
        })
        .collect()
}

fn extract_tool_results(msg: &Message) -> Vec<serde_json::Value> {
    msg.content
        .iter()
        .filter_map(|b| {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } = b
            {
                let text = match content {
                    ToolResultBody::Text(t) => t.clone(),
                    ToolResultBody::Items(items) => items
                        .iter()
                        .filter_map(|i| {
                            if let ToolResultItem::Text { text } = i {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                Some(json!({
                    "toolUseId": tool_use_id,
                    "content": [{"text": text}],
                    "status": "success",
                }))
            } else {
                None
            }
        })
        .collect()
}

// =============================================================================
// AWS Event Stream decoder
// =============================================================================

// =============================================================================
// AWS Event Stream decoder — incremental
// =============================================================================

/// Per-tool-call accumulator. Mirrors `openai_responses::PendingTool`.
struct PendingTool {
    name: String,
    arguments: String,
    /// Incremental extractor for the streamable arg (Write `content`, etc.)
    /// — `None` when the tool opts out.
    arg_extractor: Option<JsonStringExtractor>,
}

/// One decoded AWS Event Stream frame.
struct Frame {
    event_type: Option<String>,
    payload: Vec<u8>,
}

/// Incremental AWS Event Stream frame decoder.
///
/// `feed` appends bytes from the reqwest byte stream; `pop_frame` returns
/// the next complete frame (or `None` if the buffer is short). `handle_frame`
/// interprets one frame — forwarding UI events and updating the final
/// `StreamResponse` state. `finish` materialises that state when the body
/// ends.
struct FrameDecoder<'a> {
    tools: &'a [crate::core::types::ToolSchema],
    buf: Vec<u8>,
    text: String,
    /// Ordered by first-seen tool_use_id so concurrent tool calls keep
    /// their relative order in the final message.
    tool_uses: BTreeMap<String, PendingTool>,
    tool_order: Vec<String>,
    stop_reason: StopReason,
    /// Server emitted contextUsageEvent — skip client-side fallback.
    server_context_usage: bool,
}

impl<'a> FrameDecoder<'a> {
    fn new(tools: &'a [crate::core::types::ToolSchema]) -> Self {
        Self {
            tools,
            buf: Vec::new(),
            text: String::new(),
            tool_uses: BTreeMap::new(),
            tool_order: Vec::new(),
            stop_reason: StopReason::EndTurn,
            server_context_usage: false,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    fn pop_frame(&mut self) -> Option<Frame> {
        loop {
            if self.buf.len() < 12 {
                return None;
            }
            // Bounds checked above: next 4 bytes exist.
            let total_len =
                u32::from_be_bytes(self.buf[0..4].try_into().expect("4-byte slice")) as usize;
            if total_len == 0 || self.buf.len() < total_len {
                return None;
            }
            let headers_len =
                u32::from_be_bytes(self.buf[4..8].try_into().expect("4-byte slice")) as usize;
            let headers_end = 12 + headers_len;
            let payload_end = total_len.saturating_sub(4);
            if headers_end > payload_end || payload_end > total_len {
                // Corrupt frame — skip it and try the next one. Return
                // `None` only when the buffer genuinely has no more
                // complete frames, so the outer loop doesn't stall.
                self.buf.drain(..total_len);
                continue;
            }
            let event_type = parse_event_type(&self.buf[12..headers_end]);
            let payload = self.buf[headers_end..payload_end].to_vec();
            self.buf.drain(..total_len);
            return Some(Frame {
                event_type,
                payload,
            });
        }
    }

    async fn handle_frame(
        &mut self,
        frame: &Frame,
        tx: &EventSender,
        tool_use_tx: &Option<tokio::sync::mpsc::Sender<ContentBlock>>,
    ) {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&frame.payload) else {
            return;
        };
        match frame.event_type.as_deref() {
            Some("assistantResponseEvent") => {
                if let Some(chunk) = v.get("content").and_then(|c| c.as_str()) {
                    self.text.push_str(chunk);
                    tx.send_or_log(Event::Token(chunk.to_owned())).await;
                }
            }
            Some("toolUseEvent") => {
                let tool_use_id = v
                    .get("toolUseId")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_owned();
                if tool_use_id.is_empty() {
                    return;
                }
                let name = v
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_owned();
                let is_stop = v.get("stop").and_then(|s| s.as_bool()).unwrap_or(false);
                let input_chunk = v.get("input").and_then(|i| i.as_str()).unwrap_or("");

                let is_new = !self.tool_uses.contains_key(&tool_use_id);
                if is_new {
                    let arg_extractor = if name.is_empty() {
                        None
                    } else {
                        streamable_arg_for(self.tools, &name).map(JsonStringExtractor::new)
                    };
                    self.tool_uses.insert(
                        tool_use_id.clone(),
                        PendingTool {
                            name: name.clone(),
                            arguments: String::new(),
                            arg_extractor,
                        },
                    );
                    self.tool_order.push(tool_use_id.clone());
                    if !name.is_empty() {
                        tx.send_or_log(Event::ToolSelected { name: name.clone() })
                            .await;
                    }
                }

                if !input_chunk.is_empty()
                    && let Some(entry) = self.tool_uses.get_mut(&tool_use_id)
                {
                    entry.arguments.push_str(input_chunk);
                    let tool_name = entry.name.clone();
                    if let Some(ex) = entry.arg_extractor.as_mut() {
                        let extracted = ex.feed(input_chunk);
                        if !extracted.is_empty() {
                            tx.send_or_log(Event::ToolInput {
                                name: tool_name,
                                chunk: extracted,
                            })
                            .await;
                        }
                    }
                }

                if is_stop {
                    self.stop_reason = StopReason::ToolUse;
                    // Emit completed tool_use block mid-stream for early execution.
                    if let Some(tu_tx) = tool_use_tx
                        && let Some(entry) = self.tool_uses.get(&tool_use_id)
                    {
                        let input = crate::provider::json_stream::finalize_tool_input(
                            &entry.arguments,
                            &format!("{tool_use_id} ({})", entry.name),
                        );
                        let _ = tu_tx
                            .send(ContentBlock::ToolUse {
                                id: tool_use_id,
                                name: entry.name.clone(),
                                input,
                            })
                            .await;
                    }
                }
            }
            // meteringEvent, and anything else: ignore. Kiro reports
            // usage in credits (float), not tokens — Usage::default()
            // stays zeroed. contextUsageEvent carries the authoritative
            // server-side context percentage (input tokens / model limit).
            Some("contextUsageEvent") => {
                if let Some(pct) = v.get("contextUsagePercentage").and_then(|p| p.as_f64()) {
                    // Server value occasionally drifts slightly above 100
                    // on saturation; clamp for the UI.
                    let clamped = pct.clamp(0.0, 100.0) as f32;
                    self.server_context_usage = true;
                    crate::dbg_log!("kiro contextUsageEvent pct={clamped}");
                    tx.send_or_log(Event::ContextUsage(clamped)).await;
                } else {
                    crate::dbg_log!("kiro contextUsageEvent missing pct: {v}");
                }
            }
            _ => {}
        }
    }

    fn finish(self) -> StreamResponse {
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(ContentBlock::Text { text: self.text });
        }
        for id in self.tool_order {
            let Some(tool) = self.tool_uses.get(&id) else {
                continue;
            };
            let input = crate::provider::json_stream::finalize_tool_input(
                &tool.arguments,
                &format!("{id} ({})", tool.name),
            );
            content.push(ContentBlock::ToolUse {
                id,
                name: tool.name.clone(),
                input,
            });
        }
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content,
                origin: None,
            },
            usage: Usage::default(),
            stop_reason: self.stop_reason,
            context_usage_emitted: self.server_context_usage,
        }
    }
}

fn parse_event_type(headers_data: &[u8]) -> Option<String> {
    let mut i = 0;
    while i < headers_data.len() {
        let name_len = headers_data[i] as usize;
        i += 1;
        if i + name_len > headers_data.len() {
            break;
        }
        let name = std::str::from_utf8(&headers_data[i..i + name_len]).ok()?;
        i += name_len;
        if i >= headers_data.len() {
            break;
        }
        let _val_type = headers_data[i];
        i += 1;
        if i + 2 > headers_data.len() {
            break;
        }
        let val_len = u16::from_be_bytes(headers_data[i..i + 2].try_into().ok()?) as usize;
        i += 2;
        if i + val_len > headers_data.len() {
            break;
        }
        let val = std::str::from_utf8(&headers_data[i..i + val_len]).ok()?;
        i += val_len;
        if name == ":event-type" || name == ":exception-type" {
            return Some(val.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_bus;

    fn no_images(_: &str) -> String {
        String::new()
    }

    /// Build one AWS Event Stream frame with a single `:event-type` header.
    /// Total length = 4 (total_len) + 4 (headers_len) + 4 (prelude crc) +
    /// headers + payload + 4 (message crc). CRCs aren't validated by the
    /// decoder so we use zero placeholders — keeps the test fixture
    /// hermetic.
    fn frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
        // Header: 1 byte name_len | name | 1 byte val_type (7=string) |
        // 2 bytes val_len (big-endian) | value
        let name = b":event-type";
        let mut headers = Vec::new();
        headers.push(name.len() as u8);
        headers.extend_from_slice(name);
        headers.push(7);
        headers.extend_from_slice(&(event_type.len() as u16).to_be_bytes());
        headers.extend_from_slice(event_type.as_bytes());
        let total_len = 4 + 4 + 4 + headers.len() + payload.len() + 4;
        let headers_len = headers.len();
        let mut out = Vec::with_capacity(total_len);
        out.extend_from_slice(&(total_len as u32).to_be_bytes());
        out.extend_from_slice(&(headers_len as u32).to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]); // prelude crc (ignored)
        out.extend_from_slice(&headers);
        out.extend_from_slice(payload);
        out.extend_from_slice(&[0, 0, 0, 0]); // message crc (ignored)
        out
    }

    #[tokio::test]
    async fn decoder_streams_assistant_text_across_partial_chunks() {
        let wire = [
            frame(
                "assistantResponseEvent",
                br#"{"content":"Hi","modelId":"claude"}"#,
            ),
            frame(
                "assistantResponseEvent",
                br#"{"content":" there","modelId":"claude"}"#,
            ),
        ]
        .concat();

        let (tx, mut rx) = event_bus::channel();
        let mut decoder = FrameDecoder::new(&[]);

        // Split mid-frame so the decoder has to buffer across feeds.
        let mid = wire.len() / 2;
        decoder.feed(&wire[..mid]);
        while let Some(f) = decoder.pop_frame() {
            decoder.handle_frame(&f, &tx, &None).await;
        }
        decoder.feed(&wire[mid..]);
        while let Some(f) = decoder.pop_frame() {
            decoder.handle_frame(&f, &tx, &None).await;
        }

        drop(tx);
        let mut tokens = Vec::new();
        while let Some(evt) = rx.recv().await {
            if let Event::Token(t) = evt {
                tokens.push(t);
            }
        }
        // event_bus coalesces adjacent Token deltas — the combined string
        // is what matters, not the chunk boundary.
        assert_eq!(tokens.join(""), "Hi there");

        let resp = decoder.finish();
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        match &resp.message.content[..] {
            [ContentBlock::Text { text }] => assert_eq!(text, "Hi there"),
            other => panic!("unexpected content: {other:?}"),
        }
    }

    #[tokio::test]
    async fn decoder_accumulates_tool_input_chunks_and_parses_on_stop() {
        // Mirrors the live Kiro wire shape captured from a real probe:
        // first frame = {name, toolUseId} (no input), then deltas with
        // `input: "..."` fragments, terminated by `{stop: true}`.
        let id = "tooluse_abc";
        let wire = [
            frame(
                "toolUseEvent",
                format!(r#"{{"name":"add","toolUseId":"{id}"}}"#).as_bytes(),
            ),
            frame(
                "toolUseEvent",
                format!(r#"{{"input":"{{\"a\":3","name":"add","toolUseId":"{id}"}}"#).as_bytes(),
            ),
            frame(
                "toolUseEvent",
                format!(r#"{{"input":",\"b\":4}}","name":"add","toolUseId":"{id}"}}"#).as_bytes(),
            ),
            frame(
                "toolUseEvent",
                format!(r#"{{"name":"add","stop":true,"toolUseId":"{id}"}}"#).as_bytes(),
            ),
        ]
        .concat();

        let (tx, mut rx) = event_bus::channel();
        let mut decoder = FrameDecoder::new(&[]);
        decoder.feed(&wire);
        while let Some(f) = decoder.pop_frame() {
            decoder.handle_frame(&f, &tx, &None).await;
        }

        drop(tx);
        let mut tool_selected = None;
        while let Some(evt) = rx.recv().await {
            if let Event::ToolSelected { name } = evt {
                tool_selected = Some(name);
            }
        }
        assert_eq!(tool_selected.as_deref(), Some("add"));

        let resp = decoder.finish();
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        match &resp.message.content[..] {
            [ContentBlock::ToolUse { name, input, .. }] => {
                assert_eq!(name, "add");
                assert_eq!(input["a"], 3);
                assert_eq!(input["b"], 4);
            }
            other => panic!("unexpected content: {other:?}"),
        }
    }

    #[tokio::test]
    async fn decoder_drops_corrupt_frame_without_getting_stuck() {
        // total_len claims 20 bytes, headers_len > total_len payload → invalid.
        let mut bad = Vec::new();
        bad.extend_from_slice(&20u32.to_be_bytes());
        bad.extend_from_slice(&100u32.to_be_bytes());
        bad.extend_from_slice(&[0; 12]);
        let good = frame(
            "assistantResponseEvent",
            br#"{"content":"after","modelId":"c"}"#,
        );

        let (tx, _rx) = event_bus::channel();
        let mut decoder = FrameDecoder::new(&[]);
        decoder.feed(&bad);
        decoder.feed(&good);
        while let Some(f) = decoder.pop_frame() {
            decoder.handle_frame(&f, &tx, &None).await;
        }

        let resp = decoder.finish();
        match &resp.message.content[..] {
            [ContentBlock::Text { text }] => assert_eq!(text, "after"),
            other => panic!("corrupt frame stalled decoder: {other:?}"),
        }
    }

    #[tokio::test]
    async fn decoder_emits_context_usage_event() {
        // Real wire shape captured from a live mitmproxy session against
        // AmazonCodeWhispererStreamingService.GenerateAssistantResponse.
        let wire = frame(
            "contextUsageEvent",
            br#"{"contextUsagePercentage":1.1650999784469604}"#,
        );

        let (tx, mut rx) = event_bus::channel();
        let mut decoder = FrameDecoder::new(&[]);
        decoder.feed(&wire);
        while let Some(f) = decoder.pop_frame() {
            decoder.handle_frame(&f, &tx, &None).await;
        }

        drop(tx);
        let mut ctx = None;
        while let Some(evt) = rx.recv().await {
            if let Event::ContextUsage(pct) = evt {
                ctx = Some(pct);
            }
        }
        let pct = ctx.expect("ContextUsage event emitted");
        assert!((pct - 1.165).abs() < 0.01, "got {pct}");
    }

    #[tokio::test]
    async fn decoder_clamps_context_usage_above_100() {
        // Server occasionally reports slightly >100 on saturation; the
        // UI takes an u8 after rounding, so clamp upstream to keep the
        // type contract obvious.
        let wire = frame("contextUsageEvent", br#"{"contextUsagePercentage":105.3}"#);
        let (tx, mut rx) = event_bus::channel();
        let mut decoder = FrameDecoder::new(&[]);
        decoder.feed(&wire);
        while let Some(f) = decoder.pop_frame() {
            decoder.handle_frame(&f, &tx, &None).await;
        }
        drop(tx);
        let mut ctx = None;
        while let Some(evt) = rx.recv().await {
            if let Event::ContextUsage(pct) = evt {
                ctx = Some(pct);
            }
        }
        assert_eq!(ctx, Some(100.0));
    }

    #[test]
    fn treats_403_as_provider_unauthorized() {
        let err = anyhow::Error::new(ProviderUnauthorized {
            provider: "kiro".to_owned(),
            status: 403,
            detail: "access denied".to_owned(),
        });

        let unauth = err
            .downcast_ref::<ProviderUnauthorized>()
            .expect("typed error");
        assert_eq!(unauth.provider, "kiro");
        assert_eq!(unauth.status, 403);
        assert_eq!(unauth.detail, "access denied");
    }

    #[test]
    fn derive_session_uuids_is_stable_and_distinct() {
        let session = "7ba1f7e5-4c7a-4c5f-8a6d-8f9c7c3e1b2a";
        let (cid1, kid1) = derive_session_uuids(session);
        let (cid2, kid2) = derive_session_uuids(session);
        // Same session → same IDs across turns (server-side observability).
        assert_eq!(cid1, cid2);
        assert_eq!(kid1, kid2);
        // But conversation and continuation must differ.
        assert_ne!(cid1, kid1);
        // Kiro's regex wants proper UUID shape; our ids must pass.
        assert!(is_uuid_shape(&cid1));
        assert!(is_uuid_shape(&kid1));
    }

    #[test]
    fn derive_session_uuids_handles_non_uuid_input() {
        // App passes through session_id verbatim. If it isn't a UUID we
        // generate one rather than emit "Improperly formed request" 400s.
        let (cid, kid) = derive_session_uuids("not-a-uuid");
        assert!(is_uuid_shape(&cid));
        assert!(is_uuid_shape(&kid));
        assert_ne!(cid, kid);
    }

    #[test]
    fn history_does_not_repeat_tool_specs() {
        // Tools must ride only on `currentMessage`. Repeating them in
        // every history turn bloats the body and — more importantly —
        // breaks server-side prompt cache byte-identity when the tool
        // list shifts (new tool registered, capability toggled).
        let msgs = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                origin: None,
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "hi".into() }],
                origin: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "do thing".into(),
                }],
                origin: None,
            },
        ];
        let tools = vec![crate::core::types::ToolSchema {
            name: "fs_read".into(),
            description: "read".into(),
            parameters: json!({"type":"object"}),
            streamable_arg: None,
        }];
        let body = build_request_body(&msgs, "auto", "arn:x", &tools, "cid", "kid", &no_images);
        let history = body["conversationState"]["history"].as_array().unwrap();
        // 2 messages in history (user + assistant); tools must not appear.
        assert_eq!(history.len(), 2);
        for turn in history {
            let s = turn.to_string();
            assert!(
                !s.contains("toolSpecification"),
                "tool spec leaked into history: {s}"
            );
        }
        // But currentMessage does carry tools.
        let curr_tools = body["conversationState"]["currentMessage"]
            ["userInputMessage"]["userInputMessageContext"]["tools"]
            .as_array()
            .unwrap();
        assert_eq!(curr_tools.len(), 1);
        assert_eq!(
            curr_tools[0]["toolSpecification"]["name"].as_str(),
            Some("fs_read")
        );
    }

    #[test]
    fn every_user_turn_carries_env_state() {
        // Q Developer CLI sends `envState: { operatingSystem, currentWorkingDirectory }`
        // on every userInputMessage. Missing it trims ~100 bytes but, more
        // importantly, drops a free signal the server uses to disambiguate
        // file-path questions — keep parity with the official client.
        let msgs = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
                origin: None,
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "yo".into() }],
                origin: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "check cwd".into(),
                }],
                origin: None,
            },
        ];
        let body = build_request_body(&msgs, "auto", "arn:x", &[], "cid", "kid", &no_images);
        // history[0] is the past user turn.
        let past_ctx =
            &body["conversationState"]["history"][0]["userInputMessage"]["userInputMessageContext"];
        assert!(past_ctx["envState"]["operatingSystem"].is_string());
        assert!(past_ctx["envState"]["currentWorkingDirectory"].is_string());
        // currentMessage.
        let curr_ctx = &body["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"];
        assert!(curr_ctx["envState"]["operatingSystem"].is_string());
    }

    #[test]
    fn current_user_message_serializes_images_in_official_shape() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "describe this".into(),
                },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    id: "img_1".into(),
                },
            ],
            origin: None,
        }];

        let body = build_request_body(&msgs, "auto", "arn:x", &[], "cid", "kid", &|id| {
            assert_eq!(id, "img_1");
            "BASE64DATA".into()
        });

        let user = &body["conversationState"]["currentMessage"]["userInputMessage"];
        let images = user["images"].as_array().unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0]["format"], "png");
        assert_eq!(images[0]["source"]["bytes"], "BASE64DATA");
    }

    #[test]
    fn history_user_message_serializes_images_in_official_shape() {
        let msgs = vec![
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text {
                        text: "past".into(),
                    },
                    ContentBlock::Image {
                        media_type: "image/webp".into(),
                        id: "img_hist".into(),
                    },
                ],
                origin: None,
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "ok".into() }],
                origin: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "now".into() }],
                origin: None,
            },
        ];

        let body = build_request_body(&msgs, "auto", "arn:x", &[], "cid", "kid", &|id| {
            assert_eq!(id, "img_hist");
            "HISTDATA".into()
        });

        let user = &body["conversationState"]["history"][0]["userInputMessage"];
        let images = user["images"].as_array().unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0]["format"], "webp");
        assert_eq!(images[0]["source"]["bytes"], "HISTDATA");
    }
}
