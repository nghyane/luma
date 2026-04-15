//! Kiro OAuth provider — supports social (google/github) and AWS IAM Identity Center login.

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
const CALLBACK_PORT: u16 = 3128;
const CALLBACK_PATH: &str = "/oauth/callback";
const CALLBACK_PATHS: &[&str] = &["/signin/callback", "/oauth/callback"];
const BUILDER_ID_START_URL: &str = "https://view.awsapps.com/start";

const IDC_SCOPES: &[&str] = &[
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "sso:account:access",
];

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
        match opt {
            "awsidc" => {
                let issuer_url = callback.issuer_url.ok_or_else(|| {
                    OAuthError::ExchangeFailed("awsidc callback missing issuer_url".into())
                })?;
                let idc_region = callback.idc_region.ok_or_else(|| {
                    OAuthError::ExchangeFailed("awsidc callback missing idc_region".into())
                })?;
                login_device_flow(&issuer_url, &idc_region, IDC_SCOPES).await
            }
            "builderId" => {
                login_device_flow(BUILDER_ID_START_URL, "us-east-1", &["sso:account:access"]).await
            }
            _ => login_social(opt, callback.code, &verifier).await,
        }
    }

    pub async fn refresh(
        &self,
        refresh_token: &str,
        scopes: Option<&[String]>,
    ) -> Result<OAuthTokens, OAuthError> {
        let is_idc = scopes
            .map(|s| s.iter().any(|s| s.starts_with("codewhisperer:") || s == "sso:account:access"))
            .unwrap_or(false);
        if is_idc {
            return refresh_idc(refresh_token).await;
        }
        let json = exchange_json_token(
            "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken",
            serde_json::json!({ "refreshToken": refresh_token }).to_string(),
            &[],
        )
        .await
        .map_err(|e| OAuthError::RefreshRejected(e.to_string()))?;
        parse_tokens(&json)
    }

    /// Direct device-flow login, bypassing the Kiro portal.
    pub async fn login_device(
        start_url: &str,
        region: &str,
    ) -> Result<LoginResult, OAuthError> {
        login_device_flow(start_url, region, IDC_SCOPES).await
    }
}

// =============================================================================
// Social login (google, github)
// =============================================================================

async fn login_social(
    login_option: &str,
    code: Option<String>,
    verifier: &str,
) -> Result<LoginResult, OAuthError> {
    let code = code.ok_or_else(|| {
        OAuthError::ExchangeFailed("social callback missing authorization code".into())
    })?;
    let fixed_redirect =
        format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}?login_option={login_option}");
    let body = serde_json::json!({
        "code": code,
        "code_verifier": verifier,
        "redirect_uri": fixed_redirect,
    })
    .to_string();
    let json = exchange_json_token(TOKEN_URL, body, &[("User-Agent", "Kiro-CLI")])
        .await
        .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;
    let tokens = parse_tokens(&json)?;
    let identity = resolve_identity(&tokens, Some(login_option.to_owned())).await?;
    Ok(LoginResult { identity, tokens })
}

// =============================================================================
// Device authorization flow (IAM Identity Center / Builder ID)
// =============================================================================

async fn login_device_flow(
    start_url: &str,
    region: &str,
    scopes: &[&str],
) -> Result<LoginResult, OAuthError> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;
    let oidc_base = format!("https://oidc.{region}.amazonaws.com");

    // RegisterClient
    let reg: serde_json::Value = post_json(
        &http,
        &format!("{oidc_base}/client/register"),
        &serde_json::json!({
            "clientName": "Kiro CLI",
            "clientType": "public",
            "scopes": scopes,
            "issuerUrl": start_url,
        }),
        "OIDC register",
    )
    .await?;
    let client_id = json_str(&reg, "clientId", "register")?;
    let client_secret = json_str(&reg, "clientSecret", "register")?;

    // StartDeviceAuthorization
    let dev: serde_json::Value = post_json(
        &http,
        &format!("{oidc_base}/device_authorization"),
        &serde_json::json!({
            "clientId": client_id,
            "clientSecret": client_secret,
            "startUrl": start_url,
        }),
        "device authorization",
    )
    .await?;
    let device_code = json_str(&dev, "deviceCode", "device auth")?;
    let verification_url = dev["verificationUriComplete"]
        .as_str()
        .or_else(|| dev["verificationUri"].as_str())
        .ok_or_else(|| OAuthError::ExchangeFailed("missing verification URI".into()))?;
    let interval = dev["interval"].as_u64().unwrap_or(1).max(1);
    let expires_in = dev["expiresIn"].as_u64().unwrap_or(600);

    eprintln!("\nAuthenticating with IAM Identity Center...");
    eprintln!("Open this URL to authorize:\n  {verification_url}\n");
    let _ = open_browser(verification_url);

    // Poll CreateToken
    let deadline = now_unix().saturating_add(expires_in);
    let token_url = format!("{oidc_base}/token");
    let token_body = serde_json::json!({
        "clientId": client_id,
        "clientSecret": client_secret,
        "grantType": "urn:ietf:params:oauth:grant-type:device_code",
        "deviceCode": device_code,
    });
    let token_json = loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        if now_unix() >= deadline {
            return Err(OAuthError::Timeout);
        }
        let resp = http
            .post(&token_url)
            .json(&token_body)
            .send()
            .await
            .map_err(|e| OAuthError::ExchangeFailed(format!("token poll: {e}")))?;
        if resp.status().is_success() {
            break resp
                .json::<serde_json::Value>()
                .await
                .map_err(|e| OAuthError::ExchangeFailed(format!("token parse: {e}")))?;
        }
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        match body["error"].as_str() {
            Some("authorization_pending" | "slow_down") => continue,
            Some(err) => {
                let desc = body["error_description"].as_str().unwrap_or("");
                return Err(OAuthError::ExchangeFailed(format!("{err}: {desc}")));
            }
            None => continue,
        }
    };

    let access_token = json_str(&token_json, "accessToken", "token")?;
    let expires_in = token_json["expiresIn"].as_u64().unwrap_or(3600);
    let refresh_token = token_json["refreshToken"].as_str().map(str::to_owned);
    let profile_arn = fetch_first_profile(&http, access_token).await;

    let tokens = OAuthTokens {
        access_token: access_token.to_owned(),
        refresh_token,
        id_token: None,
        expires_at: Some(now_unix().saturating_add(expires_in)),
        scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        profile_arn,
    };
    let identity = resolve_identity(&tokens, Some("awsidc".to_owned())).await?;
    Ok(LoginResult { identity, tokens })
}

// =============================================================================
// IDC token refresh (SSO OIDC CreateToken with refresh_token grant)
// =============================================================================

async fn refresh_idc(refresh_token: &str) -> Result<OAuthTokens, OAuthError> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| OAuthError::RefreshRejected(e.to_string()))?;

    // Re-register client (SSO OIDC requires clientId+clientSecret for refresh)
    let reg: serde_json::Value = post_json(
        &http,
        "https://oidc.us-east-1.amazonaws.com/client/register",
        &serde_json::json!({
            "clientName": "Kiro CLI",
            "clientType": "public",
            "scopes": IDC_SCOPES,
        }),
        "OIDC re-register",
    )
    .await
    .map_err(|e| OAuthError::RefreshRejected(e.to_string()))?;
    let client_id = json_str(&reg, "clientId", "register")
        .map_err(|e| OAuthError::RefreshRejected(e.to_string()))?;
    let client_secret = json_str(&reg, "clientSecret", "register")
        .map_err(|e| OAuthError::RefreshRejected(e.to_string()))?;

    let token_json: serde_json::Value = post_json(
        &http,
        "https://oidc.us-east-1.amazonaws.com/token",
        &serde_json::json!({
            "clientId": client_id,
            "clientSecret": client_secret,
            "grantType": "refresh_token",
            "refreshToken": refresh_token,
        }),
        "OIDC refresh",
    )
    .await
    .map_err(|e| OAuthError::RefreshRejected(e.to_string()))?;

    let access_token = token_json["accessToken"]
        .as_str()
        .ok_or_else(|| OAuthError::RefreshRejected("missing accessToken".into()))?
        .to_owned();
    let new_refresh = token_json["refreshToken"].as_str().map(str::to_owned);
    let expires_in = token_json["expiresIn"].as_u64().unwrap_or(3600);

    Ok(OAuthTokens {
        access_token,
        refresh_token: new_refresh.or_else(|| Some(refresh_token.to_owned())),
        id_token: None,
        expires_at: Some(now_unix().saturating_add(expires_in)),
        scopes: IDC_SCOPES.iter().map(|s| (*s).to_owned()).collect(),
        profile_arn: None,
    })
}

// =============================================================================
// Helpers
// =============================================================================

async fn fetch_first_profile(http: &reqwest::Client, access_token: &str) -> Option<String> {
    let resp = http
        .post("https://codewhisperer.us-east-1.amazonaws.com/")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("X-Amz-Target", "AmazonCodeWhispererService.ListAvailableProfiles")
        .body("{}")
        .send()
        .await
        .ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;
    json.get("profiles")?
        .as_array()?
        .first()?
        .get("arn")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
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
    let via = login_option.as_deref().unwrap_or("provider");
    let key = if let Some(ref email) = email {
        AccountKey::email(AuthVendor::Kiro, email)
    } else {
        AccountKey::anonymous(AuthVendor::Kiro, crate::util::uuid_v4().unwrap_or_default())
    };
    let display_name = email
        .as_deref()
        .and_then(|e| e.split_once('@').map(|(l, d)| format!("{}@{}", l, d.split('.').next().unwrap_or(d))))
        .unwrap_or_else(|| format!("kiro:{via}"));
    Ok(AccountIdentity {
        key,
        display_name,
        email,
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
    let text = resp.text().await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("userInfo")?.get("email")?.as_str().map(str::to_owned)
}

async fn post_json(
    http: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    step: &str,
) -> Result<serde_json::Value, OAuthError> {
    let resp = http
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|e| OAuthError::ExchangeFailed(format!("{step}: {e}")))?;
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(OAuthError::ExchangeFailed(format!(
            "{step} failed: {}",
            &text[..text.len().min(300)]
        )));
    }
    resp.json()
        .await
        .map_err(|e| OAuthError::ExchangeFailed(format!("{step} parse: {e}")))
}

fn json_str<'a>(v: &'a serde_json::Value, key: &str, step: &str) -> Result<&'a str, OAuthError> {
    v[key]
        .as_str()
        .ok_or_else(|| OAuthError::ExchangeFailed(format!("{step} missing {key}")))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
