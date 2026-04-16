use serde::{Deserialize, Serialize};

// ── JSON-RPC 2.0 base ──────────────────────────────────────────────

#[derive(Deserialize)]
pub struct Request {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

#[derive(Serialize)]
pub struct Notification {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    pub params: serde_json::Value,
}

// ── ACP method params ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SessionNewParams {
    pub cwd: String,
}

#[derive(Deserialize)]
pub struct SessionPromptParams {
    #[serde(rename = "sessionId")]
    #[allow(dead_code)]
    pub session_id: String,
    pub prompt: Vec<PromptContent>,
}

#[derive(Deserialize)]
pub struct SessionLoadParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub enum PromptContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(other)]
    Other,
}
