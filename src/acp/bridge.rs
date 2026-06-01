use super::transport;
use super::types::*;
use crate::core;
use crate::core::types::{ContentBlock, Role};
use crate::event::{AgentCommand, Event};
use crate::event_bus;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Run Luma in ACP server mode (stdin/stdout JSON-RPC).
pub async fn run() -> anyhow::Result<()> {
    let (req_tx, mut req_rx) = mpsc::channel::<Request>(32);
    tokio::task::spawn_blocking(move || transport::read_stdin(req_tx));

    let (event_tx, mut event_rx) = event_bus::channel();

    let mut agent_tx: Option<mpsc::Sender<AgentCommand>> = None;
    let mut session_id = String::new();
    let mut cancel: Option<CancellationToken> = None;
    let mut pending_prompt_id: Option<serde_json::Value> = None;
    // Counter for generating unique tool call IDs within a turn.
    let mut tool_call_seq: u64 = 0;
    let mut current_tool_call_id = String::new();

    loop {
        tokio::select! {
            req = req_rx.recv() => {
                let Some(req) = req else { break };
                let id = req.id.clone().unwrap_or(serde_json::Value::Null);

                match req.method.as_str() {
                    "initialize" => {
                        transport::respond(id, serde_json::json!({
                            "protocolVersion": 1,
                            "agentInfo": {
                                "name": "Luma",
                                "version": env!("CARGO_PKG_VERSION"),
                            },
                            "agentCapabilities": {
                                "loadSession": true,
                            },
                        }));
                    }

                    "session/new" => {
                        let params: SessionNewParams = serde_json::from_value(req.params)
                            .unwrap_or(SessionNewParams { cwd: ".".into() });

                        let _ = std::env::set_current_dir(&params.cwd);

                        let (sid, atx) = spawn_agent(event_tx.clone());
                        session_id = sid.clone();
                        agent_tx = Some(atx);

                        transport::respond(id, session_new_response(&sid));
                    }

                    "session/prompt" => {
                        if let Ok(params) = serde_json::from_value::<SessionPromptParams>(req.params) {
                            let text = extract_text(&params.prompt);
                            let ct = CancellationToken::new();
                            cancel = Some(ct.clone());
                            pending_prompt_id = Some(id);
                            tool_call_seq = 0;

                            if let Some(tx) = &agent_tx {
                                let _ = tx.send(AgentCommand::Chat {
                                    content: vec![crate::core::types::ContentBlock::Text { text }],
                                    images: vec![],
                                    files: vec![],
                                    cancel: ct,
                                }).await;
                            }
                        } else {
                            transport::respond_error(id, -32602, "Invalid params".into());
                        }
                    }

                    "session/load" => {
                        if let Ok(params) = serde_json::from_value::<SessionLoadParams>(req.params) {
                            match load_session(&params.session_id, &session_id, &event_tx, &mut agent_tx) {
                                Ok(sid) => {
                                    session_id = sid;
                                    transport::respond(id, serde_json::Value::Null);
                                }
                                Err(msg) => {
                                    transport::respond_error(id, -32000, msg);
                                }
                            }
                        } else {
                            transport::respond_error(id, -32602, "Invalid params".into());
                        }
                    }

                    "session/cancel" => {
                        if let Some(ct) = cancel.take() {
                            ct.cancel();
                        }
                        // cancel is a notification (no id), but respond if id present
                        if !id.is_null() {
                            transport::respond(id, serde_json::Value::Null);
                        }
                    }

                    _ => {
                        // Log unknown methods for debugging, return null.
                        eprintln!("ACP unknown method: {} id={}", req.method, id);
                        if !id.is_null() {
                            transport::respond(id, serde_json::Value::Null);
                        }
                    }
                }
            }

            event = event_rx.recv() => {
                let Some(event) = event else { break };
                let sid = &session_id;

                match event {
                    Event::Token(text) => {
                        transport::notify("session/update", serde_json::json!({
                            "sessionId": sid,
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": { "type": "text", "text": text }
                            }
                        }));
                    }

                    Event::Thinking(text) => {
                        transport::notify("session/update", serde_json::json!({
                            "sessionId": sid,
                            "update": {
                                "sessionUpdate": "agent_reasoning_chunk",
                                "content": { "type": "text", "text": text }
                            }
                        }));
                    }

                    Event::ToolStart { name, summary } => {
                        tool_call_seq += 1;
                        current_tool_call_id = format!("tc_{tool_call_seq}");
                        let kind = tool_kind(&name);
                        transport::notify("session/update", serde_json::json!({
                            "sessionId": sid,
                            "update": {
                                "sessionUpdate": "tool_call",
                                "toolCallId": current_tool_call_id,
                                "title": format!("{name}: {summary}"),
                                "kind": kind,
                                "status": "in_progress",
                            }
                        }));
                    }

                    Event::ToolOutput { chunk, .. } if !current_tool_call_id.is_empty() => {
                        transport::notify("session/update", serde_json::json!({
                            "sessionId": sid,
                            "update": {
                                "sessionUpdate": "tool_call_update",
                                "toolCallId": current_tool_call_id,
                                "status": "in_progress",
                                "content": [{ "type": "content", "content": { "type": "text", "text": chunk } }]
                            }
                        }));
                    }

                    Event::ToolEnd { name, summary } if !current_tool_call_id.is_empty() => {
                        let kind = tool_kind(&name);
                        transport::notify("session/update", serde_json::json!({
                            "sessionId": sid,
                            "update": {
                                "sessionUpdate": "tool_call_update",
                                "toolCallId": current_tool_call_id,
                                "kind": kind,
                                "status": "completed",
                                "content": [{ "type": "content", "content": { "type": "text", "text": summary } }]
                            }
                        }));
                        current_tool_call_id.clear();
                    }

                    Event::AgentDone => {
                        if let Some(id) = pending_prompt_id.take() {
                            transport::respond(id, serde_json::json!({
                                "stopReason": "end_turn"
                            }));
                        }
                    }

                    Event::AgentError(msg) => {
                        if let Some(id) = pending_prompt_id.take() {
                            transport::respond_error(id, -32000, msg);
                        }
                    }

                    // Ignore TUI-only events
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

/// Spawn the Luma agent loop, returning session ID and command sender.
fn spawn_agent(event_tx: event_bus::Sender) -> (String, mpsc::Sender<AgentCommand>) {
    let mode = crate::config::prefs::load_mode();
    let model = crate::config::models::resolve_default(mode);

    let (model_id, source, capabilities) = match &model {
        Some(m) => (m.id.clone(), m.source.clone(), m.capabilities.clone()),
        None => ("sonnet".into(), "anthropic".into(), vec![]),
    };

    let skills = crate::config::skills::discover();
    let skill_catalog = crate::config::skills::build_catalog(&skills);
    let project_instructions = crate::config::instructions::discover();
    let instructions_block = crate::config::instructions::build_instructions(&project_instructions);
    let style = crate::tool::ToolStyle::for_mode(mode, &source);
    let base_prompt = crate::config::prompt::build(mode, style);
    let env_context = crate::build_env_context();
    let system_prompt = format!("{base_prompt}\n{env_context}{skill_catalog}{instructions_block}");

    let config = core::agent::AgentConfig {
        model_id,
        source: source.clone(),
        system_prompt,
        thinking: crate::core::types::ThinkingLevel::Off,
        capabilities,
    };

    let search = resolve_search();
    let search_pref = crate::tool::search_preference_for(&source);
    let registry = crate::tool::build_registry(style, search, search_pref);

    let session = crate::core::session::Session::new();
    let sid = session.id.clone();
    let tx = core::agent::spawn(config, registry, event_tx);
    (sid, tx)
}

fn resolve_search() -> Option<crate::tool::web_search::SearchBackend> {
    use crate::tool::web_search::SearchBackend;
    if crate::config::auth::has_kiro_credential() {
        return Some(SearchBackend::Kiro);
    }
    if let Ok(key) = std::env::var("EXA_API_KEY") {
        return Some(SearchBackend::Exa { api_key: key });
    }
    if let Ok(key) = std::env::var("TAVILY_API_KEY") {
        return Some(SearchBackend::Tavily { api_key: key });
    }
    None
}

/// Build the `session/new` response with models and modes.
fn session_new_response(session_id: &str) -> serde_json::Value {
    let models = crate::config::models::all_models();
    let available: Vec<serde_json::Value> = models
        .iter()
        .map(|m| {
            serde_json::json!({
                "modelId": m.id,
                "name": format!("{} ({})", m.id, m.source),
            })
        })
        .collect();

    let current_mode = crate::config::prefs::load_mode();
    let current_model = crate::config::models::resolve_default(current_mode);
    let current_model_id = current_model.as_ref().map(|m| m.id.as_str());

    let modes: Vec<serde_json::Value> = [
        crate::config::models::AgentMode::Rush,
        crate::config::models::AgentMode::Smart,
        crate::config::models::AgentMode::Deep,
    ]
    .iter()
    .map(|m| {
        serde_json::json!({
            "id": m.as_str(),
            "name": m.as_str(),
        })
    })
    .collect();

    let mode_options: Vec<serde_json::Value> = [
        crate::config::models::AgentMode::Rush,
        crate::config::models::AgentMode::Smart,
        crate::config::models::AgentMode::Deep,
    ]
    .iter()
    .map(|m| {
        serde_json::json!({
            "value": m.as_str(),
            "name": m.as_str(),
        })
    })
    .collect();

    let model_options: Vec<serde_json::Value> = models
        .iter()
        .map(|m| {
            serde_json::json!({
                "value": m.id,
                "name": format!("{} ({})", m.id, m.source),
            })
        })
        .collect();

    serde_json::json!({
        "sessionId": session_id,
        "models": {
            "availableModels": available,
            "currentModelId": current_model_id,
        },
        "modes": {
            "availableModes": modes,
            "currentModeId": current_mode.as_str(),
        },
        "configOptions": [
            {
                "id": "mode",
                "type": "select",
                "category": "mode",
                "name": "Mode",
                "options": mode_options,
                "currentValue": current_mode.as_str(),
            },
            {
                "id": "model",
                "type": "select",
                "category": "model",
                "name": "Model",
                "options": model_options,
                "currentValue": current_model_id,
            }
        ],
    })
}

/// Map Luma tool names to ACP ToolKind enum values.
fn tool_kind(name: &str) -> &'static str {
    match name {
        "Bash" => "execute",
        "Read" => "read",
        "Write" | "Edit" | "MultiEdit" | "ApplyPatch" => "edit",
        "Glob" | "Grep" | "GhSearch" => "search",
        "WebSearch" => "search",
        "WebFetch" | "GhFile" | "GhLs" => "fetch",
        _ => "other",
    }
}

fn extract_text(blocks: &[PromptContent]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            PromptContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Load a persisted session, replay its history as ACP notifications,
/// and feed it into the agent loop so subsequent prompts continue the
/// conversation.
fn load_session(
    load_id: &str,
    _current_sid: &str,
    event_tx: &event_bus::Sender,
    agent_tx: &mut Option<mpsc::Sender<AgentCommand>>,
) -> Result<String, String> {
    let session = crate::core::session::Session::load(load_id)
        .ok_or_else(|| format!("Session '{load_id}' not found"))?;

    let sid = session.id.clone();

    // Replay history to the client before handing off to the agent.
    replay_history(&sid, &session);

    // If no agent loop yet, spawn one.
    if agent_tx.is_none() {
        let (new_sid, atx) = spawn_agent(event_tx.clone());
        *agent_tx = Some(atx);
        // Ignore new_sid — we'll load the persisted session into it.
        let _ = new_sid;
    }

    // Feed the loaded session into the agent loop.
    if let Some(tx) = agent_tx {
        let _ = tx.try_send(AgentCommand::LoadSession {
            session: Box::new(session),
            is_new: false,
        });
    }

    Ok(sid)
}

/// Stream the persisted conversation history back to the ACP client as
/// `session/update` notifications, matching the ACP session/load spec.
fn replay_history(sid: &str, session: &crate::core::session::Session) {
    let mut _tool_seq: u64 = 0;

    for msg in &session.messages {
        match msg.role {
            Role::System => {} // skip system prompt
            Role::User => {
                // Emit user messages and tool results.
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            transport::notify(
                                "session/update",
                                serde_json::json!({
                                    "sessionId": sid,
                                    "update": {
                                        "sessionUpdate": "user_message_chunk",
                                        "content": { "type": "text", "text": text }
                                    }
                                }),
                            );
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            transport::notify(
                                "session/update",
                                serde_json::json!({
                                    "sessionId": sid,
                                    "update": {
                                        "sessionUpdate": "tool_call_update",
                                        "toolCallId": tool_use_id,
                                        "status": "completed",
                                        "content": [{ "type": "content", "content": { "type": "text", "text": content.as_text() } }]
                                    }
                                }),
                            );
                        }
                        _ => {}
                    }
                }
            }
            Role::Assistant => {
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            transport::notify(
                                "session/update",
                                serde_json::json!({
                                    "sessionId": sid,
                                    "update": {
                                        "sessionUpdate": "agent_message_chunk",
                                        "content": { "type": "text", "text": text }
                                    }
                                }),
                            );
                        }
                        ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                            transport::notify(
                                "session/update",
                                serde_json::json!({
                                    "sessionId": sid,
                                    "update": {
                                        "sessionUpdate": "agent_reasoning_chunk",
                                        "content": { "type": "text", "text": thinking }
                                    }
                                }),
                            );
                        }
                        ContentBlock::CodexReasoning { summary, .. } if !summary.is_empty() => {
                            let text = summary
                                .iter()
                                .map(|part| part.text.as_str())
                                .filter(|text| !text.is_empty())
                                .collect::<Vec<_>>()
                                .join("\n");
                            if !text.is_empty() {
                                transport::notify(
                                    "session/update",
                                    serde_json::json!({
                                        "sessionId": sid,
                                        "update": {
                                            "sessionUpdate": "agent_reasoning_chunk",
                                            "content": { "type": "text", "text": text }
                                        }
                                    }),
                                );
                            }
                        }
                        ContentBlock::ToolUse { id, name, .. } => {
                            _tool_seq += 1;
                            transport::notify(
                                "session/update",
                                serde_json::json!({
                                    "sessionId": sid,
                                    "update": {
                                        "sessionUpdate": "tool_call",
                                        "toolCallId": id,
                                        "title": name,
                                        "kind": tool_kind(name),
                                        "status": "completed",
                                    }
                                }),
                            );
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
