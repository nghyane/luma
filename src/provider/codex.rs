/// Codex provider — OpenAI Responses API at chatgpt.com/backend-api/codex.
use crate::config::auth::{CODEX_ORIGINATOR, codex_user_agent, resolve_installation_id};
use crate::core::provider::{Provider, StopReason, StreamRequest, StreamResponse};
use crate::core::types::{ContentBlock, Message, Role, ThinkingLevel, ToolSchema, Usage};
use crate::event::Event;
use crate::event_bus::Sender as EventSender;
use crate::provider::json_stream::{JsonStringExtractor, streamable_arg_for};
use anyhow::Result;
use std::collections::BTreeMap;

const CODEX_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";

/// Per-output-index tool-call accumulator used while the Codex Responses
/// stream is in flight. Converted to `ContentBlock::ToolUse` at commit time.
#[derive(Default, Clone, Debug)]
struct PendingTool {
    id: String,
    name: String,
    /// Raw accumulated argument bytes — parsed once on commit.
    arguments: String,
}

/// Codex provider using the Responses API.
pub struct CodexProvider {
    model: String,
    api_key: String,
    account_id: Option<String>,
    thinking: ThinkingLevel,
    session_id: Option<String>,
    account_label: String,
}

impl CodexProvider {
    /// Create with model, token, optional account ID, session ID for cache
    /// routing, and pool account label.
    pub fn new(
        model: &str,
        api_key: &str,
        account_id: Option<String>,
        session_id: &str,
        account_label: &str,
    ) -> Self {
        Self {
            model: model.to_owned(),
            api_key: api_key.to_owned(),
            account_id,
            thinking: ThinkingLevel::Low,
            session_id: Some(session_id.to_owned()),
            account_label: account_label.to_owned(),
        }
    }
}

impl Provider for CodexProvider {
    fn name(&self) -> &str {
        "codex"
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
                resolve_image: _,
                // Ignored — see `supports_max_tokens_override` impl above.
                max_tokens_override: _,
                tx,
                cancel,
            } = req;
            let system = extract_system(messages);
            let input = build_input(messages);
            let mut api_tools = to_api_tools(tools);

            // Append server-side tools
            for st in server_tools {
                api_tools.push(st.clone());
            }

            let mut body = serde_json::json!({
                "model": self.model,
                "input": input,
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
            // `client_metadata.x-codex-installation-id` matches
            // codex-rs/core/src/client.rs::build_responses_request.
            if let Some(installation_id) = resolve_installation_id() {
                body["client_metadata"] = serde_json::json!({
                    "x-codex-installation-id": installation_id,
                });
            }

            // Reasoning: map ThinkingLevel → effort + summary for Responses API
            let effort = match self.thinking {
                ThinkingLevel::Off => None,
                ThinkingLevel::Low => Some("low"),
                ThinkingLevel::Medium => Some("medium"),
                ThinkingLevel::High => Some("high"),
            };
            if let Some(effort) = effort {
                body["reasoning"] = serde_json::json!({
                    "effort": effort,
                    "summary": "auto",
                });
            }

            // Headers match `codex-rs/core/src/client.rs` +
            // `codex-rs/login/src/auth/default_client.rs::default_headers`.
            // Any drift breaks the backend's first-party client check.
            let auth_header = format!("Bearer {}", self.api_key);
            let user_agent = codex_user_agent();
            let mut header_vec: Vec<(&str, &str)> = vec![
                ("Authorization", &auth_header),
                ("originator", CODEX_ORIGINATOR),
                ("User-Agent", user_agent.as_str()),
            ];
            if let Some(aid) = &self.account_id {
                header_vec.push(("chatgpt-account-id", aid.as_str()));
            }
            if let Some(sid) = &self.session_id {
                header_vec.push(("session_id", sid.as_str()));
            }

            let mut stream = crate::provider::sse::post_sse(
                "codex",
                &self.account_label,
                CODEX_ENDPOINT,
                &header_vec,
                &body,
                &tx,
                &cancel,
            )
            .await?;
            run_codex_stream_loop(&mut stream, tools, &tx).await
        })
    }
}

struct CodexStreamState {
    text: String,
    tool_calls: BTreeMap<u64, PendingTool>,
    arg_extractors: BTreeMap<u64, JsonStringExtractor>,
    extractor_probed: std::collections::BTreeSet<u64>,
    usage: Usage,
    saw_terminal: bool,
    incomplete_reason: String,
    failure_error: Option<anyhow::Error>,
}

impl CodexStreamState {
    fn new() -> Self {
        Self {
            text: String::new(),
            tool_calls: BTreeMap::new(),
            arg_extractors: BTreeMap::new(),
            extractor_probed: std::collections::BTreeSet::new(),
            usage: Usage::default(),
            saw_terminal: false,
            incomplete_reason: String::new(),
            failure_error: None,
        }
    }
}

async fn run_codex_stream_loop(
    stream: &mut crate::provider::sse::SseEventStream,
    tools: &[ToolSchema],
    tx: &EventSender,
) -> Result<StreamResponse> {
    let mut state = CodexStreamState::new();

    'outer: while let Some(event_result) = stream.next().await {
        let sse_event = event_result?;
        let event = sse_event.data;
        let event_type = sse_event.event_type.as_str();

        crate::dbg_log!("codex event: {event_type}");
        match event_type {
            "response.output_text.delta" | "response.content_part.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    state.text.push_str(delta);
                    let _ = tx.send(Event::Token(delta.to_owned())).await;
                }
            }
            "response.reasoning_summary_text.delta"
            | "response.reasoning_summary.delta"
            | "response.reasoning_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    let _ = tx.send(Event::Thinking(delta.to_owned())).await;
                }
            }
            "response.web_search_call.in_progress" => {
                let _ = tx
                    .send(Event::WebSearchStart {
                        query: String::new(),
                    })
                    .await;
            }
            "response.web_search_call.searching" => {}
            "response.output_item.added" => {
                maybe_store_tool_call(
                    &mut state.tool_calls,
                    event["output_index"].as_u64(),
                    &event["item"],
                );
                if event["item"]["type"].as_str() == Some("function_call")
                    && let Some(name) = event["item"]["name"].as_str()
                    && !name.is_empty()
                {
                    let _ = tx
                        .send(Event::ToolSelected {
                            name: name.to_owned(),
                        })
                        .await;
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(idx) = event["output_index"].as_u64()
                    && let Some(delta) = event["delta"].as_str()
                {
                    let entry = state.tool_calls.entry(idx).or_default();
                    entry.arguments.push_str(delta);

                    if !state.extractor_probed.contains(&idx) && !entry.name.is_empty() {
                        state.extractor_probed.insert(idx);
                        if let Some(field) = streamable_arg_for(tools, &entry.name) {
                            state
                                .arg_extractors
                                .insert(idx, JsonStringExtractor::new(field));
                        }
                    }

                    let tool_name = entry.name.clone();
                    if let Some(ex) = state.arg_extractors.get_mut(&idx) {
                        let chunk = ex.feed(delta);
                        if !chunk.is_empty() {
                            let _ = tx
                                .send(Event::ToolInput {
                                    name: tool_name,
                                    chunk,
                                })
                                .await;
                        }
                    }
                }
            }
            "response.function_call_arguments.done" | "response.output_item.done" => {
                maybe_store_tool_call(
                    &mut state.tool_calls,
                    event["output_index"].as_u64(),
                    &event["item"],
                );

                let item_type = event["item"]["type"].as_str().unwrap_or("");
                if item_type == "web_search_call" {
                    let query = event["item"]["action"]["query"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    let _ = tx
                        .send(Event::WebSearchDone {
                            query,
                            results: vec![],
                        })
                        .await;
                }
            }
            "response.web_search_call.completed"
            | "response.created"
            | "response.in_progress"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary.part.added"
            | "response.reasoning_summary.part.done"
            | "response.reasoning_text.done" => {}
            "response.completed" => {
                state.saw_terminal = true;
                if let Some(output) = event["response"]["output"].as_array() {
                    for (idx, item) in output.iter().enumerate() {
                        maybe_store_tool_call(&mut state.tool_calls, Some(idx as u64), item);
                    }
                }
                record_usage(&event["response"]["usage"], &mut state.usage, tx).await;
                break 'outer;
            }
            "response.incomplete" => {
                state.saw_terminal = true;
                state.incomplete_reason = event["response"]["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_owned();
                if let Some(output) = event["response"]["output"].as_array() {
                    for (idx, item) in output.iter().enumerate() {
                        maybe_store_tool_call(&mut state.tool_calls, Some(idx as u64), item);
                    }
                }
                record_usage(&event["response"]["usage"], &mut state.usage, tx).await;
                break 'outer;
            }
            "response.failed" => {
                state.saw_terminal = true;
                let err_code = event["response"]["error"]["code"].as_str().unwrap_or("");
                let err_msg = event["response"]["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown error");
                state.failure_error = Some(if err_code == "context_length_exceeded" {
                    anyhow::anyhow!(
                        "codex context window exceeded: {err_msg}. Try /compact or switch model."
                    )
                } else {
                    anyhow::anyhow!("codex response.failed ({err_code}): {err_msg}")
                });
                break 'outer;
            }
            _ => {}
        }
    }

    if let Some(err) = state.failure_error {
        return Err(err);
    }
    if !state.saw_terminal {
        return Err(crate::provider::sse::StreamInterrupted(
            "Codex stream closed before response.completed".into(),
        )
        .into());
    }

    // Build ordered content: text first, then tool_use blocks by ascending
    // output_index. Codex Responses streams text via `output_text.delta`
    // and tool calls via `output_item.added` / function_call_arguments —
    // per-token interleave isn't exposed, so this text-then-tools order
    // is the closest faithful reconstruction.
    let mut content: Vec<ContentBlock> = Vec::new();
    if !state.text.is_empty() {
        content.push(ContentBlock::Text { text: state.text });
    }
    for (_, tool) in state.tool_calls {
        if tool.id.is_empty() || tool.name.is_empty() {
            continue;
        }
        let input: serde_json::Value = if tool.arguments.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&tool.arguments).unwrap_or_else(|_| serde_json::json!({}))
        };
        content.push(ContentBlock::ToolUse {
            id: tool.id,
            name: tool.name,
            input,
        });
    }

    let stop_reason = if state.incomplete_reason.is_empty() {
        StopReason::EndTurn
    } else if state.incomplete_reason == "max_output_tokens" {
        StopReason::MaxTokens
    } else {
        anyhow::bail!(
            "codex response.incomplete (reason={}). Try again or switch model.",
            state.incomplete_reason
        );
    };

    Ok(StreamResponse {
        message: Message {
            role: Role::Assistant,
            content,
        },
        usage: state.usage,
        stop_reason,
    })
}

/// Parse `response.usage` JSON into [`Usage`] and emit a Usage event.
async fn record_usage(usage_val: &serde_json::Value, usage: &mut Usage, tx: &EventSender) {
    let Some(u) = usage_val.as_object() else {
        return;
    };
    let cached = u
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64());
    let input = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    // Codex input_tokens includes cached — subtract to match Claude semantics
    let non_cached = input.saturating_sub(cached.unwrap_or(0));
    let u_data = Usage {
        input_tokens: non_cached,
        output_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cache_read: cached,
        cache_write: None,
    };
    *usage = u_data.clone();
    let _ = tx.send(Event::Usage(u_data)).await;
}

fn maybe_store_tool_call(
    tool_calls: &mut BTreeMap<u64, PendingTool>,
    output_index: Option<u64>,
    item: &serde_json::Value,
) {
    if item["type"].as_str().unwrap_or("") != "function_call" {
        return;
    }
    let Some(idx) = output_index else { return };
    let entry = tool_calls.entry(idx).or_default();
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

fn extract_system(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn build_input(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut input = Vec::new();
    for msg in messages {
        if msg.role == Role::System {
            continue;
        }
        match msg.role {
            Role::User => {
                // Tool results on a user message become `function_call_output`
                // items — one per result block, unnested.
                let mut had_result = false;
                for block in &msg.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                    {
                        input.push(serde_json::json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": content,
                        }));
                        had_result = true;
                    }
                }
                if had_result {
                    continue;
                }
                // Plain user message — Codex Responses API wants a flat string
                // body, images are delivered via a different path.
                input.push(serde_json::json!({
                    "role": "user",
                    "content": msg.text(),
                }));
            }
            Role::Assistant => {
                // Walk content blocks in order: ToolUse → function_call
                // item; Text → assistant content. Thinking blocks aren't
                // representable on the Codex wire and are dropped.
                for block in &msg.content {
                    match block {
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
    fn stores_tool_call_from_incremental_codex_events() {
        let mut tool_calls = BTreeMap::new();
        let item = serde_json::json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "exec_command",
            "arguments": ""
        });

        maybe_store_tool_call(&mut tool_calls, Some(0), &item);
        let entry = tool_calls.get_mut(&0).unwrap();
        entry.arguments.push_str("{\"command\":\"git status\"}");

        assert_eq!(entry.id, "call_1");
        assert_eq!(entry.name, "exec_command");
        assert_eq!(entry.arguments, "{\"command\":\"git status\"}");
    }

    #[test]
    fn completed_snapshot_fills_missing_codex_tool_fields() {
        let mut tool_calls = BTreeMap::new();
        let partial = serde_json::json!({"type": "function_call", "name": "exec_command"});
        let done = serde_json::json!({
            "type": "function_call",
            "call_id": "call_2",
            "name": "exec_command",
            "arguments": "{\"command\":\"pwd\"}"
        });

        maybe_store_tool_call(&mut tool_calls, Some(1), &partial);
        maybe_store_tool_call(&mut tool_calls, Some(1), &done);

        let entry = tool_calls.get(&1).unwrap();
        assert_eq!(entry.id, "call_2");
        assert_eq!(entry.arguments, "{\"command\":\"pwd\"}");
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

        let mut stream = stream_from_events(events, true);
        let (tx, mut rx) = event_bus::channel();
        let result = run_codex_stream_loop(&mut stream, &[], &tx).await.unwrap();

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

        let mut stream = stream_from_events(events, true);
        let (tx, mut rx) = event_bus::channel();
        let result = run_codex_stream_loop(&mut stream, &[tool], &tx)
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

        let mut stream = stream_from_events(events, false);
        let (tx, _rx) = event_bus::channel();
        let err = run_codex_stream_loop(&mut stream, &[], &tx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("stream closed before response.completed"));
    }
}
