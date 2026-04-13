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
            _ => Self::OpenAI,
        }
    }

    /// Which auth-pool bucket this gateway's credentials live in.
    pub fn auth_vendor(self) -> AuthVendor {
        match self {
            Self::Anthropic => AuthVendor::Anthropic,
            Self::Codex | Self::OpenAI => AuthVendor::OpenAI,
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
        (GatewayId::Codex, _) | (GatewayId::OpenAI, _) => QuirkSet::EMPTY,
    }
}

/// A `(gateway, model_id)` pair the user can select. Unit of registry lookup.
#[derive(Debug, Clone)]
pub struct ModelBinding {
    pub gateway: GatewayId,
    pub model_id: String,
}

/// Parse `(source, model_id)` into a binding. Total over all inputs.
pub fn resolve(source: &str, model_id: &str) -> ModelBinding {
    ModelBinding {
        gateway: GatewayId::from_source(source),
        model_id: model_id.to_owned(),
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
        GatewayId::OpenAI => ThinkingCapabilities::off_only(),
    }
}

/// Build a ready-to-stream provider. Thinking level is coerced to the
/// gateway's supported set before the provider is returned.
pub fn build_provider(
    binding: &ModelBinding,
    credential: &Credential,
    session_id: &str,
    thinking: ThinkingLevel,
) -> Box<dyn Provider> {
    let mut provider: Box<dyn Provider> = match binding.gateway {
        GatewayId::Anthropic => {
            let kind = credential.auth_kind();
            Box::new(AnthropicRuntime::new(
                &binding.model_id,
                &credential.token,
                kind,
                quirks_for(GatewayId::Anthropic, kind),
                &credential.label,
            ))
        }
        GatewayId::Codex => Box::new(OpenAIResponsesRuntime::new(
            &binding.model_id,
            &credential.token,
            credential.account_id.clone(),
            session_id,
            &credential.label,
        )),
        GatewayId::OpenAI => Box::new(OpenAIChatRuntime::new(
            &binding.model_id,
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
}
