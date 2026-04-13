//! `ProviderRuntime` — the sole `impl Provider` after RFC 0002 commit 9.
//!
//! Dispatches to one of the three wire-protocol runtimes under
//! `provider::protocol`. Today this is a thin enum that forwards every
//! `Provider` method to its inner runtime; once the `Protocol` trait is
//! wired (pull model + `MessageAssembler`), this struct collapses into
//! `{ gateway, protocol, quirks, model_id, credential, state }` and the
//! three sibling runtimes go away.
//!
//! Keeping the façade as an enum (instead of `Box<dyn Provider>` all the
//! way down) lets `turn.rs` keep calling `&dyn Provider` while still
//! giving us a single grep-able choke-point for the coming migration.

use crate::config::auth::Credential;
use crate::core::provider::{Provider, StreamRequest, StreamResponse, ThinkingCapabilities};
use crate::core::types::ThinkingLevel;
use crate::provider::binding::{GatewayId, ModelBinding};
use crate::provider::protocol::anthropic::AnthropicRuntime;
use crate::provider::protocol::openai_chat::OpenAIChatRuntime;
use crate::provider::protocol::openai_responses::OpenAIResponsesRuntime;
use anyhow::Result;

/// Unified provider runtime. Variants mirror `GatewayId` 1-1 today but
/// will be merged into a single struct once `Protocol` + quirks composition
/// lands.
pub enum ProviderRuntime {
    Anthropic(AnthropicRuntime),
    OpenAIResponses(OpenAIResponsesRuntime),
    OpenAIChat(OpenAIChatRuntime),
}

impl ProviderRuntime {
    /// Construct from a resolved `(binding, credential)` pair.
    pub fn build(
        binding: &ModelBinding,
        credential: &Credential,
        session_id: &str,
        thinking: ThinkingLevel,
    ) -> Self {
        let mut runtime = match binding.gateway {
            GatewayId::Anthropic => Self::Anthropic(AnthropicRuntime::new(
                &binding.model_id,
                &credential.token,
                credential.auth_kind(),
                &credential.label,
            )),
            GatewayId::Codex => Self::OpenAIResponses(OpenAIResponsesRuntime::new(
                &binding.model_id,
                &credential.token,
                credential.account_id.clone(),
                session_id,
                &credential.label,
            )),
            GatewayId::OpenAI => Self::OpenAIChat(OpenAIChatRuntime::new(
                &binding.model_id,
                &credential.token,
                &credential.label,
            )),
        };
        let coerced = runtime.thinking_capabilities().coerce(thinking);
        runtime.set_thinking(coerced);
        runtime
    }

    /// Thinking capabilities without needing a credential — used by
    /// `tui::app` to render the status line for a model picked from the
    /// catalog before any turn runs.
    pub fn thinking_caps_for(gateway: GatewayId, model_id: &str) -> ThinkingCapabilities {
        match gateway {
            GatewayId::Anthropic => {
                AnthropicRuntime::new(model_id, "", crate::config::auth::AuthKind::ApiKey, "")
                    .thinking_capabilities()
            }
            GatewayId::Codex => {
                OpenAIResponsesRuntime::new(model_id, "", None, "", "").thinking_capabilities()
            }
            GatewayId::OpenAI => OpenAIChatRuntime::new(model_id, "", "").thinking_capabilities(),
        }
    }
}

impl Provider for ProviderRuntime {
    fn name(&self) -> &str {
        match self {
            Self::Anthropic(p) => p.name(),
            Self::OpenAIResponses(p) => p.name(),
            Self::OpenAIChat(p) => p.name(),
        }
    }

    fn thinking_capabilities(&self) -> ThinkingCapabilities {
        match self {
            Self::Anthropic(p) => p.thinking_capabilities(),
            Self::OpenAIResponses(p) => p.thinking_capabilities(),
            Self::OpenAIChat(p) => p.thinking_capabilities(),
        }
    }

    fn set_thinking(&mut self, level: ThinkingLevel) {
        match self {
            Self::Anthropic(p) => p.set_thinking(level),
            Self::OpenAIResponses(p) => p.set_thinking(level),
            Self::OpenAIChat(p) => p.set_thinking(level),
        }
    }

    fn server_tool_schemas(&self, capabilities: &[String]) -> Vec<serde_json::Value> {
        match self {
            Self::Anthropic(p) => p.server_tool_schemas(capabilities),
            Self::OpenAIResponses(p) => p.server_tool_schemas(capabilities),
            Self::OpenAIChat(p) => p.server_tool_schemas(capabilities),
        }
    }

    fn supports_max_tokens_override(&self) -> bool {
        match self {
            Self::Anthropic(p) => p.supports_max_tokens_override(),
            Self::OpenAIResponses(p) => p.supports_max_tokens_override(),
            Self::OpenAIChat(p) => p.supports_max_tokens_override(),
        }
    }

    fn stream<'a>(
        &'a self,
        req: StreamRequest<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<StreamResponse>> + Send + 'a>>
    {
        match self {
            Self::Anthropic(p) => p.stream(req),
            Self::OpenAIResponses(p) => p.stream(req),
            Self::OpenAIChat(p) => p.stream(req),
        }
    }
}
