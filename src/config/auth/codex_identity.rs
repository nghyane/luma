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
    let fresh = new_uuid_v4()?;
    let _ = fs::write(&path, &fresh);
    Some(fresh)
}

fn installation_id_path() -> PathBuf {
    home_dir().join(".codex").join(INSTALLATION_ID_FILENAME)
}

/// Loose UUID check — 8-4-4-4-12 hex groups, case-insensitive. Matches
/// upstream's `Uuid::parse_str` acceptance for the `installation_id` file.
fn is_uuid(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    let dashes = [8, 13, 18, 23];
    for (i, b) in bytes.iter().enumerate() {
        if dashes.contains(&i) {
            if *b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// Generate a random UUIDv4-format string without pulling in `uuid` crate.
/// 16 bytes of OS entropy, with the version/variant nibbles set per RFC 4122.
fn new_uuid_v4() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).ok()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 10
    Some(format!(
        "{:02x}{:02x}{:02x}{:02x}-\
         {:02x}{:02x}-\
         {:02x}{:02x}-\
         {:02x}{:02x}-\
         {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    ))
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

    #[test]
    fn is_uuid_accepts_canonical_form() {
        assert!(is_uuid("550e8400-e29b-41d4-a716-446655440000"));
        assert!(is_uuid("550E8400-E29B-41D4-A716-446655440000"));
    }

    #[test]
    fn is_uuid_rejects_bad_shape() {
        assert!(!is_uuid("not-a-uuid"));
        assert!(!is_uuid("550e8400e29b41d4a716446655440000"));
        assert!(!is_uuid("550e8400-e29b-41d4-a716-44665544000z"));
        assert!(!is_uuid(""));
    }

    #[test]
    fn new_uuid_v4_is_well_formed() {
        let u = new_uuid_v4().expect("entropy");
        assert!(is_uuid(&u));
        // Version nibble at byte 6 (hex position 14) must be '4'.
        assert_eq!(u.as_bytes()[14], b'4');
        // Variant nibble at byte 8 (hex position 19) must be 8/9/a/b.
        let v = u.as_bytes()[19];
        assert!(matches!(v, b'8' | b'9' | b'a' | b'b'));
    }

    #[test]
    fn new_uuid_v4_is_random() {
        let a = new_uuid_v4().unwrap();
        let b = new_uuid_v4().unwrap();
        assert_ne!(a, b);
    }
}
