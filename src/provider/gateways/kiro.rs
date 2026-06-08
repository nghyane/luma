//! Kiro (Amazon Q / CodeWhisperer) gateway.

use crate::config::auth::{AuthVendor, Credential};
use crate::core::provider::{Provider, ThinkingCapabilities};
use crate::provider::binding::{ModelBinding, ProtocolId};
use crate::provider::gateway::{Gateway, GatewayId};
use crate::provider::protocol::kiro::KiroRuntime;
use crate::provider::quirks::QuirkSet;

pub struct Kiro;

impl Gateway for Kiro {
    fn id(&self) -> GatewayId {
        GatewayId::Kiro
    }
    fn source(&self) -> &'static str {
        "kiro"
    }
    fn vendor(&self) -> AuthVendor {
        AuthVendor::Kiro
    }
    fn base_url(&self) -> &'static str {
        "https://q.us-east-1.amazonaws.com"
    }
    fn quirks(&self, _is_oauth: bool) -> QuirkSet {
        QuirkSet::EMPTY
    }
    fn protocol_for(&self, _model_id: &str) -> ProtocolId {
        ProtocolId::KiroEventStream
    }
    fn thinking(&self, _model_id: &str) -> ThinkingCapabilities {
        ThinkingCapabilities::off_only()
    }
    fn build(
        &self,
        binding: &ModelBinding,
        credential: &Credential,
        session_id: &str,
        _options: crate::provider::gateway::ProviderOptions,
    ) -> Box<dyn Provider> {
        Box::new(KiroRuntime::new(
            &binding.model_id,
            &binding.base_url,
            &credential.token,
            credential.profile_arn.clone(),
            session_id,
        ))
    }
}
