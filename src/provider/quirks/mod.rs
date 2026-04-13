//! Vendor-specific middleware for LLM providers.
//!
//! Each quirk is a pure function that augments a request body, set of
//! headers, or response stream for a specific vendor behavior. RFC 0002
//! will migrate legacy providers to invoke these via a `QuirkSet`
//! bitflag; during PR1 they are called directly from the legacy provider
//! modules to preserve behavior.

pub mod adaptive_thinking;
pub mod cache_breakpoint;
pub mod claude_identity;
pub mod oauth_system_rewrite;
