//! Wire protocol for authd — JSON lines over Unix socket.
//!
//! Messages with `id` are request/response pairs.
//! Messages without `id` are server-push events (broadcast to all clients).

use serde::{Deserialize, Serialize};

use crate::auth::domain::{AccountHealth, AccountKey};
use crate::config::auth::UsageSnapshot;

// =============================================================================
// Client → Daemon
// =============================================================================

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub body: RequestBody,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
#[serde(rename_all = "snake_case")]
pub enum RequestBody {
    Resolve {
        vendor: String,
    },
    Refresh {
        account_key: AccountKey,
    },
    Login {
        vendor: String,
    },
    SaveApiKey {
        vendor: String,
        token: String,
    },
    MarkRateLimited {
        account_key: AccountKey,
        retry_after_secs: u64,
    },
    MarkAuthFailed {
        account_key: AccountKey,
        failure: String,
    },
    ListAccounts,
    ToggleDisabled {
        account_key: AccountKey,
    },
    RemoveAccount {
        account_key: AccountKey,
    },
    RecordUsage {
        label: String,
        usage: WireUsage,
    },
    Ping,
    Shutdown,
}

// =============================================================================
// Daemon → Client (response)
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(flatten)]
    pub body: ResponseBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseBody {
    Ok { result: ResponseResult },
    Err { error: RpcError },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseResult {
    Credential(WireCredential),
    Accounts(Vec<WireAccountView>),
    Account(WireAccountView),
    Pong,
    Ok,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

// =============================================================================
// Daemon → Client (push event, no id)
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
#[serde(rename_all = "snake_case")]
pub enum AuthEvent {
    AccountUpdated {
        key: AccountKey,
        health: AccountHealth,
    },
    AccountRemoved {
        key: AccountKey,
    },
    TokenRefreshed {
        key: AccountKey,
        label: String,
    },
    AccountAdded {
        view: WireAccountView,
    },
    PoolChanged {
        vendor: String,
        active: usize,
        cooling: usize,
        total: usize,
    },
}

// =============================================================================
// Wire types
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireCredential {
    pub token: String,
    pub is_oauth: bool,
    pub account_id: Option<String>,
    pub label: String,
    pub profile_arn: Option<String>,
    pub account_key: Option<AccountKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireAccountView {
    pub key: AccountKey,
    pub display_name: String,
    pub email: Option<String>,
    pub vendor: String,
    pub health: AccountHealth,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WireUsage {
    pub requests_remaining: Option<u64>,
    pub requests_limit: Option<u64>,
    pub tokens_remaining: Option<u64>,
    pub tokens_limit: Option<u64>,
    pub reset_at: Option<u64>,
    pub updated_at: u64,
}

// =============================================================================
// Conversions
// =============================================================================

impl WireCredential {
    pub fn to_credential(&self) -> crate::config::auth::Credential {
        crate::config::auth::Credential {
            token: self.token.clone(),
            is_oauth: self.is_oauth,
            account_id: self.account_id.clone(),
            label: self.label.clone(),
            profile_arn: self.profile_arn.clone(),
            account_key: self.account_key.clone(),
        }
    }

    pub fn from_credential(c: &crate::config::auth::Credential) -> Self {
        Self {
            token: c.token.clone(),
            is_oauth: c.is_oauth,
            account_id: c.account_id.clone(),
            label: c.label.clone(),
            profile_arn: c.profile_arn.clone(),
            account_key: c.account_key.clone(),
        }
    }
}

impl WireUsage {
    pub fn to_usage(&self) -> UsageSnapshot {
        UsageSnapshot {
            requests_remaining: self.requests_remaining,
            requests_limit: self.requests_limit,
            tokens_remaining: self.tokens_remaining,
            tokens_limit: self.tokens_limit,
            reset_at: self.reset_at,
            updated_at: self.updated_at,
        }
    }

    pub fn from_usage(u: &UsageSnapshot) -> Self {
        Self {
            requests_remaining: u.requests_remaining,
            requests_limit: u.requests_limit,
            tokens_remaining: u.tokens_remaining,
            tokens_limit: u.tokens_limit,
            reset_at: u.reset_at,
            updated_at: u.updated_at,
        }
    }
}

impl From<&crate::auth::domain::AccountView> for WireAccountView {
    fn from(v: &crate::auth::domain::AccountView) -> Self {
        Self {
            key: v.key.clone(),
            display_name: v.display_name.clone(),
            email: v.email.clone(),
            vendor: v.vendor.as_str().to_owned(),
            health: v.health.clone(),
        }
    }
}

// =============================================================================
// Server message — wraps both Response and push Event for the write loop
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Response(Response),
    Event(AuthEvent),
}

impl ResponseBody {
    pub fn ok(result: ResponseResult) -> Self {
        Self::Ok { result }
    }

    pub fn err(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Err {
            error: RpcError {
                code: code.into(),
                message: message.into(),
            },
        }
    }
}
