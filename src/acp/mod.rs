/// ACP (Agent Client Protocol) server mode.
///
/// When launched with `luma --acp`, Luma speaks JSON-RPC 2.0 over
/// stdin/stdout instead of running the TUI. This makes Luma compatible
/// with any ACP client (Paseo, Zed, etc.).
pub mod bridge;
mod transport;
mod types;
