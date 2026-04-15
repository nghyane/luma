//! Account selection policy — picks the best eligible account from a slice.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::domain::{AccountHealth, AccountKey, AccountRecord, AuthState};

pub trait AccountSelectionPolicy: Send + Sync {
    fn select(&self, accounts: &[AccountRecord]) -> Option<AccountKey>;
}

pub struct DefaultSelectionPolicy;

impl AccountSelectionPolicy for DefaultSelectionPolicy {
    fn select(&self, accounts: &[AccountRecord]) -> Option<AccountKey> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        accounts
            .iter()
            .filter(|a| is_eligible(a, now))
            .max_by_key(|a| rank(a))
            .map(|a| a.key.clone())
    }
}

fn is_eligible(a: &AccountRecord, now: u64) -> bool {
    match &a.health {
        AccountHealth::Active => true,
        AccountHealth::CoolingDown { until_unix } => *until_unix <= now,
        AccountHealth::NeedsRelogin { .. } | AccountHealth::Disabled => false,
    }
}

fn rank(a: &AccountRecord) -> (u8, u8, u8) {
    let is_oauth = u8::from(matches!(a.auth, AuthState::OAuth(_)));
    let has_email = u8::from(a.email.is_some());
    let has_refresh = u8::from(matches!(&a.auth, AuthState::OAuth(c) if c.refresh_token.is_some()));
    (is_oauth, has_email, has_refresh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::domain::{
        AccountKey, AccountMetadata, AccountRecord, AuthState, AuthVendor, OAuthCredential,
    };

    fn make(vendor: AuthVendor, health: AccountHealth, has_refresh: bool) -> AccountRecord {
        AccountRecord {
            key: AccountKey::anonymous(vendor, "x"),
            display_name: "x".into(),
            email: None,
            auth: AuthState::OAuth(OAuthCredential {
                access_token: "t".into(),
                refresh_token: if has_refresh { Some("r".into()) } else { None },
                expires_at: None,
                scopes: vec![],
            }),
            health,
            metadata: AccountMetadata::default(),
        }
    }

    #[test]
    fn selects_active_over_cooling() {
        let accounts = vec![
            make(
                AuthVendor::Anthropic,
                AccountHealth::CoolingDown {
                    until_unix: u64::MAX,
                },
                false,
            ),
            make(AuthVendor::Anthropic, AccountHealth::Active, false),
        ];
        let key = DefaultSelectionPolicy.select(&accounts).unwrap();
        assert_eq!(key, accounts[1].key);
    }

    #[test]
    fn excludes_needs_relogin_and_disabled() {
        use crate::auth::domain::ReloginReason;
        let accounts = vec![
            make(
                AuthVendor::Anthropic,
                AccountHealth::NeedsRelogin {
                    reason: ReloginReason::RefreshFailed,
                },
                false,
            ),
            make(AuthVendor::Anthropic, AccountHealth::Disabled, false),
        ];
        assert!(DefaultSelectionPolicy.select(&accounts).is_none());
    }

    #[test]
    fn prefers_account_with_refresh_token() {
        let accounts = vec![
            make(AuthVendor::Anthropic, AccountHealth::Active, false),
            make(AuthVendor::Anthropic, AccountHealth::Active, true),
        ];
        let key = DefaultSelectionPolicy.select(&accounts).unwrap();
        assert_eq!(key, accounts[1].key);
    }
}
