//! Auth daemon — Unix socket server, single owner of auth state.

pub mod manager;
pub mod protocol;

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

use crate::auth::domain::AuthVendor;
use manager::TokenManager;
use protocol::*;

const IDLE_TIMEOUT: Duration = Duration::from_secs(600); // 10 min

pub fn sock_path() -> std::path::PathBuf {
    crate::config::home_dir()
        .join(".config")
        .join("luma")
        .join("authd.sock")
}

fn pid_path() -> std::path::PathBuf {
    crate::config::home_dir()
        .join(".config")
        .join("luma")
        .join("authd.pid")
}

/// Check if a daemon is already running via PID file + socket probe.
fn is_daemon_running() -> bool {
    let path = pid_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(pid) = content.trim().parse::<i32>() else {
        return false;
    };
    // signal(0) checks process existence without sending a signal.
    #[cfg(unix)]
    {
        // SAFETY: kill(pid, 0) is a standard POSIX existence check.
        unsafe { libc::kill(pid, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Run the daemon. Blocks until idle timeout or SIGTERM.
pub async fn run_daemon() -> anyhow::Result<()> {
    if is_daemon_running() {
        anyhow::bail!("authd already running");
    }

    let path = sock_path();
    // Clean stale socket.
    let _ = std::fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Write PID file.
    let pid_file = pid_path();
    std::fs::write(&pid_file, std::process::id().to_string())?;

    let listener = UnixListener::bind(&path)?;
    // Restrict permissions: owner-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    let (event_tx, _) = broadcast::channel::<AuthEvent>(128);
    let mgr = Arc::new(TokenManager::new(event_tx.clone()));

    eprintln!("authd: listening on {}", path.display());

    let active_conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let shutdown = async {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            ctrl_c.await.ok();
        }
    };
    tokio::pin!(shutdown);

    loop {
        let accept = tokio::select! {
            a = listener.accept() => a,
            _ = &mut shutdown => break,
            _ = idle_wait(active_conns.clone()) => {
                eprintln!("authd: idle timeout, shutting down");
                break;
            }
        };

        let (stream, _) = match accept {
            Ok(s) => s,
            Err(_) => continue,
        };

        active_conns.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mgr = mgr.clone();
        let event_tx = event_tx.clone();
        let conns = active_conns.clone();

        tokio::spawn(async move {
            let _ = handle_conn(stream, mgr, event_tx).await;
            conns.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        });
    }

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&pid_file);
    Ok(())
}

async fn idle_wait(conns: Arc<std::sync::atomic::AtomicUsize>) {
    loop {
        tokio::time::sleep(IDLE_TIMEOUT).await;
        if conns.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            return;
        }
    }
}

async fn handle_conn(
    stream: UnixStream,
    mgr: Arc<TokenManager>,
    event_tx: broadcast::Sender<AuthEvent>,
) -> anyhow::Result<()> {
    let (reader, writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // Channel for outbound messages (responses + push events).
    let (out_tx, mut out_rx) = mpsc::channel::<String>(128);

    // Push event forwarder.
    let mut event_rx = event_tx.subscribe();
    let push_tx = out_tx.clone();
    let push_task = tokio::spawn(async move {
        while let Ok(ev) = event_rx.recv().await {
            let msg = ServerMessage::Event(ev);
            if let Ok(line) = serde_json::to_string(&msg)
                && push_tx.send(line).await.is_err()
            {
                break;
            }
        }
    });

    // Writer task.
    let write_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(line) = out_rx.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if writer.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = writer.flush().await;
        }
    });

    // Read loop — dispatch requests.
    while let Ok(Some(line)) = lines.next_line().await {
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let id = req.id;
        let body = dispatch(&mgr, req.body).await;
        let resp = Response { id, body };
        let msg = ServerMessage::Response(resp);
        if let Ok(json) = serde_json::to_string(&msg)
            && out_tx.send(json).await.is_err()
        {
            break;
        }
    }

    drop(out_tx);
    push_task.abort();
    let _ = write_task.await;
    Ok(())
}

async fn dispatch(mgr: &TokenManager, body: RequestBody) -> ResponseBody {
    match body {
        RequestBody::Resolve { vendor } => {
            let Some(v) = AuthVendor::from_str(&vendor) else {
                return ResponseBody::err("invalid_vendor", format!("unknown vendor: {vendor}"));
            };
            match mgr.resolve(v).await {
                Ok(cred) => ResponseBody::ok(ResponseResult::Credential(WireCredential::from_credential(&cred))),
                Err(e) => ResponseBody::err("resolve_failed", e.to_string()),
            }
        }
        RequestBody::Refresh { account_key } => {
            match mgr.refresh(&account_key).await {
                Ok(cred) => ResponseBody::ok(ResponseResult::Credential(WireCredential::from_credential(&cred))),
                Err(e) => ResponseBody::err("refresh_failed", e.to_string()),
            }
        }
        RequestBody::Login { vendor } => {
            let Some(v) = AuthVendor::from_str(&vendor) else {
                return ResponseBody::err("invalid_vendor", format!("unknown vendor: {vendor}"));
            };
            match mgr.login(v).await {
                Ok(view) => ResponseBody::ok(ResponseResult::Account(WireAccountView::from(&view))),
                Err(e) => ResponseBody::err("login_failed", e.to_string()),
            }
        }
        RequestBody::SaveApiKey { vendor, token } => {
            let Some(v) = AuthVendor::from_str(&vendor) else {
                return ResponseBody::err("invalid_vendor", format!("unknown vendor: {vendor}"));
            };
            match mgr.save_api_key(v, &token).await {
                Ok(view) => ResponseBody::ok(ResponseResult::Account(WireAccountView::from(&view))),
                Err(e) => ResponseBody::err("save_failed", e.to_string()),
            }
        }
        RequestBody::MarkRateLimited { account_key, retry_after_secs } => {
            mgr.mark_rate_limited(&account_key, retry_after_secs).await;
            ResponseBody::ok(ResponseResult::Ok)
        }
        RequestBody::MarkAuthFailed { account_key, failure } => {
            mgr.mark_auth_failed(&account_key, &failure).await;
            ResponseBody::ok(ResponseResult::Ok)
        }
        RequestBody::ListAccounts => {
            let views = mgr.list_accounts().await;
            let wire: Vec<_> = views.iter().map(WireAccountView::from).collect();
            ResponseBody::ok(ResponseResult::Accounts(wire))
        }
        RequestBody::ToggleDisabled { account_key } => {
            mgr.toggle_disabled(&account_key).await;
            ResponseBody::ok(ResponseResult::Ok)
        }
        RequestBody::RemoveAccount { account_key } => {
            mgr.remove_account(&account_key).await;
            ResponseBody::ok(ResponseResult::Ok)
        }
        RequestBody::RecordUsage { label, usage } => {
            mgr.record_usage(&label, usage.to_usage()).await;
            ResponseBody::ok(ResponseResult::Ok)
        }
        RequestBody::Ping => ResponseBody::ok(ResponseResult::Pong),
        RequestBody::Shutdown => {
            // Caller should handle graceful shutdown.
            ResponseBody::ok(ResponseResult::Ok)
        }
    }
}
