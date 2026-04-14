//! `src/auth` — new auth architecture (RFC 0009).
//!
//! Phase 1 (PR1): domain model + error types.
//! Subsequent PRs will add `repo`, `service`, `oauth/*`, `import/*`.

pub mod domain;
pub mod error;
pub mod oauth;
pub mod repo;
pub mod selection;
pub mod service;
