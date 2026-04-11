/// Trait for LLM providers. Streams events into the shared event bus.
use crate::core::types::{Message, ThinkingLevel, ToolSchema, Usage};
use crate::event_bus::Sender as EventSender;
use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;

/// Why the stream ended. Mirrors the relevant subset of provider stop reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StopReason {
    /// Normal end of turn — model finished its response.
    #[default]
    EndTurn,
    /// Output was cut off because max_tokens was reached.
    MaxTokens,
    /// Stream ended to invoke tools (model wants to call functions).
    ToolUse,
    /// Any other reason we don't specifically handle (refusal, stop_sequence, etc.).
    Other,
}

/// Response from a provider stream: assistant message, token usage, and stop reason.
#[derive(Debug)]
pub struct StreamResponse {
    pub message: Message,
    pub usage: Usage,
    pub stop_reason: StopReason,
}

/// Resolves image id → base64 data. Passed to providers so they don't touch filesystem.
pub type ImageResolver = dyn Fn(&str) -> String + Send + Sync;

/// Per-call input to [`Provider::stream`]. Bundled to keep the trait signature small.
pub struct StreamRequest<'a> {
    pub messages: &'a [Message],
    pub tools: &'a [ToolSchema],
    pub server_tools: &'a [serde_json::Value],
    pub resolve_image: &'a ImageResolver,
    /// Override the provider's default max output tokens. `None` = use default.
    pub max_tokens_override: Option<u32>,
    pub tx: EventSender,
    pub cancel: CancellationToken,
}

/// An LLM provider that streams responses as Events. Object-safe.
pub trait Provider: Send + Sync {
    /// Provider display name (e.g. "claude", "openai").
    fn name(&self) -> &str;

    /// Set thinking level. Called once after construction before boxing.
    fn set_thinking(&mut self, level: ThinkingLevel);

    /// Build native schemas for server capabilities this provider supports.
    fn server_tool_schemas(&self, capabilities: &[String]) -> Vec<serde_json::Value> {
        let _ = capabilities;
        vec![]
    }

    /// Whether this provider honors [`StreamRequest::max_tokens_override`].
    ///
    /// Callers use this to decide whether escalation (bumping `max_tokens`
    /// after hitting the limit once) has any effect. Providers whose backend
    /// does not accept a max output tokens field should return `false` so
    /// the caller can skip the escalation retry instead of silently making
    /// the same request twice.
    fn supports_max_tokens_override(&self) -> bool {
        true
    }

    /// Stream a chat completion.
    fn stream<'a>(
        &'a self,
        req: StreamRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<StreamResponse>> + Send + 'a>>;
}
