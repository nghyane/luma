/// Trait for LLM providers. Streams events into the shared event bus.
use crate::core::types::{Message, ThinkingLevel, ToolSchema, Usage};
use crate::event_bus::Sender as EventSender;
use anyhow::Result;
use futures::stream::BoxStream;
use std::future::Future;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;

/// A thinking level surfaced by a provider for the current model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinkingOption {
    pub level: ThinkingLevel,
    pub label: &'static str,
}

/// Per-model thinking capabilities used by the app to render and cycle UI.
#[derive(Debug, Clone)]
pub struct ThinkingCapabilities {
    options: Vec<ThinkingOption>,
}

impl ThinkingCapabilities {
    /// Build from provider-declared options.
    pub fn new(options: Vec<ThinkingOption>) -> Self {
        Self { options }
    }

    /// Canonical fallback: off/low/medium/high.
    pub fn standard() -> Self {
        Self::new(vec![
            ThinkingOption {
                level: ThinkingLevel::Off,
                label: "off",
            },
            ThinkingOption {
                level: ThinkingLevel::Low,
                label: "low",
            },
            ThinkingOption {
                level: ThinkingLevel::Medium,
                label: "medium",
            },
            ThinkingOption {
                level: ThinkingLevel::High,
                label: "high",
            },
        ])
    }

    /// Provider has no configurable thinking surface.
    pub fn off_only() -> Self {
        Self::new(vec![ThinkingOption {
            level: ThinkingLevel::Off,
            label: "off",
        }])
    }

    /// Ordered visible options.
    #[cfg(test)]
    pub fn options(&self) -> &[ThinkingOption] {
        &self.options
    }

    /// Best supported level at or below `desired`, preserving explicit provider order.
    pub fn coerce(&self, desired: ThinkingLevel) -> ThinkingLevel {
        let mut best = self
            .options
            .first()
            .map(|o| o.level)
            .unwrap_or(ThinkingLevel::Off);
        for option in &self.options {
            if option.level.rank() > desired.rank() {
                break;
            }
            best = option.level;
        }
        best
    }

    /// Next visible level after `current`, wrapping within this provider's options.
    pub fn next(&self, current: ThinkingLevel) -> ThinkingLevel {
        if self.options.is_empty() {
            return ThinkingLevel::Off;
        }
        let current = self.coerce(current);
        let idx = self
            .options
            .iter()
            .position(|o| o.level == current)
            .unwrap_or(0);
        self.options[(idx + 1) % self.options.len()].level
    }

    /// Display label for `level` after coercing unsupported values.
    pub fn label(&self, level: ThinkingLevel) -> &'static str {
        let level = self.coerce(level);
        self.options
            .iter()
            .find(|o| o.level == level)
            .map(|o| o.label)
            .unwrap_or("off")
    }
}

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

/// Normalized streaming event emitted by a `Protocol` decoder.
///
/// Protocols (Anthropic Messages, OpenAI Chat, OpenAI Responses) each have
/// their own SSE / JSON-stream shape; `StreamEvent` is the common vocabulary
/// the rest of the system consumes (see `MessageAssembler` and the caller
/// loop in `turn.rs`).
///
/// `index` identifies the ordinal content block within the assistant
/// message. It is required for protocols that interleave thinking and text
/// blocks whose order matters for signature validation (Anthropic) and lets
/// the assembler reconstruct a single ordered `Vec<ContentBlock>`.
///
/// Introduced by RFC 0002. Not yet wired to any `Provider` impl — legacy
/// providers continue to use the push model until commit 4 of PR1.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Reasoning / chain-of-thought delta for block `index`.
    ThinkingDelta { index: u32, text: String },
    /// Signature for the thinking block at `index`. Must round-trip verbatim
    /// on later turns or the Anthropic backend rejects the request.
    ThinkingSignature { index: u32, sig: String },
    /// Assistant text delta for block `index`.
    TextDelta { index: u32, text: String },
    /// Model requested a tool call. Arguments arrive via `ToolUseDelta`.
    ToolUseStart {
        index: u32,
        id: String,
        name: String,
    },
    /// Incremental JSON fragment for the tool call at `index`.
    ToolUseDelta { index: u32, json_delta: String },
    /// Tool call at `index` is fully specified.
    ToolUseStop { index: u32 },
    /// Server-side tool (e.g. Claude web_search) invoked by the backend.
    ServerToolCall {
        name: String,
        input: serde_json::Value,
    },
    /// Result of a server-side tool, returned inline in the stream.
    ServerToolResult {
        name: String,
        output: serde_json::Value,
    },
    /// Running token usage. May be emitted multiple times; the last wins.
    UsageUpdate(Usage),
    /// Terminal event. Stream MUST NOT emit further events after `Done`.
    Done { stop: StopReason },
}

/// Identifier for a wire protocol. Used by registry lookup and catalog.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtocolId {
    /// Anthropic Messages API (`/v1/messages`, SSE event blocks).
    AnthropicMessages,
    /// OpenAI Chat Completions (`/v1/chat/completions`, SSE `data:` frames).
    OpenAIChat,
    /// OpenAI Responses API (`/v1/responses`, SSE with typed events).
    OpenAIResponses,
}

/// Byte stream yielded by the HTTP transport, fed into `Protocol::decode_stream`.
///
/// Boundary uses `Vec<u8>` instead of `bytes::Bytes` to avoid pulling an
/// extra dependency outside the project allowlist; the overhead is a single
/// copy per chunk which is negligible relative to JSON parsing cost.
#[allow(dead_code)]
pub type ByteStream = BoxStream<'static, Result<Vec<u8>>>;

/// Stream of normalized events produced by a protocol decoder.
#[allow(dead_code)]
pub type EventStream<'a> = BoxStream<'a, Result<StreamEvent>>;

/// Wire-format adapter for a family of LLM APIs.
///
/// A `Protocol` is pure: `encode_request` builds a request body + headers
/// from the abstract `StreamRequest`, and `decode_stream` transforms the
/// raw HTTP byte stream into normalized `StreamEvent`s. Protocols MUST NOT
/// perform I/O, touch the filesystem, or reference vendor-specific quirks
/// (Claude betas, Codex session headers, OAuth system rewrites). Those
/// concerns live in the `quirks` middleware layer (RFC 0002 §Quirks).
///
/// Introduced by RFC 0002. No implementations exist yet — this commit
/// establishes the trait shape; subsequent commits in PR1 extract the
/// three concrete protocols from the legacy provider modules.
#[allow(dead_code)]
pub trait Protocol: Send + Sync {
    /// Stable identifier for registry lookup and catalog entries.
    fn id(&self) -> ProtocolId;

    /// Path appended to the gateway base URL for streaming requests.
    fn endpoint_path(&self) -> &str;

    /// Build the request body and protocol-specific headers. Pure.
    ///
    /// `ctx` supplies per-request state (model id, thinking level, max
    /// tokens, image resolver) that the protocol needs to shape the body
    /// but that is not part of the abstract message history.
    fn encode_request(
        &self,
        req: &StreamRequest<'_>,
        ctx: &RequestCtx<'_>,
    ) -> (serde_json::Value, Vec<(String, String)>);

    /// Decode the raw byte stream into normalized events. Pure.
    ///
    /// Implementations MUST emit a terminal `StreamEvent::Done` and then
    /// terminate. They MUST NOT perform I/O or call into `EventSender`.
    fn decode_stream(&self, bytes: ByteStream) -> EventStream<'static>;
}

/// Per-request context passed to `Protocol::encode_request`.
///
/// Bundles the small slice of provider configuration that influences body
/// shape without coupling protocols to any specific `Provider` impl.
#[allow(dead_code)]
pub struct RequestCtx<'a> {
    pub model_id: &'a str,
    pub thinking: ThinkingLevel,
    pub max_tokens: u32,
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

    /// Thinking levels surfaced by this provider for the current model.
    fn thinking_capabilities(&self) -> ThinkingCapabilities {
        ThinkingCapabilities::standard()
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_capabilities_coerce_to_supported_level() {
        let caps = ThinkingCapabilities::new(vec![
            ThinkingOption {
                level: ThinkingLevel::Off,
                label: "off",
            },
            ThinkingOption {
                level: ThinkingLevel::Low,
                label: "low",
            },
            ThinkingOption {
                level: ThinkingLevel::High,
                label: "high",
            },
        ]);
        assert_eq!(caps.coerce(ThinkingLevel::Medium), ThinkingLevel::Low);
        assert_eq!(caps.coerce(ThinkingLevel::Max), ThinkingLevel::High);
    }

    #[test]
    fn thinking_capabilities_next_wraps_visible_options() {
        let caps = ThinkingCapabilities::new(vec![
            ThinkingOption {
                level: ThinkingLevel::Off,
                label: "off",
            },
            ThinkingOption {
                level: ThinkingLevel::Low,
                label: "low",
            },
            ThinkingOption {
                level: ThinkingLevel::High,
                label: "high",
            },
        ]);
        assert_eq!(caps.next(ThinkingLevel::Off), ThinkingLevel::Low);
        assert_eq!(caps.next(ThinkingLevel::Low), ThinkingLevel::High);
        assert_eq!(caps.next(ThinkingLevel::High), ThinkingLevel::Off);
        assert_eq!(caps.next(ThinkingLevel::Medium), ThinkingLevel::High);
    }
}
