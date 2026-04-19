//! MCP OAuth/secret persistence.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use anyhow::Context;

use super::config::McpHttpServerEntry;

/// Authorization-server metadata subset needed by Luma.
#[derive(Debug, Clone, Deserialize)]
struct AuthorizationServerMetadata {
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
    registration_endpoint: Option<String>,
    scopes_supported: Option<Vec<String>>,
    #[serde(default)]
    client_id_metadata_document_supported: bool,
}

#[derive(Debug, Clone, Serialize)]
struct ClientRegistrationRequest {
    client_name: String,
    redirect_uris: Vec<String>,
    grant_types: Vec<String>,
    token_endpoint_auth_method: String,
    response_types: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ClientRegistrationResponse {
    client_id: String,
    client_secret: Option<String>,
}

/// Resource-server metadata subset used for Protected Resource Metadata discovery.
#[derive(Debug, Clone, Deserialize)]
struct ResourceServerMetadata {
    authorization_server: Option<String>,
    authorization_servers: Option<Vec<String>>,
}

/// Parameters extracted from a `WWW-Authenticate` header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WwwAuthenticateParams {
    pub resource_metadata_url: Option<String>,
    pub scope: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// Per-server MCP OAuth metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpOAuthEntry {
    /// Stable lookup key derived from server name + transport + URL.
    pub server_key: String,
    pub server_name: String,
    pub server_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_server_metadata_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_metadata_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_unix_ms: Option<u64>,
}

/// SQLite-backed OAuth store so MCP auth can evolve independently from generic provider auth.
pub struct SqliteMcpOAuthRepository {
    path: PathBuf,
}

impl SqliteMcpOAuthRepository {
    /// Create a repository at the given sqlite path.
    pub fn new(path: PathBuf) -> Self {
        let repo = Self { path };
        if let Ok(conn) = repo.connect() {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS mcp_oauth (
                    server_key TEXT PRIMARY KEY,
                    data TEXT NOT NULL
                );",
            );
        }
        repo
    }

    /// Default repository path under the Luma config directory.
    pub fn default_path() -> PathBuf {
        crate::config::home_dir()
            .join(".config")
            .join("luma")
            .join("mcp_oauth.db")
    }

    /// Open the default MCP OAuth repository.
    pub fn with_default_path() -> Self {
        Self::new(Self::default_path())
    }

    fn connect(&self) -> anyhow::Result<rusqlite::Connection> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(&self.path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    /// Upsert one server entry.
    pub fn upsert(&self, entry: &McpOAuthEntry) -> anyhow::Result<()> {
        let conn = self.connect()?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let data = serde_json::to_string(entry)?;
        conn.execute(
            "INSERT OR REPLACE INTO mcp_oauth (server_key, data) VALUES (?1, ?2)",
            rusqlite::params![entry.server_key, data],
        )?;
        conn.execute_batch("COMMIT")?;
        Ok(())
    }

    /// Delete one server entry.
    pub fn remove(&self, server_key: &str) -> anyhow::Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "DELETE FROM mcp_oauth WHERE server_key = ?1",
            rusqlite::params![server_key],
        )?;
        Ok(())
    }

    /// Read one server entry by key.
    pub fn get(&self, server_key: &str) -> anyhow::Result<Option<McpOAuthEntry>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare("SELECT data FROM mcp_oauth WHERE server_key = ?1")?;
        let mut rows = stmt.query(rusqlite::params![server_key])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let json: String = row.get(0)?;
        Ok(serde_json::from_str(&json).ok())
    }
}

/// Stable storage key modeled after Claude Code's server-key approach: key by
/// identity, not just display name, so URL changes create a fresh auth slot.
pub fn server_key(server_name: &str, config: &McpHttpServerEntry) -> String {
    format!("{server_name}|{}|{}", config.r#type, config.url)
}

/// Merge config-provided auth hints with stored secrets/tokens.
pub fn resolve_remote_auth(
    server_name: &str,
    config: &McpHttpServerEntry,
) -> anyhow::Result<ResolvedRemoteAuth> {
    let key = server_key(server_name, config);
    let stored = SqliteMcpOAuthRepository::with_default_path().get(&key)?;
    Ok(ResolvedRemoteAuth {
        bearer_token: stored.as_ref().and_then(|entry| entry.access_token.clone()),
        refresh_token: stored
            .as_ref()
            .and_then(|entry| entry.refresh_token.clone()),
        client_id: config
            .oauth
            .as_ref()
            .and_then(|oauth| oauth.client_id.clone())
            .or_else(|| stored.as_ref().and_then(|entry| entry.client_id.clone())),
        client_secret: stored
            .as_ref()
            .and_then(|entry| entry.client_secret.clone()),
        auth_server_metadata_url: config
            .oauth
            .as_ref()
            .and_then(|oauth| oauth.auth_server_metadata_url.clone())
            .or_else(|| {
                stored
                    .as_ref()
                    .and_then(|entry| entry.auth_server_metadata_url.clone())
            }),
        token_endpoint: stored
            .as_ref()
            .and_then(|entry| entry.token_endpoint.clone()),
        authorization_endpoint: stored
            .as_ref()
            .and_then(|entry| entry.authorization_endpoint.clone()),
        revocation_endpoint: stored
            .as_ref()
            .and_then(|entry| entry.revocation_endpoint.clone()),
        scopes: stored.as_ref().and_then(|entry| entry.scopes.clone()),
    })
}

/// Resolved remote auth state used by the transport layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedRemoteAuth {
    pub bearer_token: Option<String>,
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub auth_server_metadata_url: Option<String>,
    pub token_endpoint: Option<String>,
    pub authorization_endpoint: Option<String>,
    pub revocation_endpoint: Option<String>,
    pub scopes: Option<Vec<String>>,
}

/// Discover and cache MCP auth metadata from a `WWW-Authenticate` header.
pub async fn discover_from_www_authenticate(
    server_name: &str,
    config: &McpHttpServerEntry,
    www_authenticate: &str,
) -> anyhow::Result<Option<McpOAuthEntry>> {
    let params = parse_www_authenticate(www_authenticate, &config.url);
    let Some(resource_metadata_url) = params.resource_metadata_url else {
        return Ok(None);
    };

    let resource_metadata = fetch_resource_metadata(&resource_metadata_url).await?;
    let auth_server_url = resource_metadata.authorization_server.or_else(|| {
        resource_metadata
            .authorization_servers
            .and_then(|mut xs| xs.drain(..).next())
    });
    let Some(auth_server_url) = auth_server_url else {
        return Ok(None);
    };

    let metadata_url = authorization_server_metadata_url(&auth_server_url)?;
    let metadata = fetch_authorization_server_metadata(&metadata_url).await?;

    let key = server_key(server_name, config);
    let repo = SqliteMcpOAuthRepository::with_default_path();
    let mut entry = repo.get(&key)?.unwrap_or(McpOAuthEntry {
        server_key: key,
        server_name: server_name.to_owned(),
        server_url: config.url.clone(),
        client_id: config.oauth.as_ref().and_then(|x| x.client_id.clone()),
        client_secret: None,
        access_token: None,
        refresh_token: None,
        auth_server_metadata_url: Some(metadata_url.clone()),
        resource_metadata_url: Some(resource_metadata_url.clone()),
        authorization_endpoint: metadata.authorization_endpoint.clone(),
        revocation_endpoint: metadata.revocation_endpoint.clone(),
        scopes: params
            .scope
            .clone()
            .map(|s| s.split_whitespace().map(str::to_owned).collect()),
        token_endpoint: metadata.token_endpoint.clone(),
        expires_at_unix_ms: None,
    });
    entry.resource_metadata_url = Some(resource_metadata_url);
    entry.auth_server_metadata_url = Some(metadata_url);
    entry.authorization_endpoint = metadata.authorization_endpoint.clone();
    entry.revocation_endpoint = metadata.revocation_endpoint.clone();
    entry.token_endpoint = metadata.token_endpoint.clone();
    if entry.scopes.is_none() {
        entry.scopes = params
            .scope
            .map(|s| s.split_whitespace().map(str::to_owned).collect());
    }
    repo.upsert(&entry)?;
    Ok(Some(entry))
}

pub async fn discover_from_url_hint(
    server_name: &str,
    config: &McpHttpServerEntry,
) -> anyhow::Result<bool> {
    let key = server_key(server_name, config);
    let repo = SqliteMcpOAuthRepository::with_default_path();
    let mut entry = repo.get(&key)?.unwrap_or(McpOAuthEntry {
        server_key: key,
        server_name: server_name.to_owned(),
        server_url: config.url.clone(),
        client_id: config.oauth.as_ref().and_then(|x| x.client_id.clone()),
        client_secret: None,
        access_token: None,
        refresh_token: None,
        auth_server_metadata_url: None,
        resource_metadata_url: None,
        authorization_endpoint: None,
        revocation_endpoint: None,
        scopes: None,
        token_endpoint: None,
        expires_at_unix_ms: None,
    });

    if entry.authorization_endpoint.is_some() && entry.token_endpoint.is_some() {
        return Ok(true);
    }

    if let Some(metadata_url) = entry.auth_server_metadata_url.clone().or_else(|| {
        config
            .oauth
            .as_ref()
            .and_then(|oauth| oauth.auth_server_metadata_url.clone())
    }) {
        let metadata = fetch_authorization_server_metadata(&metadata_url).await?;
        entry.auth_server_metadata_url = Some(metadata_url);
        entry.authorization_endpoint = metadata.authorization_endpoint;
        entry.revocation_endpoint = metadata.revocation_endpoint;
        entry.token_endpoint = metadata.token_endpoint;
        repo.upsert(&entry)?;
        return Ok(entry.authorization_endpoint.is_some() && entry.token_endpoint.is_some());
    }

    let metadata_url = authorization_server_metadata_url(&config.url)?;
    let metadata = fetch_authorization_server_metadata(&metadata_url).await?;
    entry.auth_server_metadata_url = Some(metadata_url);
    entry.authorization_endpoint = metadata.authorization_endpoint;
    entry.revocation_endpoint = metadata.revocation_endpoint;
    entry.token_endpoint = metadata.token_endpoint;
    repo.upsert(&entry)?;
    Ok(entry.authorization_endpoint.is_some() && entry.token_endpoint.is_some())
}

pub async fn register_client_if_needed(
    server_name: &str,
    config: &McpHttpServerEntry,
) -> anyhow::Result<bool> {
    let key = server_key(server_name, config);
    let repo = SqliteMcpOAuthRepository::with_default_path();
    let mut entry = repo.get(&key)?.unwrap_or(McpOAuthEntry {
        server_key: key,
        server_name: server_name.to_owned(),
        server_url: config.url.clone(),
        client_id: config.oauth.as_ref().and_then(|x| x.client_id.clone()),
        client_secret: None,
        access_token: None,
        refresh_token: None,
        auth_server_metadata_url: None,
        resource_metadata_url: None,
        authorization_endpoint: None,
        revocation_endpoint: None,
        scopes: None,
        token_endpoint: None,
        expires_at_unix_ms: None,
    });

    if entry.client_id.is_some() {
        return Ok(true);
    }

    let metadata_url = entry.auth_server_metadata_url.clone().or_else(|| {
        config
            .oauth
            .as_ref()
            .and_then(|oauth| oauth.auth_server_metadata_url.clone())
    });
    let Some(metadata_url) = metadata_url else {
        return Ok(false);
    };
    let metadata = fetch_authorization_server_metadata(&metadata_url).await?;
    entry.auth_server_metadata_url = Some(metadata_url);
    entry.authorization_endpoint = metadata.authorization_endpoint;
    entry.revocation_endpoint = metadata.revocation_endpoint;
    entry.token_endpoint = metadata.token_endpoint;

    // SEP-991: if the auth server supports CIMD, use the metadata URL as client_id directly
    if metadata.client_id_metadata_document_supported {
        entry.client_id = Some("https://claude.ai/oauth/claude-code-client-metadata".to_owned());
        repo.upsert(&entry)?;
        return Ok(true);
    }

    // Fallback to Dynamic Client Registration (RFC 7591)
    let Some(registration_endpoint) = metadata.registration_endpoint else {
        return Ok(false);
    };

    let redirect_uri = "http://localhost/callback";
    let registration_request = ClientRegistrationRequest {
        client_name: format!("Claude Code ({server_name})"),
        redirect_uris: vec![redirect_uri.to_owned()],
        grant_types: vec!["authorization_code".to_owned(), "refresh_token".to_owned()],
        token_endpoint_auth_method: "none".to_owned(),
        response_types: vec!["code".to_owned()],
        scope: entry
            .scopes
            .as_ref()
            .filter(|s| !s.is_empty())
            .or(metadata.scopes_supported.as_ref())
            .map(|s| s.join(" ")),
    };

    let response: ClientRegistrationResponse = reqwest::Client::new()
        .post(&registration_endpoint)
        .json(&registration_request)
        .send()
        .await
        .with_context(|| format!("failed to call registration endpoint {registration_endpoint}"))?
        .error_for_status()
        .with_context(|| format!("registration failed at {registration_endpoint}"))?
        .json()
        .await
        .with_context(|| {
            format!("failed to parse client registration response from {registration_endpoint}")
        })?;

    entry.client_id = Some(response.client_id);
    entry.client_secret = response.client_secret.filter(|s| !s.is_empty());
    repo.upsert(&entry)?;
    Ok(true)
}

/// Refresh a bearer token using an OAuth refresh token if enough metadata is available.
pub async fn refresh_access_token(
    server_name: &str,
    config: &McpHttpServerEntry,
) -> anyhow::Result<Option<String>> {
    let key = server_key(server_name, config);
    let repo = SqliteMcpOAuthRepository::with_default_path();
    let Some(mut entry) = repo.get(&key)? else {
        return Ok(None);
    };
    let Some(refresh_token) = entry.refresh_token.clone() else {
        return Ok(None);
    };
    let client_id = entry.client_id.clone().or_else(|| {
        config
            .oauth
            .as_ref()
            .and_then(|oauth| oauth.client_id.clone())
    });
    let token_endpoint = resolve_token_endpoint(&mut entry, config)
        .await?
        .or_else(|| entry.token_endpoint.clone());
    let Some(token_endpoint) = token_endpoint else {
        return Ok(None);
    };

    let mut params = vec![
        ("grant_type", String::from("refresh_token")),
        ("refresh_token", refresh_token),
    ];
    if let Some(client_id) = &client_id {
        params.push(("client_id", client_id.clone()));
    }
    if let Some(client_secret) = &entry.client_secret {
        params.push(("client_secret", client_secret.clone()));
    }

    let response = reqwest::Client::new()
        .post(&token_endpoint)
        .form(&params)
        .send()
        .await?
        .error_for_status()?;
    let body: serde_json::Value = response.json().await?;
    let Some(access_token) = body
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(None);
    };

    entry.access_token = Some(access_token.clone());
    if let Some(refresh_token) = body
        .get("refresh_token")
        .and_then(serde_json::Value::as_str)
    {
        entry.refresh_token = Some(refresh_token.to_owned());
    }
    if let Some(expires_in) = body.get("expires_in").and_then(serde_json::Value::as_u64) {
        entry.expires_at_unix_ms =
            Some(now_unix_ms().saturating_add(expires_in.saturating_mul(1000)));
    }
    if let Some(token_endpoint) = body
        .get("token_endpoint")
        .and_then(serde_json::Value::as_str)
    {
        entry.token_endpoint = Some(token_endpoint.to_owned());
    }
    repo.upsert(&entry)?;
    Ok(Some(access_token))
}

/// Clear only access and refresh tokens while preserving client registration data.
#[allow(dead_code)]
pub fn clear_tokens(server_name: &str, config: &McpHttpServerEntry) -> anyhow::Result<()> {
    let key = server_key(server_name, config);
    let repo = SqliteMcpOAuthRepository::with_default_path();
    let Some(mut entry) = repo.get(&key)? else {
        return Ok(());
    };
    entry.access_token = None;
    entry.refresh_token = None;
    entry.expires_at_unix_ms = None;
    repo.upsert(&entry)
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

async fn resolve_token_endpoint(
    entry: &mut McpOAuthEntry,
    config: &McpHttpServerEntry,
) -> anyhow::Result<Option<String>> {
    if let Some(token_endpoint) = &entry.token_endpoint {
        return Ok(Some(token_endpoint.clone()));
    }

    if let Some(metadata_url) = entry.auth_server_metadata_url.clone().or_else(|| {
        config
            .oauth
            .as_ref()
            .and_then(|oauth| oauth.auth_server_metadata_url.clone())
    }) {
        let metadata = fetch_authorization_server_metadata(&metadata_url).await?;
        if let Some(token_endpoint) = metadata.token_endpoint {
            entry.auth_server_metadata_url = Some(metadata_url);
            entry.authorization_endpoint = metadata.authorization_endpoint;
            entry.revocation_endpoint = metadata.revocation_endpoint;
            entry.token_endpoint = Some(token_endpoint.clone());
            SqliteMcpOAuthRepository::with_default_path().upsert(entry)?;
            return Ok(Some(token_endpoint));
        }
    }

    Ok(None)
}

async fn fetch_authorization_server_metadata(
    metadata_url: &str,
) -> anyhow::Result<AuthorizationServerMetadata> {
    reqwest::Client::new()
        .get(metadata_url)
        .send()
        .await
        .with_context(|| format!("failed to fetch auth server metadata from {metadata_url}"))?
        .error_for_status()
        .with_context(|| format!("auth server metadata request failed for {metadata_url}"))?
        .json()
        .await
        .with_context(|| format!("failed to parse auth server metadata from {metadata_url}"))
}

async fn fetch_resource_metadata(
    resource_metadata_url: &str,
) -> anyhow::Result<ResourceServerMetadata> {
    reqwest::Client::new()
        .get(resource_metadata_url)
        .send()
        .await
        .with_context(|| format!("failed to fetch resource metadata from {resource_metadata_url}"))?
        .error_for_status()
        .with_context(|| format!("resource metadata request failed for {resource_metadata_url}"))?
        .json()
        .await
        .with_context(|| format!("failed to parse resource metadata from {resource_metadata_url}"))
}

fn authorization_server_metadata_url(auth_server_url: &str) -> anyhow::Result<String> {
    let base = reqwest::Url::parse(auth_server_url)
        .with_context(|| format!("invalid authorization server URL: {auth_server_url}"))?;
    if base.path() == "/.well-known/oauth-authorization-server" {
        return Ok(base.to_string());
    }
    Ok(base
        .join("/.well-known/oauth-authorization-server")
        .with_context(|| {
            format!("failed to construct authorization server metadata URL from {auth_server_url}")
        })?
        .to_string())
}

fn parse_www_authenticate(header: &str, base_url: &str) -> WwwAuthenticateParams {
    let mut params = WwwAuthenticateParams::default();
    let header_lowercase = header.to_ascii_lowercase();

    let mut search_offset = 0;
    let resource_key = "resource_metadata=";
    while let Some(pos) = header_lowercase[search_offset..].find(resource_key) {
        let global_pos = search_offset + pos + resource_key.len();
        let value_slice = &header[global_pos..];
        if let Some((value, consumed)) = parse_next_header_value(value_slice) {
            if reqwest::Url::parse(&value).is_ok() {
                params.resource_metadata_url = Some(value);
                break;
            }
            if let Ok(base) = reqwest::Url::parse(base_url)
                && let Ok(url) = base.join(&value)
            {
                params.resource_metadata_url = Some(url.to_string());
                break;
            }
            search_offset = global_pos + consumed;
            continue;
        }
        break;
    }

    params.scope = extract_header_param(header, "scope=");
    params.error = extract_header_param(header, "error=");
    params.error_description = extract_header_param(header, "error_description=");
    params
}

fn extract_header_param(header: &str, key: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let pos = lower.find(key)?;
    let global_pos = pos + key.len();
    let value_slice = &header[global_pos..];
    parse_next_header_value(value_slice).map(|(value, _)| value)
}

fn parse_next_header_value(header_fragment: &str) -> Option<(String, usize)> {
    let trimmed = header_fragment.trim_start();
    let leading_ws = header_fragment.len() - trimmed.len();

    if let Some(stripped) = trimmed.strip_prefix('"') {
        let mut escaped = false;
        let mut result = String::new();
        for (idx, ch) in stripped.char_indices() {
            if escaped {
                result.push(ch);
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => return Some((result, leading_ws + idx + 2)),
                _ => result.push(ch),
            }
        }
        return None;
    }

    let end = trimmed.find(',').unwrap_or(trimmed.len());
    let token = trimmed[..end].trim();
    if token.is_empty() {
        return None;
    }
    Some((token.to_owned(), leading_ws + end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_repo(dir: &TempDir) -> SqliteMcpOAuthRepository {
        SqliteMcpOAuthRepository::new(dir.path().join("mcp_oauth.db"))
    }

    #[test]
    fn server_key_includes_transport_and_url() {
        let key = server_key(
            "figma",
            &McpHttpServerEntry {
                r#type: String::from("http"),
                url: String::from("https://example.com/mcp"),
                headers: Default::default(),
                headers_helper: None,
                oauth: None,
            },
        );
        assert_eq!(key, "figma|http|https://example.com/mcp");
    }

    #[test]
    fn upsert_and_get_roundtrip() {
        let dir = TempDir::new().unwrap();
        let repo = make_repo(&dir);
        let entry = McpOAuthEntry {
            server_key: String::from("figma|http|https://example.com/mcp"),
            server_name: String::from("figma"),
            server_url: String::from("https://example.com/mcp"),
            client_id: Some(String::from("cid")),
            client_secret: Some(String::from("secret")),
            access_token: Some(String::from("at")),
            refresh_token: Some(String::from("rt")),
            auth_server_metadata_url: Some(String::from(
                "https://issuer/.well-known/openid-configuration",
            )),
            token_endpoint: Some(String::from("https://issuer/token")),
            resource_metadata_url: None,
            authorization_endpoint: None,
            revocation_endpoint: None,
            scopes: None,
            expires_at_unix_ms: Some(42),
        };
        repo.upsert(&entry).unwrap();
        let loaded = repo.get(&entry.server_key).unwrap().unwrap();
        assert_eq!(loaded, entry);
    }

    #[test]
    fn remove_deletes_entry() {
        let dir = TempDir::new().unwrap();
        let repo = make_repo(&dir);
        let entry = McpOAuthEntry {
            server_key: String::from("figma|http|https://example.com/mcp"),
            server_name: String::from("figma"),
            server_url: String::from("https://example.com/mcp"),
            client_id: None,
            client_secret: Some(String::from("secret")),
            access_token: None,
            refresh_token: None,
            auth_server_metadata_url: None,
            token_endpoint: None,
            resource_metadata_url: None,
            authorization_endpoint: None,
            revocation_endpoint: None,
            scopes: None,
            expires_at_unix_ms: None,
        };
        repo.upsert(&entry).unwrap();
        repo.remove(&entry.server_key).unwrap();
        assert!(repo.get(&entry.server_key).unwrap().is_none());
    }

    #[test]
    fn parses_www_authenticate_resource_metadata() {
        let params = parse_www_authenticate(
            "Bearer resource_metadata=\"https://example.com/.well-known/oauth-protected-resource\", scope=\"read write\", error=\"invalid_token\"",
            "https://example.com/mcp",
        );
        assert_eq!(
            params.resource_metadata_url.as_deref(),
            Some("https://example.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(params.scope.as_deref(), Some("read write"));
        assert_eq!(params.error.as_deref(), Some("invalid_token"));
    }

    #[test]
    fn builds_authorization_server_metadata_url() {
        let url = authorization_server_metadata_url("https://issuer.example.com").unwrap();
        assert_eq!(
            url,
            "https://issuer.example.com/.well-known/oauth-authorization-server"
        );
    }
}
