/// Codex provider — OpenAI Responses API at chatgpt.com/backend-api/codex.
use crate::core::provider::{Provider, StopReason, StreamRequest, StreamResponse};
use crate::core::types::{
    Message, Role, ThinkingLevel, ToolCall, ToolCallFunction, ToolSchema, Usage,
};
use crate::event::Event;
use crate::event_bus::Sender as EventSender;
use crate::provider::json_stream::{JsonStringExtractor, streamable_arg_for};
use anyhow::Result;
use std::collections::BTreeMap;

const CODEX_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";

/// Codex provider using the Responses API.
pub struct CodexProvider {
    model: String,
    api_key: String,
    account_id: Option<String>,
    thinking: ThinkingLevel,
    session_id: Option<String>,
}

impl CodexProvider {
    /// Create with model, token, optional account ID, and session ID for cache routing.
    pub fn new(model: &str, api_key: &str, account_id: Option<String>, session_id: &str) -> Self {
        Self {
            model: model.to_owned(),
            api_key: api_key.to_owned(),
            account_id,
            thinking: ThinkingLevel::Low,
            session_id: Some(session_id.to_owned()),
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

    /// Codex Responses API at `chatgpt.com/backend-api/codex/responses` does
    /// not accept a `max_output_tokens` field — codex-rs itself omits it
    /// (see `codex-rs/codex-api/src/common.rs:ResponsesApiRequest`). An
    /// escalation retry would re-run the same request and hit the same
    /// limit, so we opt out of escalation entirely.
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

            let auth_header = format!("Bearer {}", self.api_key);
            let mut header_vec: Vec<(&str, &str)> = vec![("Authorization", &auth_header)];
            if let Some(aid) = &self.account_id {
                header_vec.push(("chatgpt-account-id", aid.as_str()));
            }

            let mut text = String::new();
            let mut tool_calls: BTreeMap<u64, ToolCall> = BTreeMap::new();
            // Per-tool JSON string extractors for streamable args, keyed by
            // output_index. Constructed lazily when we first see the tool name.
            let mut arg_extractors: BTreeMap<u64, JsonStringExtractor> = BTreeMap::new();
            // Track which output indices we've already attempted to set up
            // extractors for (so we don't re-check on every delta).
            let mut extractor_probed: std::collections::BTreeSet<u64> =
                std::collections::BTreeSet::new();
            let mut usage = Usage::default();
            let mut saw_terminal = false;
            let mut incomplete_reason = String::new();
            let mut failure_error: Option<anyhow::Error> = None;

            let mut stream = crate::provider::sse::post_sse(
                "codex",
                CODEX_ENDPOINT,
                &header_vec,
                &body,
                &tx,
                &cancel,
            )
            .await?;

            'outer: while let Some(event_result) = stream.next().await {
                let sse_event = event_result?;
                let event = sse_event.data;
                let event_type = sse_event.event_type.as_str();

                crate::dbg_log!("codex event: {event_type}");
                match event_type {
                    "response.output_text.delta" | "response.content_part.delta" => {
                        if let Some(delta) = event["delta"].as_str() {
                            text.push_str(delta);
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
                    // Web search: show spinner on first event only
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
                            &mut tool_calls,
                            event["output_index"].as_u64(),
                            &event["item"],
                        );
                        // Signal tool block creation immediately so the UI
                        // shows a pending card during the gap between the
                        // function_call start and the first arguments delta.
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
                            let entry = tool_calls.entry(idx).or_insert_with(|| ToolCall {
                                id: String::new(),
                                r#type: "function".into(),
                                function: ToolCallFunction {
                                    name: String::new(),
                                    arguments: String::new(),
                                },
                            });
                            entry.function.arguments.push_str(delta);

                            // Lazily install an extractor for this tool's
                            // streamable arg the first time we see its name.
                            if !extractor_probed.contains(&idx) && !entry.function.name.is_empty() {
                                extractor_probed.insert(idx);
                                if let Some(field) = streamable_arg_for(tools, &entry.function.name)
                                {
                                    arg_extractors.insert(idx, JsonStringExtractor::new(field));
                                }
                            }

                            // Need the tool name before the mutable borrow on
                            // arg_extractors, since ToolInput needs to clone it.
                            let tool_name = entry.function.name.clone();
                            if let Some(ex) = arg_extractors.get_mut(&idx) {
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
                            &mut tool_calls,
                            event["output_index"].as_u64(),
                            &event["item"],
                        );

                        let item_type = event["item"]["type"].as_str().unwrap_or("");
                        crate::dbg_log!("codex output_item.done type={item_type}");
                        if item_type == "web_search_call" {
                            let query = event["item"]["action"]["query"]
                                .as_str()
                                .unwrap_or("")
                                .to_owned();
                            crate::dbg_log!("codex web_search done query={query}");
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
                        saw_terminal = true;
                        if let Some(output) = event["response"]["output"].as_array() {
                            for (idx, item) in output.iter().enumerate() {
                                maybe_store_tool_call(&mut tool_calls, Some(idx as u64), item);
                            }
                        }
                        record_usage(&event["response"]["usage"], &mut usage, &tx).await;
                        break 'outer;
                    }
                    "response.incomplete" => {
                        // Mirror codex-rs: incomplete is a terminal state,
                        // usually fatal. We special-case `max_output_tokens`
                        // to return StopReason::MaxTokens so turn.rs can
                        // surface a clear error.
                        saw_terminal = true;
                        incomplete_reason = event["response"]["incomplete_details"]["reason"]
                            .as_str()
                            .unwrap_or("unknown")
                            .to_owned();
                        if let Some(output) = event["response"]["output"].as_array() {
                            for (idx, item) in output.iter().enumerate() {
                                maybe_store_tool_call(&mut tool_calls, Some(idx as u64), item);
                            }
                        }
                        record_usage(&event["response"]["usage"], &mut usage, &tx).await;
                        break 'outer;
                    }
                    "response.failed" => {
                        // Terminal error from server (context_length_exceeded,
                        // server error, etc.). Mirror codex-rs classification.
                        saw_terminal = true;
                        let err_code = event["response"]["error"]["code"].as_str().unwrap_or("");
                        let err_msg = event["response"]["error"]["message"]
                            .as_str()
                            .unwrap_or("unknown error");
                        failure_error = Some(if err_code == "context_length_exceeded" {
                            anyhow::anyhow!(
                                "codex context window exceeded: {err_msg}. \
                                 Try /compact or switch model."
                            )
                        } else {
                            anyhow::anyhow!("codex response.failed ({err_code}): {err_msg}")
                        });
                        break 'outer;
                    }
                    _ => {}
                }
            }

            // response.failed: surface the classified error. Mirrors codex-rs,
            // which treats all failed events as fatal (context_length, quota,
            // server error, etc.).
            if let Some(err) = failure_error {
                return Err(err);
            }

            let mut msg = Message::assistant(text);
            let tool_calls: Vec<_> = tool_calls
                .into_values()
                .filter(|tc| !tc.id.is_empty() && !tc.function.name.is_empty())
                .collect();

            // Stream ended without any terminal event (completed / incomplete
            // / failed). Mirrors codex-rs "stream closed before response.completed".
            if !saw_terminal {
                return Err(crate::provider::sse::StreamInterrupted(
                    "Codex stream closed before response.completed".into(),
                )
                .into());
            }

            if !tool_calls.is_empty() {
                msg.tool_calls = Some(tool_calls);
            }

            // response.incomplete with reason other than max_output_tokens is
            // fatal. max_output_tokens stays non-fatal so turn.rs can escalate
            // (though for Codex the escalation currently has no body effect —
            // see note above about ResponsesApiRequest not supporting the field).
            let stop_reason = if incomplete_reason.is_empty() {
                StopReason::EndTurn
            } else if incomplete_reason == "max_output_tokens" {
                StopReason::MaxTokens
            } else {
                anyhow::bail!(
                    "codex response.incomplete (reason={incomplete_reason}). \
                     Try again or switch model."
                );
            };

            Ok(StreamResponse {
                message: msg,
                usage,
                stop_reason,
            })
        })
    }
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
    tool_calls: &mut BTreeMap<u64, ToolCall>,
    output_index: Option<u64>,
    item: &serde_json::Value,
) {
    if item["type"].as_str().unwrap_or("") != "function_call" {
        return;
    }
    let Some(idx) = output_index else { return };
    let entry = tool_calls.entry(idx).or_insert_with(|| ToolCall {
        id: String::new(),
        r#type: "function".into(),
        function: ToolCallFunction {
            name: String::new(),
            arguments: String::new(),
        },
    });
    if let Some(call_id) = item["call_id"].as_str()
        && !call_id.is_empty()
    {
        entry.id = call_id.to_owned();
    }
    if let Some(name) = item["name"].as_str()
        && !name.is_empty()
    {
        entry.function.name = name.to_owned();
    }
    if let Some(arguments) = item["arguments"].as_str()
        && !arguments.is_empty()
        && entry.function.arguments.is_empty()
    {
        entry.function.arguments = arguments.to_owned();
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
                input.push(serde_json::json!({"role": "user", "content": msg.text()}));
            }
            Role::Assistant => {
                if let Some(tcs) = &msg.tool_calls {
                    for tc in tcs {
                        input.push(serde_json::json!({
                            "type": "function_call",
                            "name": tc.function.name,
                            "call_id": tc.id,
                            "arguments": tc.function.arguments,
                        }));
                    }
                }
                if msg.has_text() {
                    input.push(serde_json::json!({"role": "assistant", "content": msg.text()}));
                }
            }
            Role::Tool => {
                input.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": msg.tool_call_id.as_deref().unwrap_or(""),
                    "output": msg.text(),
                }));
            }
            _ => {}
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
        entry
            .function
            .arguments
            .push_str("{\"command\":\"git status\"}");

        assert_eq!(entry.id, "call_1");
        assert_eq!(entry.function.name, "exec_command");
        assert_eq!(entry.function.arguments, "{\"command\":\"git status\"}");
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
        assert_eq!(entry.function.arguments, "{\"command\":\"pwd\"}");
    }
}
