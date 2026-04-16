use super::types::*;
use std::io::{self, BufRead, Write};
use tokio::sync::mpsc;

/// Read ndjson requests from stdin. Runs on a blocking thread.
pub fn read_stdin(tx: mpsc::Sender<Request>) {
    let stdin = io::stdin().lock();
    for line in stdin.lines() {
        let line = match line {
            Ok(l) if !l.trim().is_empty() => l,
            Ok(_) => continue,
            Err(_) => break,
        };
        if let Ok(req) = serde_json::from_str::<Request>(&line)
            && tx.blocking_send(req).is_err()
        {
            break;
        }
    }
}

fn write_stdout(msg: &impl serde::Serialize) {
    let mut out = io::stdout().lock();
    let _ = serde_json::to_writer(&mut out, msg);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

pub fn respond(id: serde_json::Value, result: serde_json::Value) {
    write_stdout(&Response {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    });
}

pub fn respond_error(id: serde_json::Value, code: i32, message: String) {
    write_stdout(&Response {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(RpcError { code, message }),
    });
}

pub fn notify(method: &'static str, params: serde_json::Value) {
    write_stdout(&Notification {
        jsonrpc: "2.0",
        method,
        params,
    });
}
