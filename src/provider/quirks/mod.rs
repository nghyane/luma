//! Vendor-specific middleware for LLM providers.
//!
//! Each quirk is a pure function that augments a request body, set of
//! headers, or response stream for a specific vendor behavior. Callers
//! invoke them directly from the protocol module that needs them.

pub mod adaptive_thinking;
pub mod cache_breakpoint;
pub mod claude_identity;
pub mod oauth_system_rewrite;
