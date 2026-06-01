//! Provider-owned session state shared by the session owner and runtimes.

use serde::{Deserialize, Serialize};

/// Provider-specific state persisted with a conversation session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderSessionState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexSessionState>,
}

/// Stable provider state required by the Codex backend for routing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexSessionState {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_model: Option<String>,
}

impl CodexSessionState {
    /// Create Codex routing state with a stable thread id.
    pub fn new(thread_id: String) -> Self {
        Self {
            thread_id,
            last_request_id: None,
            last_response_id: None,
            server_model: None,
        }
    }
}

/// Provider state family requested by a runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStateKind {
    Codex,
}

/// Typed view of state passed to a provider request.
#[derive(Debug, Clone, Copy)]
pub enum ProviderRequestState<'a> {
    Codex {
        session: &'a CodexSessionState,
        turn_state: Option<&'a str>,
    },
}

/// Provider-specific state returned from a successful stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStateUpdate {
    Codex(CodexStateUpdate),
}

/// Codex routing metadata captured from HTTP headers and response events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexStateUpdate {
    pub turn_state: Option<String>,
    pub request_id: Option<String>,
    pub response_id: Option<String>,
    pub server_model: Option<String>,
}

impl CodexStateUpdate {
    /// Whether this update contains at least one usable field.
    pub fn has_any(&self) -> bool {
        self.turn_state.is_some()
            || self.request_id.is_some()
            || self.response_id.is_some()
            || self.server_model.is_some()
    }
}
