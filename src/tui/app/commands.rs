use super::Action;
/// App commands — slash commands, mode/model selection, session resume.
use super::state::PickerMode;
use crate::config::auth::{self, AccountHealth, AuthProvider};
use crate::config::models::{self, AgentMode};
use crate::event::{AgentCommand, Event};
use crate::tui::status::PoolHealth;
use crate::tui::theme::palette;

impl super::App {
    fn request_session_load(
        &mut self,
        session: crate::core::session::Session,
        is_new: bool,
        busy_message: &str,
    ) -> bool {
        self.ensure_agent_loop();

        // ensure_agent_loop is a no-op when model is None — agent.tx stays
        // None. Guard here so we never set is_loading_session without a
        // command actually in the channel.
        let Some(tx) = &self.agent.tx else {
            self.doc.error("no model — run 'luma sync'");
            return false;
        };

        if let Some(cancel) = self.agent.cancel.take() {
            cancel.cancel();
        }
        self.agent.pending_content = None;
        self.agent.pending_images = None;
        self.agent.turn_start = None;

        if tx
            .try_send(AgentCommand::LoadSession { session, is_new })
            .is_err()
        {
            self.doc.warn(busy_message);
            return false;
        }

        self.agent.is_loading_session = true;
        self.ui
            .status
            .set_state(crate::tui::status::StatusState::Thinking);
        true
    }

    pub(super) fn handle_command(&mut self, cmd: &str) -> Action {
        match cmd {
            "new" => {
                let _ = self.request_session_load(
                    crate::core::session::Session::new(),
                    true,
                    "agent is busy; could not start a new thread right now",
                );
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
            "accounts" => {
                self.open_accounts_dialog();
                Action::Render
            }
            "login" | "login anthropic" | "login claude" => {
                self.start_login(AuthProvider::Anthropic);
                Action::Render
            }
            "login openai" | "login codex" => {
                self.start_login(AuthProvider::OpenAI);
                Action::Render
            }
            "exit" => Action::Quit,
            _ => {
                self.doc.warn(&format!("unknown command: /{cmd}"));
                Action::Render
            }
        }
    }

    /// Open the /accounts dialog — centered modal with toggle + remove.
    pub(super) fn open_accounts_dialog(&mut self) {
        self.refresh_pool_health();
        let accounts = auth::list_accounts();
        if accounts.is_empty() {
            self.doc.info("no accounts · run /login to add one");
            return;
        }
        let items = accounts
            .iter()
            .map(|a| {
                let provider = match a.provider {
                    AuthProvider::Anthropic => "claude",
                    AuthProvider::OpenAI => "codex",
                };
                let status = match a.health {
                    AccountHealth::Ok if a.disabled => "off",
                    AccountHealth::Ok => "ok",
                    AccountHealth::Cooldown { .. } => "cooling",
                    AccountHealth::NeedsRelogin => "relogin",
                };
                // col1: email if available, else label; col2: provider · status
                let col1 = a.email.clone().unwrap_or_else(|| a.label.clone());
                let col2 = format!("{provider}  {status}");
                crate::tui::dialog::DialogItem {
                    id: a.label.clone(),
                    col1,
                    col2,
                    dim: a.disabled,
                }
            })
            .collect();
        self.ui.dialog.open("accounts", items);
    }

    /// Spawn a detached PKCE login flow for `provider`. Progress and the
    /// final outcome are reported to the UI via the event bus.
    pub(super) fn start_login(&mut self, provider: AuthProvider) {
        let Some(tx) = self.tx.clone() else {
            self.doc.error("internal: event bus not ready");
            return;
        };
        self.doc
            .info(&format!("{} login · opening browser…", provider.as_str()));
        tokio::spawn(async move {
            let tx_url = tx.clone();
            let outcome = auth::login_with_reporter(provider, move |url| {
                let _ = tx_url.try_send(Event::LoginUrl(url.to_owned()));
            })
            .await;
            match outcome {
                Ok(o) => {
                    let _ = tx
                        .send(Event::LoginDone {
                            label: o.label,
                            email: o.email,
                            provider: o.provider.as_str().to_owned(),
                        })
                        .await;
                }
                Err(e) => {
                    let _ = tx.send(Event::LoginFailed(e.to_string())).await;
                }
            }
        });
    }

    /// Re-read the pool and push a fresh health summary into the status bar.
    pub(super) fn refresh_pool_health(&mut self) {
        let mut health = PoolHealth::default();
        for a in auth::list_accounts() {
            match a.health {
                AccountHealth::Ok => {}
                AccountHealth::Cooldown { .. } => health.cooling = health.cooling.saturating_add(1),
                AccountHealth::NeedsRelogin => {
                    health.needs_relogin = health.needs_relogin.saturating_add(1)
                }
            }
        }
        self.ui.status.set_pool_health(health);
    }

    pub(super) fn select_model(&mut self, model_id: &str) {
        let all = models::all_models();
        if let Some(m) = all.iter().find(|m| m.id == model_id) {
            self.config.model = Some(m.clone());
            crate::config::prefs::save_mode_model(self.config.mode, model_id);
            if let Some(tx) = &self.agent.tx
                && tx
                    .try_send(AgentCommand::SetModel {
                        model_id: m.id.clone(),
                        source: m.source.clone(),
                    })
                    .is_err()
            {
                self.doc
                    .warn("agent is busy; model switch will apply next turn");
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
        self.agent.is_loading_session = false;
        if let Some(tx) = self.agent.tx.take() {
            let _ = tx.try_send(AgentCommand::Shutdown);
        }
        self.enter_chat();
        self.doc.clear();
        self.view.clear();
        self.doc.divider_with_label(self.config.mode.as_str());
        self.ui.status.reset_usage();
        self.update_status();
        self.sync_prompt_commands();
    }

    pub(super) fn resume_session(&mut self, picker_id: &str) {
        let session_id = picker_id.split(" — ").next().unwrap_or(picker_id).trim();
        let Some(session) = crate::core::session::Session::load(session_id) else {
            self.doc.error("session not found");
            return;
        };

        let _ = self.request_session_load(
            session,
            false,
            "agent is busy; could not load session right now",
        );
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
                    for (_, name, input) in msg.tool_uses() {
                        let summary = crate::core::agent::format_tool_summary(name, input);
                        self.doc.tool_history(name, &summary);
                    }
                }
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
        if let Some(tx) = &self.agent.tx
            && tx
                .try_send(AgentCommand::SetThinking(self.config.thinking))
                .is_err()
        {
            self.doc
                .warn("agent is busy; thinking change will apply next turn");
        }
        crate::config::prefs::save_thinking(self.config.thinking);
        self.update_status();
        self.ui
            .status
            .set_thinking_level(self.config.thinking.as_str());
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

/// Format a single account as one picker row:
/// `●  nghia@gmail  ·  anthropic  ·  ok  ·  847/1000 req`
///
/// The picker is text-only (no per-item color), so we use unicode dot
/// glyphs to convey health: `●` ok, `◐` cooling, `○` needs re-login.
#[cfg(test)]
fn format_account_row(a: &crate::config::auth::AccountView) -> String {
    let dot = match a.health {
        AccountHealth::Ok => "●",
        AccountHealth::Cooldown { .. } => "◐",
        AccountHealth::NeedsRelogin => "○",
    };
    let who = a.email.as_deref().unwrap_or(a.label.as_str());
    let provider = match a.provider {
        AuthProvider::Anthropic => "anthropic",
        AuthProvider::OpenAI => "openai",
    };
    let status = match a.health {
        AccountHealth::Ok => "ok".to_owned(),
        AccountHealth::Cooldown { until_unix } => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("cooling {}s", until_unix.saturating_sub(now))
        }
        AccountHealth::NeedsRelogin => "needs re-login".to_owned(),
    };
    format!("{dot}  {who}  ·  {provider}  ·  {status}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::auth::AccountView;

    fn view(health: AccountHealth, email: Option<&str>) -> AccountView {
        AccountView {
            label: "nghia@gmail".into(),
            provider: AuthProvider::Anthropic,
            email: email.map(str::to_owned),
            health,
            disabled: false,
        }
    }

    #[test]
    fn account_row_healthy() {
        let row = format_account_row(&view(AccountHealth::Ok, Some("nghia@gmail.com")));
        assert!(row.starts_with("●"));
        assert!(row.contains("nghia@gmail.com"));
        assert!(row.contains("anthropic"));
        assert!(row.contains("ok"));
    }

    #[test]
    fn account_row_needs_relogin() {
        let row = format_account_row(&view(AccountHealth::NeedsRelogin, Some("x@y.com")));
        assert!(row.starts_with("○"));
        assert!(row.contains("needs re-login"));
    }

    #[test]
    fn account_row_falls_back_to_label_when_no_email() {
        let row = format_account_row(&view(AccountHealth::Ok, None));
        assert!(row.contains("nghia@gmail"));
    }
}
