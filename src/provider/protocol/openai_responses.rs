/// Codex provider — OpenAI Responses API at chatgpt.com/backend-api/codex.
use crate::config::auth::{CODEX_ORIGINATOR, codex_user_agent, resolve_installation_id};
use crate::core::provider::{Provider, StopReason, StreamEvent, StreamRequest, StreamResponse};
use crate::core::provider_state::{
    CodexSessionState, CodexStateUpdate, ProviderRequestState, ProviderStateKind,
    ProviderStateUpdate,
};
use crate::core::types::{
    CodexReasoningSummaryPart, ContentBlock, Message, Role, ThinkingLevel, ToolResultBody,
    ToolResultItem, ToolSchema, Usage,
};
use crate::event::Event;
use crate::event_bus::Sender as EventSender;
use crate::provider::json_stream::{JsonStringExtractor, streamable_arg_for};
use crate::provider::sse::SseEventStream;
use anyhow::Result;
use futures::stream::{BoxStream, StreamExt};
use std::collections::{BTreeMap, VecDeque};

const REASONING_ENCRYPTED_CONTENT_INCLUDE: &str = "reasoning.encrypted_content";
const CODEX_TOOL_CHOICE_AUTO: &str = "auto";
const CODEX_PARALLEL_TOOL_CALLS: bool = true;
const HEADER_AUTHORIZATION: &str = "Authorization";
const HEADER_ORIGINATOR: &str = "originator";
const HEADER_USER_AGENT: &str = "User-Agent";
const HEADER_CHATGPT_ACCOUNT_ID: &str = "chatgpt-account-id";
const HEADER_SESSION_ID: &str = "session-id";
const HEADER_THREAD_ID: &str = "thread-id";
const HEADER_CODEX_TURN_STATE: &str = "x-codex-turn-state";
const HEADER_CODEX_INSTALLATION_ID: &str = "x-codex-installation-id";
const HEADER_REQUEST_ID: &str = "x-request-id";
const HEADER_OPENAI_MODEL: &str = "openai-model";

/// Per-output-index tool-call accumulator used while the Codex Responses
/// stream is in flight. Converted to `ContentBlock::ToolUse` at commit time.
#[derive(Default, Clone, Debug)]
struct PendingTool {
    id: String,
    name: String,
    /// Raw accumulated argument bytes — parsed once on commit.
    arguments: String,
}

#[derive(Clone, Debug)]
struct PendingReasoning {
    id: Option<String>,
    summary: Vec<CodexReasoningSummaryPart>,
    encrypted_content: Option<String>,
}

#[derive(Clone, Debug)]
enum PendingOutput {
    Reasoning(PendingReasoning),
    Tool(PendingTool),
}

fn append_codex_routing_headers<'a>(
    headers: &mut Vec<(&'static str, &'a str)>,
    session: &'a CodexSessionState,
    turn_state: Option<&'a str>,
) {
    headers.push((HEADER_THREAD_ID, session.thread_id.as_str()));
    if let Some(turn_state) = turn_state {
        headers.push((HEADER_CODEX_TURN_STATE, turn_state));
    }
}

/// Codex provider using the Responses API.
pub struct OpenAIResponsesRuntime {
    model: String,
    base_url: String,
    api_key: String,
    account_id: Option<String>,
    thinking: ThinkingLevel,
    service_tier: Option<String>,
    session_id: Option<String>,
    account_label: String,
}

impl OpenAIResponsesRuntime {
    /// Create with model, gateway base URL, token, optional account ID,
    /// session ID for cache routing, and pool account label. `base_url`
    /// is the gateway's scheme+host+path-prefix with no trailing slash
    /// (e.g. `https://chatgpt.com/backend-api/codex`); the runtime
    /// appends `/responses`.
    pub fn new(
        model: &str,
        base_url: &str,
        api_key: &str,
        account_id: Option<String>,
        session_id: &str,
        account_label: &str,
    ) -> Self {
        Self {
            model: model.to_owned(),
            base_url: base_url.to_owned(),
            api_key: api_key.to_owned(),
            account_id,
            thinking: ThinkingLevel::Low,
            service_tier: None,
            session_id: Some(session_id.to_owned()),
            account_label: account_label.to_owned(),
        }
    }

    /// Override the Responses API service tier for this runtime.
    pub fn with_service_tier(mut self, service_tier: Option<String>) -> Self {
        self.service_tier = service_tier;
        self
    }

    /// Build the OpenAI Responses API request body. Pure.
    fn build_request_body(
        &self,
        messages: &[crate::core::types::Message],
        tools: &[crate::core::types::ToolSchema],
        server_tools: &[serde_json::Value],
        resolve_image: &crate::core::provider::ImageResolver,
    ) -> serde_json::Value {
        let system = extract_system(messages);
        let input = build_input(messages, resolve_image);
        let mut api_tools = to_api_tools(tools);
        for st in server_tools {
            api_tools.push(st.clone());
        }
        let mut body = serde_json::json!({
            "model": self.model,
            "input": input,
            "tool_choice": CODEX_TOOL_CHOICE_AUTO,
            "parallel_tool_calls": CODEX_PARALLEL_TOOL_CALLS,
            "store": false,
            "stream": true,
        });
        if !system.is_empty() {
            body["instructions"] = system.into();
        }
        if !api_tools.is_empty() {
            body["tools"] = api_tools.into();
        }
        if let Some(key) = &self.session_id {
            body["prompt_cache_key"] = serde_json::json!(key);
        }
        if let Some(service_tier) = &self.service_tier {
            body["service_tier"] = serde_json::json!(service_tier);
        }
        if let Some(installation_id) = resolve_installation_id() {
            body["client_metadata"] = serde_json::json!({
                "x-codex-installation-id": installation_id,
            });
        }
        let effort = match self.thinking {
            ThinkingLevel::Off => None,
            ThinkingLevel::Low => Some("low"),
            ThinkingLevel::Medium => Some("medium"),
            ThinkingLevel::High | ThinkingLevel::Max => Some("high"),
        };
        if let Some(effort) = effort {
            body["reasoning"] = serde_json::json!({
                "effort": effort,
                "summary": "auto",
            });
            body["include"] = serde_json::json!([REASONING_ENCRYPTED_CONTENT_INCLUDE]);
        }
        body
    }
}

impl Provider for OpenAIResponsesRuntime {
    fn name(&self) -> &str {
        "codex"
    }

    fn session_state_kind(&self) -> Option<ProviderStateKind> {
        Some(ProviderStateKind::Codex)
    }

    fn set_thinking(&mut self, level: ThinkingLevel) {
        self.thinking = level;
    }

    /// The Responses API rejects `max_output_tokens`; codex-rs omits it too
    /// (`codex-rs/codex-api/src/common.rs:ResponsesApiRequest`). Escalation
    /// retries would just repeat the same failure.
    fn supports_max_tokens_override(&self) -> bool {
        false
    }

    fn server_tool_schemas(&self, capabilities: &[String]) -> Vec<serde_json::Value> {
        capabilities
            .iter()
            .filter_map(|cap| {
                if cap == "web_search" {
                    Some(serde_json::json!({"type": "web_search"}))
                } else {
                    None
                }
            })
            .collect()
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
                provider_state,
                // Ignored — see `supports_max_tokens_override` impl above.
                max_tokens_override: _,
                tx,
                cancel,
                tool_use_tx,
            } = req;
            let body = self.build_request_body(messages, tools, server_tools, resolve_image);

            // Headers match `codex-rs/core/src/client.rs` +
            // `codex-rs/login/src/auth/default_client.rs::default_headers`.
            // Any drift breaks the backend's first-party client check.
            let auth_header = format!("Bearer {}", self.api_key);
            let user_agent = codex_user_agent();
            let installation_id = resolve_installation_id();
            let mut header_vec: Vec<(&str, &str)> = vec![
                (HEADER_AUTHORIZATION, &auth_header),
                (HEADER_ORIGINATOR, CODEX_ORIGINATOR),
                (HEADER_USER_AGENT, user_agent.as_str()),
            ];
            if let Some(aid) = &self.account_id {
                header_vec.push((HEADER_CHATGPT_ACCOUNT_ID, aid.as_str()));
            }
            if let Some(sid) = &self.session_id {
                header_vec.push((HEADER_SESSION_ID, sid.as_str()));
            }
            if let Some(installation_id) = installation_id.as_deref() {
                header_vec.push((HEADER_CODEX_INSTALLATION_ID, installation_id));
            }
            if let Some(ProviderRequestState::Codex {
                session,
                turn_state,
            }) = provider_state
            {
                append_codex_routing_headers(&mut header_vec, session, turn_state);
            }

            let endpoint = format!("{}/responses", self.base_url);
            let sse = crate::provider::sse::post_sse_with_headers(
                "codex",
                &self.account_label,
                &endpoint,
                &header_vec,
                &body,
                &tx,
                &cancel,
            )
            .await?;
            let headers = sse.headers;
            consume_responses_stream(sse.stream, tools, &tx, &cancel, tool_use_tx, headers).await
        })
    }
}

/// Drain a decoded Codex Responses stream into a `StreamResponse`.
async fn consume_responses_stream(
    sse: SseEventStream,
    tools: &[ToolSchema],
    tx: &EventSender,
    cancel: &tokio_util::sync::CancellationToken,
    tool_use_tx: Option<tokio::sync::mpsc::Sender<ContentBlock>>,
    headers: crate::provider::sse::ResponseHeaders,
) -> Result<StreamResponse> {
    let mut events = decode_responses_sse(sse, tools.to_vec());
    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut usage = Usage::default();
    let mut stop_reason = StopReason::default();
    let mut saw_done = false;
    let mut response_id = None;

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
                tx.send_or_log(Event::WebSearchDone {
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
                if let Some(ref tu_tx) = tool_use_tx
                    && matches!(&b, ContentBlock::ToolUse { .. })
                {
                    let _ = tu_tx.send(b.clone()).await;
                }
                blocks.push(b);
            }
            StreamEvent::Done { stop } => {
                stop_reason = stop;
                saw_done = true;
                break;
            }
            StreamEvent::ProviderMetadata(metadata) => match metadata {
                crate::core::provider::ProviderStreamMetadata::Codex { response_id: id } => {
                    response_id = id;
                }
            },
        }
    }

    if !saw_done {
        return Err(crate::provider::sse::StreamInterrupted(
            "Codex stream closed before response.completed".into(),
        )
        .into());
    }

    Ok(StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: blocks,
            origin: Some(crate::core::types::MessageOrigin {
                provider: "codex".into(),
                model: None,
            }),
        },
        usage,
        stop_reason,
        context_usage_emitted: false,
        provider_state: codex_state_update(&headers, response_id),
    })
}

fn codex_state_update(
    headers: &crate::provider::sse::ResponseHeaders,
    response_id: Option<String>,
) -> Option<ProviderStateUpdate> {
    let update = CodexStateUpdate {
        turn_state: headers.get_str(HEADER_CODEX_TURN_STATE),
        request_id: headers.get_str(HEADER_REQUEST_ID),
        response_id,
        server_model: headers.get_str(HEADER_OPENAI_MODEL),
    };
    update
        .has_any()
        .then_some(ProviderStateUpdate::Codex(update))
}

/// Pure decoder for the Codex Responses SSE dialect.
///
/// Reads one typed `sse_event.event_type` at a time; accumulates text,
/// per-output-index tool calls, reasoning deltas, and web-search state;
/// emits normalized `StreamEvent`s via an internal queue.
struct ResponsesDecoder {
    tools: Vec<ToolSchema>,
    text: String,
    outputs: BTreeMap<u64, PendingOutput>,
    arg_extractors: BTreeMap<u64, JsonStringExtractor>,
    extractor_probed: std::collections::BTreeSet<u64>,
    reasoning_text_emitted: bool,
    usage: Usage,
    response_id: Option<String>,
    /// Terminal classifiers. Exactly one is set when the decoder finalises.
    saw_completed: bool,
    incomplete_reason: String,
    failure_error: Option<anyhow::Error>,
    out: VecDeque<StreamEvent>,
}

impl ResponsesDecoder {
    fn new(tools: Vec<ToolSchema>) -> Self {
        Self {
            tools,
            text: String::new(),
            outputs: BTreeMap::new(),
            arg_extractors: BTreeMap::new(),
            extractor_probed: std::collections::BTreeSet::new(),
            reasoning_text_emitted: false,
            usage: Usage::default(),
            response_id: None,
            saw_completed: false,
            incomplete_reason: String::new(),
            failure_error: None,
            out: VecDeque::new(),
        }
    }

    /// Whether the decoder has reached a terminal frame (completed,
    /// incomplete, or failed). After this, further SSE frames are ignored
    /// and the outer stream finalises.
    fn is_terminal(&self) -> bool {
        self.saw_completed || !self.incomplete_reason.is_empty() || self.failure_error.is_some()
    }

    fn feed(&mut self, event_type: &str, event: &serde_json::Value) {
        crate::dbg_log!("codex event: {event_type}");
        match event_type {
            "response.output_text.delta" | "response.content_part.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    self.text.push_str(delta);
                    self.out.push_back(StreamEvent::TextDelta(delta.to_owned()));
                }
            }
            "response.reasoning_summary_text.delta"
            | "response.reasoning_summary.delta"
            | "response.reasoning_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    self.reasoning_text_emitted = true;
                    self.out
                        .push_back(StreamEvent::ThinkingDelta(delta.to_owned()));
                }
            }
            "response.web_search_call.in_progress" => {
                self.out.push_back(StreamEvent::WebSearchStart {
                    query: String::new(),
                });
            }
            "response.web_search_call.searching" => {}
            "response.output_item.added" => {
                store_output_item(
                    &mut self.outputs,
                    event["output_index"].as_u64(),
                    &event["item"],
                );
                if event["item"]["type"].as_str() == Some("function_call")
                    && let Some(name) = event["item"]["name"].as_str()
                    && !name.is_empty()
                {
                    self.out.push_back(StreamEvent::ToolSelected {
                        name: name.to_owned(),
                    });
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(idx) = event["output_index"].as_u64()
                    && let Some(delta) = event["delta"].as_str()
                {
                    let entry = pending_tool_mut(&mut self.outputs, idx);
                    entry.arguments.push_str(delta);
                    if !self.extractor_probed.contains(&idx) && !entry.name.is_empty() {
                        self.extractor_probed.insert(idx);
                        if let Some(field) = streamable_arg_for(&self.tools, &entry.name) {
                            self.arg_extractors
                                .insert(idx, JsonStringExtractor::new(field));
                        }
                    }
                    let tool_name = entry.name.clone();
                    if let Some(ex) = self.arg_extractors.get_mut(&idx) {
                        let chunk = ex.feed(delta);
                        if !chunk.is_empty() {
                            self.out.push_back(StreamEvent::ToolInput {
                                name: tool_name,
                                chunk,
                            });
                        }
                    }
                }
            }
            "response.function_call_arguments.done" | "response.output_item.done" => {
                store_output_item(
                    &mut self.outputs,
                    event["output_index"].as_u64(),
                    &event["item"],
                );
                if event["item"]["type"].as_str() == Some("web_search_call") {
                    // Codex Responses doesn't surface per-hit URLs; the
                    // consumer side emits an empty-results WebSearchDone.
                    self.out.push_back(StreamEvent::WebSearchDone {
                        results: Vec::new(),
                    });
                }
            }
            "response.completed" => {
                self.saw_completed = true;
                self.response_id = event["response"]["id"].as_str().map(str::to_owned);
                if let Some(output) = event["response"]["output"].as_array() {
                    for (idx, item) in output.iter().enumerate() {
                        self.emit_reasoning_item_if_needed(item);
                        store_output_item(&mut self.outputs, Some(idx as u64), item);
                    }
                }
                self.record_usage(&event["response"]["usage"]);
            }
            "response.incomplete" => {
                self.response_id = event["response"]["id"].as_str().map(str::to_owned);
                self.incomplete_reason = event["response"]["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_owned();
                if let Some(output) = event["response"]["output"].as_array() {
                    for (idx, item) in output.iter().enumerate() {
                        self.emit_reasoning_item_if_needed(item);
                        store_output_item(&mut self.outputs, Some(idx as u64), item);
                    }
                }
                self.record_usage(&event["response"]["usage"]);
            }
            "response.failed" => {
                let err_code = event["response"]["error"]["code"].as_str().unwrap_or("");
                let err_msg = event["response"]["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown error");
                self.failure_error = Some(if err_code == "context_length_exceeded" {
                    anyhow::anyhow!(
                        "codex context window exceeded: {err_msg}. Start a new session or switch to a model with larger context."
                    )
                } else {
                    anyhow::anyhow!("codex response.failed ({err_code}): {err_msg}")
                });
            }
            _ => {}
        }
    }

    fn emit_reasoning_item_if_needed(&mut self, item: &serde_json::Value) {
        if self.reasoning_text_emitted || item["type"].as_str() != Some("reasoning") {
            return;
        }
        let text = extract_reasoning_item_text(item);
        if text.is_empty() {
            return;
        }
        self.reasoning_text_emitted = true;
        self.out.push_back(StreamEvent::ThinkingDelta(text));
    }

    fn record_usage(&mut self, usage_val: &serde_json::Value) {
        let Some(u) = usage_val.as_object() else {
            return;
        };
        let cached = u
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64());
        let input = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        // Codex input_tokens includes cached — subtract to match Claude.
        let non_cached = input.saturating_sub(cached.unwrap_or(0));
        let snapshot = Usage {
            input_tokens: non_cached,
            output_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            cache_read: cached,
            cache_write: None,
        };
        self.usage = snapshot.clone();
        self.out.push_back(StreamEvent::UsageUpdate(snapshot));
    }

    /// Emit `BlockComplete` for text (if any) then each concrete tool
    /// call in output-index order, followed by the terminal `Done`.
    /// Returns `Err` if the Responses API surfaced a structured failure.
    fn finalize(&mut self) -> Result<()> {
        if let Some(err) = self.failure_error.take() {
            return Err(err);
        }
        if self.response_id.is_some() {
            self.out.push_back(StreamEvent::ProviderMetadata(
                crate::core::provider::ProviderStreamMetadata::Codex {
                    response_id: self.response_id.take(),
                },
            ));
        }
        if !self.text.is_empty() {
            let text = std::mem::take(&mut self.text);
            self.out
                .push_back(StreamEvent::BlockComplete(ContentBlock::Text { text }));
        }
        for (_, output) in std::mem::take(&mut self.outputs) {
            match output {
                PendingOutput::Reasoning(reasoning) => {
                    if reasoning.encrypted_content.is_none() {
                        continue;
                    }
                    self.out
                        .push_back(StreamEvent::BlockComplete(ContentBlock::CodexReasoning {
                            id: reasoning.id,
                            summary: reasoning.summary,
                            encrypted_content: reasoning.encrypted_content,
                        }));
                }
                PendingOutput::Tool(tool) => {
                    if tool.id.is_empty() || tool.name.is_empty() {
                        continue;
                    }
                    let input = crate::provider::json_stream::finalize_tool_input(
                        &tool.arguments,
                        &format!("{} ({})", tool.id, tool.name),
                    );
                    self.out
                        .push_back(StreamEvent::BlockComplete(ContentBlock::ToolUse {
                            id: tool.id,
                            name: tool.name,
                            input,
                        }));
                }
            }
        }

        let stop = if self.incomplete_reason.is_empty() {
            StopReason::EndTurn
        } else if self.incomplete_reason == "max_output_tokens" {
            StopReason::MaxTokens
        } else {
            anyhow::bail!(
                "codex response.incomplete (reason={}). Try again or switch model.",
                self.incomplete_reason
            );
        };
        self.out.push_back(StreamEvent::Done { stop });
        Ok(())
    }
}

fn decode_responses_sse(
    sse: SseEventStream,
    tools: Vec<ToolSchema>,
) -> BoxStream<'static, Result<StreamEvent>> {
    let decoder = ResponsesDecoder::new(tools);
    futures::stream::unfold(
        (sse, decoder, false),
        |(mut sse, mut decoder, mut finalized)| async move {
            if let Some(evt) = decoder.out.pop_front() {
                return Some((Ok(evt), (sse, decoder, finalized)));
            }
            if finalized {
                return None;
            }
            loop {
                match sse.next().await {
                    Some(Ok(frame)) => {
                        decoder.feed(frame.event_type.as_str(), &frame.data);
                        if decoder.is_terminal() {
                            finalized = true;
                            if let Err(e) = decoder.finalize() {
                                return Some((Err(e), (sse, decoder, finalized)));
                            }
                            if let Some(evt) = decoder.out.pop_front() {
                                return Some((Ok(evt), (sse, decoder, finalized)));
                            }
                            return None;
                        }
                        if let Some(evt) = decoder.out.pop_front() {
                            return Some((Ok(evt), (sse, decoder, finalized)));
                        }
                    }
                    Some(Err(e)) => return Some((Err(e), (sse, decoder, true))),
                    None => return None,
                }
            }
        },
    )
    .boxed()
}

fn extract_reasoning_item_text(item: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    if let Some(summary) = item["summary"].as_array() {
        for entry in summary {
            if let Some(text) = entry["text"].as_str()
                && !text.is_empty()
            {
                parts.push(text);
            }
        }
    }
    if parts.is_empty()
        && let Some(text) = item["text"].as_str()
        && !text.is_empty()
    {
        parts.push(text);
    }
    parts.join("\n")
}

fn pending_tool_mut(outputs: &mut BTreeMap<u64, PendingOutput>, idx: u64) -> &mut PendingTool {
    let should_replace = !matches!(outputs.get(&idx), Some(PendingOutput::Tool(_)));
    if should_replace {
        outputs.insert(idx, PendingOutput::Tool(PendingTool::default()));
    }
    let Some(PendingOutput::Tool(tool)) = outputs.get_mut(&idx) else {
        unreachable!("pending output was normalized to a tool")
    };
    tool
}

fn store_output_item(
    outputs: &mut BTreeMap<u64, PendingOutput>,
    output_index: Option<u64>,
    item: &serde_json::Value,
) {
    let Some(idx) = output_index else { return };
    match item["type"].as_str().unwrap_or("") {
        "function_call" => store_tool_item(outputs, idx, item),
        "reasoning" => store_reasoning_item(outputs, idx, item),
        _ => {}
    }
}

fn store_tool_item(outputs: &mut BTreeMap<u64, PendingOutput>, idx: u64, item: &serde_json::Value) {
    let entry = pending_tool_mut(outputs, idx);
    if let Some(call_id) = item["call_id"].as_str()
        && !call_id.is_empty()
    {
        entry.id = call_id.to_owned();
    }
    if let Some(name) = item["name"].as_str()
        && !name.is_empty()
    {
        entry.name = name.to_owned();
    }
    if let Some(arguments) = item["arguments"].as_str()
        && !arguments.is_empty()
        && entry.arguments.is_empty()
    {
        entry.arguments = arguments.to_owned();
    }
}

fn store_reasoning_item(
    outputs: &mut BTreeMap<u64, PendingOutput>,
    idx: u64,
    item: &serde_json::Value,
) {
    let summary = extract_reasoning_summary_parts(item);
    let id = item["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    let encrypted_content = item["encrypted_content"].as_str().map(str::to_owned);
    let should_replace = !matches!(outputs.get(&idx), Some(PendingOutput::Reasoning(_)));
    if should_replace {
        outputs.insert(
            idx,
            PendingOutput::Reasoning(PendingReasoning {
                id,
                summary,
                encrypted_content,
            }),
        );
        return;
    }

    let Some(PendingOutput::Reasoning(existing)) = outputs.get_mut(&idx) else {
        unreachable!("pending output was normalized to reasoning")
    };
    if id.is_some() {
        existing.id = id;
    }
    if !summary.is_empty() {
        existing.summary = summary;
    }
    if encrypted_content.is_some() {
        existing.encrypted_content = encrypted_content;
    }
}

fn extract_reasoning_summary_parts(item: &serde_json::Value) -> Vec<CodexReasoningSummaryPart> {
    item["summary"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let text = entry["text"].as_str()?;
            if text.is_empty() {
                return None;
            }
            let kind = entry["type"].as_str().unwrap_or("summary_text").to_owned();
            Some(CodexReasoningSummaryPart {
                kind,
                text: text.to_owned(),
            })
        })
        .collect()
}

fn extract_system(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Render `function_call_output.output` for Codex Responses wire.
///
/// `ToolResultBody::Text` → JSON string (matches Codex's text-only path
/// byte-for-byte — this is the common case and must not drift).
/// `ToolResultBody::Items` → array of `input_text` / `input_image` entries
/// so image-producing tools (e.g. `Read` on a PNG) attach real bytes as
/// data URLs. Unresolvable image ids (resolver returned empty string) are
/// dropped so the request body stays valid — the text portion still lands,
/// model sees a stripped result rather than a 400.
fn tool_result_output_responses(
    body: &ToolResultBody,
    resolve: &crate::core::provider::ImageResolver,
) -> serde_json::Value {
    match body {
        ToolResultBody::Text(s) => serde_json::json!(s),
        ToolResultBody::Items(items) => {
            let entries: Vec<serde_json::Value> = items
                .iter()
                .filter_map(|item| match item {
                    ToolResultItem::Text { text } if !text.is_empty() => {
                        Some(serde_json::json!({"type": "input_text", "text": text}))
                    }
                    ToolResultItem::Text { .. } => None,
                    ToolResultItem::Image { media_type, id } => {
                        let data = resolve(id);
                        if data.is_empty() {
                            return None;
                        }
                        Some(serde_json::json!({
                            "type": "input_image",
                            "image_url": format!("data:{media_type};base64,{data}"),
                        }))
                    }
                })
                .collect();
            serde_json::json!(entries)
        }
    }
}

fn build_input(
    messages: &[Message],
    resolve: &crate::core::provider::ImageResolver,
) -> Vec<serde_json::Value> {
    let mut input = Vec::new();
    for msg in messages {
        if msg.role == Role::System {
            continue;
        }
        match msg.role {
            Role::User => {
                // Tool results on a user message become `function_call_output`
                // items — one per result block, unnested.
                //
                // `ToolResultBody::Text` serializes as `output: "string"` to
                // match Codex wire for the common text-only case.
                // `ToolResultBody::Items` serializes as an array with
                // `input_text` / `input_image` entries so image-aware tools
                // (e.g. Read on a PNG) attach real bytes.
                let mut had_result = false;
                for block in &msg.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                    {
                        let output = tool_result_output_responses(content, resolve);
                        input.push(serde_json::json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": output,
                        }));
                        had_result = true;
                    }
                }
                if had_result {
                    continue;
                }
                let mut content = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } | ContentBlock::Paste { text }
                            if !text.is_empty() =>
                        {
                            content.push(serde_json::json!({
                                "type": "input_text",
                                "text": text,
                            }));
                        }
                        ContentBlock::Image { media_type, id } => {
                            let data = resolve(id);
                            if data.is_empty() {
                                continue;
                            }
                            content.push(serde_json::json!({
                                "type": "input_image",
                                "image_url": format!("data:{media_type};base64,{data}"),
                            }));
                        }
                        _ => {}
                    }
                }
                if content.is_empty() {
                    input.push(serde_json::json!({
                        "role": "user",
                        "content": msg.text(),
                    }));
                } else if content.len() == 1 && content[0]["type"] == "input_text" {
                    input.push(serde_json::json!({
                        "role": "user",
                        "content": content[0]["text"].as_str().unwrap_or_default(),
                    }));
                } else {
                    input.push(serde_json::json!({
                        "role": "user",
                        "content": content,
                    }));
                }
            }
            Role::Assistant => {
                // Walk content blocks in order and reconstruct the Codex
                // Responses item stream, including opaque reasoning state.
                for block in &msg.content {
                    match block {
                        ContentBlock::CodexReasoning {
                            id,
                            summary,
                            encrypted_content: Some(encrypted_content),
                        } => {
                            let mut item = serde_json::json!({
                                "type": "reasoning",
                                "summary": summary,
                                "encrypted_content": encrypted_content,
                            });
                            if let Some(id) = id {
                                item["id"] = serde_json::json!(id);
                            }
                            input.push(item);
                        }
                        ContentBlock::ToolUse {
                            id,
                            name,
                            input: args,
                        } => {
                            input.push(serde_json::json!({
                                "type": "function_call",
                                "name": name,
                                "call_id": id,
                                "arguments": serde_json::to_string(args).unwrap_or_default(),
                            }));
                        }
                        ContentBlock::Text { text } | ContentBlock::Paste { text }
                            if !text.is_empty() =>
                        {
                            input.push(serde_json::json!({
                                "role": "assistant",
                                "content": text,
                            }));
                        }
                        _ => {}
                    }
                }
            }
            Role::System => unreachable!(),
        }
    }
    input
}

fn to_api_tools(tools: &[ToolSchema]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::ToolSchema;
    use crate::event::Event;
    use crate::event_bus;
    use crate::provider::sse::{SseEvent, stream_from_events};

    #[test]
    fn request_body_includes_configured_service_tier() {
        let runtime = OpenAIResponsesRuntime::new(
            "gpt-5.4",
            "https://chatgpt.com/backend-api/codex",
            "token",
            None,
            "session",
            "acct",
        )
        .with_service_tier(Some("priority".into()));

        let body = runtime.build_request_body(&[], &[], &[], &|_| String::new());

        assert_eq!(body["service_tier"], "priority");
    }

    #[test]
    fn request_body_enables_auto_parallel_tools() {
        let runtime = OpenAIResponsesRuntime::new(
            "gpt-5.4",
            "https://chatgpt.com/backend-api/codex",
            "token",
            None,
            "session",
            "acct",
        );

        let body = runtime.build_request_body(&[], &[], &[], &|_| String::new());

        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], true);
    }

    #[test]
    fn codex_state_update_uses_selected_response_headers() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(HEADER_CODEX_TURN_STATE, "turn-state".parse().unwrap());
        headers.insert(HEADER_REQUEST_ID, "req_1".parse().unwrap());
        headers.insert(HEADER_OPENAI_MODEL, "gpt-5.4".parse().unwrap());

        let update = codex_state_update(
            &crate::provider::sse::ResponseHeaders::new(&headers),
            Some("resp_1".into()),
        );

        let Some(ProviderStateUpdate::Codex(update)) = update else {
            panic!("expected codex state update")
        };
        assert_eq!(update.turn_state.as_deref(), Some("turn-state"));
        assert_eq!(update.request_id.as_deref(), Some("req_1"));
        assert_eq!(update.response_id.as_deref(), Some("resp_1"));
        assert_eq!(update.server_model.as_deref(), Some("gpt-5.4"));
    }

    #[test]
    fn build_input_roundtrips_codex_reasoning_state() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::CodexReasoning {
                id: Some("rs_1".into()),
                summary: vec![CodexReasoningSummaryPart {
                    kind: "summary_text".into(),
                    text: "Checked constraints.".into(),
                }],
                encrypted_content: Some("encrypted-state".into()),
            }],
            origin: Some(crate::core::types::MessageOrigin {
                provider: "codex".into(),
                model: Some("gpt-5.4".into()),
            }),
        }];

        let input = build_input(&messages, &|_| String::new());

        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["id"], "rs_1");
        assert_eq!(input[0]["summary"][0]["type"], "summary_text");
        assert_eq!(input[0]["summary"][0]["text"], "Checked constraints.");
        assert_eq!(input[0]["encrypted_content"], "encrypted-state");
    }

    #[test]
    fn request_body_includes_encrypted_reasoning_content_when_reasoning_enabled() {
        let runtime = OpenAIResponsesRuntime::new(
            "gpt-5.4",
            "https://chatgpt.com/backend-api/codex",
            "token",
            None,
            "session",
            "acct",
        );

        let body = runtime.build_request_body(&[], &[], &[], &|_| String::new());

        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(
            body["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );
    }

    #[test]
    fn stores_tool_call_from_incremental_codex_events() {
        let mut outputs = BTreeMap::new();
        let item = serde_json::json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "exec_command",
            "arguments": ""
        });

        store_output_item(&mut outputs, Some(0), &item);
        let entry = pending_tool_mut(&mut outputs, 0);
        entry.arguments.push_str("{\"command\":\"git status\"}");

        assert_eq!(entry.id, "call_1");
        assert_eq!(entry.name, "exec_command");
        assert_eq!(entry.arguments, "{\"command\":\"git status\"}");
    }

    #[test]
    fn completed_snapshot_fills_missing_codex_tool_fields() {
        let mut outputs = BTreeMap::new();
        let partial = serde_json::json!({"type": "function_call", "name": "exec_command"});
        let done = serde_json::json!({
            "type": "function_call",
            "call_id": "call_2",
            "name": "exec_command",
            "arguments": "{\"command\":\"pwd\"}"
        });

        store_output_item(&mut outputs, Some(1), &partial);
        store_output_item(&mut outputs, Some(1), &done);

        let Some(PendingOutput::Tool(entry)) = outputs.get(&1) else {
            panic!("expected tool output")
        };
        assert_eq!(entry.id, "call_2");
        assert_eq!(entry.arguments, "{\"command\":\"pwd\"}");
    }

    #[test]
    fn reasoning_snapshots_preserve_existing_encrypted_content() {
        let mut outputs = BTreeMap::new();
        let with_state = serde_json::json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "first"}],
            "encrypted_content": "encrypted-state"
        });
        let without_state = serde_json::json!({
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": "latest"}]
        });

        store_output_item(&mut outputs, Some(0), &with_state);
        store_output_item(&mut outputs, Some(0), &without_state);

        let Some(PendingOutput::Reasoning(entry)) = outputs.get(&0) else {
            panic!("expected reasoning output")
        };
        assert_eq!(entry.id.as_deref(), Some("rs_1"));
        assert_eq!(entry.summary[0].text, "latest");
        assert_eq!(entry.encrypted_content.as_deref(), Some("encrypted-state"));
    }

    #[tokio::test]
    async fn codex_stream_loop_emits_tokens_and_completes() {
        let events = vec![
            Ok(SseEvent {
                event_type: "response.output_text.delta".into(),
                data: serde_json::json!({"delta": "Hello"}),
            }),
            Ok(SseEvent {
                event_type: "response.output_text.delta".into(),
                data: serde_json::json!({"delta": " world"}),
            }),
            Ok(SseEvent {
                event_type: "response.completed".into(),
                data: serde_json::json!({
                    "response": {
                        "output": [],
                        "usage": {
                            "input_tokens": 10,
                            "output_tokens": 2,
                            "input_tokens_details": {"cached_tokens": 0}
                        }
                    }
                }),
            }),
        ];

        let stream = stream_from_events(events, true);
        let (tx, mut rx) = event_bus::channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let result = consume_responses_stream(stream, &[], &tx, &cancel, None, Default::default())
            .await
            .unwrap();

        assert_eq!(result.message.text(), "Hello world");
        assert_eq!(result.stop_reason, StopReason::EndTurn);

        let mut seen = String::new();
        while let Some(event) = rx.try_recv() {
            if let Event::Token(t) = event {
                seen.push_str(&t);
            }
        }
        assert_eq!(seen, "Hello world");
    }

    #[tokio::test]
    async fn codex_stream_loop_emits_final_reasoning_summary_when_no_delta_arrived() {
        let events = vec![Ok(SseEvent {
            event_type: "response.completed".into(),
            data: serde_json::json!({
                "response": {
                    "output": [{
                        "type": "reasoning",
                        "summary": [
                            {"type": "summary_text", "text": "Checked constraints."},
                            {"type": "summary_text", "text": "Chose the safe path."}
                        ]
                    }],
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 2,
                        "input_tokens_details": {"cached_tokens": 0}
                    }
                }
            }),
        })];

        let stream = stream_from_events(events, true);
        let (tx, mut rx) = event_bus::channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        consume_responses_stream(stream, &[], &tx, &cancel, None, Default::default())
            .await
            .unwrap();

        let mut seen = String::new();
        while let Some(event) = rx.try_recv() {
            if let Event::Thinking(t) = event {
                seen.push_str(&t);
            }
        }
        assert_eq!(seen, "Checked constraints.\nChose the safe path.");
    }

    #[tokio::test]
    async fn codex_stream_loop_preserves_encrypted_reasoning_without_leaking_it_to_ui() {
        let events = vec![Ok(SseEvent {
            event_type: "response.completed".into(),
            data: serde_json::json!({
                "response": {
                    "output": [{
                        "type": "reasoning",
                        "id": "rs_1",
                        "summary": [{"type": "summary_text", "text": "Checked constraints."}],
                        "encrypted_content": "encrypted-state"
                    }],
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 2,
                        "input_tokens_details": {"cached_tokens": 0}
                    }
                }
            }),
        })];

        let stream = stream_from_events(events, true);
        let (tx, mut rx) = event_bus::channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let result = consume_responses_stream(stream, &[], &tx, &cancel, None, Default::default())
            .await
            .unwrap();

        assert_eq!(result.message.content.len(), 1);
        match &result.message.content[0] {
            ContentBlock::CodexReasoning {
                id,
                summary,
                encrypted_content,
            } => {
                assert_eq!(id.as_deref(), Some("rs_1"));
                assert_eq!(summary[0].text, "Checked constraints.");
                assert_eq!(encrypted_content.as_deref(), Some("encrypted-state"));
            }
            other => panic!("expected codex reasoning, got {other:?}"),
        }

        let mut seen = String::new();
        while let Some(event) = rx.try_recv() {
            if let Event::Thinking(t) = event {
                seen.push_str(&t);
            }
        }
        assert_eq!(seen, "Checked constraints.");
        assert!(!seen.contains("encrypted-state"));
    }

    #[tokio::test]
    async fn codex_stream_loop_emits_tool_selected_and_input() {
        let tool = ToolSchema {
            name: "exec_command".into(),
            description: String::new(),
            parameters: serde_json::json!({}),
            streamable_arg: Some("command".into()),
        };
        let events = vec![
            Ok(SseEvent {
                event_type: "response.output_item.added".into(),
                data: serde_json::json!({
                    "output_index": 0,
                    "item": {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "exec_command",
                        "arguments": ""
                    }
                }),
            }),
            Ok(SseEvent {
                event_type: "response.function_call_arguments.delta".into(),
                data: serde_json::json!({
                    "output_index": 0,
                    "delta": "{\"command\":\"git status\"}"
                }),
            }),
            Ok(SseEvent {
                event_type: "response.completed".into(),
                data: serde_json::json!({
                    "response": {
                        "output": [{
                            "type": "function_call",
                            "call_id": "call_1",
                            "name": "exec_command",
                            "arguments": "{\"command\":\"git status\"}"
                        }],
                        "usage": {
                            "input_tokens": 10,
                            "output_tokens": 2,
                            "input_tokens_details": {"cached_tokens": 0}
                        }
                    }
                }),
            }),
        ];

        let stream = stream_from_events(events, true);
        let (tx, mut rx) = event_bus::channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let result =
            consume_responses_stream(stream, &[tool], &tx, &cancel, None, Default::default())
                .await
                .unwrap();

        let tool_uses: Vec<_> = result.message.tool_uses().collect();
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].1, "exec_command");

        let mut saw_selected = false;
        let mut saw_input = false;
        while let Some(event) = rx.try_recv() {
            match event {
                Event::ToolSelected { name } if name == "exec_command" => saw_selected = true,
                Event::ToolInput { name, chunk }
                    if name == "exec_command" && chunk.contains("git status") =>
                {
                    saw_input = true
                }
                _ => {}
            }
        }

        assert!(saw_selected);
        assert!(saw_input);
    }

    #[tokio::test]
    async fn codex_stream_loop_reports_missing_terminal_event() {
        let events = vec![Ok(SseEvent {
            event_type: "response.output_text.delta".into(),
            data: serde_json::json!({"delta": "partial"}),
        })];

        let stream = stream_from_events(events, false);
        let (tx, _rx) = event_bus::channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let err = consume_responses_stream(stream, &[], &tx, &cancel, None, Default::default())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("stream closed before response.completed"));
    }

    /// Planner smooshes `<system-reminder>` evidence chunks into the last
    /// `tool_result.content` (see RFC §9.1). This test pins that the
    /// Codex adapter's `build_input` (a) forwards that content verbatim
    /// as a `function_call_output` item and (b) drops the internal
    /// `evidence_id` marker so it never reaches the Responses API.
    #[test]
    fn build_input_passes_smooshed_tool_result_through_and_drops_evidence_id() {
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

        let input = build_input(&messages, &|_| String::new());

        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "call_1");
        assert_eq!(input[0]["output"].as_str(), Some(smooshed));
        assert!(
            input[0].get("evidence_id").is_none(),
            "evidence_id leaked to Codex wire: {}",
            input[0]
        );
        assert!(
            !input[0].to_string().contains("evidence_id"),
            "evidence_id string leaked anywhere in payload: {}",
            input[0]
        );
    }

    #[test]
    fn tool_result_text_body_serializes_as_plain_string_output() {
        // Text-only tool results MUST ride as a plain string to match
        // Codex's byte-for-byte wire for the common case. Regression
        // guard against accidentally wrapping in a content-items array.
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "ok".into(),
                is_error: false,
                evidence_id: None,
            }],
            origin: None,
        }];
        let input = build_input(&messages, &|_| String::new());
        assert_eq!(input[0]["output"], serde_json::json!("ok"));
    }

    #[test]
    fn tool_result_items_body_serializes_as_input_image_array() {
        use crate::core::types::{ToolResultBody, ToolResultItem};

        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
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
        }];
        let input = build_input(&messages, &|id| {
            assert_eq!(id, "img_abc");
            "BASE64DATA".into()
        });
        let output = input[0]["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "input_text");
        assert_eq!(output[0]["text"], "PNG 512x512");
        assert_eq!(output[1]["type"], "input_image");
        // Responses API consumes images as data URLs in the `image_url`
        // field — no separate base64/media_type split like Anthropic.
        assert_eq!(output[1]["image_url"], "data:image/png;base64,BASE64DATA");
    }

    #[test]
    fn tool_result_items_body_drops_unresolvable_images() {
        use crate::core::types::{ToolResultBody, ToolResultItem};

        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
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
        }];
        // Resolver returns empty for unknown id — image item dropped,
        // text item preserved. Single-entry output array.
        let input = build_input(&messages, &|_| String::new());
        let output = input[0]["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "input_text");
    }

    #[test]
    fn user_message_with_image_serializes_as_input_content_array() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "look at this".into(),
                },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    id: "img_user".into(),
                },
            ],
            origin: None,
        }];

        let input = build_input(&messages, &|id| {
            assert_eq!(id, "img_user");
            "USERBASE64".into()
        });

        let content = input[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[0]["text"], "look at this");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "data:image/png;base64,USERBASE64");
    }

    #[test]
    fn text_only_user_message_stays_flat_string_for_wire_compat() {
        let messages = vec![Message::user("hello".to_owned())];

        let input = build_input(&messages, &|_| String::new());

        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"], "hello");
    }
}
