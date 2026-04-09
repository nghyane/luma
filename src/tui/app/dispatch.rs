/// Event dispatch — routes events to document (model) or view.
use super::state::{PickerMode, RunState};
use super::{Action, ABORT_HINT_TICKS};
use crate::config::models;
use crate::event::Event;
use crate::tui::picker::PickerAction;
use crate::tui::prompt::PromptAction;
use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyModifiers};

impl super::App {
    pub(super) fn handle(&mut self, event: Event) -> Action {
        if self.agent.state == RunState::Aborting {
            return match event {
                Event::Term(CtEvent::Key(k)) => self.on_key(k),
                Event::Term(CtEvent::Mouse(m)) => self.on_mouse(m),
                Event::Term(CtEvent::Paste(text)) => self.on_paste(text),
                Event::Term(CtEvent::Resize(w, h)) => {
                    self.handle_resize(w, h);
                    Action::Render
                }
                Event::ClipboardImage(result) => {
                    self.on_clipboard_image(result);
                    Action::Render
                }
                Event::Tick => {
                    self.ui.status.tick();
                    Action::Render
                }
                Event::AgentDone => {
                    self.on_agent_done();
                    Action::Render
                }
                Event::AgentError(msg) => {
                    self.on_agent_error(&msg);
                    Action::Render
                }
                _ => Action::Continue,
            };
        }

        match event {
            Event::Term(CtEvent::Key(k)) => self.on_key(k),
            Event::Term(CtEvent::Mouse(m)) => self.on_mouse(m),
            Event::Term(CtEvent::Paste(text)) => self.on_paste(text),
            Event::Term(CtEvent::Resize(w, h)) => {
                self.handle_resize(w, h);
                Action::Render
            }
            Event::Term(_) => Action::Continue,
            Event::ClipboardImage(result) => {
                self.on_clipboard_image(result);
                Action::Render
            }
            Event::Tick => {
                self.ui.status.tick();
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
                let active = matches!(
                    self.agent.state,
                    RunState::Streaming | RunState::PendingAbort
                );
                if active {
                    Action::Render
                } else {
                    Action::Continue
                }
            }
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
            Event::ToolInput { name, chunk } => {
                crate::dbg_log!("tool_input {name}: {}B", chunk.len());
                self.doc.tool_input(&name, &chunk);
                Action::Continue
            }
            Event::ToolOutput { name, chunk } => {
                crate::dbg_log!(
                    "tool_output {name}: {:?}",
                    chunk.chars().take(60).collect::<String>()
                );
                self.doc.tool_output(&name, &chunk);
                Action::Continue
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
                // Only update cache display when values are present (message_start).
                // message_delta sends None to avoid overwriting.
                if usage.cache_read.is_some() || usage.cache_write.is_some() {
                    self.ui.status.set_cache(
                        usage.cache_read.unwrap_or(0),
                        usage.cache_write.unwrap_or(0),
                    );
                }
                // Use reported cache values, or fall back to last known from status bar.
                let (cr, cw) = self.ui.status.cache_values();
                let cache_read = usage.cache_read.unwrap_or(cr);
                let cache_write = usage.cache_write.unwrap_or(cw);
                let total = usage.input_tokens + cache_read + cache_write + usage.output_tokens;
                let ctx_window = self
                    .config
                    .model
                    .as_ref()
                    .map(|m| models::context_window(&m.id))
                    .unwrap_or(200_000);
                let pct = ((total as f64 / ctx_window as f64) * 100.0).min(100.0) as u8;
                self.ui.status.set_context(total, pct);
                Action::Render
            }
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
        }
    }

    pub(super) fn on_key(&mut self, key: KeyEvent) -> Action {
        crate::dbg_log!("key {:?} state={:?}", key, self.agent.state);

        let is_esc = key.code == KeyCode::Esc;
        let is_ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);

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

        // Ctrl+V: try clipboard image first (async), text fallback via bracketed paste
        if key.code == KeyCode::Char('v')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !self.ui.picker.is_active
        {
            self.paste_clipboard_image();
            return Action::Render;
        }

        match self.ui.prompt.handle_key(&key) {
            PromptAction::None => Action::Continue,
            PromptAction::Redraw => Action::Render,
            PromptAction::Submit(content) => self.on_submit(content),
            PromptAction::ToggleThinking => {
                self.cycle_thinking();
                Action::Render
            }
        }
    }

    /// Handle bracketed paste — detect image path vs text.
    pub(super) fn on_paste(&mut self, text: String) -> Action {
        crate::dbg_log!("paste: {}B", text.len());
        if text.is_empty() {
            return Action::Continue;
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
    let path = std::path::Path::new(cleaned);
    let is_image = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.to_lowercase().as_str()));
    if is_image && path.is_file() {
        Some(cleaned.to_owned())
    } else {
        None
    }
}
