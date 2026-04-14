//! Codex/OpenAI OAuth provider.

use crate::auth::domain::{AccountKey, AuthVendor};
use crate::auth::error::OAuthError;
use crate::auth::oauth::{AccountIdentity, LoginResult, OAuthTokens};
use crate::auth::oauth::shared::{
    LOGIN_TIMEOUT_SECS, accept_callback, bind_loopback, decode_jwt_payload, exchange_form_token,
    gen_challenge, gen_state, gen_verifier, open_browser, url_encode,
};

pub struct CodexProvider;

const ISSUER: &str = "https://auth.openai.com";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";

impl CodexProvider {
    pub async fn login(&self) -> Result<LoginResult, OAuthError> {
        let verifier = gen_verifier();
        let challenge = gen_challenge(&verifier);
        let state = gen_state();
        let redirect_uri = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}");
        let authorize_url = format!(
            "{ISSUER}/oauth/authorize?response_type=code&client_id={CLIENT_ID}&redirect_uri={redirect}&scope={scope}&code_challenge={challenge}&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state={state}&originator={originator}",
            redirect = url_encode(&redirect_uri),
            scope = url_encode(SCOPE),
            originator = url_encode(crate::config::auth::CODEX_ORIGINATOR),
        );

        let listener = bind_loopback(CALLBACK_PORT)
            .await
            .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;
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
            return Err(OAuthError::ExchangeFailed("oauth state mismatch".to_owned()));
        }

        let body = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
            url_encode(&callback.code),
            url_encode(&redirect_uri),
            url_encode(CLIENT_ID),
            url_encode(&verifier),
        );
        let json = exchange_form_token(TOKEN_URL, body)
            .await
            .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;
        let tokens = parse_tokens(&json)?;
        let identity = resolve_identity(&tokens)?;
        Ok(LoginResult { identity, tokens })
    }
}

fn parse_tokens(json: &serde_json::Value) -> Result<OAuthTokens, OAuthError> {
    let access_token = json
        .get("access_token")
        .or_else(|| json.get("accessToken"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| OAuthError::ExchangeFailed("token response missing access_token".to_owned()))?
        .to_owned();
    let id_token = json.get("id_token").and_then(|v| v.as_str()).map(str::to_owned);
    let refresh_token = json
        .get("refresh_token")
        .or_else(|| json.get("refreshToken"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let expires_at = decode_jwt_payload(&access_token)
        .as_ref()
        .and_then(|c| c.get("exp")?.as_u64());
    let scopes = json
        .get("scope")
        .and_then(|v| v.as_str())
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    Ok(OAuthTokens {
        access_token,
        refresh_token,
        id_token,
        expires_at,
        scopes,
        profile_arn: None,
    })
}

fn resolve_identity(tokens: &OAuthTokens) -> Result<AccountIdentity, OAuthError> {
    let id_claims = tokens.id_token.as_deref().and_then(decode_jwt_payload);
    let access_claims = decode_jwt_payload(&tokens.access_token);
    let email = id_claims
        .as_ref()
        .and_then(|c| c.get("email")?.as_str().map(str::to_owned))
        .or_else(|| access_claims.as_ref()?.get("email")?.as_str().map(str::to_owned));
    let account_id = id_claims
        .as_ref()
        .and_then(extract_account_id)
        .or_else(|| access_claims.as_ref().and_then(extract_account_id));
    let key = if let Some(id) = account_id.filter(|s| !s.is_empty()) {
        AccountKey::account_id(AuthVendor::OpenAI, id)
    } else if let Some(ref email) = email {
        AccountKey::email(AuthVendor::OpenAI, email)
    } else {
        return Err(OAuthError::IdentityFailed(
            "codex login returned no account_id or email".to_owned(),
        ));
    };
    let display_name = email
        .as_deref()
        .and_then(|e| e.split_once('@').map(|(l, d)| format!("{}@{}", l, d.split('.').next().unwrap_or(d))))
        .ok_or_else(|| OAuthError::IdentityFailed("codex login returned no email for display name".to_owned()))?;
    Ok(AccountIdentity { key, display_name, email })
}

fn extract_account_id(claims: &serde_json::Value) -> Option<String> {
    let auth = claims.get("https://api.openai.com/auth")?;
    auth.get("chatgpt_account_id")
        .or_else(|| auth.get("account_id"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}
