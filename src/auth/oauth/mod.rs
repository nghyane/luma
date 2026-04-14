//! OAuth providers and shared contract.

pub mod codex;
pub mod kiro;
pub mod shared;

use crate::auth::domain::{AccountKey, AuthVendor};
use crate::auth::error::OAuthError;

#[derive(Debug, Clone)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_at: Option<u64>,
    pub scopes: Vec<String>,
    pub profile_arn: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AccountIdentity {
    pub key: AccountKey,
    pub display_name: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LoginResult {
    pub identity: AccountIdentity,
    pub tokens: OAuthTokens,
}

pub enum ProviderRef<'a> {
    Codex(&'a codex::CodexProvider),
    Kiro(&'a kiro::KiroProvider),
}

impl ProviderRef<'_> {
    pub fn vendor(&self) -> AuthVendor {
        match self {
            Self::Codex(_) => AuthVendor::OpenAI,
            Self::Kiro(_) => AuthVendor::Kiro,
        }
    }

    pub async fn login(&self) -> Result<LoginResult, OAuthError> {
        match self {
            Self::Codex(p) => p.login().await,
            Self::Kiro(p) => p.login().await,
        }
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<OAuthTokens, OAuthError> {
        match self {
            Self::Codex(p) => p.refresh(refresh_token).await,
            Self::Kiro(p) => p.refresh(refresh_token).await,
        }
    }
}

pub struct OAuthRegistry {
    codex: codex::CodexProvider,
    kiro: kiro::KiroProvider,
}

impl OAuthRegistry {
    pub fn new() -> Self {
        Self {
            codex: codex::CodexProvider,
            kiro: kiro::KiroProvider,
        }
    }

    pub fn get(&self, vendor: AuthVendor) -> Option<ProviderRef<'_>> {
        match vendor {
            AuthVendor::OpenAI => Some(ProviderRef::Codex(&self.codex)),
            AuthVendor::Kiro => Some(ProviderRef::Kiro(&self.kiro)),
            _ => None,
        }
    }
}

impl Default for OAuthRegistry {
    fn default() -> Self {
        Self::new()
    }
}
