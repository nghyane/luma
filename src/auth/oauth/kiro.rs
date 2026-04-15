//! Kiro OAuth provider.

use crate::auth::domain::{AccountKey, AuthVendor};
use crate::auth::error::OAuthError;
use crate::auth::oauth::{AccountIdentity, LoginResult, OAuthTokens};
use crate::auth::oauth::shared::{
    LOGIN_TIMEOUT_SECS, accept_callback_any, bind_loopback, decode_jwt_payload,
    exchange_json_token, gen_challenge, gen_state, gen_verifier, open_browser, url_encode,
};

pub struct KiroProvider;

const AUTHORIZE_URL: &str = "https://app.kiro.dev/signin";
const TOKEN_URL: &str = "https://prod.us-east-1.auth.desktop.kiro.dev/oauth/token";
const CALLBACK_PORT: u16 = 3128;
const CALLBACK_PATH: &str = "/oauth/callback";
const CALLBACK_PATHS: &[&str] = &["/signin/callback", "/oauth/callback"];

impl KiroProvider {
    pub async fn login(&self) -> Result<LoginResult, OAuthError> {
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
            return Err(OAuthError::ExchangeFailed("oauth state mismatch".to_owned()));
        }

        let opt = callback.login_option.as_deref().unwrap_or("google");
        let fixed_redirect = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}?login_option={opt}");
        let body = serde_json::json!({
            "code": callback.code,
            "code_verifier": verifier,
            "redirect_uri": fixed_redirect,
        })
        .to_string();
        let json = exchange_json_token(TOKEN_URL, body, &[("User-Agent", "Kiro-CLI")])
            .await
            .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;
        let tokens = parse_tokens(&json)?;
        let identity = resolve_identity(&tokens, callback.login_option).await?;
        Ok(LoginResult { identity, tokens })
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<OAuthTokens, OAuthError> {
        let json = exchange_json_token(
            "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken",
            serde_json::json!({ "refreshToken": refresh_token }).to_string(),
            &[],
        )
        .await
        .map_err(|e| OAuthError::RefreshRejected(e.to_string()))?;
        parse_tokens(&json)
    }
}

fn parse_tokens(json: &serde_json::Value) -> Result<OAuthTokens, OAuthError> {
    let access_token = json
        .get("access_token")
        .or_else(|| json.get("accessToken"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| OAuthError::ExchangeFailed("token response missing access_token".to_owned()))?
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
    let profile_arn = json.get("profileArn").and_then(|v| v.as_str()).map(str::to_owned);
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
    login_option: Option<String>,
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
        let via = login_option.unwrap_or_else(|| "provider".to_owned());
        OAuthError::IdentityFailed(format!("kiro login returned no email (via {via})"))
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
    let body =
        serde_json::json!({"profileArn": profile_arn, "isEmailRequired": true}).to_string();
    let resp = client
        .post("https://codewhisperer.us-east-1.amazonaws.com/")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("X-Amz-Target", "AmazonCodeWhispererService.GetUsageLimits")
        .body(body)
        .send()
        .await
        .ok()?;
    let text = resp.text().await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("userInfo")?.get("email")?.as_str().map(str::to_owned)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
