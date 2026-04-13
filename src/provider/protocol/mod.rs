//! Wire-protocol modules.
//!
//! Each submodule owns the concrete streaming logic for one LLM wire
//! format and exposes it as a `Provider` impl (see `core::provider`).
//! `binding::build_provider` picks one based on the resolved
//! `ModelBinding.gateway`.

pub mod anthropic;
pub mod openai_chat;
pub mod openai_responses;
