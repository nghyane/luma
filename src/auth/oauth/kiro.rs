//! Kiro social OAuth provider (Google/GitHub via Kiro portal).

use crate::auth::domain::{AccountKey, AuthVendor};
use crate::auth::error::OAuthError;
use crate::auth::oauth::shared::{
    LOGIN_TIMEOUT_SECS, accept_callback_any, bind_loopback, decode_jwt_payload,
    exchange_json_token, gen_challenge, gen_state, gen_verifier, open_browser, url_encode,
};
use crate::auth::oauth::{AccountIdentity, LoginResult, OAuthTokens};

pub struct KiroProvider;

const AUTHORIZE_URL: &str = "https://app.kiro.dev/signin";
const TOKEN_URL: &str = "https://prod.us-east-1.auth.desktop.kiro.dev/oauth/token";
const REFRESH_URL: &str = "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken";
const CALLBACK_PORT: u16 = 3128;
const CALLBACK_PATH: &str = "/oauth/callback";
const CALLBACK_PATHS: &[&str] = &["/signin/callback", "/oauth/callback"];

/// Portal callback result — tells the caller which flow to use.
pub enum PortalOutcome {
    /// Social login completed — tokens ready.
    Social(LoginResult),
    /// User chose IAM Identity Center — caller must run device flow.
    Idc {
        issuer_url: String,
        idc_region: String,
    },
    /// User chose Builder ID — caller must run device flow.
    BuilderId,
}

impl KiroProvider {
    /// Open Kiro portal and handle the callback.
    /// Returns `PortalOutcome` so the caller can dispatch device flow if needed.
    pub async fn login(&self) -> Result<PortalOutcome, OAuthError> {
        let verifier = gen_verifier();
        let challenge = gen_challenge(&verifier);
        let state = gen_state();
        let redirect_uri = format!("http://localhost:{CALLBACK_PORT}");
        let authorize_url = format!(
            "{AUTHORIZE_URL}?state={state}&code_challenge={challenge}&code_challenge_method=S256&redirect_uri={redirect}&redirect_from=kirocli",
            redirect = url_encode(&redirect_uri),
        );

        let listener = bind_loopback(CALLBACK_PORT)
            .await
            .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;
        eprintln!("\nOpen this URL to sign in:\n  {authorize_url}\n");
        let _ = open_browser(&authorize_url);
        let callback = tokio::time::timeout(
            std::time::Duration::from_secs(LOGIN_TIMEOUT_SECS),
            accept_callback_any(listener, CALLBACK_PATHS),
        )
        .await
        .map_err(|_| OAuthError::Timeout)
        .and_then(|r| r.map_err(|e| OAuthError::ExchangeFailed(e.to_string())))?;
        if callback.state != state {
            return Err(OAuthError::ExchangeFailed(
                "oauth state mismatch".to_owned(),
            ));
        }

        match callback.login_option.as_deref().unwrap_or("google") {
            "awsidc" => {
                let issuer_url = callback.issuer_url.ok_or_else(|| {
                    OAuthError::ExchangeFailed("awsidc callback missing issuer_url".into())
                })?;
                let idc_region = callback.idc_region.ok_or_else(|| {
                    OAuthError::ExchangeFailed("awsidc callback missing idc_region".into())
                })?;
                Ok(PortalOutcome::Idc {
                    issuer_url,
                    idc_region,
                })
            }
            "builderId" => Ok(PortalOutcome::BuilderId),
            opt => {
                let result = exchange_social(opt, callback.code, &verifier).await?;
                Ok(PortalOutcome::Social(result))
            }
        }
    }

    /// Refresh a social (Google/GitHub) token.
    pub async fn refresh_social(&self, refresh_token: &str) -> Result<OAuthTokens, OAuthError> {
        let json = exchange_json_token(
            REFRESH_URL,
            serde_json::json!({ "refreshToken": refresh_token }).to_string(),
            &[],
        )
        .await
        .map_err(|e| OAuthError::RefreshRejected(e.to_string()))?;
        parse_tokens(&json)
    }
}

async fn exchange_social(
    login_option: &str,
    code: Option<String>,
    verifier: &str,
) -> Result<LoginResult, OAuthError> {
    let code = code.ok_or_else(|| {
        OAuthError::ExchangeFailed("social callback missing authorization code".into())
    })?;
    let redirect =
        format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}?login_option={login_option}");
    let body = serde_json::json!({
        "code": code,
        "code_verifier": verifier,
        "redirect_uri": redirect,
    })
    .to_string();
    let json = exchange_json_token(TOKEN_URL, body, &[("User-Agent", "Kiro-CLI")])
        .await
        .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;
    let tokens = parse_tokens(&json)?;
    let identity = resolve_identity(&tokens, login_option).await?;
    Ok(LoginResult { identity, tokens })
}

fn parse_tokens(json: &serde_json::Value) -> Result<OAuthTokens, OAuthError> {
    let access_token = json
        .get("access_token")
        .or_else(|| json.get("accessToken"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            OAuthError::ExchangeFailed("token response missing access_token".to_owned())
        })?
        .to_owned();
    let refresh_token = json
        .get("refresh_token")
        .or_else(|| json.get("refreshToken"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let expires_at = json
        .get("expiresIn")
        .and_then(|v| v.as_u64())
        .map(|secs| now_unix().saturating_add(secs));
    let profile_arn = json
        .get("profileArn")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    Ok(OAuthTokens {
        access_token,
        refresh_token,
        id_token: None,
        expires_at,
        scopes: vec![],
        profile_arn,
    })
}

async fn resolve_identity(
    tokens: &OAuthTokens,
    login_option: &str,
) -> Result<AccountIdentity, OAuthError> {
    let email = fetch_email(
        &tokens.access_token,
        tokens.profile_arn.as_deref().unwrap_or(""),
    )
    .await
    .or_else(|| {
        decode_jwt_payload(&tokens.access_token)
            .and_then(|c| c.get("email")?.as_str().map(str::to_owned))
    });
    let email = email.ok_or_else(|| {
        OAuthError::IdentityFailed(format!("kiro login returned no email (via {login_option})"))
    })?;
    let display_name = email
        .split_once('@')
        .map(|(l, d)| format!("{}@{}", l, d.split('.').next().unwrap_or(d)))
        .unwrap_or_else(|| email.clone());
    Ok(AccountIdentity {
        key: AccountKey::email(AuthVendor::Kiro, &email),
        display_name,
        email: Some(email),
    })
}

async fn fetch_email(access_token: &str, profile_arn: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let body = serde_json::json!({"profileArn": profile_arn, "isEmailRequired": true}).to_string();
    let resp = client
        .post("https://codewhisperer.us-east-1.amazonaws.com/")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("X-Amz-Target", "AmazonCodeWhispererService.GetUsageLimits")
        .body(body)
        .send()
        .await
        .ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    v.get("userInfo")?.get("email")?.as_str().map(str::to_owned)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
