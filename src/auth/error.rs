//! Typed error hierarchy for the auth subsystem.

use thiserror::Error;

// =============================================================================
// Store / persistence errors
// =============================================================================

#[derive(Debug, Error)]
pub enum AuthStoreError {
    #[error("I/O error reading auth store: {0}")]
    Io(#[from] std::io::Error),

    #[error("auth store is malformed: {0}")]
    Malformed(#[from] serde_json::Error),

    #[error("atomic write failed: temp file could not be renamed")]
    AtomicWriteFailed,
}

// =============================================================================
// Import errors
// =============================================================================

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum AuthImportError {
    #[error("local credential source not found for {vendor}")]
    SourceNotFound { vendor: String },

    #[error("failed to parse local credentials: {0}")]
    ParseError(String),

    #[error("keychain access failed: {0}")]
    KeychainError(String),
}

// =============================================================================
// OAuth errors
// =============================================================================

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum OAuthError {
    #[error("network error: {0}")]
    Network(String),

    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },

    #[error("token exchange failed: {0}")]
    ExchangeFailed(String),

    #[error("refresh rejected: {0}")]
    RefreshRejected(String),

    #[error("identity resolution failed: {0}")]
    IdentityFailed(String),

    #[error("login timed out")]
    Timeout,

    #[error("login cancelled")]
    Cancelled,
}

// =============================================================================
// Service-level errors
// =============================================================================

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("store error: {0}")]
    Store(#[from] AuthStoreError),

    #[error("OAuth error: {0}")]
    OAuth(#[from] OAuthError),

    #[error("import error: {0}")]
    Import(#[from] AuthImportError),

    #[error("account not found")]
    AccountNotFound,

    #[error("no eligible account for vendor {vendor}")]
    NoEligibleAccount { vendor: String },

    #[error("save succeeded but read-back verification failed")]
    ReadBackFailed,
}

// =============================================================================
// Provider runtime errors
// =============================================================================

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum ProviderError {
    #[error("transport error: {0}")]
    Transport(String),

    #[error("auth failure: {0:?}")]
    Auth(crate::auth::domain::AuthFailure),

    #[error("rate limited: retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("remote error {status}: {message}")]
    Remote { status: u16, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::domain::AuthFailure;

    #[test]
    fn construct_import_errors() {
        let _ = AuthImportError::SourceNotFound {
            vendor: "openai".to_owned(),
        };
        let _ = AuthImportError::ParseError("bad json".to_owned());
        let _ = AuthImportError::KeychainError("denied".to_owned());
    }

    #[test]
    fn construct_oauth_errors() {
        let _ = OAuthError::Network("offline".to_owned());
        let _ = OAuthError::Http {
            status: 401,
            body: "unauthorized".to_owned(),
        };
        let _ = OAuthError::Cancelled;
    }

    #[test]
    fn construct_provider_error() {
        let _ = ProviderError::Auth(AuthFailure::Unauthorized);
        let _ = ProviderError::Transport("reset".to_owned());
    }
}
