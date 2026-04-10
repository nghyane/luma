/// Auth — resolve credentials from Claude Code keychain, Codex auth, or managed cache.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Provider identity for auth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthProvider {
    Anthropic,
    OpenAI,
}

/// Resolved credential from any auth source. Providers pick what they need.
/// Refresh token and expiry live in ManagedEntry (internal cache), not here.
#[derive(Debug, Clone)]
pub struct Credential {
    pub token: String,
    pub is_oauth: bool,
    pub account_id: Option<String>,
}

const CLAUDE_OAUTH_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const OPENAI_OAUTH_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

#[derive(Debug, Serialize, Deserialize, Default)]
struct ManagedStore {
    credentials: Vec<ManagedEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ManagedEntry {
    provider: String,
    access_token: String,
    refresh_token: Option<String>,
    account_id: Option<String>,
    is_oauth: bool,
    /// Unix timestamp (seconds) when the access token expires. `None` only
    /// when we can't determine it (non-JWT token, missing `exp` claim).
    expires_at: Option<String>,
}

/// Resolve auth for a provider. Checks managed cache, then local sources.
/// Automatically refreshes expired tokens when refresh_token is available.
pub async fn resolve(provider: AuthProvider) -> Result<Credential> {
    resolve_inner(provider, false).await
}

/// Force a refresh of cached credentials even if the local clock says the
/// token is still valid. Used after a 401 from the provider, which can
/// happen when the server has revoked a token, the client clock is skewed,
/// or the local copy is stale (e.g. from another app rotating the keychain).
pub async fn force_refresh(provider: AuthProvider) -> Result<Credential> {
    resolve_inner(provider, true).await
}

async fn resolve_inner(provider: AuthProvider, force: bool) -> Result<Credential> {
    // Start from whichever source has the freshest refresh_token.
    // Managed cache wins if present (we keep it in sync on refresh); fall
    // back to local source (keychain / ~/.codex/auth.json) which is shared
    // with the upstream CLI and may have been rotated externally.
    let entry = load_managed(provider).or_else(|| load_local(provider).ok());
    let entry = entry.ok_or_else(|| missing_credential_error(provider))?;

    // Fast path: cached token still valid and not forced.
    if !force && !is_expired(&entry.expires_at) {
        return Ok(entry.to_credential());
    }

    // Need a refresh. Fail loudly if we have no refresh_token to work with.
    if entry.refresh_token.is_none() {
        if force {
            anyhow::bail!(
                "{} token rejected (401) and no refresh_token is available. \
                 Re-login with the upstream CLI.",
                provider_name(provider)
            );
        }
        anyhow::bail!(
            "{} token expired and no refresh_token is available. \
             Re-login with the upstream CLI.",
            provider_name(provider)
        );
    }

    match try_refresh(&entry, provider).await {
        Some(refreshed) => {
            save_managed(&refreshed, provider);
            Ok(refreshed.to_credential())
        }
        None => anyhow::bail!(
            "{} OAuth refresh failed. Re-login with the upstream CLI.",
            provider_name(provider)
        ),
    }
}

fn missing_credential_error(provider: AuthProvider) -> anyhow::Error {
    match provider {
        AuthProvider::Anthropic => {
            anyhow::anyhow!("No Claude credentials. Log in with Claude Code first.")
        }
        AuthProvider::OpenAI => {
            anyhow::anyhow!("No OpenAI credentials. Log in with the Codex CLI first.")
        }
    }
}

impl ManagedEntry {
    fn to_credential(&self) -> Credential {
        Credential {
            token: self.access_token.clone(),
            is_oauth: self.is_oauth,
            account_id: self.account_id.clone(),
        }
    }
}

fn managed_path() -> PathBuf {
    dirs_home().join(".config").join("luma").join("auth.json")
}

fn dirs_home() -> PathBuf {
    super::home_dir()
}

fn load_managed(provider: AuthProvider) -> Option<ManagedEntry> {
    let data: ManagedStore =
        serde_json::from_str(&fs::read_to_string(managed_path()).ok()?).ok()?;
    let name = provider_name(provider);
    data.credentials.into_iter().find(|c| c.provider == name)
}

fn save_managed(entry: &ManagedEntry, provider: AuthProvider) {
    let path = managed_path();
    let mut store: ManagedStore = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let name = provider_name(provider);
    store.credentials.retain(|c| c.provider != name);
    let mut e = entry.clone();
    e.provider = name.to_owned();
    store.credentials.push(e);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(
        &path,
        serde_json::to_string_pretty(&store).unwrap_or_default(),
    )
    .ok();
}

fn load_local(provider: AuthProvider) -> Result<ManagedEntry> {
    match provider {
        AuthProvider::Anthropic => load_claude_local(),
        AuthProvider::OpenAI => load_codex_local(),
    }
}

fn load_claude_local() -> Result<ManagedEntry> {
    // Try macOS keychain first
    #[cfg(target_os = "macos")]
    if let Some(entry) = load_claude_keychain() {
        return Ok(entry);
    }
    // Fall back to credentials file
    let cred_file = dirs_home().join(".claude").join(".credentials.json");
    let raw = fs::read_to_string(&cred_file)?;
    parse_claude_json(&raw)
        .ok_or_else(|| anyhow::anyhow!("No Claude credentials. Log in with Claude Code first."))
}

#[cfg(target_os = "macos")]
fn load_claude_keychain() -> Option<ManagedEntry> {
    use std::process::Command;
    let services = list_keychain_services();
    for svc in &services {
        let output = Command::new("security")
            .args(["find-generic-password", "-s", svc, "-w"])
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if let Some(entry) = parse_claude_json(&raw) {
            return Some(entry);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn list_keychain_services() -> Vec<String> {
    use std::process::Command;
    let output = Command::new("security").arg("dump-keychain").output().ok();

    let stdout = output
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let mut services: Vec<String> = Vec::new();

    for cap in stdout.split('"') {
        if cap.starts_with("Claude Code-credentials") && !services.contains(&cap.to_owned()) {
            services.push(cap.to_owned());
        }
    }

    if services.is_empty() {
        services.push("Claude Code-credentials".into());
    }
    services
}

fn parse_claude_json(raw: &str) -> Option<ManagedEntry> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let oauth = v.get("claudeAiOauth").unwrap_or(&v);
    let token = oauth.get("accessToken")?.as_str()?;

    Some(ManagedEntry {
        provider: "anthropic".into(),
        access_token: token.to_owned(),
        refresh_token: oauth
            .get("refreshToken")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned()),
        account_id: None,
        is_oauth: true,
        expires_at: oauth.get("expiresAt").map(|v| v.to_string()),
    })
}

fn load_codex_local() -> Result<ManagedEntry> {
    let auth_file = dirs_home().join(".codex").join("auth.json");
    let raw = fs::read_to_string(&auth_file)?;
    let v: serde_json::Value = serde_json::from_str(&raw)?;
    let tokens = v
        .get("tokens")
        .ok_or_else(|| anyhow::anyhow!("No OpenAI credentials"))?;
    let token = tokens
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("No OpenAI access_token"))?;

    // Extract account ID + expiration from the access_token JWT itself.
    // The access_token is what we actually send; its `exp` claim is the
    // authoritative expiration. `last_refresh` in auth.json is only a hint.
    let access_claims = decode_jwt_payload(token);
    let account_id = tokens
        .get("id_token")
        .and_then(|v| v.as_str())
        .and_then(extract_account_id)
        .or_else(|| {
            access_claims
                .as_ref()
                .and_then(extract_account_id_from_claims)
        });
    let expires_at = access_claims
        .as_ref()
        .and_then(|claims| claims.get("exp").and_then(|v| v.as_u64()))
        .map(|secs| secs.to_string());

    Ok(ManagedEntry {
        provider: "openai".into(),
        access_token: token.to_owned(),
        refresh_token: tokens
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned()),
        account_id,
        is_oauth: true,
        expires_at,
    })
}

async fn try_refresh(entry: &ManagedEntry, provider: AuthProvider) -> Option<ManagedEntry> {
    let refresh_token = entry.refresh_token.as_ref()?;
    let client = reqwest::Client::new();

    let (url, body) = match provider {
        AuthProvider::Anthropic => (
            CLAUDE_OAUTH_ENDPOINT,
            serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": CLAUDE_CLIENT_ID,
                "scope": CLAUDE_SCOPES,
            })
            .to_string(),
        ),
        AuthProvider::OpenAI => (
            OPENAI_OAUTH_ENDPOINT,
            format!(
                "grant_type=refresh_token&refresh_token={refresh_token}&client_id={OPENAI_CLIENT_ID}"
            ),
        ),
    };

    let content_type = if provider == AuthProvider::OpenAI {
        "application/x-www-form-urlencoded"
    } else {
        "application/json"
    };

    let res = client
        .post(url)
        .header("Content-Type", content_type)
        .body(body)
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }

    let json: serde_json::Value = res.json().await.ok()?;
    let new_token = json.get("access_token")?.as_str()?;

    Some(ManagedEntry {
        provider: provider_name(provider).to_owned(),
        access_token: new_token.to_owned(),
        refresh_token: json
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned())
            .or_else(|| entry.refresh_token.clone()),
        account_id: entry.account_id.clone(),
        is_oauth: true,
        expires_at: json.get("expires_in").and_then(|v| v.as_u64()).map(|secs| {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                + secs;
            ts.to_string()
        }),
    })
}

fn is_expired(expires_at: &Option<String>) -> bool {
    let Some(exp) = expires_at else {
        return false;
    };
    let Ok(ts) = exp.parse::<u64>() else {
        return false;
    };
    // Normalize: if timestamp > year 2100 in seconds, it's milliseconds
    let ts_secs = if ts > 4_102_444_800 { ts / 1000 } else { ts };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    now >= ts_secs.saturating_sub(300)
}

fn provider_name(p: AuthProvider) -> &'static str {
    match p {
        AuthProvider::Anthropic => "anthropic",
        AuthProvider::OpenAI => "openai",
    }
}

fn extract_account_id(id_token: &str) -> Option<String> {
    let payload = decode_jwt_payload(id_token)?;
    extract_account_id_from_claims(&payload)
}

fn extract_account_id_from_claims(payload: &serde_json::Value) -> Option<String> {
    let auth = payload.get("https://api.openai.com/auth")?;
    auth.get("chatgpt_account_id")
        .or_else(|| auth.get("account_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
}

/// Decode the payload of an unverified JWT. Returns the parsed claims JSON
/// on success. Signature verification is not attempted — we only use the
/// claims as hints (expiration, account id), and the token itself is
/// validated by the remote API on every request.
fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let padded = match parts[1].len() % 4 {
        2 => format!("{}==", parts[1]),
        3 => format!("{}=", parts[1]),
        _ => parts[1].to_owned(),
    };
    let decoded = padded.replace('-', "+").replace('_', "/");
    let bytes = base64_decode(&decoded)?;
    serde_json::from_slice(&bytes).ok()
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let bytes: Vec<u8> = input.bytes().filter(|&b| b != b'=').collect();
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &b) in chunk.iter().enumerate() {
            let val = TABLE.iter().position(|&c| c == b)? as u32;
            n |= val << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct an unsigned JWT whose payload is the given JSON. Signature
    /// segment is a dummy — our decoder ignores it.
    fn make_jwt(payload: &serde_json::Value) -> String {
        fn b64url(bytes: &[u8]) -> String {
            use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
            URL_SAFE_NO_PAD.encode(bytes)
        }
        let header = b64url(br#"{"alg":"none","typ":"JWT"}"#);
        let body = b64url(serde_json::to_string(payload).unwrap().as_bytes());
        let sig = b64url(b"dummy");
        format!("{header}.{body}.{sig}")
    }

    #[test]
    fn decode_jwt_payload_extracts_claims() {
        let jwt = make_jwt(&serde_json::json!({
            "sub": "user-123",
            "exp": 1_700_000_000u64,
        }));
        let claims = decode_jwt_payload(&jwt).expect("payload decodes");
        assert_eq!(claims["sub"], "user-123");
        assert_eq!(claims["exp"], 1_700_000_000u64);
    }

    #[test]
    fn decode_jwt_payload_rejects_non_jwt() {
        assert!(decode_jwt_payload("not.a.jwt").is_none());
        assert!(decode_jwt_payload("single-segment").is_none());
    }

    #[test]
    fn extract_account_id_from_id_token() {
        let jwt = make_jwt(&serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acc-abc"
            }
        }));
        assert_eq!(extract_account_id(&jwt), Some("acc-abc".to_owned()));
    }

    #[test]
    fn is_expired_none_is_not_expired() {
        assert!(!is_expired(&None));
    }

    #[test]
    fn is_expired_past_timestamp() {
        assert!(is_expired(&Some("1".to_owned())));
    }

    #[test]
    fn is_expired_future_timestamp() {
        // Year 2099 in seconds.
        assert!(!is_expired(&Some("4_070_908_800".replace('_', ""))));
    }

    #[test]
    fn is_expired_normalizes_milliseconds() {
        // Past ms timestamp (2001-01-01).
        assert!(is_expired(&Some("978307200000".to_owned())));
    }

    #[test]
    fn is_expired_grace_window() {
        // An expiry that's 10 seconds in the future should still count as
        // "expired" because the grace window is 300s.
        let soon = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 10;
        assert!(is_expired(&Some(soon.to_string())));
    }
}
