//! Model binding — `(source, model_id)` → concrete provider runtime.
//!
//! Dispatcher only. All per-gateway behaviour lives in
//! `provider::gateways::*` (see [`Gateway`] trait); this module just
//! resolves a binding and forwards `build` to the right gateway.

use crate::config::auth::Credential;
use crate::core::provider::{Provider, ThinkingCapabilities};
use crate::core::types::ThinkingLevel;
pub use crate::provider::gateway::GatewayId;
use crate::provider::gateways;

/// Identifier for a wire protocol. Decoupled from `GatewayId` so a
/// single gateway can serve multiple protocols (OpenCode Go) and a
/// single protocol can be served by multiple gateways.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolId {
    /// `/v1/messages` with Anthropic's typed SSE event blocks.
    AnthropicMessages,
    /// `/v1/chat/completions` — OpenAI Chat Completions SSE.
    OpenAIChat,
    /// `/v1/responses` — OpenAI Responses API typed SSE (Codex).
    OpenAIResponses,
    /// `/generateAssistantResponse` — AWS Event Stream binary (Kiro/Amazon Q).
    KiroEventStream,
}

/// A selected model on a selected gateway. `protocol` and `base_url`
/// are snapshots at resolve-time; the gateway impl is the source of
/// truth and is re-consulted on `build_provider`.
#[derive(Debug, Clone)]
pub struct ModelBinding {
    pub gateway: GatewayId,
    pub model_id: String,
    pub protocol: ProtocolId,
    pub base_url: String,
}

/// Compatibility shim — most callers historically asked the binding
/// layer for the gateway directly. Now just forwards to the registry.
impl GatewayId {
    pub fn from_source(source: &str) -> Self {
        gateways::lookup_source(source)
            .map(|g| g.id())
            .unwrap_or(GatewayId::OpenAI)
    }

    pub fn auth_vendor(self) -> crate::config::auth::AuthVendor {
        gateways::lookup(self).vendor()
    }
}

/// Parse `(source, model_id)` into a binding. Total over all inputs:
/// unknown sources fall back to OpenAI, unknown models on a multi-protocol
/// gateway use that gateway's `protocol_for` default.
pub fn resolve(source: &str, model_id: &str) -> ModelBinding {
    let gateway = GatewayId::from_source(source);
    let g = gateways::lookup(gateway);
    ModelBinding {
        gateway,
        model_id: model_id.to_owned(),
        protocol: g.protocol_for(model_id),
        base_url: g.base_url().to_owned(),
    }
}

/// Thinking capabilities for a model on a given gateway. Pure.
/// Called by the TUI status line before any credential is resolved.
pub fn thinking_capabilities(gateway: GatewayId, model_id: &str) -> ThinkingCapabilities {
    gateways::lookup(gateway).thinking(model_id)
}

/// Build a ready-to-stream provider. Thinking level is coerced to the
/// gateway's supported set before the provider is returned.
pub fn build_provider(
    binding: &ModelBinding,
    credential: &Credential,
    session_id: &str,
    thinking: ThinkingLevel,
) -> Box<dyn Provider> {
    let g = gateways::lookup(binding.gateway);
    let mut provider = g.build(binding, credential, session_id);
    let coerced = g.coerce_thinking(&binding.model_id, thinking);
    provider.set_thinking(coerced);
    provider
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_source_known_and_fallback() {
        assert_eq!(GatewayId::from_source("anthropic"), GatewayId::Anthropic);
        assert_eq!(GatewayId::from_source("codex"), GatewayId::Codex);
        assert_eq!(GatewayId::from_source("openai"), GatewayId::OpenAI);
        assert_eq!(GatewayId::from_source("opencode-go"), GatewayId::OpenCodeGo);
        assert_eq!(GatewayId::from_source("unknown"), GatewayId::OpenAI);
    }

    #[test]
    fn resolve_picks_per_model_protocol_on_opencode_go() {
        assert_eq!(
            resolve("opencode-go", "kimi-k2.5").protocol,
            ProtocolId::OpenAIChat
        );
        assert_eq!(
            resolve("opencode-go", "minimax-m2.7").protocol,
            ProtocolId::AnthropicMessages
        );
    }

    #[test]
    fn thinking_caps_adaptive_for_claude_sonnet_4_6() {
        let labels: Vec<_> = thinking_capabilities(GatewayId::Anthropic, "claude-sonnet-4-6")
            .options()
            .iter()
            .map(|o| o.label)
            .collect();
        assert_eq!(labels, ["off", "low", "medium", "high", "max"]);
    }
}
