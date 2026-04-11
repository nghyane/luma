/// Central event type. All input (keyboard, mouse, resize) and agent output
/// flow through a single `mpsc::channel<Event>`. The app loop matches exhaustively.
use crate::core::types::{FileChangeArtifact, Usage};

/// A single web search result.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Every event the app loop handles.
#[derive(Debug, Clone)]
pub enum Event {
    /// Terminal event (key, mouse, resize, paste, focus).
    Term(termina::Event),

    Token(String),
    Thinking(String),
    /// Provider has started a tool_use block and knows the tool name, but
    /// arguments are still streaming. Emitted by providers once per tool
    /// invocation, before any [`Self::ToolInput`]. Gives the UI a chance to
    /// show a pending block during the gap between tool selection and the
    /// first streamable-arg delta (Claude in particular may pause ~10s
    /// between the `path` field and the `content` field of a Write call).
    ToolSelected {
        name: String,
    },
    /// Orchestrator is about to execute the tool. Carries the final, parsed
    /// argument summary (e.g. file path). Emitted by the agent turn loop
    /// after the provider stream resolves the tool call.
    ToolStart {
        name: String,
        summary: String,
    },
    /// Streaming tool input args (e.g. file content being written).
    ToolInput {
        name: String,
        chunk: String,
    },
    ToolOutput {
        name: String,
        chunk: String,
    },
    ToolArtifact {
        name: String,
        artifact: Box<FileChangeArtifact>,
    },
    ToolEnd {
        name: String,
        summary: String,
    },
    /// Server-side web search started.
    WebSearchStart {
        query: String,
    },
    /// Server-side web search completed.
    WebSearchDone {
        query: String,
        results: Vec<SearchHit>,
    },
    SkillStart(String),
    SkillEnd(String),
    ProviderRetry {
        provider: String,
        delay_secs: u64,
        attempt: u8,
        max_attempts: u8,
    },
    Usage(Usage),
    SessionLoaded {
        session: Box<crate::core::session::Session>,
        is_new: bool,
    },
    AgentDone,
    AgentError(String),

    /// Async clipboard image result — None means no image found.
    ClipboardImage(Option<(String, Vec<u8>)>),

    /// PKCE login produced an authorize URL. Surface it in the TUI so the
    /// user can click/copy (the flow already tries to open the browser, but
    /// we want a visible fallback in case that fails on SSH / WSL / etc.).
    LoginUrl(String),
    /// PKCE login completed successfully. The account is already in the pool.
    LoginDone {
        label: String,
        email: Option<String>,
        provider: String,
    },
    /// PKCE login failed — surface the reason to the user.
    LoginFailed(String),

    Tick,
}

/// An image attachment — raw bytes, saved by agent to session dir.
pub struct ImageAttach {
    pub media_type: String,
    pub data: Vec<u8>,
}

/// A file reference attached to a message (content read at send time).
pub struct FileAttach {
    pub path: String,
    pub content: String,
}

/// Commands sent from App to the agent loop task.
pub enum AgentCommand {
    /// Run a user turn. Agent pushes user message, calls provider, runs tools.
    Chat {
        content: Vec<crate::core::types::ContentBlock>,
        images: Vec<ImageAttach>,
        files: Vec<FileAttach>,
        cancel: tokio_util::sync::CancellationToken,
    },
    /// Switch model (agent rebuilds provider with auth on next turn).
    SetModel { model_id: String, source: String },
    /// Update thinking level on current provider.
    SetThinking(crate::core::types::ThinkingLevel),
    /// Replace the current thread with a specific session.
    LoadSession {
        session: crate::core::session::Session,
    },
    /// Shut down the agent loop.
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Event>();
    }
}
