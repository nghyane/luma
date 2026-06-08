/// Agent loop — actor that owns messages, provider, registry.
/// Receives commands from App, streams events back.
mod summary;
mod turn;

pub use summary::format_tool_summary;

use crate::core::registry::Registry;
use crate::core::session::Session;
use crate::core::types::ContentBlock;
use crate::core::types::{LatencyMode, Message, Role, ThinkingLevel};
use crate::event::{AgentCommand, Event};
use tokio::sync::mpsc;

/// Configuration for spawning an agent loop.
pub struct AgentConfig {
    pub model_id: String,
    pub source: String,
    pub system_prompt: String,
    pub thinking: ThinkingLevel,
    pub latency: LatencyMode,
    /// Capability flags from the model catalog (e.g. `"vision"`). Passed
    /// through to tool execution so tools can branch on what the model
    /// can consume.
    pub capabilities: Vec<String>,
}

/// Spawn the agent loop task. Returns a command sender.
pub fn spawn(
    config: AgentConfig,
    registry: Registry,
    event_tx: crate::event_bus::Sender,
) -> mpsc::Sender<AgentCommand> {
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let tx = event_tx.clone();
    tokio::spawn(async move {
        let result = std::panic::AssertUnwindSafe(agent_loop(config, registry, cmd_rx, event_tx));
        match futures::FutureExt::catch_unwind(result).await {
            Ok(()) => {}
            Err(e) => {
                let detail = e
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| e.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown cause".into());
                tx.send_or_log(Event::AgentError(format!("agent task panicked: {detail}")))
                    .await;
            }
        }
    });
    cmd_tx
}

async fn agent_loop(
    mut config: AgentConfig,
    mut registry: Registry,
    mut cmd_rx: mpsc::Receiver<AgentCommand>,
    event_tx: crate::event_bus::Sender,
) {
    let mut session = Session::new();
    let writer = crate::core::session::SessionWriter::spawn();

    if !config.system_prompt.is_empty() {
        session
            .messages
            .push(Message::system(config.system_prompt.clone()));
    }

    loop {
        // Wait for next command.
        let Some(cmd) = cmd_rx.recv().await else {
            break; // channel closed — shutting down
        };

        match cmd {
            AgentCommand::Chat {
                content,
                images,
                files,
                cancel,
            } => {
                // Build user message.
                let mut blocks: Vec<ContentBlock> = content
                    .into_iter()
                    .filter(|b| !matches!(b, ContentBlock::Image { id, .. } if id.is_empty()))
                    .collect();
                for f in files {
                    let ext = std::path::Path::new(&f.path)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("");
                    blocks.push(ContentBlock::Text {
                        text: format!(
                            "<file path=\"{}\">\n```{ext}\n{}\n```\n</file>",
                            f.path, f.content
                        ),
                    });
                }
                for img in images {
                    let ext = img.media_type.rsplit('/').next().unwrap_or("png");
                    let id = crate::core::session::save_image(&session.id, &img.data, ext);
                    blocks.push(ContentBlock::Image {
                        media_type: img.media_type,
                        id,
                    });
                }
                session.messages.push(Message {
                    role: Role::User,
                    content: blocks,
                    origin: None,
                });

                let turn_start = std::time::Instant::now();
                let mut deferred_cmds: Vec<AgentCommand> = Vec::new();

                // Run turn in a block so the pinned future (which borrows
                // session/config/registry) drops before post-turn code.
                let result = {
                    let turn_fut = turn::run_chat_turn(
                        &mut session,
                        &config,
                        &registry,
                        &event_tx,
                        cancel.clone(),
                        &writer,
                    );
                    tokio::pin!(turn_fut);

                    loop {
                        tokio::select! {
                            biased;
                            r = &mut turn_fut => break r,
                            Some(mid_cmd) = cmd_rx.recv() => {
                                match mid_cmd {
                                    AgentCommand::SetThinking(_)
                                    | AgentCommand::SetLatencyMode(_)
                                    | AgentCommand::SetRuntimeConfig { .. } => {
                                        deferred_cmds.push(mid_cmd);
                                    }
                                    AgentCommand::LoadSession { .. }
                                    | AgentCommand::Chat { .. } => {
                                        cancel.cancel();
                                    }
                                }
                            }
                        }
                    }
                };

                fix_orphaned_tool_uses(&mut session.messages);
                session
                    .turn_durations
                    .push(turn_start.elapsed().as_secs_f64());
                writer.enqueue(&session);
                crate::config::prefs::save_last_session(&session.id);

                match result {
                    Ok(_) => {
                        event_tx.send_or_log(Event::AgentDone).await;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("Aborted") {
                            session.messages.push(Message::system(
                                "[user interrupted the previous turn]".to_owned(),
                            ));
                        }
                        writer.enqueue(&session);
                        event_tx.send_or_log(Event::AgentError(msg)).await;
                    }
                }

                // Apply deferred commands that arrived mid-turn.
                for dc in deferred_cmds {
                    apply_config_cmd(dc, &mut config, &mut registry, &mut session);
                }
            }
            AgentCommand::LoadSession {
                session: loaded,
                is_new,
            } => {
                writer.enqueue(&session);
                session = *loaded;
                if !config.system_prompt.is_empty()
                    && !session
                        .messages
                        .first()
                        .is_some_and(|m| m.role == crate::core::types::Role::System)
                {
                    session
                        .messages
                        .insert(0, Message::system(config.system_prompt.clone()));
                }
                fix_orphaned_tool_uses(&mut session.messages);
                event_tx
                    .send_or_log(Event::SessionLoaded {
                        session: Box::new(session.clone()),
                        is_new,
                    })
                    .await;
            }
            AgentCommand::SetThinking(_)
            | AgentCommand::SetLatencyMode(_)
            | AgentCommand::SetRuntimeConfig { .. } => {
                apply_config_cmd(cmd, &mut config, &mut registry, &mut session);
            }
        }
    }
    // Channel closed — app is shutting down. Persist before exit.
    session.save();
}

/// Apply a config-only command to the agent state. Extracted so the same
/// logic serves both the idle path and deferred mid-turn commands.
fn apply_config_cmd(
    cmd: AgentCommand,
    config: &mut AgentConfig,
    registry: &mut Registry,
    session: &mut Session,
) {
    match cmd {
        AgentCommand::SetThinking(level) => {
            config.thinking = level;
        }
        AgentCommand::SetLatencyMode(mode) => {
            config.latency = mode;
        }
        AgentCommand::SetRuntimeConfig {
            model_id,
            source,
            system_prompt,
            registry: new_registry,
            thinking,
            latency,
        } => {
            config.model_id = model_id;
            config.source = source;
            config.thinking = thinking;
            config.latency = latency;
            if let (Some(system_prompt), Some(new_registry)) = (system_prompt, new_registry) {
                config.system_prompt = system_prompt.clone();
                *registry = new_registry;
                if let Some(first) = session.messages.first_mut()
                    && first.role == Role::System
                {
                    first.content = vec![ContentBlock::Text {
                        text: system_prompt,
                    }];
                } else if !system_prompt.is_empty() {
                    session.messages.insert(0, Message::system(system_prompt));
                }
            }
        }
        _ => {}
    }
}

/// Ensure every `tool_use` in assistant messages has a matching `tool_result`.
///
/// When a turn is aborted mid-execution, the assistant message with tool_use
/// blocks may already be in the history but the corresponding tool_result
/// blocks may be missing. This violates the Anthropic contract which
/// requires a matching `tool_result` for every `tool_use` in the
/// immediately following user message.
///
/// Walks backwards to find the last assistant message with tool_use blocks
/// and inserts placeholder "[aborted]" tool_result blocks into the
/// immediately following user message (creating one if needed).
fn fix_orphaned_tool_uses(messages: &mut Vec<Message>) {
    use crate::core::types::ContentBlock;

    let Some(asst_idx) = messages
        .iter()
        .rposition(|m| m.role == Role::Assistant && m.has_tool_use())
    else {
        return;
    };

    let expected_ids: Vec<String> = messages[asst_idx]
        .tool_uses()
        .map(|(id, _, _)| id.to_owned())
        .collect();

    let user_idx = if messages
        .get(asst_idx + 1)
        .is_some_and(|m| m.role == Role::User)
    {
        asst_idx + 1
    } else {
        messages.insert(
            asst_idx + 1,
            Message {
                role: Role::User,
                content: Vec::new(),
                origin: None,
            },
        );
        asst_idx + 1
    };

    let user_msg = &mut messages[user_idx];
    let existing_ids: std::collections::HashSet<String> = user_msg
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect();

    for id in expected_ids {
        if existing_ids.contains(&id) {
            continue;
        }
        user_msg.content.push(ContentBlock::ToolResult {
            tool_use_id: id,
            content: "[aborted]".into(),
            is_error: true,
            evidence_id: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{ContentBlock, MessageOrigin, Role};

    #[test]
    fn fixes_orphaned_tool_use_into_immediately_following_user_message() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "exec_command".into(),
                    input: serde_json::json!({"command": "pwd"}),
                }],
                origin: Some(MessageOrigin {
                    provider: "codex".into(),
                    model: Some("gpt-5.4".into()),
                }),
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "Output token limit hit. Resume directly.".into(),
                }],
                origin: None,
            },
        ];

        fix_orphaned_tool_uses(&mut messages);

        assert_eq!(messages.len(), 2);
        let user = &messages[1];
        assert_eq!(user.role, Role::User);
        assert_eq!(user.content.len(), 2);
        match &user.content[1] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "call_1");
                assert_eq!(content.as_text(), "[aborted]");
                assert!(*is_error);
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }
}
