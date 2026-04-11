use super::{
    AuthProvider, CLAUDE_CLIENT_ID, CLAUDE_OAUTH_ENDPOINT, CLAUDE_REFRESH_SCOPES, OPENAI_CLIENT_ID,
    OPENAI_OAUTH_ENDPOINT, should_use_claude_ai_auth,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailureKind {
    Expired,
    Invalid,
    Unauthorized,
    Forbidden,
}

pub struct RefreshRequest {
    pub url: &'static str,
    pub body: String,
    pub content_type: &'static str,
}

impl AuthProvider {
    pub fn classify_auth_failure(self, status_code: u16, detail: &str) -> Option<AuthFailureKind> {
        if status_code != 401 && status_code != 403 {
            return None;
        }
        let lower = detail.to_ascii_lowercase();
        if lower.contains("expired") || lower.contains("token_expired") {
            return Some(AuthFailureKind::Expired);
        }
        if lower.contains("invalid") || lower.contains("revoked") || lower.contains("reused") {
            return Some(AuthFailureKind::Invalid);
        }

        match (self, status_code) {
            (_, 403) => Some(AuthFailureKind::Forbidden),
            (Self::Anthropic, _) => {
                if lower.contains("authentication")
                    || lower.contains("api key")
                    || lower.contains("oauth")
                    || lower.contains("token")
                    || lower.contains("unauthorized")
                    || lower.contains("forbidden")
                {
                    Some(AuthFailureKind::Unauthorized)
                } else {
                    None
                }
            }
            (Self::OpenAI, _) => {
                if lower.contains("auth")
                    || lower.contains("token")
                    || lower.contains("unauthorized")
                    || lower.contains("forbidden")
                {
                    Some(AuthFailureKind::Unauthorized)
                } else {
                    None
                }
            }
        }
    }

    pub fn build_refresh_request(
        self,
        refresh_token: &str,
        scopes: Option<&[String]>,
    ) -> RefreshRequest {
        match self {
            Self::Anthropic => {
                let mut body = serde_json::json!({
                    "grant_type": "refresh_token",
                    "refresh_token": refresh_token,
                    "client_id": CLAUDE_CLIENT_ID,
                });
                if should_use_claude_ai_auth(scopes) {
                    // Claude.ai subscriber refresh omits scope; backend applies current defaults.
                } else if let Some(scopes) = scopes.filter(|s| !s.is_empty()) {
                    body["scope"] = serde_json::Value::String(scopes.join(" "));
                } else {
                    body["scope"] = serde_json::Value::String(CLAUDE_REFRESH_SCOPES.join(" "));
                }
                RefreshRequest {
                    url: CLAUDE_OAUTH_ENDPOINT,
                    body: body.to_string(),
                    content_type: "application/json",
                }
            }
            Self::OpenAI => {
                // Upstream codex-rs refresh body is JSON with exactly these
                // three fields — no scope echo. Matching byte-for-byte keeps
                // luma classified as a first-party client during refresh.
                // See `codex-rs/login/src/auth/manager.rs::RefreshRequest`.
                let body = serde_json::json!({
                    "client_id": OPENAI_CLIENT_ID,
                    "grant_type": "refresh_token",
                    "refresh_token": refresh_token,
                });
                RefreshRequest {
                    url: OPENAI_OAUTH_ENDPOINT,
                    body: body.to_string(),
                    content_type: "application/json",
                }
            }
        }
    }
}
