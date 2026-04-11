//! PKCE OAuth login flow — Claude (Anthropic) and Codex (OpenAI).
//!
//! Both providers use Authorization Code + PKCE. The flow is identical;
//! only the endpoints, scopes, redirect path, and token parsing differ.
//! A `ProviderFlow` struct captures those per-provider constants so the
//! shared listener/exchange logic doesn't branch on provider type.

use super::{
    AccountEntry, AuthProvider, CLAUDE_CLIENT_ID, CLAUDE_OAUTH_ENDPOINT, CLAUDE_SCOPES,
    CODEX_ORIGINATOR, OPENAI_CLIENT_ID, OPENAI_OAUTH_ENDPOINT, UsageRec, decode_jwt_payload,
    derive_label, extract_email_from_jwt, now_unix, upsert_by_label, with_pool_mut,
};
use anyhow::{Context as _, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// --- Claude constants ---

/// Routes through claude.com/cai/* for attribution; 307s to
/// claude.ai/oauth/authorize in two hops (same as upstream Claude Code).
const CLAUDE_AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";

// --- Codex constants ---

const CODEX_ISSUER: &str = "https://auth.openai.com";
const CODEX_SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";

/// How long we wait for the browser redirect before giving up.
const LOGIN_TIMEOUT_SECS: u64 = 300;

// =============================================================================
// ProviderFlow — per-provider config carried through the shared flow
// =============================================================================

struct ProviderFlow {
    provider: AuthProvider,
    authorize_url: String,
    token_url: &'static str,
    client_id: &'static str,
    /// Path the provider will redirect to on the loopback server.
    callback_path: &'static str,
    /// Body format for token exchange.
    exchange_format: ExchangeFormat,
}

enum ExchangeFormat {
    /// JSON body (Claude).
    Json { state: String },
    /// Form-encoded body (Codex).
    Form,
}

impl ProviderFlow {
    fn claude(challenge: &str, state: &str, redirect_uri: &str) -> Self {
        use std::fmt::Write as _;
        let scope = CLAUDE_SCOPES.join(" ");
        let mut url = format!(
            "{CLAUDE_AUTHORIZE_URL}?response_type=code&client_id={CLAUDE_CLIENT_ID}\
             &redirect_uri={redirect}\
             &code_challenge={challenge}&code_challenge_method=S256\
             &state={state}",
            redirect = url_encode(redirect_uri),
        );
        let _ = write!(url, "&scope={}", url_encode(&scope));
        // Tells the login page to surface the Max upsell and keeps the grant
        // in the "Claude AI" lane (same as upstream Claude Code).
        url.push_str("&code=true");

        Self {
            provider: AuthProvider::Anthropic,
            authorize_url: url,
            token_url: CLAUDE_OAUTH_ENDPOINT,
            client_id: CLAUDE_CLIENT_ID,
            callback_path: "/callback",
            exchange_format: ExchangeFormat::Json {
                state: state.to_owned(),
            },
        }
    }

    fn codex(challenge: &str, state: &str, redirect_uri: &str) -> Self {
        let url = format!(
            "{CODEX_ISSUER}/oauth/authorize\
             ?response_type=code\
             &client_id={OPENAI_CLIENT_ID}\
             &redirect_uri={redirect}\
             &scope={scope}\
             &code_challenge={challenge}\
             &code_challenge_method=S256\
             &id_token_add_organizations=true\
             &codex_cli_simplified_flow=true\
             &state={state}\
             &originator={originator}",
            redirect = url_encode(redirect_uri),
            scope = url_encode(CODEX_SCOPE),
            originator = url_encode(CODEX_ORIGINATOR),
        );

        Self {
            provider: AuthProvider::OpenAI,
            authorize_url: url,
            token_url: OPENAI_OAUTH_ENDPOINT,
            client_id: OPENAI_CLIENT_ID,
            callback_path: "/auth/callback",
            exchange_format: ExchangeFormat::Form,
        }
    }
}

// =============================================================================
// public API
// =============================================================================

/// Outcome of a successful PKCE login.
#[derive(Debug, Clone)]
pub struct LoginOutcome {
    pub label: String,
    pub email: Option<String>,
    pub provider: AuthProvider,
}

/// CLI convenience wrapper — prints the authorize URL to stderr.
pub async fn login(provider: AuthProvider) -> Result<LoginOutcome> {
    login_with_reporter(provider, |url| {
        eprintln!("\nOpen this URL to sign in:\n  {url}\n");
    })
    .await
}

/// Run an end-to-end PKCE login with a custom URL reporter. The reporter is
/// called exactly once with the authorize URL. Use this from the TUI so the
/// URL surfaces via the event bus instead of a stray `eprintln!`.
pub async fn login_with_reporter<F>(provider: AuthProvider, on_url: F) -> Result<LoginOutcome>
where
    F: FnOnce(&str),
{
    let verifier = gen_verifier();
    let challenge = gen_challenge(&verifier);
    let state = gen_state();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("could not bind loopback listener")?;
    let port = listener
        .local_addr()
        .context("could not read listener port")?
        .port();

    let flow = match provider {
        AuthProvider::Anthropic => {
            let redirect = format!("http://127.0.0.1:{port}/callback");
            ProviderFlow::claude(&challenge, &state, &redirect)
        }
        AuthProvider::OpenAI => {
            let redirect = format!("http://127.0.0.1:{port}/auth/callback");
            ProviderFlow::codex(&challenge, &state, &redirect)
        }
    };

    on_url(&flow.authorize_url);
    let _ = open_browser(&flow.authorize_url);

    let callback_path = flow.callback_path;
    let (code, returned_state) = tokio::time::timeout(
        std::time::Duration::from_secs(LOGIN_TIMEOUT_SECS),
        accept_callback(listener, callback_path),
    )
    .await
    .map_err(|_| anyhow::anyhow!("login timed out after 5 minutes"))??;

    if returned_state != state {
        bail!("oauth state mismatch — possible CSRF attempt, aborting");
    }

    let tokens = exchange_code(&flow, &code, &verifier).await?;
    let entry = build_account_entry(provider, tokens);
    let outcome = LoginOutcome {
        label: entry.label.clone(),
        email: entry.email.clone(),
        provider,
    };
    with_pool_mut(|p| upsert_by_label(p, entry));
    Ok(outcome)
}

// =============================================================================
// PKCE crypto
// =============================================================================

fn gen_verifier() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("system entropy source unavailable");
    URL_SAFE_NO_PAD.encode(bytes)
}

fn gen_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn gen_state() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("system entropy source unavailable");
    URL_SAFE_NO_PAD.encode(bytes)
}

// =============================================================================
// browser opener
// =============================================================================

#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .context("failed to open browser")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .context("failed to open browser")?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .context("failed to open browser")?;
    Ok(())
}

// =============================================================================
// loopback callback listener
// =============================================================================

async fn accept_callback(
    listener: tokio::net::TcpListener,
    path: &str,
) -> Result<(String, String)> {
    loop {
        let (mut stream, _addr) = listener
            .accept()
            .await
            .context("callback listener accept failed")?;
        match read_request(&mut stream, path).await {
            Ok(Some((code, state))) => {
                let _ = stream.write_all(SUCCESS_RESPONSE.as_bytes()).await;
                let _ = stream.shutdown().await;
                return Ok((code, state));
            }
            _ => {
                let _ = stream.write_all(NOT_FOUND_RESPONSE.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        }
    }
}

/// Parse `code` and `state` from an incoming GET request on `expected_path`.
async fn read_request(
    stream: &mut tokio::net::TcpStream,
    expected_path: &str,
) -> Result<Option<(String, String)>> {
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let Some(first_line) = req.lines().next() else {
        return Ok(None);
    };
    // "GET /callback?code=...&state=... HTTP/1.1"
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    if method != "GET" {
        return Ok(None);
    }
    // Accept only the expected callback path.
    let Some((path_part, query)) = target.split_once('?') else {
        return Ok(None);
    };
    if path_part != expected_path {
        return Ok(None);
    }

    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        match k {
            "code" => code = Some(url_decode(v)),
            "state" => state = Some(url_decode(v)),
            _ => {}
        }
    }
    match (code, state) {
        (Some(c), Some(s)) => Ok(Some((c, s))),
        _ => Ok(None),
    }
}

const SUCCESS_RESPONSE: &str = concat!(
    "HTTP/1.1 200 OK\r\n",
    "Content-Type: text/html; charset=utf-8\r\n",
    "Connection: close\r\n",
    "\r\n",
    "<!doctype html><html><head><meta charset=\"utf-8\"><title>luma · signed in</title>",
    "<style>body{font-family:-apple-system,system-ui,sans-serif;display:flex;min-height:100vh;",
    "margin:0;align-items:center;justify-content:center;background:#0a0a0a;color:#e5e5e5}",
    "div{text-align:center;max-width:32rem;padding:2rem}h1{font-size:1.5rem;font-weight:500}",
    "p{color:#888;font-size:0.9rem}</style></head><body><div><h1>Signed in</h1>",
    "<p>You can close this tab and return to luma.</p></div></body></html>",
);

const NOT_FOUND_RESPONSE: &str = "HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n";

// =============================================================================
// URL codec
// =============================================================================

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Cannot collapse: inner `if let` uses `continue` to skip fallthrough.
        #[allow(clippy::collapsible_if)]
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// =============================================================================
// token exchange
// =============================================================================

struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<u64>,
    scopes: Option<Vec<String>>,
    /// Claude: `account` object with email_address + uuid.
    /// Codex: parsed from id_token JWT claims.
    email: Option<String>,
    account_id: Option<String>,
}

async fn exchange_code(flow: &ProviderFlow, code: &str, verifier: &str) -> Result<TokenResponse> {
    let client = reqwest::Client::new();

    let (body, content_type) = match &flow.exchange_format {
        ExchangeFormat::Json { state } => (
            serde_json::json!({
                "grant_type": "authorization_code",
                "code": code,
                "redirect_uri": extract_redirect(flow),
                "client_id": flow.client_id,
                "code_verifier": verifier,
                "state": state,
            })
            .to_string(),
            "application/json",
        ),
        ExchangeFormat::Form => (
            format!(
                "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
                url_encode(code),
                url_encode(&extract_redirect(flow)),
                url_encode(flow.client_id),
                url_encode(verifier),
            ),
            "application/x-www-form-urlencoded",
        ),
    };

    let res = client
        .post(flow.token_url)
        .header("Content-Type", content_type)
        .header("Accept", "application/json")
        .body(body)
        .send()
        .await
        .context("token exchange network error")?;

    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "token exchange HTTP {status}: {}",
            &text[..text.len().min(300)]
        );
    }

    let json: serde_json::Value = serde_json::from_str(&text).context("bad token exchange json")?;

    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("token response missing access_token"))?
        .to_owned();
    let refresh_token = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());

    let (email, account_id, expires_at) = match flow.provider {
        AuthProvider::Anthropic => {
            let account = json.get("account");
            let email = account
                .and_then(|a| a.get("email_address").or_else(|| a.get("email")))
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned())
                .or_else(|| extract_email_from_jwt(&access_token));
            let account_id = account
                .and_then(|a| a.get("uuid").or_else(|| a.get("id")))
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned());
            let expires_at = json
                .get("expires_in")
                .and_then(|v| v.as_u64())
                .map(|secs| now_unix().saturating_add(secs))
                .or_else(|| {
                    decode_jwt_payload(&access_token)
                        .as_ref()
                        .and_then(|c| c.get("exp").and_then(|v| v.as_u64()))
                });
            (email, account_id, expires_at)
        }
        AuthProvider::OpenAI => {
            // Codex: identity lives in id_token JWT, expiry in access_token JWT.
            let id_token = json
                .get("id_token")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let id_claims = decode_jwt_payload(id_token);
            let email = id_claims.as_ref().and_then(|c| {
                c.get("email")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_owned())
            });
            let account_id = id_claims.as_ref().and_then(|c| {
                c.get("https://api.openai.com/auth")
                    .and_then(|auth| {
                        auth.get("chatgpt_account_id")
                            .or_else(|| auth.get("account_id"))
                    })
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_owned())
            });
            let expires_at = decode_jwt_payload(&access_token)
                .as_ref()
                .and_then(|c| c.get("exp").and_then(|v| v.as_u64()));
            (email, account_id, expires_at)
        }
    };

    let scopes = json
        .get("scope")
        .and_then(|v| v.as_str())
        .map(|s| s.split_whitespace().map(|w| w.to_owned()).collect());

    Ok(TokenResponse {
        access_token,
        refresh_token,
        expires_at,
        scopes,
        email,
        account_id,
    })
}

/// Extract the redirect_uri that was used when building the authorize URL.
/// We reconstruct it from the flow so it matches exactly what was registered.
fn extract_redirect(flow: &ProviderFlow) -> String {
    // The redirect_uri is embedded in the authorize URL query string.
    flow.authorize_url
        .split("redirect_uri=")
        .nth(1)
        .and_then(|s| s.split('&').next())
        .map(url_decode)
        .unwrap_or_default()
}

fn build_account_entry(provider: AuthProvider, tokens: TokenResponse) -> AccountEntry {
    let label = derive_label(provider, tokens.email.as_deref());
    AccountEntry {
        label,
        provider: provider.as_str().to_owned(),
        email: tokens.email,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        account_id: tokens.account_id,
        is_oauth: true,
        expires_at: tokens.expires_at,
        scopes: tokens.scopes,
        cooldown_until: None,
        usage: UsageRec::default(),
        needs_relogin: false,
        disabled: false,
    }
}

// =============================================================================
// tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_and_challenge_are_deterministic() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = gen_challenge(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn verifier_is_sufficiently_random() {
        let a = gen_verifier();
        let b = gen_verifier();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert_eq!(b.len(), 43);
    }

    #[test]
    fn state_is_random_and_urlsafe() {
        let s = gen_state();
        assert_eq!(s.len(), 22);
        assert!(
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn url_encode_preserves_unreserved() {
        assert_eq!(url_encode("abc-_.~123"), "abc-_.~123");
    }

    #[test]
    fn url_encode_escapes_reserved() {
        assert_eq!(url_encode("a b&c"), "a%20b%26c");
    }

    #[test]
    fn url_decode_reverses_encode() {
        let original = "http://127.0.0.1:45678/callback";
        assert_eq!(url_decode(&url_encode(original)), original);
    }

    #[test]
    fn url_decode_handles_plus_as_space() {
        assert_eq!(url_decode("hello+world"), "hello world");
    }

    #[test]
    fn claude_authorize_url_correct() {
        let flow = ProviderFlow::claude("CHALLENGE", "STATE", "http://127.0.0.1:1/callback");
        let url = &flow.authorize_url;
        assert!(url.starts_with(CLAUDE_AUTHORIZE_URL));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge=CHALLENGE"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=STATE"));
        assert!(url.contains("code=true"));
        assert!(url.contains("scope="));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1%2Fcallback"));
    }

    #[test]
    fn codex_authorize_url_correct() {
        let flow = ProviderFlow::codex("CHALLENGE", "STATE", "http://127.0.0.1:1/auth/callback");
        let url = &flow.authorize_url;
        assert!(url.contains("auth.openai.com/oauth/authorize"));
        assert!(url.contains("id_token_add_organizations=true"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
        assert!(url.contains("code_challenge=CHALLENGE"));
        assert!(url.contains("state=STATE"));
        assert!(url.contains("originator=codex_cli_rs"));
        assert_eq!(flow.callback_path, "/auth/callback");
        // redirect must be encoded
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1%2Fauth%2Fcallback"));
    }

    #[test]
    fn claude_scopes_do_not_include_org_create_api_key() {
        let flow = ProviderFlow::claude("C", "S", "http://127.0.0.1:1/callback");
        assert!(!flow.authorize_url.contains("org%3Acreate_api_key"));
    }

    #[test]
    fn extract_redirect_roundtrips() {
        let redirect = "http://127.0.0.1:54321/callback";
        let flow = ProviderFlow::claude("C", "S", redirect);
        assert_eq!(extract_redirect(&flow), redirect);
    }

    #[test]
    fn extract_redirect_codex_roundtrips() {
        let redirect = "http://127.0.0.1:54321/auth/callback";
        let flow = ProviderFlow::codex("C", "S", redirect);
        assert_eq!(extract_redirect(&flow), redirect);
    }
}
