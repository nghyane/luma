//! Alibaba Cloud Coding Plan gateway.
//!
//! Coding Plan exposes OpenAI-compatible and Anthropic-compatible lanes on
//! the same host family. We route per model so the user sees one `alibaba`
//! source while reusing the existing protocol runtimes.

use crate::config::auth::{AuthVendor, Credential};
use crate::core::provider::{Provider, ThinkingCapabilities, ThinkingOption};
use crate::core::types::ThinkingLevel;
use crate::provider::binding::{ModelBinding, ProtocolId};
use crate::provider::gateway::{Gateway, GatewayId};
use crate::provider::protocol::anthropic::AnthropicRuntime;
use crate::provider::protocol::openai_chat::{OpenAIChatConfig, OpenAIChatRuntime};
use crate::provider::quirks::QuirkSet;

pub struct Alibaba;

const OPENAI_BASE_URL: &str = "https://coding-intl.dashscope.aliyuncs.com/v1";
const ANTHROPIC_BASE_URL: &str = "https://coding-intl.dashscope.aliyuncs.com/apps/anthropic/v1";

/// Per-model wire protocol. Coding Plan docs expose both compatibility
/// lanes; Qwen-family and GLM-family coding models use the OpenAI lane,
/// while Claude/MiniMax-compatible models use the Anthropic lane.
const MODELS: &[(&str, ProtocolId)] = &[
    ("glm-4.7", ProtocolId::OpenAIChat),
    ("glm-5", ProtocolId::OpenAIChat),
    ("kimi-k2.5", ProtocolId::OpenAIChat),
    ("qwen3.5-plus", ProtocolId::OpenAIChat),
    ("qwen3-coder-plus", ProtocolId::OpenAIChat),
    ("qwen3-coder-next", ProtocolId::OpenAIChat),
    ("qwen3-max-2026-01-23", ProtocolId::OpenAIChat),
    ("claude-sonnet-4-5", ProtocolId::AnthropicMessages),
    ("claude-sonnet-4-6", ProtocolId::AnthropicMessages),
    ("claude-opus-4-5", ProtocolId::AnthropicMessages),
    ("claude-opus-4-6", ProtocolId::AnthropicMessages),
    ("minimax-m2.5", ProtocolId::AnthropicMessages),
];

impl Alibaba {
    fn base_url_for(protocol: ProtocolId) -> &'static str {
        match protocol {
            ProtocolId::AnthropicMessages => ANTHROPIC_BASE_URL,
            ProtocolId::OpenAIChat => OPENAI_BASE_URL,
            ProtocolId::OpenAIResponses => {
                unreachable!("alibaba coding plan does not expose Responses API")
            }
            ProtocolId::KiroEventStream => {
                unreachable!("alibaba coding plan does not use Kiro Event Stream")
            }
        }
    }

    /// Qwen-family models support `enable_thinking` via the OpenAI lane.
    fn model_supports_thinking(model_id: &str) -> bool {
        matches!(
            model_id,
            "qwen3.5-plus"
                | "qwen3-coder-plus"
                | "qwen3-coder-next"
                | "qwen3-max-2026-01-23"
                | "glm-4.7"
                | "glm-5"
                | "kimi-k2.5"
        )
    }
}

impl Gateway for Alibaba {
    fn id(&self) -> GatewayId {
        GatewayId::Alibaba
    }

    fn source(&self) -> &'static str {
        "alibaba"
    }

    fn vendor(&self) -> AuthVendor {
        AuthVendor::Alibaba
    }

    fn base_url(&self) -> &'static str {
        OPENAI_BASE_URL
    }

    fn quirks(&self, _is_oauth: bool) -> QuirkSet {
        QuirkSet::EMPTY
    }

    fn protocol_for(&self, model_id: &str) -> ProtocolId {
        MODELS
            .iter()
            .find(|(id, _)| *id == model_id)
            .map(|(_, protocol)| *protocol)
            .unwrap_or(ProtocolId::OpenAIChat)
    }

    fn thinking(&self, model_id: &str) -> ThinkingCapabilities {
        if Self::model_supports_thinking(model_id) {
            // Alibaba models expose on/off only — no granular levels.
            ThinkingCapabilities::new(vec![
                ThinkingOption {
                    level: ThinkingLevel::Off,
                    label: "off",
                },
                ThinkingOption {
                    level: ThinkingLevel::High,
                    label: "on",
                },
            ])
        } else {
            ThinkingCapabilities::off_only()
        }
    }

    fn build(
        &self,
        binding: &ModelBinding,
        credential: &Credential,
        _session_id: &str,
        _options: crate::provider::gateway::ProviderOptions,
    ) -> Box<dyn Provider> {
        let base_url = Self::base_url_for(binding.protocol);
        match binding.protocol {
            ProtocolId::AnthropicMessages => Box::new(AnthropicRuntime::new(
                &binding.model_id,
                base_url,
                &credential.token,
                false,
                self.quirks(credential.is_oauth),
                &credential.label,
            )),
            ProtocolId::OpenAIChat => {
                let config = if Self::model_supports_thinking(&binding.model_id) {
                    OpenAIChatConfig::default()
                        .with_endpoint_path("/chat/completions")
                        .with_thinking_support()
                } else {
                    OpenAIChatConfig::default().with_endpoint_path("/chat/completions")
                };
                Box::new(OpenAIChatRuntime::new_with_config(
                    &binding.model_id,
                    base_url,
                    &credential.token,
                    &credential.label,
                    config,
                ))
            }
            ProtocolId::OpenAIResponses => {
                unreachable!("alibaba coding plan does not expose the Responses API")
            }
            ProtocolId::KiroEventStream => {
                unreachable!("alibaba coding plan does not use Kiro Event Stream")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_table_routes_qwen_to_openai() {
        assert_eq!(
            Alibaba.protocol_for("qwen3-coder-next"),
            ProtocolId::OpenAIChat
        );
    }

    #[test]
    fn protocol_table_routes_claude_to_anthropic() {
        assert_eq!(
            Alibaba.protocol_for("claude-sonnet-4-6"),
            ProtocolId::AnthropicMessages
        );
    }

    #[test]
    fn unknown_models_fall_back_to_openai_lane() {
        assert_eq!(Alibaba.protocol_for("unknown"), ProtocolId::OpenAIChat);
    }

    #[test]
    fn glm_4_7_routes_to_openai() {
        assert_eq!(Alibaba.protocol_for("glm-4.7"), ProtocolId::OpenAIChat);
    }

    #[test]
    fn qwen_models_have_thinking_capabilities() {
        let caps = Alibaba.thinking("qwen3.5-plus");
        let labels: Vec<_> = caps.options().iter().map(|o| o.label).collect();
        assert_eq!(labels, ["off", "on"]);
    }

    #[test]
    fn glm_models_have_thinking_capabilities() {
        let caps = Alibaba.thinking("glm-4.7");
        let labels: Vec<_> = caps.options().iter().map(|o| o.label).collect();
        assert_eq!(labels, ["off", "on"]);
    }

    #[test]
    fn claude_models_no_thinking_through_openai_lane() {
        // Claude models use Anthropic lane; their thinking is handled
        // by the Anthropic runtime, not the OpenAI enable_thinking flag.
        let caps = Alibaba.thinking("claude-sonnet-4-6");
        let labels: Vec<_> = caps.options().iter().map(|o| o.label).collect();
        assert_eq!(labels, ["off"]);
    }
}
