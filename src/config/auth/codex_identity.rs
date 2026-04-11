//! Codex upstream identity — originator, user-agent, and installation id.
//!
//! Mirrors the values the official `codex_cli_rs` CLI sets on every request
//! to `chatgpt.com/backend-api/codex/responses`:
//!
//! * `originator` header — fixed literal `codex_cli_rs`, enforced by the
//!   backend as a "first-party" client check.
//! * `User-Agent` header — `codex_cli_rs/<version>`.
//! * `session_id` header — per-conversation stable id (we pass the luma
//!   session id directly; upstream uses a UUID string but the backend
//!   accepts any stable opaque identifier).
//! * `client_metadata.x-codex-installation-id` body field — persisted UUID
//!   shared with the official CLI at `~/.codex/installation_id` so luma
//!   and codex-cli correlate to the same install. Generated on first use.

use super::home_dir;
use crate::util::{is_uuid, uuid_v4};
use std::fs;
use std::path::PathBuf;

/// Originator value sent both as a PKCE `&originator=...` query param and
/// as an `originator` request header. Must exactly match upstream or the
/// backend's first-party check fails.
pub(crate) const CODEX_ORIGINATOR: &str = "codex_cli_rs";

/// Filename under `~/.codex/` holding the persistent installation UUID.
const INSTALLATION_ID_FILENAME: &str = "installation_id";

/// Returns the `User-Agent` string upstream sends.
///
/// Format matches `get_codex_user_agent()` in
/// `codex-rs/login/src/auth/default_client.rs`: `{originator}/{version}`.
pub(crate) fn codex_user_agent() -> String {
    format!("{CODEX_ORIGINATOR}/{}", env!("CARGO_PKG_VERSION"))
}

/// Resolve the persistent installation id, reusing the existing file when
/// present (so codex-cli and luma see the same id) and creating one on
/// first run. Returns `None` only when both read and write fail; callers
/// should degrade gracefully by omitting the `client_metadata` field.
pub(crate) fn resolve_installation_id() -> Option<String> {
    let path = installation_id_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(raw) = fs::read_to_string(&path) {
        let trimmed = raw.trim();
        if is_uuid(trimmed) {
            return Some(trimmed.to_ascii_lowercase());
        }
    }
    let fresh = uuid_v4()?;
    let _ = fs::write(&path, &fresh);
    Some(fresh)
}

fn installation_id_path() -> PathBuf {
    home_dir().join(".codex").join(INSTALLATION_ID_FILENAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn originator_is_first_party() {
        assert_eq!(CODEX_ORIGINATOR, "codex_cli_rs");
    }

    #[test]
    fn user_agent_has_originator_slash_version() {
        let ua = codex_user_agent();
        assert!(ua.starts_with("codex_cli_rs/"));
        assert!(ua.len() > "codex_cli_rs/".len());
    }
}
