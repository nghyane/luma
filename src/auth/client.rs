//! Auth client — connects to authd over Unix socket.
//!
//! All auth operations go through this client. If the daemon is not
//! running, `connect()` spawns it automatically.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, broadcast, oneshot};

use crate::authd::protocol::*;
use crate::config::auth::{Credential, UsageSnapshot};

pub struct AuthClient {
    writer: Mutex<tokio::net::unix::OwnedWriteHalf>,
    next_id: AtomicU64,
    pending: Arc<dashmap::DashMap<u64, oneshot::Sender<ResponseBody>>>,
    #[allow(dead_code)]
    event_tx: broadcast::Sender<AuthEvent>,
    #[allow(dead_code)]
    _reader_task: tokio::task::JoinHandle<()>,
}

impl AuthClient {
    /// Connect to the daemon, spawning it if needed.
    pub async fn connect() -> Result<Self> {
        let sock = crate::authd::sock_path();
        let stream = match UnixStream::connect(&sock).await {
            Ok(s) => s,
            Err(_) => {
                spawn_daemon()?;
                let mut last_err = None;
                for _ in 0..40 {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    match UnixStream::connect(&sock).await {
                        Ok(s) => return Self::from_stream(s),
                        Err(e) => last_err = Some(e),
                    }
                }
                bail!("authd did not start within 2s: {}", last_err.map(|e| e.to_string()).unwrap_or_default());
            }
        };
        Self::from_stream(stream)
    }

    fn from_stream(stream: UnixStream) -> Result<Self> {
        let (reader, writer) = stream.into_split();
        let pending: Arc<dashmap::DashMap<u64, oneshot::Sender<ResponseBody>>> =
            Arc::new(dashmap::DashMap::new());
        let (event_tx, _) = broadcast::channel(128);

        let p = pending.clone();
        let etx = event_tx.clone();
        let reader_task = tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(msg) = serde_json::from_str::<Response>(&line) {
                    if let Some((_, tx)) = p.remove(&msg.id) {
                        let _ = tx.send(msg.body);
                    }
                    continue;
                }
                if let Ok(ev) = serde_json::from_str::<AuthEvent>(&line) {
                    let _ = etx.send(ev);
                }
            }
        });

        Ok(Self {
            writer: Mutex::new(writer),
            next_id: AtomicU64::new(1),
            pending,
            event_tx,
            _reader_task: reader_task,
        })
    }

    /// Subscribe to realtime push events from the daemon.
    /// Used by TUI event loop for cross-process state updates.
    #[allow(dead_code)]
    pub fn subscribe(&self) -> broadcast::Receiver<AuthEvent> {
        self.event_tx.subscribe()
    }

    // =========================================================================
    // RPC methods
    // =========================================================================

    pub async fn resolve(&self, vendor: crate::config::auth::AuthVendor) -> Result<Credential> {
        match self.call(RequestBody::Resolve { vendor: vendor.as_str().to_owned() }).await? {
            ResponseResult::Credential(c) => Ok(c.to_credential()),
            _ => bail!("unexpected response"),
        }
    }

    pub async fn refresh(&self, key: &crate::auth::domain::AccountKey) -> Result<Credential> {
        match self.call(RequestBody::Refresh { account_key: key.clone() }).await? {
            ResponseResult::Credential(c) => Ok(c.to_credential()),
            _ => bail!("unexpected response"),
        }
    }

    pub async fn login(&self, vendor: &str) -> Result<WireAccountView> {
        match self.call(RequestBody::Login { vendor: vendor.to_owned() }).await? {
            ResponseResult::Account(v) => Ok(v),
            _ => bail!("unexpected response"),
        }
    }

    pub async fn save_api_key(&self, vendor: &str, token: &str) -> Result<WireAccountView> {
        match self.call(RequestBody::SaveApiKey { vendor: vendor.to_owned(), token: token.to_owned() }).await? {
            ResponseResult::Account(v) => Ok(v),
            _ => bail!("unexpected response"),
        }
    }

    pub async fn mark_rate_limited(&self, key: &crate::auth::domain::AccountKey, retry_after_secs: u64) -> Result<()> {
        self.call(RequestBody::MarkRateLimited { account_key: key.clone(), retry_after_secs }).await?;
        Ok(())
    }

    pub async fn mark_auth_failed(&self, key: &crate::auth::domain::AccountKey, failure: &str) -> Result<()> {
        self.call(RequestBody::MarkAuthFailed { account_key: key.clone(), failure: failure.to_owned() }).await?;
        Ok(())
    }

    pub async fn list_accounts(&self) -> Result<Vec<WireAccountView>> {
        match self.call(RequestBody::ListAccounts).await? {
            ResponseResult::Accounts(v) => Ok(v),
            _ => bail!("unexpected response"),
        }
    }

    pub async fn toggle_disabled(&self, key: &crate::auth::domain::AccountKey) -> Result<()> {
        self.call(RequestBody::ToggleDisabled { account_key: key.clone() }).await?;
        Ok(())
    }

    pub async fn remove_account(&self, key: &crate::auth::domain::AccountKey) -> Result<()> {
        self.call(RequestBody::RemoveAccount { account_key: key.clone() }).await?;
        Ok(())
    }

    pub async fn record_usage(&self, label: &str, usage: &UsageSnapshot) -> Result<()> {
        self.call(RequestBody::RecordUsage { label: label.to_owned(), usage: WireUsage::from_usage(usage) }).await?;
        Ok(())
    }

    // =========================================================================
    // Internal
    // =========================================================================

    async fn call(&self, body: RequestBody) -> Result<ResponseResult> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request { id, body };
        let json = serde_json::to_string(&req)?;

        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, tx);

        {
            let mut w = self.writer.lock().await;
            w.write_all(json.as_bytes()).await?;
            w.write_all(b"\n").await?;
            w.flush().await?;
        }

        let resp = tokio::time::timeout(std::time::Duration::from_secs(30), rx).await??;
        match resp {
            ResponseBody::Ok { result } => Ok(result),
            ResponseBody::Err { error } => bail!("{}: {}", error.code, error.message),
        }
    }
}

fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("authd")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    Ok(())
}
