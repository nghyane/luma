//! Kiro (Amazon Q) protocol — AWS Event Stream binary framing.
//!
//! Endpoint: POST /generateAssistantResponse?origin=KIRO_CLI&profileArn=<arn>
//! Response: AWS Event Stream frames, each containing a JSON payload.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
use serde_json::json;

use crate::core::provider::{Provider, StopReason, StreamRequest, StreamResponse};
use crate::core::types::{
    ContentBlock, Message, Role, ThinkingLevel, ToolResultBody, ToolResultItem, Usage,
};
use crate::util::uuid_v4;

pub struct KiroRuntime {
    model_id: String,
    base_url: String,
    token: String,
    profile_arn: Option<String>,
}

impl KiroRuntime {
    /// Create from model, gateway base URL, credential token, and optional
    /// profile ARN. `base_url` is the gateway's scheme+host with no
    /// trailing slash; the runtime appends `/generateAssistantResponse`.
    pub fn new(
        model_id: &str,
        base_url: &str,
        token: &str,
        profile_arn: Option<String>,
    ) -> Self {
        Self {
            model_id: model_id.to_owned(),
            base_url: base_url.to_owned(),
            token: token.to_owned(),
            profile_arn,
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
            .timeout(std::time::Duration::from_secs(60))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| anyhow::anyhow!("Kiro client build: {e}"))?;

        let encoded_arn = url_encode(profile_arn);
        let url = format!(
            "{}/generateAssistantResponse?origin=KIRO_CLI&profileArn={encoded_arn}",
            self.base_url
        );

        let body = build_request_body(req.messages, &self.model_id, profile_arn, req.tools);

        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let snippet: String = text.chars().take(300).collect();
            // Wrap as non-retryable so stream_with_retry doesn't loop
            return Err(anyhow::anyhow!("Kiro HTTP {status}: {snippet}"));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!("Kiro read error: {e}"))?;
        decode_event_stream(&bytes, &req)
    }
}

// =============================================================================
// Request builder
// =============================================================================

fn build_request_body(
    messages: &[Message],
    model_id: &str,
    profile_arn: &str,
    tools: &[crate::core::types::ToolSchema],
) -> serde_json::Value {
    let conversation_id = uuid_v4();
    let continuation_id = uuid_v4();

    if messages.is_empty() {
        return json!({});
    }

    let (history_msgs, current_msg) = (&messages[..messages.len() - 1], &messages[messages.len() - 1]);

    let tool_specs = build_tool_specs(tools);
    let history = build_history(history_msgs, &tool_specs, model_id);
    let current = build_current_message(current_msg, model_id, &tool_specs);

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
    tools.iter().map(|t| json!({
        "toolSpecification": {
            "name": t.name,
            "description": t.description,
            "inputSchema": { "json": t.parameters }
        }
    })).collect()
}

fn msg_text(msg: &Message) -> String {
    Message::content_text(&msg.content)
}

fn build_history(messages: &[Message], tool_specs: &[serde_json::Value], model_id: &str) -> Vec<serde_json::Value> {
    let mut result = Vec::new();
    for msg in messages {
        match msg.role {
            Role::User => {
                let tool_results = extract_tool_results(msg);
                if !tool_results.is_empty() {
                    // Tool result turn — include in history
                    result.push(json!({
                        "userInputMessage": {
                            "content": "",
                            "origin": "KIRO_CLI",
                            "modelId": model_id,
                            "userInputMessageContext": {
                                "tools": tool_specs,
                                "toolResults": tool_results,
                            }
                        }
                    }));
                } else {
                    result.push(json!({
                        "userInputMessage": {
                            "content": msg_text(msg),
                            "origin": "KIRO_CLI",
                            "modelId": model_id,
                            "userInputMessageContext": {
                                "tools": tool_specs,
                            }
                        }
                    }));
                }
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
            _ => {}
        }
    }
    result
}

fn build_current_message(msg: &Message, model_id: &str, tool_specs: &[serde_json::Value]) -> serde_json::Value {
    let tool_results = extract_tool_results(msg);
    if !tool_results.is_empty() {
        json!({
            "userInputMessage": {
                "content": "",
                "origin": "KIRO_CLI",
                "modelId": model_id,
                "userInputMessageContext": {
                    "tools": tool_specs,
                    "toolResults": tool_results,
                }
            }
        })
    } else {
        json!({
            "userInputMessage": {
                "content": msg_text(msg),
                "origin": "KIRO_CLI",
                "modelId": model_id,
                "userInputMessageContext": {
                    "tools": tool_specs,
                }
            }
        })
    }
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
            if let ContentBlock::ToolResult { tool_use_id, content, .. } = b {
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

fn decode_event_stream(data: &[u8], req: &StreamRequest<'_>) -> Result<StreamResponse> {
    let mut text = String::new();
    // (tool_use_id, name, input_buf)
    let mut tool_uses: Vec<(String, String, String)> = Vec::new();
    let mut stop_reason = StopReason::EndTurn;

    let mut pos = 0;
    while pos < data.len() {
        if pos + 12 > data.len() {
            break;
        }
        // Bounds checked above: `&data[pos..pos+4]` is exactly 4 bytes.
        let total_len =
            u32::from_be_bytes(data[pos..pos + 4].try_into().expect("4-byte slice")) as usize;
        if total_len == 0 || pos + total_len > data.len() {
            break;
        }
        let headers_len = u32::from_be_bytes(
            data[pos + 4..pos + 8]
                .try_into()
                .expect("4-byte slice"),
        ) as usize;
        let headers_end = pos + 12 + headers_len;
        let payload_end = pos + total_len - 4;

        if headers_end > payload_end || payload_end > data.len() {
            break;
        }

        let event_type = parse_event_type(&data[pos + 12..headers_end]);
        let payload = &data[headers_end..payload_end];

        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) {
            match event_type.as_deref() {
                Some("assistantResponseEvent") => {
                    if let Some(chunk) = v.get("content").and_then(|c| c.as_str()) {
                        text.push_str(chunk);
                        let _ = req.tx.try_send(crate::event::Event::Token(chunk.to_owned()));
                    }
                }
                Some("toolUseEvent") => {
                    let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_owned();
                    let tool_use_id = v
                        .get("toolUseId")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let is_stop = v.get("stop").and_then(|s| s.as_bool()).unwrap_or(false);
                    let input_chunk = v.get("input").and_then(|i| i.as_str()).unwrap_or("").to_owned();

                    if let Some(existing) = tool_uses.iter_mut().find(|(id, _, _)| id == &tool_use_id) {
                        existing.2.push_str(&input_chunk);
                    } else if !tool_use_id.is_empty() {
                        tool_uses.push((tool_use_id.clone(), name.clone(), input_chunk));
                        let _ = req.tx.try_send(crate::event::Event::ToolSelected {
                            name: name.clone(),
                        });
                    }

                    if is_stop {
                        stop_reason = StopReason::ToolUse;
                    }
                }
                _ => {}
            }
        }

        pos += total_len;
    }

    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(ContentBlock::Text { text });
    }
    for (id, name, input_str) in &tool_uses {
        let input: serde_json::Value =
            serde_json::from_str(input_str).unwrap_or_else(|_| json!({}));
        content.push(ContentBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input,
        });
    }

    Ok(StreamResponse {
        message: Message {
            role: Role::Assistant,
            content,
            origin: None,
        },
        usage: Usage::default(),
        stop_reason,
    })
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

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
