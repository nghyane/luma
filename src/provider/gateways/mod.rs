//! Builtin gateway registry.
//!
//! Each gateway lives in its own module. `GATEWAYS` is the single
//! enumeration site; adding a gateway = new module + one row here +
//! one variant in `GatewayId`.

pub mod anthropic;
pub mod codex;
pub mod openai;
pub mod opencode_go;

use crate::provider::gateway::{Gateway, GatewayId};

/// Static list of every builtin gateway. Order is the display order
/// used by anything that wants to enumerate (e.g. login picker, account
/// listing).
pub static GATEWAYS: &[&dyn Gateway] = &[
    &anthropic::Anthropic,
    &codex::Codex,
    &openai::OpenAI,
    &opencode_go::OpenCodeGo,
];

/// Look up the gateway implementation for `id`. Infallible — every
/// `GatewayId` variant has exactly one entry in `GATEWAYS`; missing
/// entries are a compile-time bug caught by the test below.
pub fn lookup(id: GatewayId) -> &'static dyn Gateway {
    GATEWAYS
        .iter()
        .copied()
        .find(|g| g.id() == id)
        .expect("GATEWAYS missing entry for GatewayId variant")
}

/// Look up by `AgentConfig.source` string. `None` for unknown sources;
/// callers decide the fallback (today: `binding::resolve` returns OpenAI).
pub fn lookup_source(source: &str) -> Option<&'static dyn Gateway> {
    GATEWAYS.iter().copied().find(|g| g.source() == source)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time-ish guarantee: every GatewayId variant has a row.
    /// If the registry drifts from the enum, this test fails immediately.
    #[test]
    fn every_gateway_id_variant_is_registered() {
        for id in [
            GatewayId::Anthropic,
            GatewayId::Codex,
            GatewayId::OpenAI,
            GatewayId::OpenCodeGo,
        ] {
            let g = lookup(id);
            assert_eq!(g.id(), id);
            assert!(!g.source().is_empty());
            assert!(g.base_url().starts_with("https://"));
        }
    }

    #[test]
    fn lookup_source_is_none_for_unknown() {
        assert!(lookup_source("unknown").is_none());
    }
}
