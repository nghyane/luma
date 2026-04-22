//! Cloudflare AI Gateway (OpenAI-compatible, user-provided URL + key).

use crate::config::auth::{AuthVendor, Credential};
use crate::core::provider::{Provider, ThinkingCapabilities};
use crate::provider::binding::{ModelBinding, ProtocolId};
use crate::provider::gateway::{Gateway, GatewayId};
use crate::provider::protocol::openai_chat::{OpenAIChatConfig, OpenAIChatRuntime};
use crate::provider::quirks::QuirkSet;

pub struct Cloudflare;

impl Gateway for Cloudflare {
    fn id(&self) -> GatewayId {
        GatewayId::Cloudflare
    }
    fn source(&self) -> &'static str {
        "cloudflare"
    }
    fn vendor(&self) -> AuthVendor {
        AuthVendor::Cloudflare
    }
    fn base_url(&self) -> &'static str {
        // Placeholder — actual URL comes from the credential's per-account base_url.
        "https://gateway.ai.cloudflare.com"
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
        let base_url = credential.base_url.as_deref().unwrap_or(&binding.base_url);
        let config = OpenAIChatConfig::default()
            .with_endpoint_path("/chat/completions")
            .with_auth_header("cf-aig-authorization");
        Box::new(OpenAIChatRuntime::new_with_config(
            &binding.model_id,
            base_url,
            &credential.token,
            &credential.label,
            config,
        ))
    }
}
