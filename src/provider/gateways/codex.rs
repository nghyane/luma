//! Codex (ChatGPT-account OpenAI Responses API).

use crate::config::auth::{AuthVendor, Credential};
use crate::core::provider::{Provider, ThinkingCapabilities};
use crate::provider::binding::{ModelBinding, ProtocolId};
use crate::provider::gateway::{Gateway, GatewayId};
use crate::provider::protocol::openai_responses::OpenAIResponsesRuntime;
use crate::provider::quirks::QuirkSet;

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
    fn build(
        &self,
        binding: &ModelBinding,
        credential: &Credential,
        session_id: &str,
    ) -> Box<dyn Provider> {
        Box::new(OpenAIResponsesRuntime::new(
            &binding.model_id,
            &binding.base_url,
            &credential.token,
            credential.account_id.clone(),
            session_id,
            &credential.label,
        ))
    }
}
