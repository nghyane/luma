//! Auth domain model — stable account identity, credentials, and health state.
//!
//! This module defines the vocabulary used across `AuthRepository`,
//! `AuthService`, importers, and OAuth providers. No I/O, no side effects.

use serde::{Deserialize, Serialize};

// =============================================================================
// Vendor
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthVendor {
    Anthropic,
    #[serde(rename = "openai")]
    OpenAI,
    #[serde(rename = "opencode-go")]
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

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "anthropic" => Some(Self::Anthropic),
            "openai" | "codex" => Some(Self::OpenAI),
            "opencode-go" => Some(Self::OpenCodeGo),
            "kiro" => Some(Self::Kiro),
            _ => None,
        }
    }
}

// =============================================================================
// Account identity key
// =============================================================================

/// Stable, opaque key that uniquely identifies an account within a vendor.
/// `label` / display name MUST NOT be used as a key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountKey {
    pub vendor: AuthVendor,
    pub subject: AccountSubject,
}

impl AccountKey {
    pub fn account_id(vendor: AuthVendor, id: impl Into<String>) -> Self {
        Self {
            vendor,
            subject: AccountSubject::AccountId(id.into()),
        }
    }

    pub fn email(vendor: AuthVendor, email: impl Into<String>) -> Self {
        Self {
            vendor,
            subject: AccountSubject::Email(email.into().to_ascii_lowercase()),
        }
    }

    pub fn anonymous(vendor: AuthVendor, uuid: impl Into<String>) -> Self {
        Self {
            vendor,
            subject: AccountSubject::Anonymous(uuid.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountSubject {
    AccountId(String),
    Email(String),
    ExternalUserId(String),
    ApiKeyFingerprint(String),
    Anonymous(String),
}

// =============================================================================
// Credentials
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthState {
    OAuth(OAuthCredential),
    ApiKey(ApiKeyCredential),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyCredential {
    pub token: String,
}

// =============================================================================
// Account health
// =============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccountHealth {
    Active,
    CoolingDown { until_unix: u64 },
    NeedsRelogin { reason: ReloginReason },
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReloginReason {
    RefreshFailed,
    TokenRevoked,
    MissingRefreshToken,
    UserRequested,
}

// =============================================================================
// Account record
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRecord {
    pub key: AccountKey,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub auth: AuthState,
    pub health: AccountHealth,
    #[serde(default)]
    pub metadata: AccountMetadata,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imported_from: Option<String>,
    #[serde(default, skip_serializing_if = "UsageSnapshot::is_empty")]
    pub usage: UsageSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_flow: Option<AuthFlow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc_client: Option<SsoOidcClient>,
}

/// How this account was authenticated — determines refresh routing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthFlow {
    Social,
    Idc { region: String, start_url: String },
    BuilderId,
}

/// Cached SSO OIDC client registration (avoids re-registering on every refresh).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SsoOidcClient {
    pub client_id: String,
    pub client_secret: String,
    pub expires_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests_remaining: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_remaining: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<u64>,
    #[serde(default)]
    pub updated_at: u64,
}

impl UsageSnapshot {
    pub fn is_empty(&self) -> bool {
        self.requests_remaining.is_none()
            && self.requests_limit.is_none()
            && self.tokens_remaining.is_none()
            && self.tokens_limit.is_none()
            && self.reset_at.is_none()
            && self.updated_at == 0
    }
}

// =============================================================================
// UI view (no secrets)
// =============================================================================

/// Secrets-free snapshot for display in `/accounts` or `luma accounts`.
#[derive(Debug, Clone)]
pub struct AccountView {
    pub key: AccountKey,
    pub display_name: String,
    pub email: Option<String>,
    pub vendor: AuthVendor,
    pub health: AccountHealth,
}

impl AccountView {
    pub fn from_record(r: &AccountRecord) -> Self {
        Self {
            key: r.key.clone(),
            display_name: r.display_name.clone(),
            email: r.email.clone(),
            vendor: r.key.vendor,
            health: r.health.clone(),
        }
    }
}

// =============================================================================
// Auth failure taxonomy (used by provider runtime + AuthService)
// =============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum AuthFailure {
    Unauthorized,
    RefreshRejected,
    Revoked,
    MissingRefreshToken,
    InvalidGrant,
}

// =============================================================================
// Compatibility: convert legacy config::auth types → new domain
// =============================================================================

/// Convert a legacy `config::auth::AuthVendor` to the new `AuthVendor`.
/// Kept here so callers can migrate incrementally without touching the
/// legacy module.
impl From<crate::config::auth::AuthVendor> for AuthVendor {
    fn from(v: crate::config::auth::AuthVendor) -> Self {
        match v {
            crate::config::auth::AuthVendor::Anthropic => Self::Anthropic,
            crate::config::auth::AuthVendor::OpenAI => Self::OpenAI,
            crate::config::auth::AuthVendor::OpenCodeGo => Self::OpenCodeGo,
            crate::config::auth::AuthVendor::Kiro => Self::Kiro,
        }
    }
}

impl From<AuthVendor> for crate::config::auth::AuthVendor {
    fn from(v: AuthVendor) -> Self {
        match v {
            AuthVendor::Anthropic => Self::Anthropic,
            AuthVendor::OpenAI => Self::OpenAI,
            AuthVendor::OpenCodeGo => Self::OpenCodeGo,
            AuthVendor::Kiro => Self::Kiro,
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_key_email_is_lowercased() {
        let key = AccountKey::email(AuthVendor::Anthropic, "Me@Example.COM");
        assert_eq!(key.subject, AccountSubject::Email("me@example.com".into()));
    }

    #[test]
    fn account_key_equality_by_subject() {
        let a = AccountKey::account_id(AuthVendor::OpenAI, "acc_123");
        let b = AccountKey::account_id(AuthVendor::OpenAI, "acc_123");
        let c = AccountKey::account_id(AuthVendor::Anthropic, "acc_123");
        assert_eq!(a, b);
        assert_ne!(a, c); // different vendor
    }

    #[test]
    fn account_health_active_is_not_cooling_down() {
        assert_eq!(AccountHealth::Active, AccountHealth::Active);
        assert_ne!(
            AccountHealth::Active,
            AccountHealth::CoolingDown { until_unix: 9999 }
        );
    }

    #[test]
    fn account_view_from_record_no_secrets() {
        let record = AccountRecord {
            key: AccountKey::email(AuthVendor::Anthropic, "me@example.com"),
            display_name: "me@example".into(),
            email: Some("me@example.com".into()),
            auth: AuthState::OAuth(OAuthCredential {
                access_token: "secret-token".into(),
                refresh_token: Some("secret-refresh".into()),
                expires_at: Some(9_999_999_999),
                scopes: vec!["openid".into()],
            }),
            health: AccountHealth::Active,
            metadata: AccountMetadata::default(),
        };
        let view = AccountView::from_record(&record);
        assert_eq!(view.display_name, "me@example");
        assert_eq!(view.vendor, AuthVendor::Anthropic);
        // AccountView has no access_token field — compile-time guarantee
    }

    #[test]
    fn vendor_roundtrip_from_str() {
        for (s, v) in [
            ("anthropic", AuthVendor::Anthropic),
            ("openai", AuthVendor::OpenAI),
            ("opencode-go", AuthVendor::OpenCodeGo),
            ("kiro", AuthVendor::Kiro),
        ] {
            assert_eq!(AuthVendor::from_str(s), Some(v));
            assert_eq!(v.as_str(), s);
        }
        assert_eq!(AuthVendor::from_str("codex"), Some(AuthVendor::OpenAI));
        assert_eq!(AuthVendor::from_str("unknown"), None);
    }

    #[test]
    fn legacy_vendor_conversion_roundtrip() {
        use crate::config::auth::AuthVendor as LegacyVendor;
        let pairs = [
            (LegacyVendor::Anthropic, AuthVendor::Anthropic),
            (LegacyVendor::OpenAI, AuthVendor::OpenAI),
            (LegacyVendor::OpenCodeGo, AuthVendor::OpenCodeGo),
            (LegacyVendor::Kiro, AuthVendor::Kiro),
        ];
        for (legacy, new) in pairs {
            let converted: AuthVendor = legacy.into();
            assert_eq!(converted, new);
            let back: LegacyVendor = converted.into();
            assert_eq!(back.as_str(), legacy.as_str());
        }
    }

    #[test]
    fn auth_failure_variants_are_constructible() {
        let _ = AuthFailure::Unauthorized;
        let _ = AuthFailure::RefreshRejected;
        let _ = AuthFailure::MissingRefreshToken;
        let _ = AuthFailure::InvalidGrant;
    }
}
