/// Codex provider — OpenAI Responses API at chatgpt.com/backend-api/codex.
use crate::core::provider::Provider;
use crate::core::types::{
    Message, Role, ThinkingLevel, ToolCall, ToolCallFunction, ToolSchema, Usage,
};
use crate::event::Event;
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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
    fn thinking(&self) -> ThinkingLevel {
        self.thinking
    }
    fn set_thinking(&mut self, level: ThinkingLevel) {
        self.thinking = level;
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
        messages: &'a [Message],
        tools: &'a [ToolSchema],
        server_tools: &'a [serde_json::Value],
        _resolve_image: &'a crate::core::provider::ImageResolver,
        tx: mpsc::Sender<Event>,
        cancel: CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(Message, Usage)>> + Send + 'a>>
    {
        Box::pin(async move {
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

            let client = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .build()?;
            let response = crate::provider::retry::send_with_retry("codex", &tx, &cancel, || {
                let mut req = client
                    .post(CODEX_ENDPOINT)
                    .header("Content-Type", "application/json")
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .json(&body);

                if let Some(aid) = &self.account_id {
                    req = req.header("chatgpt-account-id", aid.as_str());
                }
                req.send()
            })
            .await?;

            let mut text = String::new();
            let mut tool_calls: BTreeMap<u64, ToolCall> = BTreeMap::new();
            let mut buf = String::new();
            let mut usage = Usage::default();
            let mut response = response;
            let chunk_timeout = std::time::Duration::from_secs(120);
            let mut saw_completed = false;

            loop {
                let chunk = tokio::select! {
                    c = response.chunk() => c?,
                    _ = cancel.cancelled() => { bail!("Aborted"); }
                    _ = tokio::time::sleep(chunk_timeout) => { bail!("SSE stream timeout — no data for 120s"); }
                };
                let Some(chunk) = chunk else { break };
                buf.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].to_owned();
                    buf = buf[pos + 1..].to_owned();

                    let Some(raw) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let raw = raw.trim();
                    let Ok(event) = serde_json::from_str::<serde_json::Value>(raw) else {
                        continue;
                    };

                    let event_type = event["type"].as_str().unwrap_or("");

                    crate::dbg_log!("codex event: {event_type}");
                    match event_type {
                        "response.output_text.delta" | "response.content_part.delta" => {
                            if let Some(delta) = event["delta"].as_str() {
                                text.push_str(delta);
                                if let Err(e) = tx.try_send(Event::Token(delta.to_owned())) {
                                    crate::dbg_log!("codex token send FAILED: {e}");
                                }
                            }
                        }
                        "response.reasoning_summary_text.delta"
                        | "response.reasoning_summary.delta"
                        | "response.reasoning_text.delta" => {
                            if let Some(delta) = event["delta"].as_str() {
                                let _ = tx.try_send(Event::Thinking(delta.to_owned()));
                            }
                        }
                        // Web search: show spinner on first event only
                        "response.web_search_call.in_progress" => {
                            let _ = tx.try_send(Event::WebSearchStart {
                                query: String::new(),
                            });
                        }
                        "response.web_search_call.searching" => {}
                        "response.output_item.added" => {
                            maybe_store_tool_call(
                                &mut tool_calls,
                                event["output_index"].as_u64(),
                                &event["item"],
                            );
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
                                let _ = tx.try_send(Event::WebSearchDone {
                                    query,
                                    results: vec![],
                                });
                            }
                        }
                        // Web search done OR message done
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
                            saw_completed = true;
                            // Extract tool calls and web search results from output
                            if let Some(output) = event["response"]["output"].as_array() {
                                for (idx, item) in output.iter().enumerate() {
                                    maybe_store_tool_call(&mut tool_calls, Some(idx as u64), item);
                                    if item["type"].as_str().unwrap_or("") != "function_call"
                                        && item["type"].as_str().unwrap_or("") != "web_search_call"
                                    {
                                        crate::dbg_log!(
                                            "codex unhandled event: {event_type} {}",
                                            raw.chars().take(200).collect::<String>()
                                        );
                                    }
                                }
                            }
                            // Web search results already emitted via output_item.done above.
                            // Usage
                            if let Some(u) = event["response"]["usage"].as_object() {
                                let cached = u
                                    .get("input_tokens_details")
                                    .and_then(|d| d.get("cached_tokens"))
                                    .and_then(|v| v.as_u64());
                                let input =
                                    u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                                // Codex input_tokens includes cached — subtract to match Claude semantics
                                let non_cached = input.saturating_sub(cached.unwrap_or(0));
                                let u_data = Usage {
                                    input_tokens: non_cached,
                                    output_tokens: u
                                        .get("output_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0),
                                    cache_read: cached,
                                    cache_write: None,
                                };
                                usage = u_data.clone();
                                let _ = tx.try_send(Event::Usage(u_data));
                            }
                        }
                        _ => {}
                    }
                }
            }

            let mut msg = Message::assistant(text);
            let tool_calls: Vec<_> = tool_calls
                .into_values()
                .filter(|tc| !tc.id.is_empty() && !tc.function.name.is_empty())
                .collect();
            // Stream ended without response.completed — network cut, server error, etc.
            // Return partial content so the turn is not silently lost.
            if !saw_completed && msg.text().is_empty() && tool_calls.is_empty() {
                bail!("Codex stream ended with no content (missing response.completed)");
            }
            if !tool_calls.is_empty() {
                msg.tool_calls = Some(tool_calls);
            }
            Ok((msg, usage))
        })
    }
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
