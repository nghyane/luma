//! OpenCode Go (opencode.ai/zen/go — paid proxy for open coding models).
//!
//! Single host serves both Anthropic Messages and OpenAI Chat on
//! distinct endpoint paths; the wire protocol is per-model. See
//! <https://opencode.ai/docs/go/>.

use crate::config::auth::{AuthVendor, Credential};
use crate::core::provider::{Provider, ThinkingCapabilities};
use crate::provider::binding::{ModelBinding, ProtocolId};
use crate::provider::gateway::{Gateway, GatewayId};
use crate::provider::protocol::anthropic::AnthropicRuntime;
use crate::provider::protocol::openai_chat::OpenAIChatRuntime;
use crate::provider::quirks::QuirkSet;

pub struct OpenCodeGo;

/// Per-model wire protocol. Unknown models default to AnthropicMessages
/// at the call site so a typo surfaces as a clean 404 from the proxy
/// rather than a silent protocol misroute.
const MODELS: &[(&str, ProtocolId)] = &[
    ("glm-5", ProtocolId::OpenAIChat),
    ("glm-5.1", ProtocolId::OpenAIChat),
    ("kimi-k2.5", ProtocolId::OpenAIChat),
    ("mimo-v2-pro", ProtocolId::OpenAIChat),
    ("mimo-v2-omni", ProtocolId::OpenAIChat),
    ("minimax-m2.5", ProtocolId::AnthropicMessages),
    ("minimax-m2.7", ProtocolId::AnthropicMessages),
];

impl Gateway for OpenCodeGo {
    fn id(&self) -> GatewayId {
        GatewayId::OpenCodeGo
    }
    fn source(&self) -> &'static str {
        "opencode-go"
    }
    fn vendor(&self) -> AuthVendor {
        AuthVendor::OpenCodeGo
    }
    fn base_url(&self) -> &'static str {
        "https://opencode.ai/zen/go"
    }
    fn quirks(&self, _is_oauth: bool) -> QuirkSet {
        QuirkSet::EMPTY
    }
    fn protocol_for(&self, model_id: &str) -> ProtocolId {
        MODELS
            .iter()
            .find(|(id, _)| *id == model_id)
            .map(|(_, p)| *p)
            .unwrap_or(ProtocolId::AnthropicMessages)
    }
    fn thinking(&self, _model_id: &str) -> ThinkingCapabilities {
        ThinkingCapabilities::off_only()
    }
    fn build(
        &self,
        binding: &ModelBinding,
        credential: &Credential,
        _session_id: &str,
    ) -> Box<dyn Provider> {
        match binding.protocol {
            ProtocolId::AnthropicMessages => Box::new(AnthropicRuntime::new(
                &binding.model_id,
                &binding.base_url,
                &credential.token,
                // OpenCode Go's `/v1/messages` requires `x-api-key`
                // (Anthropic-native shape); Bearer returns 401
                // "Missing API key". Force the api-key wire shape
                // regardless of the credential's is_oauth bit.
                false,
                self.quirks(credential.is_oauth),
                &credential.label,
            )),
            ProtocolId::OpenAIChat => Box::new(OpenAIChatRuntime::new(
                &binding.model_id,
                &binding.base_url,
                &credential.token,
                &credential.label,
            )),
            ProtocolId::OpenAIResponses => {
                unreachable!("opencode-go does not expose the Responses API")
            }
            ProtocolId::KiroEventStream => {
                unreachable!("opencode-go does not use Kiro Event Stream")
            }
        }
    }
}
