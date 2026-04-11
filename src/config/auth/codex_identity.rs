//! Upstream `codex_cli_rs` identity — originator, user-agent, install id.
//!
//! Values mirror the official Codex CLI so the ChatGPT backend accepts luma
//! as a first-party client. See `codex-rs/login/src/auth/default_client.rs`
//! and `codex-rs/core/src/installation_id.rs`.

use super::home_dir;
use crate::util::{is_uuid, uuid_v4};
use std::fs;
use std::path::PathBuf;

/// Originator literal sent as a PKCE query param and request header.
pub(crate) const CODEX_ORIGINATOR: &str = "codex_cli_rs";

const INSTALLATION_ID_FILENAME: &str = "installation_id";

/// `{originator}/{cargo-version}` — matches `get_codex_user_agent()` upstream.
pub(crate) fn codex_user_agent() -> String {
    format!("{CODEX_ORIGINATOR}/{}", env!("CARGO_PKG_VERSION"))
}

/// Read `~/.codex/installation_id` if present, otherwise generate a UUID
/// and persist it. Returns `None` only if both read and write fail.
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
