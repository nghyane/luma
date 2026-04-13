//! Gateway / binding scaffolding introduced by RFC 0002 commit 9a.
//!
//! This is a structural-only step: it names the concepts (`Gateway`,
//! `AuthScheme`, `ModelBinding`, `BindingRegistry`) and centralises the
//! `source` → concrete `Provider` dispatch that used to live inline in
//! `core::agent::turn::build_provider`. Wire behaviour is unchanged;
//! commits 9b/9c will plug `Protocol` impls + a pull-based
//! `ProviderRuntime` behind this same surface.
//!
//! Builtin bindings are hardcoded for now (see RULES §II.10 / decision
//! taken with RFC 0002 §Catalog — JSON loader is deferred to PR2).

use crate::config::auth::Credential;
use crate::core::provider::Provider;
use crate::core::types::ThinkingLevel;

/// Identifier for a transport gateway. One variant per distinct base URL
/// plus auth surface. Values are stable strings so they can appear in
/// `AgentConfig.source` and future catalog files without a mapping table.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayId {
    Anthropic,
    Codex,
    OpenAI,
}

impl GatewayId {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Codex => "codex",
            Self::OpenAI => "openai",
        }
    }

    /// Parse the `source` string currently stored in `AgentConfig`.
    ///
    /// Anything we don't recognise maps to `OpenAI` to preserve the old
    /// `_ => OpenAIProvider` fallback in `build_provider`.
    pub fn from_source(source: &str) -> Self {
        match source {
            "anthropic" => Self::Anthropic,
            "codex" => Self::Codex,
            _ => Self::OpenAI,
        }
    }
}

/// How a gateway authenticates. Intentionally coarse today — only the
/// three schemes we actually ship. Extended in PR2 when OpenCode Go lands.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthScheme {
    /// `x-api-key: <token>` or `Authorization: Bearer <token>` depending
    /// on whether the credential is an OAuth token (Claude).
    AnthropicApiKeyOrOAuth,
    /// OpenAI Responses with ChatGPT-account session headers (Codex).
    CodexSession,
    /// `Authorization: Bearer <token>` against OpenAI-compatible APIs.
    OpenAIBearer,
}

/// Transport + auth definition. Protocol and quirks intentionally live
/// outside this struct (RFC 0002 §Gateway: "Gateway MUST NOT chứa logic
/// protocol").
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Gateway {
    pub id: GatewayId,
    pub base_url: &'static str,
    pub auth: AuthScheme,
}

/// A `(gateway, model_id)` pair the user can select. This is the unit of
/// registry lookup. Protocol / quirks / thinking caps are derived today
/// from the hardcoded dispatch; commits 9b/9c will move them here as
/// explicit fields.
#[derive(Debug, Clone)]
pub struct ModelBinding {
    pub gateway: GatewayId,
    pub model_id: String,
}

impl ModelBinding {
    #[allow(dead_code)]
    pub fn display_id(&self) -> String {
        format!("{}/{}", self.gateway.as_str(), self.model_id)
    }
}

/// Hardcoded registry of builtin gateways and the dispatch table used to
/// materialise a concrete `Provider` from a `(binding, credential)` pair.
///
/// Kept intentionally small: three gateways, one `build` function. The
/// alternative (dyn `Protocol` + composed `Quirk`s) arrives with the
/// Protocol extraction in commit 9b.
pub struct BindingRegistry {
    #[allow(dead_code)]
    gateways: [Gateway; 3],
}

impl BindingRegistry {
    pub fn builtin() -> Self {
        Self {
            gateways: [
                Gateway {
                    id: GatewayId::Anthropic,
                    base_url: "https://api.anthropic.com",
                    auth: AuthScheme::AnthropicApiKeyOrOAuth,
                },
                Gateway {
                    id: GatewayId::Codex,
                    base_url: "https://chatgpt.com/backend-api/codex",
                    auth: AuthScheme::CodexSession,
                },
                Gateway {
                    id: GatewayId::OpenAI,
                    base_url: "https://api.openai.com",
                    auth: AuthScheme::OpenAIBearer,
                },
            ],
        }
    }

    /// Lookup the gateway metadata by id. Infallible — every `GatewayId`
    /// variant has a builtin entry.
    #[allow(dead_code)]
    pub fn gateway(&self, id: GatewayId) -> &Gateway {
        self.gateways
            .iter()
            .find(|g| g.id == id)
            .expect("builtin registry missing gateway variant")
    }

    /// Resolve a binding from the legacy `(source, model_id)` shape.
    pub fn resolve(&self, source: &str, model_id: &str) -> ModelBinding {
        ModelBinding {
            gateway: GatewayId::from_source(source),
            model_id: model_id.to_owned(),
        }
    }

    /// Build a ready-to-stream provider. Thinking level is coerced to the
    /// provider's supported set, matching the old `build_provider`
    /// behaviour in `turn.rs` byte-for-byte.
    pub fn build(
        &self,
        binding: &ModelBinding,
        credential: &Credential,
        session_id: &str,
        thinking: ThinkingLevel,
    ) -> Box<dyn Provider> {
        use crate::provider::claude::ClaudeProvider;
        use crate::provider::codex::CodexProvider;
        use crate::provider::openai::OpenAIProvider;

        let mut provider: Box<dyn Provider> = match binding.gateway {
            GatewayId::Anthropic => Box::new(ClaudeProvider::new(
                &binding.model_id,
                &credential.token,
                credential.is_oauth,
                &credential.label,
            )),
            GatewayId::Codex => Box::new(CodexProvider::new(
                &binding.model_id,
                &credential.token,
                credential.account_id.clone(),
                session_id,
                &credential.label,
            )),
            GatewayId::OpenAI => Box::new(OpenAIProvider::new(
                &binding.model_id,
                &credential.token,
                &credential.label,
            )),
        };
        let coerced = provider.thinking_capabilities().coerce(thinking);
        provider.set_thinking(coerced);
        provider
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_id_roundtrips_through_source_string() {
        assert_eq!(GatewayId::from_source("anthropic"), GatewayId::Anthropic);
        assert_eq!(GatewayId::from_source("codex"), GatewayId::Codex);
        assert_eq!(GatewayId::from_source("openai"), GatewayId::OpenAI);
        assert_eq!(GatewayId::from_source("unknown"), GatewayId::OpenAI);
        assert_eq!(GatewayId::Anthropic.as_str(), "anthropic");
    }

    #[test]
    fn builtin_registry_has_all_three_gateways() {
        let reg = BindingRegistry::builtin();
        assert_eq!(reg.gateway(GatewayId::Anthropic).id, GatewayId::Anthropic);
        assert_eq!(reg.gateway(GatewayId::Codex).id, GatewayId::Codex);
        assert_eq!(reg.gateway(GatewayId::OpenAI).id, GatewayId::OpenAI);
    }

    #[test]
    fn resolve_preserves_model_id_and_display_format() {
        let reg = BindingRegistry::builtin();
        let b = reg.resolve("anthropic", "claude-sonnet-4-6");
        assert_eq!(b.gateway, GatewayId::Anthropic);
        assert_eq!(b.model_id, "claude-sonnet-4-6");
        assert_eq!(b.display_id(), "anthropic/claude-sonnet-4-6");
    }
}
