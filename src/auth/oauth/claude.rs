//! Claude/Anthropic OAuth provider.

use crate::auth::domain::{AccountKey, AuthVendor};
use crate::auth::error::OAuthError;
use crate::auth::oauth::shared::{
    LOGIN_TIMEOUT_SECS, accept_callback, bind_loopback, decode_jwt_payload, exchange_json_token,
    form_encode, gen_challenge, gen_state, gen_verifier, open_browser,
};
use crate::auth::oauth::{AccountIdentity, LoginResult, OAuthTokens};

pub struct ClaudeProvider;

const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CALLBACK_PATH: &str = "/callback";
const SCOPES: &[&str] = &[
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

impl ClaudeProvider {
    pub async fn login(&self) -> Result<LoginResult, OAuthError> {
        let verifier = gen_verifier();
        let challenge = gen_challenge(&verifier);
        let state = gen_state();
        let listener = bind_loopback(0)
            .await
            .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?
            .port();
        let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");
        let authorize_url = format!(
            "{AUTHORIZE_URL}?code=true&client_id={CLIENT_ID}&response_type=code&redirect_uri={redirect}&scope={scope}&code_challenge={challenge}&code_challenge_method=S256&state={state}",
            redirect = form_encode(&redirect_uri),
            scope = form_encode(&SCOPES.join(" ")),
        );

        eprintln!("\nOpen this URL to sign in:\n  {authorize_url}\n");
        let _ = open_browser(&authorize_url);
        let callback = tokio::time::timeout(
            std::time::Duration::from_secs(LOGIN_TIMEOUT_SECS),
            accept_callback(listener, CALLBACK_PATH),
        )
        .await
        .map_err(|_| OAuthError::Timeout)
        .and_then(|r| r.map_err(|e| OAuthError::ExchangeFailed(e.to_string())))?;
        if callback.state != state {
            return Err(OAuthError::ExchangeFailed(
                "oauth state mismatch".to_owned(),
            ));
        }

        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "code": callback.code.ok_or(OAuthError::ExchangeFailed("missing code".to_owned()))?,
            "redirect_uri": redirect_uri,
            "client_id": CLIENT_ID,
            "code_verifier": verifier,
            "state": state,
        })
        .to_string();
        let json = exchange_json_token(TOKEN_URL, body, &[])
            .await
            .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;
        let tokens = parse_tokens(&json)?;
        let identity = resolve_identity(&json, &tokens)?;
        Ok(LoginResult { identity, tokens })
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<OAuthTokens, OAuthError> {
        let mut body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
        });
        body["scope"] = serde_json::Value::String(SCOPES.join(" "));
        let json = exchange_json_token(TOKEN_URL, body.to_string(), &[])
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
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .map(|secs| now_unix().saturating_add(secs))
        .or_else(|| {
            decode_jwt_payload(&access_token)
                .as_ref()
                .and_then(|c| c.get("exp")?.as_u64())
        });
    let scopes = json
        .get("scope")
        .and_then(|v| v.as_str())
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    Ok(OAuthTokens {
        access_token,
        refresh_token,
        id_token: None,
        expires_at,
        scopes,
        profile_arn: None,
    })
}

fn resolve_identity(
    json: &serde_json::Value,
    tokens: &OAuthTokens,
) -> Result<AccountIdentity, OAuthError> {
    let account = json.get("account");
    let email = account
        .and_then(|a| a.get("email_address").or_else(|| a.get("email")))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .or_else(|| {
            decode_jwt_payload(&tokens.access_token)
                .and_then(|c| c.get("email")?.as_str().map(str::to_owned))
        });
    let account_id = account
        .and_then(|a| a.get("uuid").or_else(|| a.get("id")))
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let key = if let Some(id) = account_id.filter(|s| !s.is_empty()) {
        AccountKey::account_id(AuthVendor::Anthropic, id)
    } else if let Some(ref email) = email {
        AccountKey::email(AuthVendor::Anthropic, email)
    } else {
        return Err(OAuthError::IdentityFailed(
            "claude login returned no account id or email".to_owned(),
        ));
    };
    let display_name = email
        .as_deref()
        .and_then(|e| {
            e.split_once('@')
                .map(|(l, d)| format!("{}@{}", l, d.split('.').next().unwrap_or(d)))
        })
        .ok_or_else(|| {
            OAuthError::IdentityFailed("claude login returned no email for display name".to_owned())
        })?;
    Ok(AccountIdentity {
        key,
        display_name,
        email,
    })
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
