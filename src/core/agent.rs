/// Agent loop — actor that owns messages, provider, registry.
/// Receives commands from App, streams events back.
mod summary;
mod turn;

pub use summary::format_tool_summary;

use crate::core::registry::Registry;
use crate::core::session::Session;
use crate::core::types::ContentBlock;
use crate::core::types::{Message, Role, ThinkingLevel};
use crate::event::{AgentCommand, Event};
use tokio::sync::mpsc;

/// Configuration for spawning an agent loop.
pub struct AgentConfig {
    pub model_id: String,
    pub source: String,
    pub system_prompt: String,
    pub thinking: ThinkingLevel,
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
        if futures::FutureExt::catch_unwind(result).await.is_err() {
            let _ = tx
                .send(Event::AgentError("agent task panicked".into()))
                .await;
        }
    });
    cmd_tx
}

async fn agent_loop(
    mut config: AgentConfig,
    registry: Registry,
    mut cmd_rx: mpsc::Receiver<AgentCommand>,
    event_tx: crate::event_bus::Sender,
) {
    let mut session = Session::new();

    if !config.system_prompt.is_empty() {
        session
            .messages
            .push(Message::system(config.system_prompt.clone()));
    }

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            AgentCommand::Chat {
                content,
                images,
                files,
                cancel,
            } => {
                // Filter out phantom Image blocks (id="" from prompt) — real
                // images are saved below and appended with proper ids.
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
                    tool_call_id: None,
                    tool_calls: None,
                });

                let turn_start = std::time::Instant::now();
                let result =
                    turn::run_chat_turn(&mut session, &config, &registry, &event_tx, cancel).await;

                // Fix orphaned tool_use blocks left by aborted/errored turns.
                fix_orphaned_tool_uses(&mut session.messages);

                session
                    .turn_durations
                    .push(turn_start.elapsed().as_secs_f64());
                // Save after every turn — crash recovery preserves progress.
                session.save();
                crate::config::prefs::save_last_session(&session.id);

                match result {
                    Ok(_) => {
                        let _ = event_tx.send(Event::AgentDone).await;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("Aborted") {
                            session.messages.push(Message::system(
                                "[user interrupted the previous turn]".to_owned(),
                            ));
                            fix_orphaned_tool_uses(&mut session.messages);
                        }
                        session.save();
                        let _ = event_tx.send(Event::AgentError(msg)).await;
                    }
                }
            }
            AgentCommand::LoadSession {
                session: loaded,
                is_new,
            } => {
                session.save();
                session = loaded;
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
                // Repair any orphaned tool_use blocks from a crash or forced exit.
                fix_orphaned_tool_uses(&mut session.messages);
                let _ = event_tx
                    .send(Event::SessionLoaded {
                        session: Box::new(session.clone()),
                        is_new,
                    })
                    .await;
            }
            AgentCommand::SetModel { model_id, source } => {
                config.model_id = model_id;
                config.source = source;
            }
            AgentCommand::SetThinking(level) => {
                config.thinking = level;
            }
            AgentCommand::Shutdown => {
                session.save();
                break;
            }
        }
    }
}

/// Ensure every `tool_use` in assistant messages has a matching `tool_result`.
///
/// When a turn is aborted mid-execution, the assistant message with `tool_calls`
/// may already be in the history but the corresponding `Tool` result messages
/// may be missing (partially or fully). This violates the Claude API contract
/// which requires a `tool_result` for every `tool_use` in the immediately
/// following user/tool message(s).
///
/// This function scans from the end of the message list and fills in any
/// missing tool_result messages with an "[aborted]" placeholder.
fn fix_orphaned_tool_uses(messages: &mut Vec<Message>) {
    // Walk backwards to find the last assistant message with tool_calls.
    let Some(asst_idx) = messages.iter().rposition(|m| {
        m.role == Role::Assistant && m.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty())
    }) else {
        return;
    };

    let expected_ids: Vec<String> = messages[asst_idx]
        .tool_calls
        .as_ref()
        .unwrap()
        .iter()
        .map(|tc| tc.id.clone())
        .collect();

    // Collect tool_result ids that already exist after this assistant message.
    let existing_ids: std::collections::HashSet<String> = messages[asst_idx + 1..]
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    // Add placeholder results for any missing tool_use ids.
    for id in &expected_ids {
        if !existing_ids.contains(id) {
            messages.push(Message::tool(id.clone(), "[aborted]"));
        }
    }
}
