/// Event dispatch — routes events to document (model) or view.
use super::state::{PickerMode, RunState};
use super::{ABORT_HINT_TICKS, Action};
use crate::config::auth;
use crate::config::models;
use crate::event::Event;
use crate::tui::picker::PickerAction;
use crate::tui::prompt::PromptAction;
use crate::tui::status::StatusState;
use termina::Event as TermEvent;
use termina::event::{KeyCode, KeyEvent, Modifiers};

impl super::App {
    fn apply_loaded_session(&mut self, session: &crate::core::session::Session, is_new: bool) {
        // Only clear the in-flight flag — agent is already idle when this ack arrives.
        self.agent.is_loading_session = false;
        self.enter_chat();
        self.doc.clear();
        self.view.clear();
        self.ui.status.reset_usage();
        self.ui.status.set_state(StatusState::Ready);

        self.doc.divider();
        if is_new {
            self.doc.info("new thread started");
            self.doc.divider();
        } else {
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
            self.update_context_from_tokens(total);
        }

        self.sync_prompt_commands();
    }

    /// Compute context window percentage and push to status bar.
    fn update_context_from_tokens(&mut self, total: u64) {
        let ctx_window = self
            .config
            .model
            .as_ref()
            .map(|m| models::context_window(&m.id))
            .unwrap_or(200_000);
        let pct = ((total as f64 / ctx_window as f64) * 100.0).min(100.0) as u8;
        self.ui.status.set_context(total, pct);
    }

    pub(super) fn handle(&mut self, event: Event) -> Action {
        let aborting = self.agent.state == RunState::Aborting;

        match event {
            // --- Terminal events — always handled ---
            Event::Term(TermEvent::Key(k)) => self.on_key(k),
            Event::Term(TermEvent::Mouse(m)) => self.on_mouse(m),
            Event::Term(TermEvent::Paste(text)) => self.on_paste(text),
            Event::Term(TermEvent::WindowResized(size)) => {
                self.handle_resize(size.cols, size.rows);
                Action::Render
            }
            Event::Term(TermEvent::FocusIn | TermEvent::FocusOut) => Action::Continue,
            // Csi/Osc/Dcs filtered at input layer, but handle exhaustively.
            Event::Term(_) => Action::Continue,

            Event::ClipboardImage(result) => {
                self.on_clipboard_image(result);
                Action::Render
            }
            Event::Tick => {
                self.ui.status.tick();
                if aborting {
                    return Action::Render;
                }
                if !self.screen.is_chat() {
                    return Action::Continue;
                }
                self.view.tick();
                if self.agent.state == RunState::PendingAbort {
                    self.agent.abort_countdown = self.agent.abort_countdown.saturating_sub(1);
                    if self.agent.abort_countdown == 0 {
                        self.agent.state = RunState::Streaming;
                    }
                    return Action::Render;
                }
                if matches!(
                    self.agent.state,
                    RunState::Streaming | RunState::PendingAbort
                ) {
                    Action::Render
                } else {
                    Action::Continue
                }
            }

            // --- Agent lifecycle — always handled, even while aborting ---
            Event::AgentDone => {
                crate::dbg_log!("agent done");
                self.on_agent_done();
                Action::Render
            }
            Event::AgentError(msg) => {
                crate::dbg_log!("agent error: {msg}");
                self.on_agent_error(&msg);
                Action::Render
            }
            Event::SessionLoaded { session, is_new } => {
                crate::dbg_log!(
                    "session_loaded is_new={} agent_state={:?} msgs={}",
                    is_new,
                    self.agent.state,
                    session.messages.len()
                );
                self.apply_loaded_session(&session, is_new);
                Action::Render
            }

            // --- Streaming events — skipped while aborting ---
            _ if aborting => Action::Continue,

            Event::Token(t) => {
                crate::dbg_log!("token: {}B", t.len());
                self.doc.append_token(&t);
                Action::Continue
            }
            Event::Thinking(t) => {
                crate::dbg_log!("thinking: {}B", t.len());
                self.doc.append_thinking(&t);
                Action::Continue
            }
            Event::ToolSelected { name } => {
                crate::dbg_log!("tool_selected {name}");
                self.doc.tool_selected(&name);
                Action::Render
            }
            Event::ToolInput { name, chunk } => {
                crate::dbg_log!("tool_input {name}: {}B", chunk.len());
                self.doc.tool_input(&name, &chunk);
                self.render();
                Action::Continue
            }
            Event::ToolOutput { name, chunk } => {
                crate::dbg_log!(
                    "tool_output {name}: {:?}",
                    chunk.chars().take(60).collect::<String>()
                );
                self.doc.tool_output(&name, &chunk);
                self.render();
                Action::Continue
            }
            Event::ToolArtifact { name, artifact } => {
                crate::dbg_log!("tool_artifact {name}");
                self.doc.tool_artifact(&name, *artifact);
                Action::Render
            }
            Event::ToolStart { name, summary } => {
                crate::dbg_log!("tool_start {name} {summary}");
                self.doc.tool_start(&name, &summary);
                Action::Render
            }
            Event::ToolEnd { name, summary } => {
                crate::dbg_log!("tool_end {name} {summary}");
                self.doc.tool_end(&name, &summary);
                Action::Render
            }
            Event::WebSearchStart { query } => {
                crate::dbg_log!("web_search_start: {query}");
                self.doc.tool_start("web_search", &query);
                Action::Render
            }
            Event::WebSearchDone { query, results } => {
                let end = if results.is_empty() {
                    "searched".to_owned()
                } else {
                    format!("{} results", results.len())
                };
                if !query.is_empty() {
                    self.doc.tool_start("web_search", &query);
                }
                for hit in &results {
                    let mut entry = format!("{}\n{}\n", hit.title, hit.url);
                    if !hit.snippet.is_empty() {
                        entry.push_str(&format!("{}\n", hit.snippet));
                    }
                    entry.push('\n');
                    self.doc.tool_output("web_search", &entry);
                }
                self.doc.tool_end("web_search", &end);
                Action::Render
            }
            Event::SkillStart(name) => {
                self.doc.skill_start(&name);
                Action::Render
            }
            Event::SkillEnd(summary) => {
                self.doc.skill_end(&summary);
                Action::Render
            }
            Event::ProviderRetry {
                provider,
                delay_secs,
                attempt,
                max_attempts,
            } => {
                self.doc
                    .provider_retry(&provider, delay_secs, attempt, max_attempts);
                Action::Render
            }
            Event::Usage(usage) => {
                if usage.cache_read.is_some() || usage.cache_write.is_some() {
                    self.ui.status.set_cache(
                        usage.cache_read.unwrap_or(0),
                        usage.cache_write.unwrap_or(0),
                    );
                }
                let (cr, cw) = self.ui.status.cache_values();
                let cache_read = usage.cache_read.unwrap_or(cr);
                let cache_write = usage.cache_write.unwrap_or(cw);
                let total = usage.input_tokens + cache_read + cache_write + usage.output_tokens;
                self.update_context_from_tokens(total);
                Action::Render
            }
        }
    }

    pub(super) fn on_key(&mut self, key: KeyEvent) -> Action {
        crate::dbg_log!("key {:?} state={:?}", key, self.agent.state);

        let is_esc = key.code == KeyCode::Escape;
        let is_ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(Modifiers::CONTROL);

        // Esc: interrupt streaming only
        if is_esc {
            if self.agent.state == RunState::PendingAbort {
                self.agent.state = RunState::Aborting;
                self.doc.abort();
                if let Some(c) = &self.agent.cancel {
                    c.cancel();
                }
                return Action::Render;
            }
            if self.agent.state == RunState::Streaming {
                self.agent.state = RunState::PendingAbort;
                self.agent.abort_countdown = ABORT_HINT_TICKS;
                return Action::Render;
            }
        }
        // Ctrl+C: clear buffer or quit
        if is_ctrl_c {
            if self.ui.prompt.buf.is_empty() {
                return Action::Quit;
            }
            self.ui.prompt.buf.clear();
            return Action::Render;
        }

        if self.ui.dialog.is_active {
            use crate::tui::dialog::DialogAction;
            match self.ui.dialog.handle_key(&key) {
                DialogAction::Toggle(label) => {
                    auth::toggle_disabled(&label);
                    self.open_accounts_dialog();
                }
                DialogAction::Remove(label) => {
                    auth::remove_account(&label);
                    self.refresh_pool_health();
                    if self.ui.dialog.items_is_empty() {
                        self.ui.dialog.close();
                    }
                }
                DialogAction::Close => {}
                DialogAction::Redraw | DialogAction::None => {}
            }
            return Action::Render;
        }

        if self.ui.picker.is_active {
            match self.ui.picker.handle_key(&key) {
                PickerAction::Select(id) => {
                    match self.config.picker_mode {
                        PickerMode::Model => self.select_model(&id),
                        PickerMode::Session => self.resume_session(&id),
                    }
                    return Action::Render;
                }
                PickerAction::Cancel => return Action::Render,
                PickerAction::Redraw => return Action::Render,
                PickerAction::None => return Action::Continue,
            }
        }

        if key.code == KeyCode::Tab
            && key.modifiers.is_empty()
            && self.agent.state == RunState::Idle
            && !self.ui.prompt.has_dropdown()
        {
            self.quick_cycle_mode();
            return Action::Render;
        }

        // During streaming, arrow keys scroll output instead of navigating prompt.
        if matches!(
            self.agent.state,
            RunState::Streaming | RunState::PendingAbort
        ) && key.modifiers.is_empty()
        {
            match key.code {
                KeyCode::Up | KeyCode::PageUp => {
                    self.view.scroll_up(super::SCROLL_STEP);
                    return Action::Render;
                }
                KeyCode::Down | KeyCode::PageDown => {
                    self.view.scroll_down(super::SCROLL_STEP);
                    return Action::Render;
                }
                _ => {}
            }
        }

        // Ctrl+V: try clipboard image first (async), text fallback via bracketed paste
        if key.code == KeyCode::Char('v')
            && key.modifiers.contains(Modifiers::CONTROL)
            && !self.ui.picker.is_active
        {
            self.paste_clipboard_image();
            return Action::Render;
        }

        match self.ui.prompt.handle_key(&key) {
            PromptAction::None => Action::Continue,
            PromptAction::Redraw => Action::Render,
            PromptAction::Submit(content, images) => self.on_submit(content, images),
            PromptAction::ToggleThinking => {
                self.cycle_thinking();
                Action::Render
            }
        }
    }

    /// Handle bracketed paste — detect image path vs text.
    ///
    /// Empty paste = clipboard had non-text content (typically a raw image
    /// from a screenshot). Trigger the same async clipboard read as Ctrl+V
    /// so users on terminals that forward Cmd+V as bracketed paste (iTerm2,
    /// Terminal.app) can paste screenshots without learning a new shortcut.
    /// Throttle vs the most recent raw-key trigger so iTerm2-style
    /// double-events do not spawn two reads.
    pub(super) fn on_paste(&mut self, text: String) -> Action {
        crate::dbg_log!("paste: {}B", text.len());
        if text.is_empty() {
            self.paste_clipboard_image();
            return Action::Render;
        }
        if let Some(path) = extract_image_path(&text) {
            self.paste_image_file(&path);
        } else if self.ui.prompt.handle_paste(text).is_none() {
            self.doc
                .warn("paste too large (>1 MB) — use a file reference instead");
        }
        Action::Render
    }
}

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff"];

/// Extract a valid image file path from pasted text (handles quotes, file:// URLs).
fn extract_image_path(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.contains('\n') {
        return None;
    }
    let cleaned = trimmed
        .trim_matches('\'')
        .trim_matches('"')
        .trim_start_matches("file://");
    let unescaped = unescape_pasted_path(cleaned);
    let path = std::path::Path::new(&unescaped);
    let is_image = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.to_lowercase().as_str()));
    if is_image && path.is_file() {
        Some(unescaped)
    } else {
        None
    }
}

/// Undo terminal / shell-style backslash escaping in pasted file paths.
///
/// Preview and Finder commonly paste paths as `/a/b/My\ File.png`. Treat a
/// backslash as escaping the next character so spaces and other escaped bytes
/// round-trip to the real filesystem path.
fn unescape_pasted_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut chars = path.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            } else {
                out.push(ch);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn extract_image_path_rejects_multiline() {
        assert!(extract_image_path("line1\nline2").is_none());
    }

    #[test]
    fn extract_image_path_rejects_non_image_extension() {
        assert!(extract_image_path("/etc/hosts").is_none());
    }

    #[test]
    fn extract_image_path_rejects_nonexistent_file() {
        assert!(extract_image_path("/tmp/nonexistent_xyz_123.png").is_none());
    }

    #[test]
    fn extract_image_path_accepts_png() {
        let mut tmp = tempfile::Builder::new()
            .suffix(".png")
            .tempfile()
            .unwrap();
        tmp.write_all(b"fake").unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();
        assert_eq!(extract_image_path(&path).as_deref(), Some(path.as_str()));
    }

    #[test]
    fn extract_image_path_strips_file_scheme_and_quotes() {
        let mut tmp = tempfile::Builder::new()
            .suffix(".jpg")
            .tempfile()
            .unwrap();
        tmp.write_all(b"fake").unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();
        let wrapped = format!("\"file://{path}\"");
        assert_eq!(extract_image_path(&wrapped).as_deref(), Some(path.as_str()));
    }

    #[test]
    fn extract_image_path_case_insensitive_extension() {
        let mut tmp = tempfile::Builder::new()
            .suffix(".PNG")
            .tempfile()
            .unwrap();
        tmp.write_all(b"fake").unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();
        assert_eq!(extract_image_path(&path).as_deref(), Some(path.as_str()));
    }

    #[test]
    fn extract_image_path_unescapes_backslash_escaped_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Khong co tieu de 8.png");
        std::fs::write(&path, b"fake").unwrap();
        let pasted = path.to_string_lossy().replace(' ', "\\ ");
        assert_eq!(extract_image_path(&pasted).as_deref(), path.to_str());
    }

    #[test]
    fn unescape_pasted_path_preserves_trailing_backslash() {
        assert_eq!(unescape_pasted_path("abc\\"), "abc\\");
    }
}
