//! Claude Code identity quirk: `User-Agent`, session id, fingerprint.
//!
//! Upstream references: `src/utils/http.ts::getUserAgent`,
//! `src/utils/fingerprint.ts::computeFingerprint`. The fingerprint gates
//! the backend's first-party client attribution — any drift from
//! upstream hashing breaks the OAuth path.

use crate::util::uuid_v4;

/// Upstream CLI version reverse-engineered from `~/.local/bin/claude@2.1.100`.
/// Used for `User-Agent`, `cc_version`, and as input to [`compute_fingerprint`].
/// Must match across the three so the backend's attribution validator
/// accepts the fingerprint.
pub const CLI_VERSION: &str = "2.1.100";

/// Hardcoded fingerprint salt — `src/utils/fingerprint.ts:8`.
const FINGERPRINT_SALT: &str = "59cf53e54c78";

/// First-user-message character indices sampled for the fingerprint hash.
const FINGERPRINT_POSITIONS: [usize; 3] = [4, 7, 20];

/// Stable per-process session id for the `X-Claude-Code-Session-Id` header.
pub fn claude_session_id() -> String {
    use std::sync::OnceLock;
    static SESSION_ID: OnceLock<String> = OnceLock::new();
    SESSION_ID
        .get_or_init(|| uuid_v4().unwrap_or_else(|| "unknown".to_owned()))
        .clone()
}

/// `claude-cli/{CLI_VERSION} (external, cli)` — `src/utils/http.ts::getUserAgent`.
pub fn claude_cli_user_agent() -> String {
    format!("claude-cli/{CLI_VERSION} (external, cli)")
}

/// 3-char attribution fingerprint — `src/utils/fingerprint.ts::computeFingerprint`.
/// `SHA256(SALT + msg[4] + msg[7] + msg[20] + version)[:3]`, missing positions
/// substituted with `'0'`. Backend-validated: any drift breaks attribution.
pub fn compute_fingerprint(first_user_content: &str) -> String {
    use sha2::{Digest, Sha256};
    let chars: String = FINGERPRINT_POSITIONS
        .iter()
        .map(|&p| first_user_content.chars().nth(p).unwrap_or('0'))
        .collect();
    let input = format!("{FINGERPRINT_SALT}{chars}{CLI_VERSION}");
    let hash = Sha256::digest(input.as_bytes());
    format!("{hash:x}")[..3].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_matches_upstream_shape() {
        let ua = claude_cli_user_agent();
        assert!(ua.starts_with("claude-cli/"));
        assert!(ua.ends_with(" (external, cli)"));
        assert!(ua.contains(CLI_VERSION));
    }

    #[test]
    fn session_id_is_stable_per_process() {
        let a = claude_session_id();
        let b = claude_session_id();
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn fingerprint_is_three_hex_chars() {
        let fp = compute_fingerprint("hello world, this is a short prompt");
        assert_eq!(fp.len(), 3);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn fingerprint_substitutes_zero_for_missing_positions() {
        // All three sample positions fall past the end → all '0's.
        assert_eq!(compute_fingerprint("abc"), compute_fingerprint(""));
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let a = compute_fingerprint("the quick brown fox jumps over lazy dog");
        let b = compute_fingerprint("the quick brown fox jumps over lazy dog");
        assert_eq!(a, b);
    }
}
