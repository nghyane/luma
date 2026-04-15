//! `AuthRepository` — the single layer that reads/writes `auth.json`.
//!
//! Responsibilities:
//! - atomic write (temp file + rename),
//! - load with v2→v3 migration,
//! - all save paths return `Result` (no silent swallowing).
//!
//! Does NOT: import local CLI accounts, refresh tokens, select accounts.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::auth::domain::{
    AccountHealth, AccountKey, AccountMetadata, AccountRecord, ApiKeyCredential, AuthState,
    AuthVendor, OAuthCredential, ReloginReason, UsageSnapshot,
};
use crate::auth::error::AuthStoreError;

// =============================================================================
// AuthStore — in-memory representation
// =============================================================================

pub const STORE_VERSION: u32 = 3;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthStore {
    pub version: u32,
    pub accounts: Vec<AccountRecord>,
}

// =============================================================================
// Repository trait
// =============================================================================

pub trait AuthRepository {
    fn load(&self) -> Result<AuthStore, AuthStoreError>;
    fn save(&self, store: &AuthStore) -> Result<(), AuthStoreError>;
}

// =============================================================================
// File-backed implementation
// =============================================================================

pub struct FileAuthRepository {
    path: PathBuf,
}

impl FileAuthRepository {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Default path: `~/.config/luma/auth.json`
    pub fn default_path() -> PathBuf {
        crate::config::home_dir()
            .join(".config")
            .join("luma")
            .join("auth.json")
    }

    pub fn with_default_path() -> Self {
        Self::new(Self::default_path())
    }
}

impl AuthRepository for FileAuthRepository {
    fn load(&self) -> Result<AuthStore, AuthStoreError> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AuthStore {
                    version: STORE_VERSION,
                    accounts: vec![],
                });
            }
            Err(e) => return Err(AuthStoreError::Io(e)),
        };
        load_and_migrate(&raw)
    }

    fn save(&self, store: &AuthStore) -> Result<(), AuthStoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&self.path, store)
    }
}

// =============================================================================
// Atomic write
// =============================================================================

fn atomic_write(path: &Path, store: &AuthStore) -> Result<(), AuthStoreError> {
    let json = serde_json::to_string_pretty(store)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json)?;
    fs::rename(&tmp, path).map_err(|_| AuthStoreError::AtomicWriteFailed)?;
    Ok(())
}

// =============================================================================
// Load + migration
// =============================================================================

fn load_and_migrate(raw: &str) -> Result<AuthStore, AuthStoreError> {
    // Try v3 first.
    if let Ok(store) = serde_json::from_str::<AuthStore>(raw)
        && store.version >= 3
    {
        return Ok(store);
    }
    // Try v2 (current on-disk format).
    if let Ok(v2) = serde_json::from_str::<V2Store>(raw)
        && (v2.version >= 2 || !v2.accounts.is_empty())
    {
        return Ok(migrate_v2(v2));
    }
    // Try legacy v1 (`{ "credentials": [...] }`).
    if let Ok(v1) = serde_json::from_str::<V1Store>(raw) {
        return Ok(migrate_v1(v1));
    }
    // Malformed but non-empty file.
    Err(AuthStoreError::Malformed(
        serde_json::from_str::<serde_json::Value>(raw).unwrap_err(),
    ))
}

// =============================================================================
// V2 on-disk types (current legacy format)
// =============================================================================

#[derive(Debug, Deserialize)]
struct V2Store {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    accounts: Vec<V2Account>,
}

#[derive(Debug, Deserialize)]
struct V2Account {
    label: String,
    provider: String,
    #[serde(default)]
    email: Option<String>,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    profile_arn: Option<String>,
    #[serde(default = "v2_default_true")]
    is_oauth: bool,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default)]
    cooldown_until: Option<u64>,
    #[serde(default)]
    needs_relogin: bool,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    usage: V2Usage,
}

fn v2_default_true() -> bool {
    true
}

#[derive(Debug, Default, Deserialize)]
struct V2Usage {
    #[serde(default)]
    requests_remaining: Option<u64>,
    #[serde(default)]
    requests_limit: Option<u64>,
    #[serde(default)]
    tokens_remaining: Option<u64>,
    #[serde(default)]
    tokens_limit: Option<u64>,
    #[serde(default)]
    reset_at: Option<u64>,
    #[serde(default)]
    updated_at: u64,
}

fn migrate_v2(v2: V2Store) -> AuthStore {
    let accounts = v2
        .accounts
        .into_iter()
        .filter_map(v2_account_to_record)
        .collect();
    AuthStore {
        version: STORE_VERSION,
        accounts,
    }
}

fn v2_account_to_record(a: V2Account) -> Option<AccountRecord> {
    let vendor = AuthVendor::from_str(&a.provider)?;

    let key = derive_key(
        vendor,
        a.account_id.as_deref(),
        a.email.as_deref(),
        &a.label,
    );

    let auth = if a.is_oauth {
        AuthState::OAuth(OAuthCredential {
            access_token: a.access_token,
            refresh_token: a.refresh_token,
            expires_at: a.expires_at,
            scopes: a.scopes.unwrap_or_default(),
        })
    } else {
        AuthState::ApiKey(ApiKeyCredential {
            token: a.access_token,
        })
    };

    let now = now_unix();
    let health = if a.disabled {
        AccountHealth::Disabled
    } else if a.needs_relogin {
        AccountHealth::NeedsRelogin {
            reason: ReloginReason::RefreshFailed,
        }
    } else if let Some(until) = a.cooldown_until.filter(|&t| t > now) {
        AccountHealth::CoolingDown { until_unix: until }
    } else {
        AccountHealth::Active
    };

    Some(AccountRecord {
        key,
        display_name: a.label,
        email: a.email,
        auth,
        health,
        metadata: AccountMetadata {
            profile_arn: a.profile_arn,
            last_success_at: None,
            imported_from: None,
            usage: UsageSnapshot {
                requests_remaining: a.usage.requests_remaining,
                requests_limit: a.usage.requests_limit,
                tokens_remaining: a.usage.tokens_remaining,
                tokens_limit: a.usage.tokens_limit,
                reset_at: a.usage.reset_at,
                updated_at: a.usage.updated_at,
            },
        },
    })
}

// =============================================================================
// V1 legacy types
// =============================================================================

#[derive(Debug, Deserialize)]
struct V1Store {
    #[serde(default)]
    credentials: Vec<V1Entry>,
}

#[derive(Debug, Deserialize)]
struct V1Entry {
    provider: String,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default = "v2_default_true")]
    is_oauth: bool,
}

fn migrate_v1(v1: V1Store) -> AuthStore {
    let accounts = v1
        .credentials
        .into_iter()
        .filter_map(|e| {
            let vendor = AuthVendor::from_str(&e.provider)?;
            let email = extract_email_from_jwt(&e.access_token);
            let key = derive_key(vendor, e.account_id.as_deref(), email.as_deref(), "");
            let display_name = email
                .as_deref()
                .and_then(|em| {
                    em.split_once('@')
                        .map(|(l, d)| format!("{}@{}", l, d.split('.').next().unwrap_or(d)))
                })
                .unwrap_or_else(|| format!("{}-migrated", vendor.as_str()));
            let auth = if e.is_oauth {
                AuthState::OAuth(OAuthCredential {
                    access_token: e.access_token,
                    refresh_token: e.refresh_token,
                    expires_at: None,
                    scopes: vec![],
                })
            } else {
                AuthState::ApiKey(ApiKeyCredential {
                    token: e.access_token,
                })
            };
            Some(AccountRecord {
                key,
                display_name,
                email,
                auth,
                health: AccountHealth::Active,
                metadata: AccountMetadata::default(),
            })
        })
        .collect();
    AuthStore {
        version: STORE_VERSION,
        accounts,
    }
}

// =============================================================================
// Key derivation from v2/v1 data
// =============================================================================

fn derive_key(
    vendor: AuthVendor,
    account_id: Option<&str>,
    email: Option<&str>,
    label: &str,
) -> AccountKey {
    if let Some(id) = account_id.filter(|s| !s.is_empty()) {
        return AccountKey::account_id(vendor, id);
    }
    if let Some(em) = email.filter(|s| !s.is_empty()) {
        return AccountKey::email(vendor, em);
    }
    // Fall back to label as anonymous seed for stable identity across loads.
    let seed = if label.is_empty() {
        format!("{}-unknown", vendor.as_str())
    } else {
        label.to_owned()
    };
    AccountKey::anonymous(vendor, seed)
}

// =============================================================================
// Helpers
// =============================================================================

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn extract_email_from_jwt(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let segment = parts[1];
    let padded = match segment.len() % 4 {
        2 => format!("{segment}=="),
        3 => format!("{segment}="),
        _ => segment.to_owned(),
    };
    let decoded = padded.replace('-', "+").replace('_', "/");
    let bytes = base64_decode(&decoded)?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("email")?.as_str().map(|s| s.to_owned())
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let bytes: Vec<u8> = input.bytes().filter(|&b| b != b'=').collect();
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &b) in chunk.iter().enumerate() {
            let val = TABLE.iter().position(|&c| c == b)? as u32;
            n |= val << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::domain::AccountSubject;
    use tempfile::TempDir;

    fn make_repo(dir: &TempDir) -> FileAuthRepository {
        FileAuthRepository::new(dir.path().join("auth.json"))
    }

    #[test]
    fn load_missing_file_returns_empty_store() {
        let dir = TempDir::new().unwrap();
        let repo = make_repo(&dir);
        let store = repo.load().unwrap();
        assert_eq!(store.version, STORE_VERSION);
        assert!(store.accounts.is_empty());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let repo = make_repo(&dir);

        let store = AuthStore {
            version: STORE_VERSION,
            accounts: vec![AccountRecord {
                key: AccountKey::email(AuthVendor::Anthropic, "me@example.com"),
                display_name: "me@example".into(),
                email: Some("me@example.com".into()),
                auth: AuthState::OAuth(OAuthCredential {
                    access_token: "tok".into(),
                    refresh_token: Some("ref".into()),
                    expires_at: Some(9_999_999_999),
                    scopes: vec!["openid".into()],
                }),
                health: AccountHealth::Active,
                metadata: AccountMetadata::default(),
            }],
        };

        repo.save(&store).unwrap();
        let loaded = repo.load().unwrap();
        assert_eq!(loaded.accounts.len(), 1);
        assert_eq!(loaded.accounts[0].display_name, "me@example");
    }

    #[test]
    fn save_is_atomic_temp_file_renamed() {
        let dir = TempDir::new().unwrap();
        let repo = make_repo(&dir);
        let store = AuthStore {
            version: STORE_VERSION,
            accounts: vec![],
        };
        repo.save(&store).unwrap();
        // Temp file must not remain after successful save.
        assert!(!dir.path().join("auth.json.tmp").exists());
        assert!(dir.path().join("auth.json").exists());
    }

    #[test]
    fn load_malformed_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(&path, b"not json at all!!!").unwrap();
        let repo = FileAuthRepository::new(path);
        assert!(matches!(repo.load(), Err(AuthStoreError::Malformed(_))));
    }

    #[test]
    fn migrate_v2_preserves_tokens_and_health() {
        let v2_json = serde_json::json!({
            "version": 2,
            "accounts": [{
                "label": "me@example",
                "provider": "anthropic",
                "email": "me@example.com",
                "access_token": "tok",
                "refresh_token": "ref",
                "is_oauth": true,
                "expires_at": 9_999_999_999u64,
                "scopes": ["user:inference"],
                "needs_relogin": false,
                "disabled": false
            }]
        });
        let store = load_and_migrate(&v2_json.to_string()).unwrap();
        assert_eq!(store.version, STORE_VERSION);
        let acc = &store.accounts[0];
        assert_eq!(acc.display_name, "me@example");
        assert_eq!(acc.email.as_deref(), Some("me@example.com"));
        assert_eq!(acc.health, AccountHealth::Active);
        let AuthState::OAuth(cred) = &acc.auth else {
            panic!("expected oauth")
        };
        assert_eq!(cred.access_token, "tok");
        assert_eq!(cred.refresh_token.as_deref(), Some("ref"));
    }

    #[test]
    fn migrate_v2_disabled_account() {
        let v2_json = serde_json::json!({
            "version": 2,
            "accounts": [{
                "label": "x",
                "provider": "openai",
                "access_token": "t",
                "is_oauth": false,
                "disabled": true
            }]
        });
        let store = load_and_migrate(&v2_json.to_string()).unwrap();
        assert_eq!(store.accounts[0].health, AccountHealth::Disabled);
    }

    #[test]
    fn migrate_v2_needs_relogin() {
        let v2_json = serde_json::json!({
            "version": 2,
            "accounts": [{
                "label": "x",
                "provider": "anthropic",
                "access_token": "t",
                "is_oauth": true,
                "needs_relogin": true
            }]
        });
        let store = load_and_migrate(&v2_json.to_string()).unwrap();
        assert!(matches!(
            store.accounts[0].health,
            AccountHealth::NeedsRelogin { .. }
        ));
    }

    #[test]
    fn migrate_v2_key_prefers_account_id_over_email() {
        let v2_json = serde_json::json!({
            "version": 2,
            "accounts": [{
                "label": "x",
                "provider": "openai",
                "email": "me@example.com",
                "account_id": "acc_123",
                "access_token": "t",
                "is_oauth": true
            }]
        });
        let store = load_and_migrate(&v2_json.to_string()).unwrap();
        assert_eq!(
            store.accounts[0].key.subject,
            AccountSubject::AccountId("acc_123".into())
        );
    }

    #[test]
    fn migrate_v1_legacy_credentials() {
        let v1_json = serde_json::json!({
            "credentials": [{
                "provider": "anthropic",
                "access_token": "tok",
                "refresh_token": "ref",
                "is_oauth": true
            }]
        });
        let store = load_and_migrate(&v1_json.to_string()).unwrap();
        assert_eq!(store.version, STORE_VERSION);
        assert_eq!(store.accounts.len(), 1);
    }
}
