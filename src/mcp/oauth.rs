use anyhow::{Context, anyhow};

use crate::auth::error::OAuthError;
use crate::auth::oauth::shared::{
    LOGIN_TIMEOUT_SECS, accept_callback, bind_loopback, exchange_form_token, form_encode,
    gen_challenge, gen_state, gen_verifier, open_browser,
};

use super::auth::{
    McpOAuthEntry, SqliteMcpOAuthRepository, clear_tokens, resolve_remote_auth, server_key,
};
use super::config::McpHttpServerEntry;

const CALLBACK_PATH: &str = "/callback";

/// Run an authorization-code + PKCE browser flow for one MCP remote server.
pub async fn authorize(server_name: &str, config: &McpHttpServerEntry) -> Result<(), OAuthError> {
    let auth = resolve_remote_auth(server_name, config)
        .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;

    let authorization_endpoint = auth.authorization_endpoint.clone().ok_or_else(|| {
        OAuthError::ExchangeFailed(
            "missing authorization_endpoint — run discovery first or configure auth metadata"
                .to_owned(),
        )
    })?;
    let token_endpoint = auth.token_endpoint.clone().ok_or_else(|| {
        OAuthError::ExchangeFailed(
            "missing token_endpoint — run discovery first or configure auth metadata".to_owned(),
        )
    })?;
    let client_id = auth.client_id.clone().ok_or_else(|| {
        OAuthError::ExchangeFailed(
            "missing client_id — configure oauth.clientId or store it with `luma mcp set-secret`"
                .to_owned(),
        )
    })?;

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

    let scopes = auth
        .scopes
        .filter(|s| !s.is_empty())
        .map(|s| s.join(" "))
        .unwrap_or_default();

    let authorize_url = if scopes.is_empty() {
        format!(
            "{authorization_endpoint}?response_type=code&client_id={client_id}&redirect_uri={redirect}&code_challenge={challenge}&code_challenge_method=S256&state={state}",
            redirect = form_encode(&redirect_uri),
            client_id = form_encode(&client_id),
        )
    } else {
        format!(
            "{authorization_endpoint}?response_type=code&client_id={client_id}&redirect_uri={redirect}&scope={scope}&code_challenge={challenge}&code_challenge_method=S256&state={state}",
            redirect = form_encode(&redirect_uri),
            client_id = form_encode(&client_id),
            scope = form_encode(&scopes),
        )
    };

    eprintln!("\nOpen this URL to authorize MCP server '{server_name}':\n  {authorize_url}\n");
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
    let code = callback
        .code
        .ok_or_else(|| OAuthError::ExchangeFailed("missing code".to_owned()))?;

    let mut body = format!(
        "grant_type=authorization_code&code={code}&redirect_uri={redirect}&client_id={client_id}&code_verifier={verifier}",
        code = form_encode(&code),
        redirect = form_encode(&redirect_uri),
        client_id = form_encode(&client_id),
        verifier = form_encode(&verifier),
    );
    if let Some(client_secret) = auth.client_secret.as_deref() {
        body.push_str("&client_secret=");
        body.push_str(&form_encode(client_secret));
    }

    let json = exchange_form_token(&token_endpoint, body)
        .await
        .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;
    let access_token = json
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            OAuthError::ExchangeFailed("token response missing access_token".to_owned())
        })?;
    let refresh_token = json
        .get("refresh_token")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let expires_at_unix_ms = json
        .get("expires_in")
        .and_then(serde_json::Value::as_u64)
        .map(|secs| now_unix_ms().saturating_add(secs.saturating_mul(1000)));

    let key = server_key(server_name, config);
    let repo = SqliteMcpOAuthRepository::with_default_path();
    let current = repo
        .get(&key)
        .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;
    let record = McpOAuthEntry {
        server_key: key,
        server_name: server_name.to_owned(),
        server_url: config.url.clone(),
        client_id: Some(client_id),
        client_secret: auth.client_secret,
        access_token: Some(access_token),
        refresh_token: refresh_token
            .or_else(|| current.as_ref().and_then(|x| x.refresh_token.clone())),
        auth_server_metadata_url: auth.auth_server_metadata_url,
        resource_metadata_url: current
            .as_ref()
            .and_then(|x| x.resource_metadata_url.clone()),
        authorization_endpoint: auth.authorization_endpoint,
        revocation_endpoint: auth.revocation_endpoint,
        scopes: if scopes.is_empty() {
            current.as_ref().and_then(|x| x.scopes.clone())
        } else {
            Some(scopes.split_whitespace().map(str::to_owned).collect())
        },
        token_endpoint: Some(token_endpoint),
        expires_at_unix_ms,
    };
    repo.upsert(&record)
        .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;

    Ok(())
}

/// Revoke stored tokens remotely when possible, then clear local tokens.
pub async fn revoke(server_name: &str, config: &McpHttpServerEntry) -> anyhow::Result<()> {
    let auth = resolve_remote_auth(server_name, config)?;
    let key = server_key(server_name, config);
    let repo = SqliteMcpOAuthRepository::with_default_path();
    let Some(current) = repo.get(&key)? else {
        return Ok(());
    };

    if let Some(revocation_endpoint) = auth.revocation_endpoint {
        if let Some(refresh_token) = current.refresh_token.as_deref() {
            let _ = revoke_token(
                &revocation_endpoint,
                refresh_token,
                "refresh_token",
                auth.client_id.as_deref(),
                auth.client_secret.as_deref(),
            )
            .await;
        }
        if let Some(access_token) = current.access_token.as_deref() {
            let _ = revoke_token(
                &revocation_endpoint,
                access_token,
                "access_token",
                auth.client_id.as_deref(),
                auth.client_secret.as_deref(),
            )
            .await;
        }
    }

    clear_tokens(server_name, config)?;
    Ok(())
}

async fn revoke_token(
    endpoint: &str,
    token: &str,
    token_type_hint: &str,
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> anyhow::Result<()> {
    let mut params = vec![
        ("token", token.to_owned()),
        ("token_type_hint", token_type_hint.to_owned()),
    ];
    if let Some(client_id) = client_id {
        params.push(("client_id", client_id.to_owned()));
    }
    if let Some(client_secret) = client_secret {
        params.push(("client_secret", client_secret.to_owned()));
    }

    reqwest::Client::new()
        .post(endpoint)
        .form(&params)
        .send()
        .await
        .with_context(|| format!("failed to call revocation endpoint {endpoint}"))?
        .error_for_status()
        .with_context(|| format!("revocation endpoint rejected request at {endpoint}"))?;
    Ok(())
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Verify the server has enough metadata to start a browser auth flow.
pub async fn ensure_authorizable(
    server_name: &str,
    config: &McpHttpServerEntry,
) -> anyhow::Result<()> {
    let auth = resolve_remote_auth(server_name, config)?;
    if auth.authorization_endpoint.is_none() || auth.token_endpoint.is_none() {
        let discovered = super::auth::discover_from_url_hint(server_name, config).await?;
        if !discovered {
            if auth.authorization_endpoint.is_none() {
                return Err(anyhow!(
                    "missing authorization_endpoint for MCP server '{server_name}'"
                ))
                .with_context(|| {
                    format!("MCP server '{server_name}' is not ready for browser auth")
                });
            }
            if auth.token_endpoint.is_none() {
                return Err(anyhow!(
                    "missing token_endpoint for MCP server '{server_name}'"
                ))
                .with_context(|| {
                    format!("MCP server '{server_name}' is not ready for browser auth")
                });
            }
        }
    }
    let refreshed = resolve_remote_auth(server_name, config)?;
    if refreshed.authorization_endpoint.is_none() {
        return Err(anyhow!(
            "missing authorization_endpoint for MCP server '{server_name}'"
        ))
        .with_context(|| format!("MCP server '{server_name}' is not ready for browser auth"));
    }
    if refreshed.token_endpoint.is_none() {
        return Err(anyhow!(
            "missing token_endpoint for MCP server '{server_name}'"
        ))
        .with_context(|| format!("MCP server '{server_name}' is not ready for browser auth"));
    }
    if refreshed.client_id.is_none() {
        return Err(anyhow!("missing client_id for MCP server '{server_name}'"))
            .with_context(|| format!("MCP server '{server_name}' is not ready for browser auth"));
    }
    Ok(())
}
