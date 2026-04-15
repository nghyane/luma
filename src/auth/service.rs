//! `AuthService` — the only layer allowed to mutate auth state.
//!
//! PR3 scope: list_accounts, mark_rate_limited, mark_auth_failed,
//! toggle_disabled, remove_account. Login/refresh come in later PRs.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::domain::{
    AccountHealth, AccountKey, AccountMetadata, AccountRecord, AccountSubject, AccountView,
    ApiKeyCredential, AuthFailure, AuthFlow, AuthState, AuthVendor, OAuthCredential, ReloginReason,
};
use crate::auth::error::AuthError;
use crate::auth::repo::{AuthRepository, AuthStore};
use crate::auth::selection::{AccountSelectionPolicy, DefaultSelectionPolicy};

pub struct AuthService<R> {
    repo: R,
    selection: DefaultSelectionPolicy,
}

impl<R: AuthRepository> AuthService<R> {
    pub fn new(repo: R) -> Self {
        Self {
            repo,
            selection: DefaultSelectionPolicy,
        }
    }

    pub fn list_accounts(&self) -> Result<Vec<AccountView>, AuthError> {
        let store = self.repo.load()?;
        let selected = self.selection.select(&store.accounts);
        let mut views: Vec<_> = store
            .accounts
            .iter()
            .map(AccountView::from_record)
            .collect();
        views.sort_by_key(|view| {
            (
                Some(&view.key) != selected.as_ref(),
                view.display_name.clone(),
            )
        });
        Ok(views)
    }

    pub fn mark_rate_limited(
        &self,
        key: &AccountKey,
        retry_after_secs: u64,
    ) -> Result<(), AuthError> {
        let until = now_unix().saturating_add(retry_after_secs.max(1));
        self.mutate(|store| {
            if let Some(a) = find_mut(store, key) {
                a.health = AccountHealth::CoolingDown { until_unix: until };
            }
        })
    }

    pub fn mark_auth_failed(
        &self,
        key: &AccountKey,
        failure: AuthFailure,
    ) -> Result<(), AuthError> {
        let reason = match failure {
            AuthFailure::Revoked => ReloginReason::TokenRevoked,
            AuthFailure::MissingRefreshToken => ReloginReason::MissingRefreshToken,
            _ => ReloginReason::RefreshFailed,
        };
        self.mutate(|store| {
            if let Some(a) = find_mut(store, key) {
                a.health = AccountHealth::NeedsRelogin { reason };
            }
        })
    }

    pub fn toggle_disabled(&self, key: &AccountKey) -> Result<(), AuthError> {
        self.mutate(|store| {
            if let Some(a) = find_mut(store, key) {
                a.health = match &a.health {
                    AccountHealth::Disabled => AccountHealth::Active,
                    _ => AccountHealth::Disabled,
                };
            }
        })
    }

    pub fn remove_account(&self, key: &AccountKey) -> Result<(), AuthError> {
        self.mutate(|store| store.accounts.retain(|a| &a.key != key))
    }

    pub async fn login(&self, vendor: AuthVendor) -> Result<AccountView, AuthError> {
        if vendor == AuthVendor::Kiro {
            return self.login_kiro_portal().await;
        }
        let oauth = crate::auth::oauth::OAuthRegistry::new();
        let provider = oauth.get(vendor).ok_or(AuthError::NoEligibleAccount {
            vendor: vendor.as_str().to_owned(),
        })?;
        let login = provider.login().await.map_err(AuthError::OAuth)?;
        self.save_login_result(login, AuthFlow::Social)
    }

    /// Kiro portal login — handles social, or delegates to device flow for IDC/BuilderId.
    async fn login_kiro_portal(&self) -> Result<AccountView, AuthError> {
        use crate::auth::oauth::kiro::{KiroProvider, PortalOutcome};
        let outcome = KiroProvider.login().await.map_err(AuthError::OAuth)?;
        match outcome {
            PortalOutcome::Social(login) => self.save_login_result(login, AuthFlow::Social),
            PortalOutcome::Idc { issuer_url, idc_region } => {
                self.login_device(&issuer_url, &idc_region).await
            }
            PortalOutcome::BuilderId => {
                self.login_device("https://view.awsapps.com/start", "us-east-1").await
            }
        }
    }

    /// Direct device-flow login for IAM Identity Center / Builder ID.
    pub async fn login_device(
        &self,
        start_url: &str,
        region: &str,
    ) -> Result<AccountView, AuthError> {
        let (login, client) =
            crate::auth::oauth::sso_oidc::login(start_url, region, None)
                .await
                .map_err(AuthError::OAuth)?;
        let flow = if start_url.contains("view.awsapps.com") || start_url.contains("amzn.awsapps.com") {
            AuthFlow::BuilderId
        } else {
            AuthFlow::Idc {
                region: region.to_owned(),
                start_url: start_url.to_owned(),
            }
        };
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
                auth_flow: Some(flow),
                oidc_client: Some(client),
                ..AccountMetadata::default()
            },
        };
        self.mutate(|store| upsert_account(store, record.clone()))?;
        let store = self.repo.load()?;
        store.accounts.iter().find(|a| a.key == record.key)
            .map(AccountView::from_record)
            .ok_or(AuthError::ReadBackFailed)
    }

    fn save_login_result(
        &self,
        login: crate::auth::oauth::LoginResult,
        flow: AuthFlow,
    ) -> Result<AccountView, AuthError> {
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
                auth_flow: Some(flow),
                ..AccountMetadata::default()
            },
        };
        self.mutate(|store| upsert_account(store, record.clone()))?;
        let store = self.repo.load()?;
        store.accounts.iter().find(|a| a.key == record.key)
            .map(AccountView::from_record)
            .ok_or(AuthError::ReadBackFailed)
    }

    pub fn save_api_key(&self, vendor: AuthVendor, token: &str) -> Result<AccountView, AuthError> {
        let fingerprint = api_key_fingerprint(token);
        let display_name = format!("{}:key:{}", vendor.as_str(), fingerprint);
        let record = AccountRecord {
            key: AccountKey {
                vendor,
                subject: AccountSubject::ApiKeyFingerprint(fingerprint),
            },
            display_name,
            email: None,
            auth: AuthState::ApiKey(ApiKeyCredential {
                token: token.to_owned(),
            }),
            health: AccountHealth::Active,
            metadata: AccountMetadata::default(),
        };

        self.mutate(|store| upsert_account(store, record.clone()))?;
        let store = self.repo.load()?;
        store
            .accounts
            .iter()
            .find(|a| a.key == record.key)
            .map(AccountView::from_record)
            .ok_or(AuthError::ReadBackFailed)
    }

    pub fn record_usage_by_display_name(
        &self,
        display_name: &str,
        usage: crate::config::auth::UsageSnapshot,
    ) -> Result<(), AuthError> {
        self.mutate(|store| {
            if let Some(account) = store
                .accounts
                .iter_mut()
                .find(|a| a.display_name == display_name)
            {
                account.metadata.usage = crate::auth::domain::UsageSnapshot {
                    requests_remaining: usage.requests_remaining,
                    requests_limit: usage.requests_limit,
                    tokens_remaining: usage.tokens_remaining,
                    tokens_limit: usage.tokens_limit,
                    reset_at: usage.reset_at,
                    updated_at: if usage.updated_at == 0 {
                        now_unix()
                    } else {
                        usage.updated_at
                    },
                };
            }
        })
    }

    pub async fn refresh_credential(
        &self,
        key: &AccountKey,
    ) -> Result<crate::config::auth::Credential, AuthError> {
        let store = self.repo.load()?;
        let record = store
            .accounts
            .iter()
            .find(|a| &a.key == key)
            .ok_or(AuthError::AccountNotFound)?;
        let refresh_token = match &record.auth {
            AuthState::OAuth(cred) => cred.refresh_token.as_deref().ok_or_else(|| {
                AuthError::OAuth(crate::auth::error::OAuthError::RefreshRejected(
                    "missing refresh token".to_owned(),
                ))
            })?,
            AuthState::ApiKey(_) => {
                return Err(AuthError::OAuth(
                    crate::auth::error::OAuthError::RefreshRejected(
                        "api key cannot be refreshed".to_owned(),
                    ),
                ));
            }
        };

        let oauth = crate::auth::oauth::OAuthRegistry::new();
        let provider = oauth
            .get(record.key.vendor)
            .ok_or(AuthError::NoEligibleAccount {
                vendor: record.key.vendor.as_str().to_owned(),
            })?;

        // Route refresh by auth flow. Infer from scopes for pre-refactor accounts.
        let effective_flow = record.metadata.auth_flow.clone().or_else(|| {
            match &record.auth {
                AuthState::OAuth(cred)
                    if cred.scopes.iter().any(|s| s.starts_with("codewhisperer:") || s == "sso:account:access") =>
                {
                    Some(AuthFlow::Idc {
                        region: "us-east-1".to_owned(),
                        start_url: String::new(),
                    })
                }
                _ => None,
            }
        });
        let (tokens, updated_client) = match &effective_flow {
            Some(AuthFlow::Idc { region, start_url }) => {
                let (tok, client) = crate::auth::oauth::sso_oidc::refresh(
                    refresh_token,
                    region,
                    record.metadata.oidc_client.as_ref(),
                    start_url,
                )
                .await
                .map_err(AuthError::OAuth)?;
                (tok, Some(client))
            }
            Some(AuthFlow::BuilderId) => {
                let (tok, client) = crate::auth::oauth::sso_oidc::refresh(
                    refresh_token,
                    "us-east-1",
                    record.metadata.oidc_client.as_ref(),
                    "https://view.awsapps.com/start",
                )
                .await
                .map_err(AuthError::OAuth)?;
                (tok, Some(client))
            }
            _ => {
                let tok = provider
                    .refresh(refresh_token)
                    .await
                    .map_err(AuthError::OAuth)?;
                (tok, None)
            }
        };

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
                profile_arn: tokens
                    .profile_arn
                    .clone()
                    .or_else(|| record.metadata.profile_arn.clone()),
                oidc_client: updated_client.or_else(|| record.metadata.oidc_client.clone()),
                auth_flow: effective_flow.or_else(|| record.metadata.auth_flow.clone()),
                ..record.metadata.clone()
            },
        };

        self.mutate(|store| upsert_account(store, refreshed.clone()))?;
        Ok(crate::config::auth::Credential {
            token: tokens.access_token,
            is_oauth: true,
            account_id: match &refreshed.key.subject {
                crate::auth::domain::AccountSubject::AccountId(id) => Some(id.clone()),
                _ => None,
            },
            label: refreshed.display_name,
            profile_arn: refreshed.metadata.profile_arn,
            account_key: Some(refreshed.key),
        })
    }

    pub async fn resolve_credential(
        &self,
        vendor: AuthVendor,
    ) -> Result<crate::config::auth::Credential, AuthError> {
        let store = self.repo.load()?;
        let candidates: Vec<_> = store
            .accounts
            .iter()
            .filter(|a| a.key.vendor == vendor)
            .cloned()
            .collect();
        let key = self
            .selection
            .select(&candidates)
            .ok_or(AuthError::NoEligibleAccount {
                vendor: vendor.as_str().to_owned(),
            })?;
        let record = candidates
            .into_iter()
            .find(|a| a.key == key)
            .ok_or(AuthError::AccountNotFound)?;

        if let AuthState::OAuth(cred) = &record.auth
            && is_expired(cred.expires_at)
            && cred.refresh_token.is_some()
        {
            match self.refresh_credential(&record.key).await {
                Ok(cred) => return Ok(cred),
                Err(AuthError::OAuth(ref e))
                    if e.to_string().contains("invalid_grant")
                        || e.to_string().contains("expired")
                        || e.to_string().contains("Bad credentials") =>
                {
                    let _ = self.mark_auth_failed(
                        &record.key,
                        AuthFailure::RefreshRejected,
                    );
                    return Err(AuthError::NoEligibleAccount {
                        vendor: format!(
                            "{} (token expired, run `luma login` to re-authenticate)",
                            vendor.as_str()
                        ),
                    });
                }
                Err(e) => return Err(e),
            }
        }

        Ok(credential_from_record(record))
    }

    // -------------------------------------------------------------------------

    fn mutate(&self, f: impl FnOnce(&mut AuthStore)) -> Result<(), AuthError> {
        let mut store = self.repo.load()?;
        f(&mut store);
        self.repo.save(&store).map_err(AuthError::Store)
    }
}

fn find_mut<'a>(
    store: &'a mut AuthStore,
    key: &AccountKey,
) -> Option<&'a mut crate::auth::domain::AccountRecord> {
    store.accounts.iter_mut().find(|a| &a.key == key)
}

fn upsert_account(store: &mut AuthStore, record: AccountRecord) {
    if let Some(existing) = store.accounts.iter_mut().find(|a| a.key == record.key) {
        *existing = record;
    } else {
        store.accounts.push(record);
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_expired(expires_at: Option<u64>) -> bool {
    const EXPIRY_GRACE_SECS: u64 = 300;
    let Some(ts) = expires_at else {
        return false;
    };
    now_unix() >= ts.saturating_sub(EXPIRY_GRACE_SECS)
}

fn credential_from_record(record: AccountRecord) -> crate::config::auth::Credential {
    let (token, is_oauth, account_id) = match record.auth {
        AuthState::OAuth(cred) => (
            cred.access_token,
            true,
            match &record.key.subject {
                crate::auth::domain::AccountSubject::AccountId(id) => Some(id.clone()),
                _ => None,
            },
        ),
        AuthState::ApiKey(cred) => (cred.token, false, None),
    };
    crate::config::auth::Credential {
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
    let mut hasher = Sha256::new();
    hasher.update(core.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    digest[..12.min(digest.len())].to_owned()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::domain::{
        AccountKey, AccountMetadata, AccountRecord, AuthState, AuthVendor, OAuthCredential,
    };
    use crate::auth::error::AuthStoreError;
    use crate::auth::repo::AuthStore;
    use crate::auth::repo::STORE_VERSION;
    use std::cell::RefCell;

    // In-memory repo for tests.
    struct MemRepo(RefCell<AuthStore>);

    impl MemRepo {
        fn new(accounts: Vec<AccountRecord>) -> Self {
            Self(RefCell::new(AuthStore {
                version: STORE_VERSION,
                accounts,
            }))
        }
    }

    impl AuthRepository for MemRepo {
        fn load(&self) -> Result<AuthStore, AuthStoreError> {
            Ok(self.0.borrow().clone())
        }
        fn save(&self, store: &AuthStore) -> Result<(), AuthStoreError> {
            *self.0.borrow_mut() = store.clone();
            Ok(())
        }
    }

    fn active_record(vendor: AuthVendor, id: &str) -> AccountRecord {
        AccountRecord {
            key: AccountKey::account_id(vendor, id),
            display_name: id.into(),
            email: None,
            auth: AuthState::OAuth(OAuthCredential {
                access_token: "t".into(),
                refresh_token: None,
                expires_at: None,
                scopes: vec![],
            }),
            health: AccountHealth::Active,
            metadata: AccountMetadata::default(),
        }
    }

    #[test]
    fn list_accounts_returns_views() {
        let svc = AuthService::new(MemRepo::new(vec![
            active_record(AuthVendor::Anthropic, "acc1"),
            active_record(AuthVendor::OpenAI, "acc2"),
        ]));
        let views = svc.list_accounts().unwrap();
        assert_eq!(views.len(), 2);
    }

    #[test]
    fn mark_rate_limited_sets_cooling_down() {
        let key = AccountKey::account_id(AuthVendor::Anthropic, "acc1");
        let svc = AuthService::new(MemRepo::new(vec![active_record(
            AuthVendor::Anthropic,
            "acc1",
        )]));
        svc.mark_rate_limited(&key, 60).unwrap();
        let views = svc.list_accounts().unwrap();
        assert!(matches!(views[0].health, AccountHealth::CoolingDown { .. }));
    }

    #[test]
    fn mark_auth_failed_sets_needs_relogin() {
        let key = AccountKey::account_id(AuthVendor::Anthropic, "acc1");
        let svc = AuthService::new(MemRepo::new(vec![active_record(
            AuthVendor::Anthropic,
            "acc1",
        )]));
        svc.mark_auth_failed(&key, AuthFailure::RefreshRejected)
            .unwrap();
        let views = svc.list_accounts().unwrap();
        assert!(matches!(
            views[0].health,
            AccountHealth::NeedsRelogin { .. }
        ));
    }

    #[test]
    fn toggle_disabled_roundtrip() {
        let key = AccountKey::account_id(AuthVendor::Anthropic, "acc1");
        let svc = AuthService::new(MemRepo::new(vec![active_record(
            AuthVendor::Anthropic,
            "acc1",
        )]));
        svc.toggle_disabled(&key).unwrap();
        assert!(matches!(
            svc.list_accounts().unwrap()[0].health,
            AccountHealth::Disabled
        ));
        svc.toggle_disabled(&key).unwrap();
        assert!(matches!(
            svc.list_accounts().unwrap()[0].health,
            AccountHealth::Active
        ));
    }

    #[test]
    fn remove_account_deletes_entry() {
        let key = AccountKey::account_id(AuthVendor::Anthropic, "acc1");
        let svc = AuthService::new(MemRepo::new(vec![active_record(
            AuthVendor::Anthropic,
            "acc1",
        )]));
        svc.remove_account(&key).unwrap();
        assert!(svc.list_accounts().unwrap().is_empty());
    }

    #[test]
    fn list_accounts_empty_store() {
        let svc = AuthService::new(MemRepo::new(vec![]));
        assert!(svc.list_accounts().unwrap().is_empty());
    }

    #[test]
    fn mark_rate_limited_unknown_key_is_noop() {
        let svc = AuthService::new(MemRepo::new(vec![active_record(
            AuthVendor::Anthropic,
            "acc1",
        )]));
        let unknown = AccountKey::account_id(AuthVendor::OpenAI, "nope");
        svc.mark_rate_limited(&unknown, 60).unwrap(); // must not error
        assert!(matches!(
            svc.list_accounts().unwrap()[0].health,
            AccountHealth::Active
        ));
    }
}
