use super::Action;
/// App commands — slash commands, mode/model selection, session resume.
use super::state::PickerMode;
use crate::auth::domain::{AccountHealth, AccountKey};
use crate::auth::repo::FileAuthRepository;
use crate::auth::service::AuthService;
use crate::config::models::{self, AgentMode};
use crate::event::AgentCommand;
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
                    let current_key = self
                        .config
                        .model
                        .as_ref()
                        .map(|m| format!("{}/{}", m.source, m.id))
                        .unwrap_or_default();
                    let items: Vec<String> = all
                        .iter()
                        .map(|m| format!("{}/{}", m.source, m.id))
                        .collect();
                    self.ui.picker.open(items, &current_key);
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
            "exit" => Action::Quit,
            _ => {
                self.doc.warn(&format!("unknown command: /{cmd}"));
                Action::Render
            }
        }
    }

    fn auth_service() -> AuthService<FileAuthRepository> {
        AuthService::new(FileAuthRepository::with_default_path())
    }

    /// Open the /accounts dialog — centered modal with toggle + remove.
    pub(super) fn open_accounts_dialog(&mut self) {
        self.refresh_pool_health();
        let accounts = Self::auth_service().list_accounts().unwrap_or_default();
        if accounts.is_empty() {
            self.doc.info("no accounts · run `luma login` to add one");
            return;
        }
        let items = accounts
            .iter()
            .map(|a| {
                let provider = a.vendor.as_str();
                let status = match &a.health {
                    AccountHealth::Disabled => "off",
                    AccountHealth::Active => "ok",
                    AccountHealth::CoolingDown { .. } => "cooling",
                    AccountHealth::NeedsRelogin { .. } => "relogin",
                };
                let col1 = a.email.clone().unwrap_or_else(|| a.display_name.clone());
                let col2 = format!("{provider}  {status}");
                crate::tui::dialog::DialogItem {
                    id: serde_json::to_string(&a.key).unwrap_or_default(),
                    col1,
                    col2,
                    dim: matches!(a.health, AccountHealth::Disabled),
                }
            })
            .collect();
        self.ui.dialog.open("accounts", items);
    }

    /// Re-read the pool and push a fresh health summary into the status bar.
    pub(super) fn refresh_pool_health(&mut self) {
        let mut health = PoolHealth::default();
        for a in Self::auth_service().list_accounts().unwrap_or_default() {
            match a.health {
                AccountHealth::Active => {}
                AccountHealth::CoolingDown { .. } => health.cooling = health.cooling.saturating_add(1),
                AccountHealth::NeedsRelogin { .. } => {
                    health.needs_relogin = health.needs_relogin.saturating_add(1)
                }
                AccountHealth::Disabled => {}
            }
        }
        self.ui.status.set_pool_health(health);
    }

    pub(super) fn select_model(&mut self, key: &str) {
        // Picker items are formatted as `{source}/{id}`. Split on the first
        // `/` so we match the exact (source, id) pair — some ids (e.g.
        // `glm-5`, `minimax-m2.5`) appear under multiple sources, and a
        // plain id-only lookup would silently route to the wrong gateway.
        let Some((source, model_id)) = key.split_once('/') else {
            return;
        };
        let all = models::all_models();
        if let Some(m) = all.iter().find(|m| m.source == source && m.id == model_id) {
            self.config.model = Some(m.clone());
            let thinking_caps = self.current_thinking_capabilities();
            self.config.thinking = thinking_caps.coerce(self.config.thinking);
            crate::config::prefs::save_thinking(self.config.thinking);
            // Persist the composite key so the prefs-restore path on next
            // launch routes back to the exact (source, id) the user picked
            // — bare ids can be ambiguous across sources.
            crate::config::prefs::save_mode_model(self.config.mode, key);
            // Config drift is committed to the agent loop at submit time —
            // see `commit_pending_config`. Keep this path local-only so
            // picking a model doesn't mutate the transcript.
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
        let thinking_caps = self.current_thinking_capabilities();
        self.config.thinking = thinking_caps.coerce(self.config.thinking);
        crate::config::prefs::save_mode(self.config.mode);
        crate::config::prefs::save_thinking(self.config.thinking);
        // Deferred: the new prompt + registry are pushed to the agent loop
        // on the next submit (see `commit_pending_config`). Cycling modes
        // while idle should not touch transcript, stream, or scroll.
        self.update_status();
        self.sync_prompt_commands();
    }

    /// If the user's local config has drifted from what the agent loop is
    /// running, push the minimal set of `Set*` commands to catch it up.
    /// Called right before sending `AgentCommand::Chat`.
    pub(super) fn commit_pending_config(&mut self) {
        let Some(model) = self.config.model.clone() else {
            return;
        };
        let Some(tx) = self.agent.tx.clone() else {
            return;
        };
        let desired = super::state::SentConfig {
            mode: self.config.mode,
            model_id: model.id.clone(),
            source: model.source.clone(),
            thinking: self.config.thinking,
        };
        if self.agent.last_sent.as_ref() == Some(&desired) {
            return;
        }
        let sent = self.agent.last_sent.as_ref();
        let prompt_dirty =
            sent.is_none_or(|s| s.mode != desired.mode || s.source != desired.source);
        let model_dirty =
            sent.is_none_or(|s| s.model_id != desired.model_id || s.source != desired.source);
        let thinking_dirty = sent.is_none_or(|s| s.thinking != desired.thinking);

        if prompt_dirty {
            let skills = crate::config::skills::discover();
            let skill_catalog = crate::config::skills::build_catalog(&skills);
            let project_instructions = crate::config::instructions::discover();
            let instructions_block =
                crate::config::instructions::build_instructions(&project_instructions);
            let style = crate::tool::ToolStyle::for_mode(desired.mode, &desired.source);
            let base_prompt = crate::config::prompt::build(desired.mode, style);
            let system_prompt = format!(
                "{base_prompt}\n{}{skill_catalog}{instructions_block}",
                self.config.env_context
            );
            let registry =
                crate::tool::build_registry(style, Self::search_backend(&desired.source));
            let _ = tx.try_send(AgentCommand::SetContext {
                system_prompt,
                registry,
            });
        }
        if model_dirty {
            let _ = tx.try_send(AgentCommand::SetModel {
                model_id: desired.model_id.clone(),
                source: desired.source.clone(),
            });
        }
        if thinking_dirty {
            let _ = tx.try_send(AgentCommand::SetThinking(desired.thinking));
        }
        self.agent.last_sent = Some(desired);
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

        // Count real user turns via the canonical visibility check.
        let turn_starts: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::User && m.has_visible_content())
            .map(|(i, _)| i)
            .collect();

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
            let is_visible_user = msg.role == Role::User && msg.has_visible_content();

            // Skip pre-window messages but still count turns.
            if i < render_from && msg.role != Role::System {
                if is_visible_user {
                    turn_idx += 1;
                }
                continue;
            }

            // Turn dividers between visible user messages.
            if is_visible_user {
                if seen_user {
                    self.turn_divider(turn_durations, turn_idx.wrapping_sub(1));
                }
                turn_idx += 1;
                seen_user = true;
            }

            // Single replay entry point — all block creation in one place.
            self.doc.replay_message(msg);
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
        let thinking_caps = self.current_thinking_capabilities();
        self.config.thinking = thinking_caps.next(self.config.thinking);
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
            .set_thinking_level(thinking_caps.label(self.config.thinking));
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
        let thinking_caps = self.current_thinking_capabilities();
        let thinking = thinking_caps.coerce(self.config.thinking);
        self.config.thinking = thinking;
        self.ui
            .status
            .set_thinking_level(thinking_caps.label(thinking));
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
fn format_account_row(a: &crate::auth::domain::AccountView) -> String {
    let dot = match &a.health {
        AccountHealth::Active => "●",
        AccountHealth::CoolingDown { .. } => "◐",
        AccountHealth::NeedsRelogin { .. } => "○",
        AccountHealth::Disabled => "✕",
    };
    let who = a.email.as_deref().unwrap_or(a.display_name.as_str());
    let provider = a.vendor.as_str();
    let status = match &a.health {
        AccountHealth::Active => "ok".to_owned(),
        AccountHealth::CoolingDown { until_unix } => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("cooling {}s", until_unix.saturating_sub(now))
        }
        AccountHealth::NeedsRelogin { .. } => "needs re-login".to_owned(),
        AccountHealth::Disabled => "disabled".to_owned(),
    };
    format!("{dot}  {who}  ·  {provider}  ·  {status}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::domain::{AccountKey, AccountMetadata, AccountRecord, AccountView, AuthState, AuthVendor, OAuthCredential};

    fn view(health: AccountHealth, email: Option<&str>) -> AccountView {
        AccountView {
            key: AccountKey::anonymous(AuthVendor::Anthropic, "x"),
            display_name: "nghia@gmail".into(),
            vendor: AuthVendor::Anthropic,
            email: email.map(str::to_owned),
            health,
        }
    }

    #[test]
    fn account_row_healthy() {
        let row = format_account_row(&view(AccountHealth::Active, Some("nghia@gmail.com")));
        assert!(row.starts_with("●"));
        assert!(row.contains("nghia@gmail.com"));
        assert!(row.contains("anthropic"));
        assert!(row.contains("ok"));
    }

    #[test]
    fn account_row_needs_relogin() {
        use crate::auth::domain::ReloginReason;
        let row = format_account_row(&view(AccountHealth::NeedsRelogin { reason: ReloginReason::RefreshFailed }, Some("x@y.com")));
        assert!(row.starts_with("○"));
        assert!(row.contains("needs re-login"));
    }

    #[test]
    fn account_row_falls_back_to_label_when_no_email() {
        let row = format_account_row(&view(AccountHealth::Active, None));
        assert!(row.contains("nghia@gmail"));
    }
}
