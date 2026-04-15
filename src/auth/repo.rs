//! `AuthRepository` — SQLite persistence with `BEGIN IMMEDIATE` for cross-process safety.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::auth::domain::{AccountKey, AccountRecord};
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
// SQLite-backed implementation
// =============================================================================

pub struct SqliteAuthRepository {
    path: PathBuf,
}

impl SqliteAuthRepository {
    pub fn new(path: PathBuf) -> Self {
        let repo = Self { path };
        if let Ok(conn) = repo.connect() {
            let _ = conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS accounts (
                    key TEXT PRIMARY KEY,
                    data TEXT NOT NULL
                );",
            );
        }
        repo
    }

    pub fn default_path() -> PathBuf {
        crate::config::home_dir()
            .join(".config")
            .join("luma")
            .join("auth.db")
    }

    pub fn with_default_path() -> Self {
        Self::new(Self::default_path())
    }

    fn connect(&self) -> Result<rusqlite::Connection, AuthStoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(&self.path)
            .map_err(|e| AuthStoreError::Sqlite(e.to_string()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| AuthStoreError::Sqlite(e.to_string()))?;
        Ok(conn)
    }

    fn account_key_str(key: &AccountKey) -> String {
        serde_json::to_string(key).unwrap_or_default()
    }

    /// Upsert accounts — imported accounts merge with existing, not replace.
    pub fn merge(&self, incoming: &[AccountRecord]) -> Result<(), AuthStoreError> {
        let conn = self.connect()?;
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| AuthStoreError::Sqlite(e.to_string()))?;
        for account in incoming {
            let key = Self::account_key_str(&account.key);
            let data = serde_json::to_string(account)?;
            conn.execute(
                "INSERT OR REPLACE INTO accounts (key, data) VALUES (?1, ?2)",
                rusqlite::params![key, data],
            )
            .map_err(|e| AuthStoreError::Sqlite(e.to_string()))?;
        }
        conn.execute_batch("COMMIT")
            .map_err(|e| AuthStoreError::Sqlite(e.to_string()))?;
        Ok(())
    }
}

impl AuthRepository for SqliteAuthRepository {
    fn load(&self) -> Result<AuthStore, AuthStoreError> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare("SELECT data FROM accounts")
            .map_err(|e| AuthStoreError::Sqlite(e.to_string()))?;
        let accounts: Vec<AccountRecord> = stmt
            .query_map([], |row| {
                let json: String = row.get(0)?;
                Ok(json)
            })
            .map_err(|e| AuthStoreError::Sqlite(e.to_string()))?
            .filter_map(|r| r.ok())
            .filter_map(|json| serde_json::from_str(&json).ok())
            .collect();
        Ok(AuthStore {
            version: STORE_VERSION,
            accounts,
        })
    }

    fn save(&self, store: &AuthStore) -> Result<(), AuthStoreError> {
        let conn = self.connect()?;
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| AuthStoreError::Sqlite(e.to_string()))?;
        conn.execute("DELETE FROM accounts", [])
            .map_err(|e| AuthStoreError::Sqlite(e.to_string()))?;
        for account in &store.accounts {
            let key = Self::account_key_str(&account.key);
            let data = serde_json::to_string(account)?;
            conn.execute(
                "INSERT INTO accounts (key, data) VALUES (?1, ?2)",
                rusqlite::params![key, data],
            )
            .map_err(|e| AuthStoreError::Sqlite(e.to_string()))?;
        }
        conn.execute_batch("COMMIT")
            .map_err(|e| AuthStoreError::Sqlite(e.to_string()))?;
        Ok(())
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::domain::*;
    use tempfile::TempDir;

    fn make_repo(dir: &TempDir) -> SqliteAuthRepository {
        SqliteAuthRepository::new(dir.path().join("auth.db"))
    }

    #[test]
    fn load_empty_returns_empty_store() {
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
    fn upsert_replaces_existing() {
        let dir = TempDir::new().unwrap();
        let repo = make_repo(&dir);
        let key = AccountKey::email(AuthVendor::Anthropic, "me@example.com");
        let mk = |token: &str| AuthStore {
            version: STORE_VERSION,
            accounts: vec![AccountRecord {
                key: key.clone(),
                display_name: "me".into(),
                email: None,
                auth: AuthState::ApiKey(ApiKeyCredential { token: token.into() }),
                health: AccountHealth::Active,
                metadata: AccountMetadata::default(),
            }],
        };
        repo.save(&mk("old")).unwrap();
        repo.save(&mk("new")).unwrap();
        let loaded = repo.load().unwrap();
        assert_eq!(loaded.accounts.len(), 1);
        let AuthState::ApiKey(cred) = &loaded.accounts[0].auth else { panic!() };
        assert_eq!(cred.token, "new");
    }
}
