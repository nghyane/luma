//! Codex (ChatGPT-account OpenAI Responses API).

use crate::config::auth::{AuthVendor, Credential};
use crate::core::provider::{Provider, ThinkingCapabilities};
use crate::core::types::LatencyMode;
use crate::provider::binding::{ModelBinding, ProtocolId};
use crate::provider::gateway::{Gateway, GatewayId, ProviderOptions};
use crate::provider::protocol::openai_responses::OpenAIResponsesRuntime;
use crate::provider::quirks::QuirkSet;

const CODEX_FASTMODE_SERVICE_TIER: &str = "priority";
const CODEX_STANDARD_SERVICE_TIER: &str = "default";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexServiceTier {
    Default,
    Priority,
}

impl CodexServiceTier {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => CODEX_STANDARD_SERVICE_TIER,
            Self::Priority => CODEX_FASTMODE_SERVICE_TIER,
        }
    }
}

pub struct Codex;

impl Gateway for Codex {
    fn id(&self) -> GatewayId {
        GatewayId::Codex
    }
    fn source(&self) -> &'static str {
        "codex"
    }
    fn vendor(&self) -> AuthVendor {
        AuthVendor::OpenAI
    }
    fn base_url(&self) -> &'static str {
        "https://chatgpt.com/backend-api/codex"
    }
    fn quirks(&self, _is_oauth: bool) -> QuirkSet {
        QuirkSet::EMPTY
    }
    fn protocol_for(&self, _model_id: &str) -> ProtocolId {
        ProtocolId::OpenAIResponses
    }
    fn thinking(&self, _model_id: &str) -> ThinkingCapabilities {
        ThinkingCapabilities::standard()
    }
    fn supports_fast_mode(&self) -> bool {
        true
    }
    fn build(
        &self,
        binding: &ModelBinding,
        credential: &Credential,
        session_id: &str,
        options: ProviderOptions,
    ) -> Box<dyn Provider> {
        Box::new(
            OpenAIResponsesRuntime::new(
                &binding.model_id,
                &binding.base_url,
                &credential.token,
                credential.account_id.clone(),
                session_id,
                &credential.label,
            )
            .with_service_tier(codex_oauth_service_tier(credential, options)),
        )
    }
}

fn codex_oauth_service_tier(credential: &Credential, options: ProviderOptions) -> Option<String> {
    if !credential.is_oauth {
        return None;
    }
    let tier = match options.latency {
        LatencyMode::Fast => CodexServiceTier::Priority,
        LatencyMode::Standard => CodexServiceTier::Default,
    };
    Some(tier.as_str().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_oauth_uses_priority_service_tier() {
        let credential = Credential {
            token: "token".into(),
            is_oauth: true,
            account_id: None,
            label: "acct".into(),
            profile_arn: None,
            account_key: None,
            base_url: None,
        };

        assert_eq!(
            codex_oauth_service_tier(
                &credential,
                ProviderOptions {
                    latency: LatencyMode::Fast
                }
            )
            .as_deref(),
            Some("priority")
        );
    }

    #[test]
    fn codex_oauth_can_use_default_service_tier() {
        let credential = Credential {
            token: "token".into(),
            is_oauth: true,
            account_id: None,
            label: "acct".into(),
            profile_arn: None,
            account_key: None,
            base_url: None,
        };

        assert_eq!(
            codex_oauth_service_tier(
                &credential,
                ProviderOptions {
                    latency: LatencyMode::Standard
                }
            )
            .as_deref(),
            Some("default")
        );
    }
}
