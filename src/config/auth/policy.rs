use super::{
    AuthVendor, CLAUDE_CLIENT_ID, CLAUDE_OAUTH_ENDPOINT, CLAUDE_REFRESH_SCOPES, OPENAI_CLIENT_ID,
    OPENAI_OAUTH_ENDPOINT, should_use_claude_ai_auth,
};

pub struct RefreshRequest {
    pub url: &'static str,
    pub body: String,
    pub content_type: &'static str,
}

impl AuthVendor {
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
                // Matches `codex-rs/login/src/auth/manager.rs::RefreshRequest` —
                // JSON body with exactly these three fields (no scope echo).
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
            Self::OpenCodeGo => {
                // OpenCode Go uses long-lived API keys — no refresh flow.
                // Callers MUST gate build_refresh_request on is_oauth.
                unreachable!("opencode-go does not use OAuth refresh")
            }
        }
    }
}
