use super::Action;
/// App commands — slash commands, mode/model selection, session resume.
use super::state::PickerMode;
use crate::config::models::{self, AgentMode};
use crate::core::types::ThinkingLevel;
use crate::event::AgentCommand;
use crate::tui::theme::palette;

impl super::App {
    pub(super) fn handle_command(&mut self, cmd: &str) -> Action {
        match cmd {
            "new" => {
                if let Some(tx) = &self.agent.tx {
                    let _ = tx.try_send(AgentCommand::Reset);
                }
                self.enter_chat();
                self.doc.clear();
                self.view.clear();
                self.doc.divider();
                self.doc.info("new thread started");
                self.doc.divider();
                self.ui.status.reset_cache();
                self.sync_prompt_commands();
                Action::Render
            }
            "model" => {
                let all = models::all_models();
                if all.is_empty() {
                    self.doc.error("no models — run 'luma sync'");
                } else {
                    self.config.picker_mode = PickerMode::Model;
                    let current = self
                        .config
                        .model
                        .as_ref()
                        .map(|m| m.id.as_str())
                        .unwrap_or("");
                    self.ui
                        .picker
                        .open(all.iter().map(|m| m.id.clone()).collect(), current);
                }
                Action::Render
            }
            "sessions" => {
                let sessions = crate::core::session::list_sessions();
                if sessions.is_empty() {
                    self.doc.info("no sessions yet");
                } else {
                    self.config.picker_mode = PickerMode::Session;
                    let items: Vec<String> = sessions
                        .iter()
                        .map(|s| {
                            let title = if s.title.is_empty() {
                                "(untitled)"
                            } else {
                                &s.title
                            };
                            let preview = if s.last_preview.is_empty() {
                                String::new()
                            } else {
                                format!(" • {}", s.last_preview)
                            };
                            format!("{} — {} ({} msgs){}", s.id, title, s.message_count, preview)
                        })
                        .collect();
                    self.ui.picker.open(items, "");
                }
                Action::Render
            }
            "resume" => {
                if let Some(id) = crate::config::prefs::load_last_session() {
                    self.resume_session(&id);
                } else {
                    self.doc.info("no previous session");
                }
                Action::Render
            }
            "exit" => Action::Quit,
            _ => {
                self.doc.warn(&format!("unknown command: /{cmd}"));
                Action::Render
            }
        }
    }

    pub(super) fn select_model(&mut self, model_id: &str) {
        let all = models::all_models();
        if let Some(m) = all.iter().find(|m| m.id == model_id) {
            self.config.model = Some(m.clone());
            crate::config::prefs::save_mode_model(self.config.mode, model_id);
            if let Some(tx) = &self.agent.tx {
                let _ = tx.try_send(AgentCommand::SetModel {
                    model_id: m.id.clone(),
                    source: m.source.clone(),
                });
            }
            self.update_status();
        }
    }

    pub(super) fn quick_cycle_mode(&mut self) {
        self.apply_mode(self.config.mode.next());
    }

    fn apply_mode(&mut self, new_mode: AgentMode) {
        if new_mode == self.config.mode {
            return;
        }
        self.config.mode = new_mode;
        self.config.model = models::resolve_default(self.config.mode);
        crate::config::prefs::save_mode(self.config.mode);
        // Cancel in-flight turn before shutting down — prevents orphan streaming.
        if let Some(c) = self.agent.cancel.take() {
            c.cancel();
        }
        self.agent.state = super::state::RunState::Idle;
        if let Some(tx) = self.agent.tx.take() {
            let _ = tx.try_send(AgentCommand::Shutdown);
        }
        self.enter_chat();
        self.doc.clear();
        self.view.clear();
        self.doc.divider_with_label(self.config.mode.as_str());
        self.update_status();
        self.sync_prompt_commands();
    }

    pub(super) fn resume_session(&mut self, picker_id: &str) {
        let session_id = picker_id.split(" — ").next().unwrap_or(picker_id).trim();
        let Some(session) = crate::core::session::Session::load(session_id) else {
            self.doc.error("session not found");
            return;
        };

        self.ensure_agent_loop();
        if let Some(tx) = &self.agent.tx {
            let _ = tx.try_send(AgentCommand::LoadSession {
                session: session.clone(),
            });
        }
        self.enter_chat();
        self.doc.clear();
        self.view.clear();
        self.doc.divider();
        let title = if session.title.is_empty() {
            "(untitled)"
        } else {
            &session.title
        };
        self.doc.info(&format!("resumed: {title}"));
        self.doc.divider();
        self.render_history(&session.messages, &session.turn_durations);

        let u = &session.usage;
        self.ui.status.set_cache(u.cache_read, u.cache_write);
        let total = if u.input_tokens + u.output_tokens + u.cache_read + u.cache_write > 0 {
            u.input_tokens + u.cache_read + u.cache_write + u.output_tokens
        } else {
            session
                .messages
                .iter()
                .map(|m| m.text().len())
                .sum::<usize>() as u64
                / 4
        };
        let ctx = self
            .config
            .model
            .as_ref()
            .map(|m| models::context_window(&m.id))
            .unwrap_or(200_000);
        let pct = ((total as f64 / ctx as f64) * 100.0).min(100.0) as u8;
        self.ui.status.set_context(total, pct);
        self.sync_prompt_commands();
    }

    pub(super) fn render_history(
        &mut self,
        messages: &[crate::core::types::Message],
        turn_durations: &[f64],
    ) {
        use crate::core::types::Role;

        const MAX_RENDER_TURNS: usize = 6;
        let mut turn_starts = Vec::new();
        for (i, msg) in messages.iter().enumerate() {
            if msg.role == Role::User {
                turn_starts.push(i);
            }
        }
        let skip_turns = turn_starts.len().saturating_sub(MAX_RENDER_TURNS);
        let render_from = if skip_turns > 0 {
            turn_starts[skip_turns]
        } else {
            0
        };

        if skip_turns > 0 {
            self.doc.info(&format!(
                "({skip_turns} earlier turns hidden, showing last {MAX_RENDER_TURNS})"
            ));
            self.doc.divider();
        }

        let mut turn_idx: usize = 0;
        let mut seen_user = false;
        for (i, msg) in messages.iter().enumerate() {
            match msg.role {
                Role::System => {}
                Role::User => {
                    turn_idx += 1;
                    if i < render_from {
                        continue;
                    }
                    if seen_user {
                        self.turn_divider(turn_durations, turn_idx.wrapping_sub(2));
                    }
                    seen_user = true;
                    self.doc.user_message(&msg.content);
                }
                Role::Assistant => {
                    if i < render_from {
                        continue;
                    }
                    if msg.has_text() {
                        self.doc.assistant_message(&msg.text());
                    }
                    if let Some(tcs) = &msg.tool_calls {
                        for tc in tcs {
                            let args: serde_json::Value =
                                serde_json::from_str(&tc.function.arguments)
                                    .unwrap_or(serde_json::Value::Null);
                            let summary =
                                crate::core::agent::format_tool_summary(&tc.function.name, &args);
                            self.doc.tool_history(&tc.function.name, &summary);
                        }
                    }
                }
                Role::Tool => {}
            }
        }
        if seen_user {
            self.turn_divider(turn_durations, turn_idx.wrapping_sub(1));
        }
    }

    fn turn_divider(&mut self, durations: &[f64], idx: usize) {
        self.doc.newline();
        if let Some(&dur) = durations.get(idx) {
            let d = std::time::Duration::from_secs_f64(dur);
            self.doc.divider_with_label(&super::format_duration(d));
        } else {
            self.doc.divider();
        }
    }

    pub(super) fn cycle_thinking(&mut self) {
        self.config.thinking = self.config.thinking.next();
        if let Some(tx) = &self.agent.tx {
            let _ = tx.try_send(AgentCommand::SetThinking(self.config.thinking));
        }
        crate::config::prefs::save_thinking(self.config.thinking);
        self.update_status();
        let label = match self.config.thinking {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
        };
        self.ui.status.set_thinking_level(label);
    }

    pub(super) fn update_status(&mut self) {
        let mode_color = match self.config.mode {
            AgentMode::Rush => palette::MODE_RUSH,
            AgentMode::Smart => palette::MODE_SMART,
            AgentMode::Deep => palette::MODE_DEEP,
        };
        self.ui
            .status
            .set_mode(self.config.mode.as_str(), mode_color);
        self.ui.status.set_model(
            self.config
                .model
                .as_ref()
                .map(|m| m.id.as_str())
                .unwrap_or("none"),
        );
        let provider = self
            .config
            .model
            .as_ref()
            .map(|m| match m.source.as_str() {
                "anthropic" => "Anthropic",
                "codex" => "OpenAI",
                _ => &m.source,
            })
            .unwrap_or("");
        self.ui.status.set_provider(provider);
    }

    /// Sync command visibility based on current document state.
    pub(super) fn sync_prompt_commands(&mut self) {
        let is_new_thread = !self.doc.has_user_content();
        self.ui.prompt.set_command_visible("resume", is_new_thread);
        self.ui.prompt.set_command_visible("new", !is_new_thread);
    }
}
