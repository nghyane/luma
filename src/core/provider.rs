/// Trait for LLM providers. Streams events into the shared event bus.
use crate::core::types::{Message, ThinkingLevel, ToolSchema, Usage};
use crate::event_bus::Sender as EventSender;
use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;

/// Default output token cap applied by providers that honour
/// [`StreamRequest::max_tokens_override`]. Mirrors Claude Code's capped
/// default. Caller may escalate to [`ESCALATED_MAX_TOKENS`] on the first
/// `max_tokens` stop reason (see `core::agent::turn`).
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Escalation cap used after hitting `max_tokens` once. Claude 4.x native
/// limit; OpenAI Chat providers also accept this value.
pub const ESCALATED_MAX_TOKENS: u32 = 64_000;

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
    /// Provider already emitted `Event::ContextUsage` during the stream.
    /// When true, the turn-layer fallback estimator is skipped.
    pub context_usage_emitted: bool,
    pub provider_state: Option<crate::core::provider_state::ProviderStateUpdate>,
}

/// Normalized event emitted by a `Protocol` decoder.
///
/// Shape is driven by what the Anthropic decoder currently needs; new
/// variants land only when a second protocol demands them. The consumer
/// (see `Provider::stream` impls) translates these into UI `Event`s and
/// assembles the final `StreamResponse`.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Incremental assistant text.
    TextDelta(String),
    /// Incremental reasoning / chain-of-thought text.
    ThinkingDelta(String),
    /// Tool call started — model chose a tool.
    ToolSelected { name: String },
    /// Incremental tool-argument chunk (already JSON-string-decoded via
    /// the tool's streamable_arg extractor).
    ToolInput { name: String, chunk: String },
    /// Server-side web search started; `query` is best-effort (Anthropic
    /// streams it piecewise; decoder emits this once the first non-empty
    /// chunk is available).
    WebSearchStart { query: String },
    /// Server-side web search completed. `results` may be empty if the
    /// backend's result block had no renderable hits.
    WebSearchDone {
        results: Vec<crate::event::SearchHit>,
    },
    /// Running token usage snapshot. Emitted multiple times; last wins.
    UsageUpdate(Usage),
    /// A content block has been committed in document order. The
    /// assembler appends to `Vec<ContentBlock>` in emission order.
    BlockComplete(crate::core::types::ContentBlock),
    /// Provider-specific stream metadata that is not rendered to the UI.
    ProviderMetadata(ProviderStreamMetadata),
    /// Terminal event. Decoder MUST NOT emit further events after this.
    Done { stop: StopReason },
}

/// Provider metadata emitted by a decoder before completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStreamMetadata {
    Codex { response_id: Option<String> },
}

/// Resolves image id → base64 data. Passed to providers so they don't touch filesystem.
pub type ImageResolver = dyn Fn(&str) -> String + Send + Sync;

/// How a provider handles `ToolResultItem::Image` entries.
///
/// Declared by each provider so the runtime can rewrite messages once,
/// centrally, before sending — rather than each adapter doing ad-hoc routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolResultImageRouting {
    /// Image blocks are valid inside tool-result content (Anthropic, OpenAI Responses).
    Inline,
    /// Provider has no image variant in tool-result; images must be promoted
    /// to the next user-turn attachment slot (Kiro).
    UserAttachment,
    /// Provider has no image path at all; flatten to text (OpenAI Chat).
    TextOnly,
}

/// Per-call input to [`Provider::stream`]. Bundled to keep the trait signature small.
pub struct StreamRequest<'a> {
    pub messages: &'a [Message],
    pub tools: &'a [ToolSchema],
    pub server_tools: &'a [serde_json::Value],
    pub resolve_image: &'a ImageResolver,
    pub provider_state: Option<crate::core::provider_state::ProviderRequestState<'a>>,
    /// Override the provider's default max output tokens. `None` = use default.
    pub max_tokens_override: Option<u32>,
    pub tx: EventSender,
    pub cancel: CancellationToken,
    /// Optional channel for streaming tool execution. When set, providers
    /// send completed `ToolUse` content blocks here as soon as they arrive
    /// mid-stream, allowing the caller to start tool execution before the
    /// full response is assembled. Mirrors Claude Code's
    /// `StreamingToolExecutor.addTool()` pattern.
    pub tool_use_tx: Option<tokio::sync::mpsc::Sender<crate::core::types::ContentBlock>>,
}

/// Rewrite `messages` so that `ToolResultItem::Image` entries are routed
/// according to `routing`. Returns a `Cow`-style owned copy only when a
/// rewrite is needed; otherwise returns the slice unchanged via the
/// `Borrowed` variant so the common path (Inline) pays no allocation.
pub fn route_tool_result_images<'a>(
    messages: &'a [Message],
    routing: ToolResultImageRouting,
) -> std::borrow::Cow<'a, [Message]> {
    match routing {
        // Inline: providers handle images natively — no rewrite needed.
        ToolResultImageRouting::Inline => std::borrow::Cow::Borrowed(messages),

        // TextOnly providers handle flattening in their own adapter because
        // the explanatory text is provider-specific. Runtime leaves the
        // transcript shape untouched.
        ToolResultImageRouting::TextOnly => std::borrow::Cow::Borrowed(messages),

        // UserAttachment: promote Image items from tool-result turns into
        // ContentBlock::Image on the same user message so provider adapters
        // that support user-turn images (e.g. Kiro) can pick them up via
        // their existing `msg_images` path.
        ToolResultImageRouting::UserAttachment => route_user_attachment(messages),
    }
}

fn route_user_attachment<'a>(messages: &'a [Message]) -> std::borrow::Cow<'a, [Message]> {
    use crate::core::types::{ContentBlock, ToolResultBody, ToolResultItem};

    let mut rewritten = Vec::with_capacity(messages.len());
    let mut changed = false;

    for msg in messages {
        let mut content = Vec::with_capacity(msg.content.len());
        let mut promoted = Vec::new();
        let mut msg_changed = false;

        for block in &msg.content {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content: body,
                is_error,
                evidence_id,
            } = block
                && let ToolResultBody::Items(items) = body
                && items
                    .iter()
                    .any(|item| matches!(item, ToolResultItem::Image { .. }))
            {
                msg_changed = true;
                let text = body.as_text();
                content.push(ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: ToolResultBody::Text(text),
                    is_error: *is_error,
                    evidence_id: evidence_id.clone(),
                });
                for item in items {
                    if let ToolResultItem::Image { media_type, id } = item {
                        promoted.push(ContentBlock::Image {
                            media_type: media_type.clone(),
                            id: id.clone(),
                        });
                    }
                }
                continue;
            }
            content.push(block.clone());
        }

        if !promoted.is_empty() {
            msg_changed = true;
            content.extend(promoted);
        }

        if msg_changed {
            changed = true;
        }
        rewritten.push(Message {
            content,
            ..msg.clone()
        });
    }

    if changed {
        std::borrow::Cow::Owned(rewritten)
    } else {
        std::borrow::Cow::Borrowed(messages)
    }
}

/// Provider's preference for web search routing.
///
/// Determines whether `build_registry` exposes server-side search
/// (provider built-in) or client-side `WebSearchTool` to the model.
/// At most one search surface is registered — never both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPreference {
    /// Provider has high-quality built-in search (e.g. Anthropic
    /// `web_search_20250305`). Always use server-side; ignore client.
    PreferServer,
    /// Provider has server search but client is preferred (e.g. Codex).
    /// Use client if available; fall back to server otherwise.
    PreferClient,
    /// Provider has no server search (e.g. OpenAI Chat, Kiro gateway).
    /// Use client if available; no search otherwise.
    ClientOnly,
}

/// An LLM provider that streams responses as Events. Object-safe.
pub trait Provider: Send + Sync {
    /// Provider display name (e.g. "claude", "openai").
    fn name(&self) -> &str;

    /// Provider-specific session state needed for request routing.
    fn session_state_kind(&self) -> Option<crate::core::provider_state::ProviderStateKind> {
        None
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

    /// How this provider handles `ToolResultItem::Image` entries.
    /// The runtime uses this to rewrite messages before sending.
    fn tool_result_image_routing(&self) -> ToolResultImageRouting {
        ToolResultImageRouting::Inline
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
    use crate::core::types::{ContentBlock, Message, Role, ToolResultBody, ToolResultItem};

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

    #[test]
    fn text_only_routing_leaves_messages_unchanged() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: ToolResultBody::Items(vec![
                    ToolResultItem::Text {
                        text: "meta".into(),
                    },
                    ToolResultItem::Image {
                        media_type: "image/png".into(),
                        id: "img_1".into(),
                    },
                ]),
                is_error: false,
                evidence_id: None,
            }],
            origin: None,
        }];

        let routed = route_tool_result_images(&messages, ToolResultImageRouting::TextOnly);
        assert!(matches!(routed, std::borrow::Cow::Borrowed(_)));
        assert_eq!(routed[0].content.len(), 1);
    }

    #[test]
    fn user_attachment_routing_promotes_images_and_strips_tool_result_items() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: ToolResultBody::Items(vec![
                    ToolResultItem::Text {
                        text: "meta".into(),
                    },
                    ToolResultItem::Image {
                        media_type: "image/png".into(),
                        id: "img_1".into(),
                    },
                ]),
                is_error: false,
                evidence_id: None,
            }],
            origin: None,
        }];

        let routed = route_tool_result_images(&messages, ToolResultImageRouting::UserAttachment);
        let routed = routed.as_ref();
        assert_eq!(routed.len(), 1);
        assert_eq!(routed[0].content.len(), 2);

        match &routed[0].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content, &ToolResultBody::Text("meta".into()));
            }
            other => panic!("expected tool_result, got {other:?}"),
        }

        match &routed[0].content[1] {
            ContentBlock::Image { media_type, id } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(id, "img_1");
            }
            other => panic!("expected promoted image, got {other:?}"),
        }
    }
}
