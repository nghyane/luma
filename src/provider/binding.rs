//! Model binding — maps `(source, model_id)` to a concrete provider.
//!
//! `ModelBinding` is the unit of registry lookup. Builtin gateways are
//! hardcoded; adding a catalog loader (JSON) is a future change.
//!
//! The binding layer is three flat free functions:
//!
//! * [`resolve`] — parse legacy `AgentConfig.source` into a `ModelBinding`.
//! * [`build_provider`] — materialise a `Box<dyn Provider>` for a binding.
//! * [`thinking_capabilities`] — pure `(gateway, model_id)` → caps lookup
//!   used by the TUI before any credential is resolved.

use crate::config::auth::{AuthKind, AuthVendor, Credential};
use crate::core::provider::{Provider, ThinkingCapabilities};
use crate::core::types::ThinkingLevel;
use crate::provider::protocol::anthropic::AnthropicRuntime;
use crate::provider::protocol::openai_chat::OpenAIChatRuntime;
use crate::provider::protocol::openai_responses::OpenAIResponsesRuntime;
use crate::provider::quirks::QuirkSet;

/// Identifier for a transport gateway. One variant per distinct base URL
/// plus auth surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayId {
    Anthropic,
    Codex,
    OpenAI,
    OpenCodeGo,
}

/// Identifier for a wire protocol. Decoupled from `GatewayId` so a
/// single gateway can serve multiple protocols (OpenCode Go speaks both
/// Anthropic Messages and OpenAI Chat on distinct endpoint paths) and a
/// single protocol can be served by multiple gateways (Anthropic Messages
/// on api.anthropic.com vs. an Anthropic-compatible proxy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolId {
    /// `/v1/messages` with Anthropic's typed SSE event blocks.
    AnthropicMessages,
    /// `/v1/chat/completions` — OpenAI Chat Completions SSE.
    OpenAIChat,
    /// `/v1/responses` — OpenAI Responses API typed SSE (Codex).
    OpenAIResponses,
}

impl GatewayId {
    /// Parse the `source` string currently stored in `AgentConfig`.
    ///
    /// Unknown sources fall through to `OpenAI`, preserving the legacy
    /// default branch of `build_provider`.
    pub fn from_source(source: &str) -> Self {
        match source {
            "anthropic" => Self::Anthropic,
            "codex" => Self::Codex,
            "opencode-go" => Self::OpenCodeGo,
            _ => Self::OpenAI,
        }
    }

    /// Which auth-pool bucket this gateway's credentials live in.
    pub fn auth_vendor(self) -> AuthVendor {
        match self {
            Self::Anthropic => AuthVendor::Anthropic,
            Self::Codex | Self::OpenAI => AuthVendor::OpenAI,
            Self::OpenCodeGo => AuthVendor::OpenCodeGo,
        }
    }

    /// Default wire protocol for this gateway. OpenCode Go has no single
    /// answer — bindings for that gateway MUST set `protocol` explicitly
    /// per model (some models serve OpenAI Chat, others Anthropic
    /// Messages). This default is what `resolve()` uses when no catalog
    /// entry matches, and is harmless there because OpenCode Go bindings
    /// are always resolved via the catalog (see `resolve_opencode_go`).
    fn default_protocol(self) -> ProtocolId {
        match self {
            Self::Anthropic | Self::OpenCodeGo => ProtocolId::AnthropicMessages,
            Self::Codex => ProtocolId::OpenAIResponses,
            Self::OpenAI => ProtocolId::OpenAIChat,
        }
    }

    /// Base URL (scheme + host, no trailing slash) for this gateway.
    /// Runtimes append protocol-specific endpoint paths.
    fn base_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::Codex => "https://chatgpt.com/backend-api/codex",
            Self::OpenAI => "https://api.openai.com/v1",
            // OpenCode Go exposes /v1/chat/completions + /v1/messages
            // under the same /zen/go prefix.
            Self::OpenCodeGo => "https://opencode.ai/zen/go",
        }
    }
}

/// Quirks that apply to a `(gateway, auth_kind)` combination.
///
/// Anthropic OAuth (Claude Code) gets the full Claude Code surface:
/// cache breakpoint, adaptive thinking, OAuth system rewrite, beta
/// header, identity headers. Anthropic ApiKey skips OAuth-specific ones.
/// OpenAI paths (Codex / direct) have no Anthropic quirks today.
fn quirks_for(gateway: GatewayId, auth: AuthKind) -> QuirkSet {
    match (gateway, auth) {
        (GatewayId::Anthropic, AuthKind::OAuthBearer) => {
            QuirkSet::CACHE_BREAKPOINT
                | QuirkSet::ADAPTIVE_THINKING
                | QuirkSet::OAUTH_SYSTEM_REWRITE
                | QuirkSet::ANTHROPIC_BETAS
                | QuirkSet::CLAUDE_IDENTITY
        }
        (GatewayId::Anthropic, _) => QuirkSet::CACHE_BREAKPOINT | QuirkSet::ADAPTIVE_THINKING,
        // OpenCode Go proxies Anthropic Messages but is NOT Claude Code —
        // no OAuth system rewrite, no beta header, no identity headers.
        // Plain bearer passthrough.
        (GatewayId::Codex, _) | (GatewayId::OpenAI, _) | (GatewayId::OpenCodeGo, _) => {
            QuirkSet::EMPTY
        }
    }
}

/// OpenCode Go model catalog. Each entry declares the wire protocol the
/// proxy uses for that model — the same gateway serves both Anthropic
/// Messages (`/v1/messages`) and OpenAI Chat (`/v1/chat/completions`) on
/// distinct endpoint paths. Source: <https://opencode.ai/docs/go/>.
const OPENCODE_GO_MODELS: &[(&str, ProtocolId)] = &[
    ("glm-5", ProtocolId::OpenAIChat),
    ("glm-5.1", ProtocolId::OpenAIChat),
    ("kimi-k2.5", ProtocolId::OpenAIChat),
    ("mimo-v2-pro", ProtocolId::OpenAIChat),
    ("mimo-v2-omni", ProtocolId::OpenAIChat),
    ("minimax-m2.5", ProtocolId::AnthropicMessages),
    ("minimax-m2.7", ProtocolId::AnthropicMessages),
];

/// Look up the wire protocol for a model id on OpenCode Go. Returns
/// `None` for unknown models so callers can surface a clear error instead
/// of silently defaulting to the wrong protocol.
pub fn opencode_go_protocol(model_id: &str) -> Option<ProtocolId> {
    OPENCODE_GO_MODELS
        .iter()
        .find(|(id, _)| *id == model_id)
        .map(|(_, p)| *p)
}

/// A selected model on a selected gateway, carrying enough data for the
/// provider layer to materialise a streaming runtime. Protocol, base_url,
/// and quirks are derived from the gateway today; a future JSON catalog
/// will let these be overridden per-binding (primary motivator: OpenCode
/// Go, which exposes Anthropic Messages + OpenAI Chat on one host).
#[derive(Debug, Clone)]
pub struct ModelBinding {
    pub gateway: GatewayId,
    pub model_id: String,
    pub protocol: ProtocolId,
    pub base_url: String,
}

/// Parse `(source, model_id)` into a binding. Total over all inputs.
///
/// For OpenCode Go, the protocol depends on the model id (the proxy
/// serves both Anthropic Messages and OpenAI Chat). Unknown OpenCode Go
/// model ids fall back to `AnthropicMessages` — the binding will still
/// build; the user will see a clear 404 from the proxy if the model
/// name is wrong.
pub fn resolve(source: &str, model_id: &str) -> ModelBinding {
    let gateway = GatewayId::from_source(source);
    let protocol = match gateway {
        GatewayId::OpenCodeGo => {
            opencode_go_protocol(model_id).unwrap_or(ProtocolId::AnthropicMessages)
        }
        _ => gateway.default_protocol(),
    };
    ModelBinding {
        gateway,
        model_id: model_id.to_owned(),
        protocol,
        base_url: gateway.base_url().to_owned(),
    }
}

/// Thinking capabilities for a model on a given gateway. Pure.
///
/// Called by the TUI status line before any credential is resolved, so it
/// intentionally does not construct a runtime.
pub fn thinking_capabilities(gateway: GatewayId, model_id: &str) -> ThinkingCapabilities {
    use crate::core::provider::ThinkingOption;
    use crate::provider::quirks::adaptive_thinking::is_adaptive_thinking_model;

    match gateway {
        GatewayId::Anthropic if is_adaptive_thinking_model(model_id) => {
            ThinkingCapabilities::new(vec![
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
                ThinkingOption {
                    level: ThinkingLevel::Max,
                    label: "max",
                },
            ])
        }
        GatewayId::Anthropic | GatewayId::Codex => ThinkingCapabilities::standard(),
        // OpenCode Go models don't expose thinking controls in the
        // proxy's published contract; surface off-only for now.
        GatewayId::OpenAI | GatewayId::OpenCodeGo => ThinkingCapabilities::off_only(),
    }
}

/// Build a ready-to-stream provider. Thinking level is coerced to the
/// gateway's supported set before the provider is returned.
///
/// Dispatch is by `binding.protocol`, not `binding.gateway` — the same
/// protocol can be served by different gateways (e.g. Anthropic Messages
/// via `api.anthropic.com` or via an OpenCode Go proxy). `base_url` and
/// `quirks` come from the binding / gateway combination.
pub fn build_provider(
    binding: &ModelBinding,
    credential: &Credential,
    session_id: &str,
    thinking: ThinkingLevel,
) -> Box<dyn Provider> {
    let mut provider: Box<dyn Provider> = match binding.protocol {
        ProtocolId::AnthropicMessages => {
            let kind = credential.auth_kind();
            Box::new(AnthropicRuntime::new(
                &binding.model_id,
                &binding.base_url,
                &credential.token,
                kind,
                quirks_for(binding.gateway, kind),
                &credential.label,
            ))
        }
        ProtocolId::OpenAIResponses => Box::new(OpenAIResponsesRuntime::new(
            &binding.model_id,
            &credential.token,
            credential.account_id.clone(),
            session_id,
            &credential.label,
        )),
        ProtocolId::OpenAIChat => Box::new(OpenAIChatRuntime::new(
            &binding.model_id,
            &binding.base_url,
            &credential.token,
            &credential.label,
        )),
    };
    let coerced = provider.thinking_capabilities().coerce(thinking);
    provider.set_thinking(coerced);
    provider
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_id_from_source_covers_known_and_fallback() {
        assert_eq!(GatewayId::from_source("anthropic"), GatewayId::Anthropic);
        assert_eq!(GatewayId::from_source("codex"), GatewayId::Codex);
        assert_eq!(GatewayId::from_source("openai"), GatewayId::OpenAI);
        assert_eq!(GatewayId::from_source("unknown"), GatewayId::OpenAI);
    }

    #[test]
    fn auth_vendor_maps_codex_and_openai_to_same_bucket() {
        assert_eq!(GatewayId::Anthropic.auth_vendor(), AuthVendor::Anthropic);
        assert_eq!(GatewayId::Codex.auth_vendor(), AuthVendor::OpenAI);
        assert_eq!(GatewayId::OpenAI.auth_vendor(), AuthVendor::OpenAI);
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

    #[test]
    fn thinking_caps_openai_is_off_only() {
        let labels: Vec<_> = thinking_capabilities(GatewayId::OpenAI, "gpt-5")
            .options()
            .iter()
            .map(|o| o.label)
            .collect();
        assert_eq!(labels, ["off"]);
    }

    #[test]
    fn resolve_sets_default_protocol_and_base_url_per_gateway() {
        let a = resolve("anthropic", "claude");
        assert_eq!(a.protocol, ProtocolId::AnthropicMessages);
        assert_eq!(a.base_url, "https://api.anthropic.com");

        let c = resolve("codex", "gpt-5.4");
        assert_eq!(c.protocol, ProtocolId::OpenAIResponses);

        let o = resolve("openai", "gpt-5");
        assert_eq!(o.protocol, ProtocolId::OpenAIChat);
        assert_eq!(o.base_url, "https://api.openai.com/v1");
    }
}
