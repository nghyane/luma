//! Anthropic Claude (claude.ai subscriber OAuth or raw API key).

use crate::config::auth::{AuthKind, AuthVendor, Credential};
use crate::core::provider::{Provider, ThinkingCapabilities, ThinkingOption};
use crate::core::types::ThinkingLevel;
use crate::provider::binding::{ModelBinding, ProtocolId};
use crate::provider::gateway::{Gateway, GatewayId};
use crate::provider::protocol::anthropic::AnthropicRuntime;
use crate::provider::quirks::QuirkSet;

pub struct Anthropic;

const OAUTH_QUIRKS: QuirkSet = QuirkSet::CACHE_BREAKPOINT
    .union(QuirkSet::ADAPTIVE_THINKING)
    .union(QuirkSet::OAUTH_SYSTEM_REWRITE)
    .union(QuirkSet::ANTHROPIC_BETAS)
    .union(QuirkSet::CLAUDE_IDENTITY);
const API_KEY_QUIRKS: QuirkSet = QuirkSet::CACHE_BREAKPOINT.union(QuirkSet::ADAPTIVE_THINKING);

impl Gateway for Anthropic {
    fn id(&self) -> GatewayId {
        GatewayId::Anthropic
    }
    fn source(&self) -> &'static str {
        "anthropic"
    }
    fn vendor(&self) -> AuthVendor {
        AuthVendor::Anthropic
    }
    fn base_url(&self) -> &'static str {
        "https://api.anthropic.com"
    }
    fn auth_kind(&self, is_oauth: bool) -> AuthKind {
        if is_oauth {
            AuthKind::OAuthBearer
        } else {
            AuthKind::ApiKey
        }
    }
    fn quirks(&self, is_oauth: bool) -> QuirkSet {
        if is_oauth {
            OAUTH_QUIRKS
        } else {
            API_KEY_QUIRKS
        }
    }
    fn protocol_for(&self, _model_id: &str) -> ProtocolId {
        ProtocolId::AnthropicMessages
    }
    fn thinking(&self, model_id: &str) -> ThinkingCapabilities {
        use crate::provider::quirks::adaptive_thinking::is_adaptive_thinking_model;
        if is_adaptive_thinking_model(model_id) {
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
        } else {
            ThinkingCapabilities::standard()
        }
    }
    fn build(
        &self,
        binding: &ModelBinding,
        credential: &Credential,
        _session_id: &str,
    ) -> Box<dyn Provider> {
        Box::new(AnthropicRuntime::new(
            &binding.model_id,
            &binding.base_url,
            &credential.token,
            self.auth_kind(credential.is_oauth),
            self.quirks(credential.is_oauth),
            &credential.label,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_thinking_capabilities_include_max() {
        let labels: Vec<_> = Anthropic
            .thinking("claude-sonnet-4-6")
            .options()
            .iter()
            .map(|o| o.label)
            .collect();
        assert_eq!(labels, ["off", "low", "medium", "high", "max"]);
    }

    #[test]
    fn non_adaptive_thinking_capabilities_stop_at_high() {
        let labels: Vec<_> = Anthropic
            .thinking("claude-sonnet-4-5")
            .options()
            .iter()
            .map(|o| o.label)
            .collect();
        assert_eq!(labels, ["off", "low", "medium", "high"]);
    }

    #[test]
    fn oauth_quirks_include_full_claude_code_surface() {
        let qs = Anthropic.quirks(true);
        assert!(qs.contains(QuirkSet::OAUTH_SYSTEM_REWRITE));
        assert!(qs.contains(QuirkSet::ANTHROPIC_BETAS));
        assert!(qs.contains(QuirkSet::CLAUDE_IDENTITY));
    }

    #[test]
    fn api_key_quirks_skip_oauth_only_concerns() {
        let qs = Anthropic.quirks(false);
        assert!(qs.contains(QuirkSet::CACHE_BREAKPOINT));
        assert!(qs.contains(QuirkSet::ADAPTIVE_THINKING));
        assert!(!qs.contains(QuirkSet::OAUTH_SYSTEM_REWRITE));
        assert!(!qs.contains(QuirkSet::ANTHROPIC_BETAS));
        assert!(!qs.contains(QuirkSet::CLAUDE_IDENTITY));
    }
}
