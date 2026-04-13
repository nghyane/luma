//! OpenAI direct (api.openai.com Chat Completions).

use crate::config::auth::{AuthVendor, Credential};
use crate::core::provider::{Provider, ThinkingCapabilities};
use crate::provider::binding::{ModelBinding, ProtocolId};
use crate::provider::gateway::{Gateway, GatewayId};
use crate::provider::protocol::openai_chat::OpenAIChatRuntime;
use crate::provider::quirks::QuirkSet;

pub struct OpenAI;

impl Gateway for OpenAI {
    fn id(&self) -> GatewayId {
        GatewayId::OpenAI
    }
    fn source(&self) -> &'static str {
        "openai"
    }
    fn vendor(&self) -> AuthVendor {
        AuthVendor::OpenAI
    }
    fn base_url(&self) -> &'static str {
        "https://api.openai.com/v1"
    }
    fn quirks(&self, _is_oauth: bool) -> QuirkSet {
        QuirkSet::EMPTY
    }
    fn protocol_for(&self, _model_id: &str) -> ProtocolId {
        ProtocolId::OpenAIChat
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
        Box::new(OpenAIChatRuntime::new(
            &binding.model_id,
            &binding.base_url,
            &credential.token,
            &credential.label,
        ))
    }
}
