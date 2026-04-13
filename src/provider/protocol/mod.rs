//! Wire-protocol modules for RFC 0002.
//!
//! Each submodule owns the concrete streaming logic for one LLM wire
//! format. They are the destination of the `Protocol` trait declared in
//! `core::provider` — but today (RFC 0002 commit 9 structural cutover)
//! they each still expose a thin `impl Provider` carrying the legacy
//! push-model SSE loop. `ProviderRuntime` (see `provider::runtime`) is
//! the sole object-safe façade the rest of the codebase interacts with;
//! it dispatches to whichever protocol runtime the `ModelBinding` selects.
//!
//! A follow-up session extracts pure `Protocol::encode_request` +
//! `decode_stream` halves and a `MessageAssembler` consumer, at which
//! point the per-runtime `impl Provider` blocks here get deleted in
//! favour of generic composition inside `ProviderRuntime`. The module
//! layout is already in its final shape so that refactor is localised.

pub mod anthropic;
pub mod openai_chat;
pub mod openai_responses;
