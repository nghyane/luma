//! Model binding — maps `(source, model_id)` to a concrete provider.
//!
//! `ModelBinding` is the unit of registry lookup. Builtin gateways are
//! described by a single static [`GatewaySpec`] table (`GATEWAYS`); every
//! per-gateway property (auth pool, protocol defaults, base URL, quirks,
//! header shape, thinking caps) lives in one row of that table. Adding a
//! gateway = appending one row + extending `GatewayId`.
//!
//! The binding layer is three flat free functions:
//!
//! * [`resolve`] — parse legacy `AgentConfig.source` into a `ModelBinding`.
//! * [`build_provider`] — materialise a `Box<dyn Provider>` for a binding.
//! * [`thinking_capabilities`] — pure `(gateway, model_id)` → caps lookup
//!   used by the TUI before any credential is resolved.

use crate::config::auth::{AuthKind, AuthVendor, Credential};
use crate::core::provider::{Provider, ThinkingCapabilities, ThinkingOption};
use crate::core::types::ThinkingLevel;
use crate::provider::protocol::anthropic::AnthropicRuntime;
use crate::provider::protocol::openai_chat::OpenAIChatRuntime;
use crate::provider::protocol::openai_responses::OpenAIResponsesRuntime;
use crate::provider::quirks::QuirkSet;

/// Identifier for a transport gateway. One variant per row in [`GATEWAYS`].
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

/// Static description of a gateway. One field per per-gateway concern;
/// every dispatch site (`resolve`, `build_provider`, `auth_kind_for`,
/// quirks computation) reads from here instead of pattern-matching on
/// `GatewayId` separately. Adding a gateway adds one row.
#[derive(Debug, Clone, Copy)]
struct GatewaySpec {
    id: GatewayId,
    /// `AgentConfig.source` value that resolves to this gateway.
    source: &'static str,
    /// Auth-pool bucket this gateway's credentials live in.
    vendor: AuthVendor,
    /// Wire protocol when no per-binding override applies.
    default_protocol: ProtocolId,
    /// Base URL (scheme + host, no trailing slash).
    base_url: &'static str,
    /// Wire-auth shape when the credential is OAuth-backed.
    auth_kind_oauth: AuthKind,
    /// Wire-auth shape when the credential is a raw API key.
    auth_kind_api_key: AuthKind,
    /// Quirks applied for OAuth credentials.
    quirks_oauth: QuirkSet,
    /// Quirks applied for API-key credentials.
    quirks_api_key: QuirkSet,
    /// Thinking-control capabilities surfaced to the TUI.
    thinking: ThinkingProfile,
}

/// What the TUI thinking-cycle UI exposes for this gateway.
#[derive(Debug, Clone, Copy)]
enum ThinkingProfile {
    /// Off / Low / Medium / High / Max; only models matching
    /// `is_adaptive_thinking_model` get the Max step.
    AnthropicAdaptive,
    /// Off / Low / Medium / High.
    Standard,
    /// Off only.
    OffOnly,
}

const ANTHROPIC_OAUTH_QUIRKS: QuirkSet = QuirkSet::CACHE_BREAKPOINT
    .union(QuirkSet::ADAPTIVE_THINKING)
    .union(QuirkSet::OAUTH_SYSTEM_REWRITE)
    .union(QuirkSet::ANTHROPIC_BETAS)
    .union(QuirkSet::CLAUDE_IDENTITY);
const ANTHROPIC_API_KEY_QUIRKS: QuirkSet =
    QuirkSet::CACHE_BREAKPOINT.union(QuirkSet::ADAPTIVE_THINKING);

const GATEWAYS: &[GatewaySpec] = &[
    GatewaySpec {
        id: GatewayId::Anthropic,
        source: "anthropic",
        vendor: AuthVendor::Anthropic,
        default_protocol: ProtocolId::AnthropicMessages,
        base_url: "https://api.anthropic.com",
        auth_kind_oauth: AuthKind::OAuthBearer,
        auth_kind_api_key: AuthKind::ApiKey,
        quirks_oauth: ANTHROPIC_OAUTH_QUIRKS,
        quirks_api_key: ANTHROPIC_API_KEY_QUIRKS,
        thinking: ThinkingProfile::AnthropicAdaptive,
    },
    GatewaySpec {
        id: GatewayId::Codex,
        source: "codex",
        vendor: AuthVendor::OpenAI,
        default_protocol: ProtocolId::OpenAIResponses,
        base_url: "https://chatgpt.com/backend-api/codex",
        auth_kind_oauth: AuthKind::CodexSession,
        // Codex does not ship raw API keys today; if one ever appears we
        // fall back to the same session shape rather than misroute it.
        auth_kind_api_key: AuthKind::CodexSession,
        quirks_oauth: QuirkSet::EMPTY,
        quirks_api_key: QuirkSet::EMPTY,
        thinking: ThinkingProfile::Standard,
    },
    GatewaySpec {
        id: GatewayId::OpenAI,
        source: "openai",
        vendor: AuthVendor::OpenAI,
        default_protocol: ProtocolId::OpenAIChat,
        base_url: "https://api.openai.com/v1",
        auth_kind_oauth: AuthKind::OAuthBearer,
        auth_kind_api_key: AuthKind::OAuthBearer,
        quirks_oauth: QuirkSet::EMPTY,
        quirks_api_key: QuirkSet::EMPTY,
        thinking: ThinkingProfile::OffOnly,
    },
    GatewaySpec {
        id: GatewayId::OpenCodeGo,
        source: "opencode-go",
        vendor: AuthVendor::OpenCodeGo,
        default_protocol: ProtocolId::AnthropicMessages,
        base_url: "https://opencode.ai/zen/go",
        // OpenCode Go always wants `Authorization: Bearer <token>` —
        // the proxy doesn't accept Anthropic's `x-api-key` shape.
        auth_kind_oauth: AuthKind::OAuthBearer,
        auth_kind_api_key: AuthKind::OAuthBearer,
        quirks_oauth: QuirkSet::EMPTY,
        quirks_api_key: QuirkSet::EMPTY,
        thinking: ThinkingProfile::OffOnly,
    },
];

fn spec(id: GatewayId) -> &'static GatewaySpec {
    GATEWAYS
        .iter()
        .find(|g| g.id == id)
        .expect("GATEWAYS table missing GatewayId variant")
}

impl GatewayId {
    /// Parse the `source` string currently stored in `AgentConfig`.
    /// Unknown sources fall through to `OpenAI`, preserving the legacy
    /// default branch.
    pub fn from_source(source: &str) -> Self {
        GATEWAYS
            .iter()
            .find(|g| g.source == source)
            .map(|g| g.id)
            .unwrap_or(GatewayId::OpenAI)
    }

    /// Which auth-pool bucket this gateway's credentials live in.
    pub fn auth_vendor(self) -> AuthVendor {
        spec(self).vendor
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

/// Wire protocol for a model id on OpenCode Go, or `None` for an unknown
/// model. Callers default to the gateway's `default_protocol` on `None`.
pub fn opencode_go_protocol(model_id: &str) -> Option<ProtocolId> {
    OPENCODE_GO_MODELS
        .iter()
        .find(|(id, _)| *id == model_id)
        .map(|(_, p)| *p)
}

/// A selected model on a selected gateway, carrying enough data for the
/// provider layer to materialise a streaming runtime.
#[derive(Debug, Clone)]
pub struct ModelBinding {
    pub gateway: GatewayId,
    pub model_id: String,
    pub protocol: ProtocolId,
    pub base_url: String,
}

/// Parse `(source, model_id)` into a binding. Total over all inputs.
pub fn resolve(source: &str, model_id: &str) -> ModelBinding {
    let gateway = GatewayId::from_source(source);
    let s = spec(gateway);
    let protocol = match gateway {
        GatewayId::OpenCodeGo => opencode_go_protocol(model_id).unwrap_or(s.default_protocol),
        _ => s.default_protocol,
    };
    ModelBinding {
        gateway,
        model_id: model_id.to_owned(),
        protocol,
        base_url: s.base_url.to_owned(),
    }
}

/// Thinking capabilities for a model on a given gateway. Pure.
///
/// Called by the TUI status line before any credential is resolved, so it
/// intentionally does not construct a runtime.
pub fn thinking_capabilities(gateway: GatewayId, model_id: &str) -> ThinkingCapabilities {
    use crate::provider::quirks::adaptive_thinking::is_adaptive_thinking_model;
    match spec(gateway).thinking {
        ThinkingProfile::AnthropicAdaptive if is_adaptive_thinking_model(model_id) => {
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
        ThinkingProfile::AnthropicAdaptive | ThinkingProfile::Standard => {
            ThinkingCapabilities::standard()
        }
        ThinkingProfile::OffOnly => ThinkingCapabilities::off_only(),
    }
}

/// Build a ready-to-stream provider. Thinking level is coerced to the
/// gateway's supported set before the provider is returned.
///
/// Dispatch is by `binding.protocol`, not `binding.gateway` — the same
/// protocol can be served by different gateways. `auth_kind`, `base_url`,
/// and `quirks` come from the gateway spec; `is_oauth` on the credential
/// picks between OAuth and API-key columns of the spec.
pub fn build_provider(
    binding: &ModelBinding,
    credential: &Credential,
    session_id: &str,
    thinking: ThinkingLevel,
) -> Box<dyn Provider> {
    let s = spec(binding.gateway);
    let (auth_kind, quirks) = if credential.is_oauth {
        (s.auth_kind_oauth, s.quirks_oauth)
    } else {
        (s.auth_kind_api_key, s.quirks_api_key)
    };

    let mut provider: Box<dyn Provider> = match binding.protocol {
        ProtocolId::AnthropicMessages => Box::new(AnthropicRuntime::new(
            &binding.model_id,
            &binding.base_url,
            &credential.token,
            auth_kind,
            quirks,
            &credential.label,
        )),
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
    fn gateways_table_covers_every_id_variant() {
        for id in [
            GatewayId::Anthropic,
            GatewayId::Codex,
            GatewayId::OpenAI,
            GatewayId::OpenCodeGo,
        ] {
            assert_eq!(spec(id).id, id, "GATEWAYS missing entry for {id:?}");
        }
    }

    #[test]
    fn from_source_round_trips_with_spec_table() {
        for g in GATEWAYS {
            assert_eq!(GatewayId::from_source(g.source), g.id);
        }
        assert_eq!(GatewayId::from_source("unknown"), GatewayId::OpenAI);
    }

    #[test]
    fn auth_vendor_reads_from_spec() {
        assert_eq!(GatewayId::Anthropic.auth_vendor(), AuthVendor::Anthropic);
        assert_eq!(GatewayId::Codex.auth_vendor(), AuthVendor::OpenAI);
        assert_eq!(GatewayId::OpenAI.auth_vendor(), AuthVendor::OpenAI);
        assert_eq!(GatewayId::OpenCodeGo.auth_vendor(), AuthVendor::OpenCodeGo);
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

        let og = resolve("opencode-go", "kimi-k2.5");
        assert_eq!(og.protocol, ProtocolId::OpenAIChat);
        let og2 = resolve("opencode-go", "minimax-m2.7");
        assert_eq!(og2.protocol, ProtocolId::AnthropicMessages);
    }
}
