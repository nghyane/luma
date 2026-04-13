//! Gateway abstraction.
//!
//! A `Gateway` owns every per-provider concern: which auth pool bucket it
//! pulls credentials from, how those credentials become wire headers,
//! which quirks apply, which wire protocol each model uses, and how to
//! materialise a streaming runtime. The `binding` module is a thin
//! dispatcher that looks up the gateway for a request and calls into it.
//!
//! Adding a new gateway: add `src/provider/gateways/<name>.rs`
//! (`pub struct X; impl Gateway for X { ... }`), add `pub mod <name>;`
//! and one entry in `gateways::GATEWAYS`. Compiler-checked exhaustiveness
//! comes from `GatewayId`.

use crate::config::auth::{AuthVendor, Credential};
use crate::core::provider::{Provider, ThinkingCapabilities};
use crate::core::types::ThinkingLevel;
use crate::provider::binding::{ModelBinding, ProtocolId};
use crate::provider::quirks::QuirkSet;

/// Stable identifier for a gateway. One variant per `gateways::GATEWAYS`
/// entry; consumed by `AgentConfig.source` parsing and registry lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayId {
    Anthropic,
    Codex,
    OpenAI,
    OpenCodeGo,
}

/// Per-gateway behaviour. Object-safe; `gateways::GATEWAYS` stores
/// `&'static dyn Gateway`. Methods are pure where possible — `build`
/// is the only one that constructs anything.
pub trait Gateway: Send + Sync {
    /// Compile-time identifier matching exactly one variant of
    /// `GatewayId`.
    fn id(&self) -> GatewayId;

    /// `AgentConfig.source` value that resolves to this gateway.
    fn source(&self) -> &'static str;

    /// Auth-pool bucket this gateway's credentials live in.
    fn vendor(&self) -> AuthVendor;

    /// Base URL (scheme + host, no trailing slash) that runtimes append
    /// protocol-specific endpoint paths to.
    fn base_url(&self) -> &'static str;

    /// Quirks that apply to a request from this gateway with a credential
    /// of the given kind.
    fn quirks(&self, is_oauth: bool) -> QuirkSet;

    /// Wire protocol for `model_id` on this gateway. Most gateways
    /// return a constant; OpenCode Go consults a per-model table.
    fn protocol_for(&self, model_id: &str) -> ProtocolId;

    /// Thinking-control capabilities surfaced to the TUI.
    fn thinking(&self, model_id: &str) -> ThinkingCapabilities;

    /// Materialise a streaming runtime for `binding` using `credential`.
    /// `session_id` is forwarded to runtimes that need per-turn session
    /// headers (Codex Responses).
    fn build(
        &self,
        binding: &ModelBinding,
        credential: &Credential,
        session_id: &str,
    ) -> Box<dyn Provider>;

    /// Convenience: coerce `desired` to a level this gateway supports
    /// for `model_id`. Default reads from `thinking`.
    fn coerce_thinking(&self, model_id: &str, desired: ThinkingLevel) -> ThinkingLevel {
        self.thinking(model_id).coerce(desired)
    }
}
