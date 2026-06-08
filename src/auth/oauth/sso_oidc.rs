//! SSO OIDC provider — device authorization flow for IAM Identity Center / Builder ID.

use crate::auth::domain::{AccountKey, AuthVendor, SsoOidcClient};
use crate::auth::error::OAuthError;
use crate::auth::oauth::shared::{decode_jwt_payload, open_browser};
use crate::auth::oauth::{AccountIdentity, LoginResult, OAuthTokens};

const IDC_SCOPES: &[&str] = &[
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "sso:account:access",
];

/// Register an OIDC client with SSO OIDC service.
pub async fn register_client(
    http: &reqwest::Client,
    region: &str,
    start_url: &str,
    scopes: &[&str],
) -> Result<SsoOidcClient, OAuthError> {
    let reg: serde_json::Value = post_json(
        http,
        &format!("https://oidc.{region}.amazonaws.com/client/register"),
        &serde_json::json!({
            "clientName": "Kiro CLI",
            "clientType": "public",
            "scopes": scopes,
            "issuerUrl": start_url,
        }),
        "OIDC register",
    )
    .await?;
    Ok(SsoOidcClient {
        client_id: json_str(&reg, "clientId", "register")?.to_owned(),
        client_secret: json_str(&reg, "clientSecret", "register")?.to_owned(),
        expires_at: reg["clientSecretExpiresAt"].as_u64().unwrap_or(0),
    })
}

/// Full device authorization flow: register → start device auth → poll → return tokens.
pub async fn login(
    start_url: &str,
    region: &str,
    cached_client: Option<&SsoOidcClient>,
) -> Result<(LoginResult, SsoOidcClient), OAuthError> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;
    let oidc_base = format!("https://oidc.{region}.amazonaws.com");

    // Use cached client or register new one.
    let client = match cached_client.filter(|c| c.expires_at > now_unix()) {
        Some(c) => c.clone(),
        None => register_client(&http, region, start_url, IDC_SCOPES).await?,
    };

    // StartDeviceAuthorization
    let dev: serde_json::Value = post_json(
        &http,
        &format!("{oidc_base}/device_authorization"),
        &serde_json::json!({
            "clientId": &client.client_id,
            "clientSecret": &client.client_secret,
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

    eprintln!("\nOpen this URL to authorize:\n  {verification_url}\n");
    let _ = open_browser(verification_url);

    // Poll CreateToken
    let deadline = now_unix().saturating_add(expires_in);
    let token_body = serde_json::json!({
        "clientId": &client.client_id,
        "clientSecret": &client.client_secret,
        "grantType": "urn:ietf:params:oauth:grant-type:device_code",
        "deviceCode": device_code,
    });
    let token_json = poll_token(
        &http,
        &format!("{oidc_base}/token"),
        &token_body,
        interval,
        deadline,
    )
    .await?;

    let access_token = json_str(&token_json, "accessToken", "token")?;
    let expires_in = token_json["expiresIn"].as_u64().unwrap_or(3600);
    let refresh_token = token_json["refreshToken"].as_str().map(str::to_owned);
    let profile_arn = fetch_first_profile(&http, access_token).await;

    let tokens = OAuthTokens {
        access_token: access_token.to_owned(),
        refresh_token,
        id_token: None,
        expires_at: Some(now_unix().saturating_add(expires_in)),
        scopes: IDC_SCOPES.iter().map(|s| (*s).to_owned()).collect(),
        profile_arn,
    };
    let identity = resolve_identity(&tokens).await?;
    Ok((LoginResult { identity, tokens }, client))
}

/// Refresh an IDC/BuilderId token using cached client registration.
pub async fn refresh(
    refresh_token: &str,
    region: &str,
    cached_client: Option<&SsoOidcClient>,
    start_url: &str,
) -> Result<(OAuthTokens, SsoOidcClient), OAuthError> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| OAuthError::RefreshRejected(e.to_string()))?;

    let client = match cached_client.filter(|c| c.expires_at > now_unix()) {
        Some(c) => c.clone(),
        None => register_client(&http, region, start_url, IDC_SCOPES)
            .await
            .map_err(|e| OAuthError::RefreshRejected(e.to_string()))?,
    };

    let token_json: serde_json::Value = post_json(
        &http,
        &format!("https://oidc.{region}.amazonaws.com/token"),
        &serde_json::json!({
            "clientId": &client.client_id,
            "clientSecret": &client.client_secret,
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

    Ok((
        OAuthTokens {
            access_token,
            refresh_token: new_refresh.or_else(|| Some(refresh_token.to_owned())),
            id_token: None,
            expires_at: Some(now_unix().saturating_add(expires_in)),
            scopes: IDC_SCOPES.iter().map(|s| (*s).to_owned()).collect(),
            profile_arn: None,
        },
        client,
    ))
}

// =============================================================================
// Helpers
// =============================================================================

async fn poll_token(
    http: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    interval: u64,
    deadline: u64,
) -> Result<serde_json::Value, OAuthError> {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        if now_unix() >= deadline {
            return Err(OAuthError::Timeout);
        }
        let resp = http
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|e| OAuthError::ExchangeFailed(format!("token poll: {e}")))?;
        if resp.status().is_success() {
            return resp
                .json()
                .await
                .map_err(|e| OAuthError::ExchangeFailed(format!("token parse: {e}")));
        }
        let err_body: serde_json::Value = resp.json().await.unwrap_or_default();
        match err_body["error"].as_str() {
            Some("authorization_pending" | "slow_down") => continue,
            Some(err) => {
                let desc = err_body["error_description"].as_str().unwrap_or("");
                return Err(OAuthError::ExchangeFailed(format!("{err}: {desc}")));
            }
            None => continue,
        }
    }
}

async fn fetch_first_profile(http: &reqwest::Client, access_token: &str) -> Option<String> {
    let resp = http
        .post("https://codewhisperer.us-east-1.amazonaws.com/")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/x-amz-json-1.0")
        .header(
            "X-Amz-Target",
            "AmazonCodeWhispererService.ListAvailableProfiles",
        )
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

async fn resolve_identity(tokens: &OAuthTokens) -> Result<AccountIdentity, OAuthError> {
    let email = fetch_email(
        &tokens.access_token,
        tokens.profile_arn.as_deref().unwrap_or(""),
    )
    .await
    .or_else(|| {
        decode_jwt_payload(&tokens.access_token)
            .and_then(|c| c.get("email")?.as_str().map(str::to_owned))
    });
    let key = if let Some(ref email) = email {
        AccountKey::email(AuthVendor::Kiro, email)
    } else {
        AccountKey::anonymous(AuthVendor::Kiro, crate::util::uuid_v4().unwrap_or_default())
    };
    let display_name = email
        .as_deref()
        .and_then(|e| {
            e.split_once('@')
                .map(|(l, d)| format!("{}@{}", l, d.split('.').next().unwrap_or(d)))
        })
        .unwrap_or_else(|| "kiro:idc".to_owned());
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
    let v: serde_json::Value = resp.json().await.ok()?;
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
            crate::util::byte_prefix(&text, 300)
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
