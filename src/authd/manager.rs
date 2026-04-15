//! TokenManager — single-owner auth state, memory as source of truth.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{Mutex, RwLock};

use crate::auth::domain::{
    AccountHealth, AccountKey, AccountMetadata, AccountRecord, AccountSubject, AccountView,
    ApiKeyCredential, AuthState, AuthVendor, OAuthCredential, ReloginReason,
};
use crate::auth::error::{AuthError, OAuthError};
use crate::auth::oauth::OAuthRegistry;
use crate::auth::repo::{AuthStore, FileAuthRepository, AuthRepository, STORE_VERSION};
use crate::auth::selection::{AccountSelectionPolicy, DefaultSelectionPolicy};
use crate::authd::protocol::{AuthEvent, WireAccountView};
use crate::config::auth::Credential;

pub struct TokenManager {
    store: RwLock<AuthStore>,
    persist_path: PathBuf,
    /// Per-account refresh lock — prevents double refresh.
    refresh_locks: Mutex<HashMap<AccountKey, Arc<Mutex<()>>>>,
    oauth: OAuthRegistry,
    selection: DefaultSelectionPolicy,
    /// Broadcast channel for push events to all connected clients.
    event_tx: tokio::sync::broadcast::Sender<AuthEvent>,
}

impl TokenManager {
    pub fn new(event_tx: tokio::sync::broadcast::Sender<AuthEvent>) -> Self {
        let path = FileAuthRepository::default_path();
        let repo = FileAuthRepository::new(path.clone());
        let store = repo.load().unwrap_or(AuthStore {
            version: STORE_VERSION,
            accounts: vec![],
        });
        Self {
            store: RwLock::new(store),
            persist_path: path,
            refresh_locks: Mutex::new(HashMap::new()),
            oauth: OAuthRegistry::new(),
            selection: DefaultSelectionPolicy,
            event_tx,
        }
    }

    #[allow(dead_code)]
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AuthEvent> {
        self.event_tx.subscribe()
    }

    // =========================================================================
    // Resolve — hot path, no file I/O
    // =========================================================================

    pub async fn resolve(&self, vendor: AuthVendor) -> Result<Credential, AuthError> {
        let record = {
            let store = self.store.read().await;
            let candidates: Vec<_> = store.accounts.iter().filter(|a| a.key.vendor == vendor).cloned().collect();
            let key = self.selection.select(&candidates).ok_or(AuthError::NoEligibleAccount {
                vendor: vendor.as_str().to_owned(),
            })?;
            candidates.into_iter().find(|a| a.key == key).ok_or(AuthError::AccountNotFound)?
        };

        if let AuthState::OAuth(cred) = &record.auth
            && is_expired(cred.expires_at)
            && cred.refresh_token.is_some()
        {
            return self.refresh(&record.key).await;
        }

        Ok(credential_from_record(record))
    }

    // =========================================================================
    // Refresh — serialized per account key
    // =========================================================================

    pub async fn refresh(&self, key: &AccountKey) -> Result<Credential, AuthError> {
        let lock = {
            let mut locks = self.refresh_locks.lock().await;
            locks.entry(key.clone()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
        };
        let _guard = lock.lock().await;

        // Re-check: another caller may have refreshed while we waited.
        {
            let store = self.store.read().await;
            if let Some(record) = store.accounts.iter().find(|a| &a.key == key)
                && let AuthState::OAuth(cred) = &record.auth
                && !is_expired(cred.expires_at)
            {
                return Ok(credential_from_record(record.clone()));
            }
        }

        // Read current record for refresh token.
        let record = {
            let store = self.store.read().await;
            store.accounts.iter().find(|a| &a.key == key).cloned().ok_or(AuthError::AccountNotFound)?
        };

        let refresh_token = match &record.auth {
            AuthState::OAuth(cred) => cred.refresh_token.as_deref().ok_or_else(|| {
                AuthError::OAuth(OAuthError::RefreshRejected("missing refresh token".to_owned()))
            })?,
            AuthState::ApiKey(_) => {
                return Err(AuthError::OAuth(OAuthError::RefreshRejected(
                    "cannot refresh API key".to_owned(),
                )));
            }
        };

        let provider = self.oauth.get(record.key.vendor).ok_or(AuthError::NoEligibleAccount {
            vendor: record.key.vendor.as_str().to_owned(),
        })?;
        let tokens = provider.refresh(refresh_token).await.map_err(AuthError::OAuth)?;

        let refreshed = AccountRecord {
            key: record.key.clone(),
            display_name: record.display_name.clone(),
            email: record.email.clone(),
            auth: AuthState::OAuth(OAuthCredential {
                access_token: tokens.access_token.clone(),
                refresh_token: tokens.refresh_token.clone(),
                expires_at: tokens.expires_at,
                scopes: tokens.scopes.clone(),
            }),
            health: AccountHealth::Active,
            metadata: AccountMetadata {
                profile_arn: tokens.profile_arn.clone().or_else(|| record.metadata.profile_arn.clone()),
                ..record.metadata.clone()
            },
        };

        self.upsert_and_persist(refreshed.clone()).await;
        let _ = self.event_tx.send(AuthEvent::TokenRefreshed {
            key: record.key.clone(),
            label: refreshed.display_name.clone(),
        });

        Ok(Credential {
            token: tokens.access_token,
            is_oauth: true,
            account_id: match &refreshed.key.subject {
                AccountSubject::AccountId(id) => Some(id.clone()),
                _ => None,
            },
            label: refreshed.display_name,
            profile_arn: refreshed.metadata.profile_arn,
            account_key: Some(refreshed.key),
        })
    }

    // =========================================================================
    // Mutations — all go through memory first, then persist
    // =========================================================================

    pub async fn mark_rate_limited(&self, key: &AccountKey, retry_after_secs: u64) {
        let until = now_unix().saturating_add(retry_after_secs.max(1));
        let health = AccountHealth::CoolingDown { until_unix: until };
        self.set_health(key, health).await;
    }

    pub async fn mark_auth_failed(&self, key: &AccountKey, failure: &str) {
        let reason = match failure {
            "revoked" => ReloginReason::TokenRevoked,
            "missing_refresh_token" => ReloginReason::MissingRefreshToken,
            _ => ReloginReason::RefreshFailed,
        };
        self.set_health(key, AccountHealth::NeedsRelogin { reason }).await;
    }

    pub async fn toggle_disabled(&self, key: &AccountKey) {
        let mut store = self.store.write().await;
        if let Some(a) = store.accounts.iter_mut().find(|a| &a.key == key) {
            a.health = match &a.health {
                AccountHealth::Disabled => AccountHealth::Active,
                _ => AccountHealth::Disabled,
            };
            let health = a.health.clone();
            drop(store);
            self.persist().await;
            self.broadcast_account_updated(key, &health).await;
        }
    }

    pub async fn remove_account(&self, key: &AccountKey) {
        {
            let mut store = self.store.write().await;
            store.accounts.retain(|a| &a.key != key);
        }
        self.persist().await;
        let _ = self.event_tx.send(AuthEvent::AccountRemoved { key: key.clone() });
    }

    pub async fn list_accounts(&self) -> Vec<AccountView> {
        let store = self.store.read().await;
        let selected = self.selection.select(&store.accounts);
        let mut views: Vec<_> = store.accounts.iter().map(AccountView::from_record).collect();
        views.sort_by_key(|v| (Some(&v.key) != selected.as_ref(), v.display_name.clone()));
        views
    }

    pub async fn login(&self, vendor: AuthVendor) -> Result<AccountView, AuthError> {
        let provider = self.oauth.get(vendor).ok_or(AuthError::NoEligibleAccount {
            vendor: vendor.as_str().to_owned(),
        })?;
        let login = provider.login().await.map_err(AuthError::OAuth)?;
        let record = AccountRecord {
            key: login.identity.key.clone(),
            display_name: login.identity.display_name,
            email: login.identity.email,
            auth: AuthState::OAuth(OAuthCredential {
                access_token: login.tokens.access_token,
                refresh_token: login.tokens.refresh_token,
                expires_at: login.tokens.expires_at,
                scopes: login.tokens.scopes,
            }),
            health: AccountHealth::Active,
            metadata: AccountMetadata {
                profile_arn: login.tokens.profile_arn,
                ..AccountMetadata::default()
            },
        };

        self.upsert_and_persist(record.clone()).await;
        let view = AccountView::from_record(&record);
        let _ = self.event_tx.send(AuthEvent::AccountAdded {
            view: WireAccountView::from(&view),
        });
        Ok(view)
    }

    pub async fn save_api_key(&self, vendor: AuthVendor, token: &str) -> Result<AccountView, AuthError> {
        let fingerprint = api_key_fingerprint(token);
        let display_name = format!("{}:key:{}", vendor.as_str(), fingerprint);
        let record = AccountRecord {
            key: AccountKey {
                vendor,
                subject: AccountSubject::ApiKeyFingerprint(fingerprint),
            },
            display_name,
            email: None,
            auth: AuthState::ApiKey(ApiKeyCredential { token: token.to_owned() }),
            health: AccountHealth::Active,
            metadata: AccountMetadata::default(),
        };
        self.upsert_and_persist(record.clone()).await;
        let view = AccountView::from_record(&record);
        let _ = self.event_tx.send(AuthEvent::AccountAdded {
            view: WireAccountView::from(&view),
        });
        Ok(view)
    }

    pub async fn record_usage(&self, display_name: &str, usage: crate::config::auth::UsageSnapshot) {
        let mut store = self.store.write().await;
        if let Some(account) = store.accounts.iter_mut().find(|a| a.display_name == display_name) {
            account.metadata.usage = crate::auth::domain::UsageSnapshot {
                requests_remaining: usage.requests_remaining,
                requests_limit: usage.requests_limit,
                tokens_remaining: usage.tokens_remaining,
                tokens_limit: usage.tokens_limit,
                reset_at: usage.reset_at,
                updated_at: if usage.updated_at == 0 { now_unix() } else { usage.updated_at },
            };
        }
        drop(store);
        self.persist().await;
    }

    // =========================================================================
    // Internal helpers
    // =========================================================================

    async fn set_health(&self, key: &AccountKey, health: AccountHealth) {
        {
            let mut store = self.store.write().await;
            if let Some(a) = store.accounts.iter_mut().find(|a| &a.key == key) {
                a.health = health.clone();
            }
        }
        self.persist().await;
        self.broadcast_account_updated(key, &health).await;
    }

    async fn upsert_and_persist(&self, record: AccountRecord) {
        {
            let mut store = self.store.write().await;
            if let Some(existing) = store.accounts.iter_mut().find(|a| a.key == record.key) {
                *existing = record;
            } else {
                store.accounts.push(record);
            }
        }
        self.persist().await;
    }

    async fn persist(&self) {
        let store = self.store.read().await;
        let json = match serde_json::to_string_pretty(&*store) {
            Ok(j) => j,
            Err(_) => return,
        };
        drop(store);

        if let Some(parent) = self.persist_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.persist_path.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, &self.persist_path);
        }
    }

    async fn broadcast_account_updated(&self, key: &AccountKey, health: &AccountHealth) {
        let _ = self.event_tx.send(AuthEvent::AccountUpdated {
            key: key.clone(),
            health: health.clone(),
        });
        self.broadcast_pool_changed(key.vendor).await;
    }

    async fn broadcast_pool_changed(&self, vendor: AuthVendor) {
        let store = self.store.read().await;
        let now = now_unix();
        let mut active = 0usize;
        let mut cooling = 0usize;
        let mut total = 0usize;
        for a in store.accounts.iter().filter(|a| a.key.vendor == vendor) {
            total += 1;
            match &a.health {
                AccountHealth::Active => active += 1,
                AccountHealth::CoolingDown { until_unix } if *until_unix > now => cooling += 1,
                AccountHealth::CoolingDown { .. } => active += 1,
                _ => {}
            }
        }
        let _ = self.event_tx.send(AuthEvent::PoolChanged {
            vendor: vendor.as_str().to_owned(),
            active,
            cooling,
            total,
        });
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

const EXPIRY_GRACE_SECS: u64 = 300;

fn is_expired(expires_at: Option<u64>) -> bool {
    let Some(ts) = expires_at else { return false };
    now_unix() >= ts.saturating_sub(EXPIRY_GRACE_SECS)
}

fn credential_from_record(record: AccountRecord) -> Credential {
    let (token, is_oauth, account_id) = match record.auth {
        AuthState::OAuth(cred) => (
            cred.access_token,
            true,
            match &record.key.subject {
                AccountSubject::AccountId(id) => Some(id.clone()),
                _ => None,
            },
        ),
        AuthState::ApiKey(cred) => (cred.token, false, None),
    };
    Credential {
        token,
        is_oauth,
        account_id,
        label: record.display_name,
        profile_arn: record.metadata.profile_arn,
        account_key: Some(record.key),
    }
}

fn api_key_fingerprint(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let core = token.strip_prefix("sk-").unwrap_or(token);
    let digest = format!("{:x}", Sha256::digest(core.as_bytes()));
    digest[..12.min(digest.len())].to_owned()
}
