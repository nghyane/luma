/// App state decomposition.
use crate::config::models::{AgentMode, ModelEntry};
use crate::core::types::ThinkingLevel;
use crate::event::AgentCommand;
use crate::tui::picker::Picker;
use crate::tui::prompt::PromptState;
use crate::tui::selection::Selection;
use crate::tui::status::StatusBar;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Which screen the TUI is showing.
/// Welcome carries its own display data — dropped on transition.
/// Chat uses doc+view on App (always present, needed 99% of runtime).
pub enum Screen {
    Welcome { lines: Vec<crate::tui::text::Line> },
    Chat,
}

impl Screen {
    pub fn is_chat(&self) -> bool {
        matches!(self, Screen::Chat)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Streaming,
    PendingAbort,
    Aborting,
}

pub enum DragState {
    Scrollbar { start_row: u16, start_offset: usize },
    Selecting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerMode {
    Model,
    Session,
}

pub struct AppConfig {
    pub mode: AgentMode,
    pub model: Option<ModelEntry>,
    pub env_context: String,
    pub thinking: ThinkingLevel,
    pub picker_mode: PickerMode,
    pub is_mcp_loading: bool,
}

pub struct AgentHandle {
    pub tx: Option<mpsc::Sender<AgentCommand>>,
    pub cancel: Option<CancellationToken>,
    pub turn_start: Option<std::time::Instant>,
    pub state: RunState,
    pub pending_content: Option<Vec<crate::core::types::ContentBlock>>,
    pub pending_images: Option<Vec<(String, Vec<u8>)>>,
    /// Set while a `LoadSession` command is in-flight. Suppresses the
    /// intermediate Ready state that `on_agent_done` would otherwise set
    /// before `SessionLoaded` arrives.
    pub is_loading_session: bool,
    /// Snapshot of the config the agent loop is currently running with.
    /// `None` before the loop is spawned. Used to compute whether the
    /// user's local [`AppConfig`] has drifted since the last turn —
    /// pending changes are committed right before the next `Chat`
    /// instead of eagerly on every mode/model toggle.
    pub last_sent: Option<SentConfig>,
    /// Timestamp of the most recent `paste_clipboard_image()` call.
    /// Used to debounce duplicate triggers when a single user action
    /// produces both a raw `Ctrl+V` keystroke and an empty bracketed
    /// paste event (observed on iTerm2 and some macOS terminals).
    pub last_clipboard_paste: Option<std::time::Instant>,
}

/// Snapshot of the fields the agent loop cares about. Compared against
/// `AppConfig` at submit time to decide which `Set*` commands to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentConfig {
    pub mode: AgentMode,
    pub model_id: String,
    pub source: String,
    pub thinking: ThinkingLevel,
}

impl AgentHandle {
    pub fn new() -> Self {
        Self {
            tx: None,
            cancel: None,
            turn_start: None,
            state: RunState::Idle,
            pending_content: None,
            pending_images: None,
            is_loading_session: false,
            last_sent: None,
            last_clipboard_paste: None,
        }
    }
}

pub struct UiComponents {
    pub prompt: PromptState,
    pub picker: Picker,
    pub dialog: crate::tui::dialog::Dialog,
    pub status: StatusBar,
    pub selection: Selection,
    pub drag: Option<DragState>,
    pub last_output_width: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_handle_new() {
        let h = AgentHandle::new();
        assert_eq!(h.state, RunState::Idle);
        assert!(h.tx.is_none());
    }
}
