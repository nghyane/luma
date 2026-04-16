//! Thin compatibility surface over the new auth architecture.
//!
//! Business logic lives in `src/auth/*`. This module remains only because a
//! few call-sites still depend on `config::auth::{AuthVendor, Credential,
//! UsageSnapshot, resolve, force_refresh, record_usage}`.

use anyhow::Result;

mod codex_identity;
pub(crate) use codex_identity::{CODEX_ORIGINATOR, codex_user_agent, resolve_installation_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthVendor {
    Anthropic,
    OpenAI,
    OpenCodeGo,
    Kiro,
}

impl AuthVendor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAI => "openai",
            Self::OpenCodeGo => "opencode-go",
            Self::Kiro => "kiro",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Credential {
    pub token: String,
    pub is_oauth: bool,
    pub account_id: Option<String>,
    pub label: String,
    pub profile_arn: Option<String>,
    pub account_key: Option<crate::auth::domain::AccountKey>,
}

#[derive(Debug, Clone, Default)]
pub struct UsageSnapshot {
    pub requests_remaining: Option<u64>,
    pub requests_limit: Option<u64>,
    pub tokens_remaining: Option<u64>,
    pub tokens_limit: Option<u64>,
    pub reset_at: Option<u64>,
    pub updated_at: u64,
}

pub async fn resolve(provider: AuthVendor) -> Result<Credential> {
    crate::auth::service::AuthService::new(
        crate::auth::repo::SqliteAuthRepository::with_default_path(),
    )
    .resolve_credential(provider.into())
    .await
    .map_err(anyhow::Error::from)
}

pub async fn force_refresh(provider: AuthVendor) -> Result<Credential> {
    let service = crate::auth::service::AuthService::new(
        crate::auth::repo::SqliteAuthRepository::with_default_path(),
    );
    let cred = service
        .resolve_credential(provider.into())
        .await
        .map_err(anyhow::Error::from)?;
    let key = cred
        .account_key
        .clone()
        .ok_or_else(|| anyhow::anyhow!("resolved credential missing account key"))?;
    service
        .refresh_credential(&key)
        .await
        .map_err(anyhow::Error::from)
}

pub fn record_usage(label: &str, usage: UsageSnapshot) {
    let _ = crate::auth::service::AuthService::new(
        crate::auth::repo::SqliteAuthRepository::with_default_path(),
    )
    .record_usage_by_display_name(label, usage);
}

/// Sync, non-blocking check: is there at least one Kiro account in the
/// auth store? Used by search routing to decide whether the Kiro MCP
/// search backend is available without triggering a network call.
pub fn has_kiro_credential() -> bool {
    use crate::auth::repo::AuthRepository;
    let Ok(store) = crate::auth::repo::SqliteAuthRepository::with_default_path().load() else {
        return false;
    };
    store
        .accounts
        .iter()
        .any(|a| a.key.vendor == crate::auth::domain::AuthVendor::Kiro)
}

fn home_dir() -> std::path::PathBuf {
    crate::config::home_dir()
}
