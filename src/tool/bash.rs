/// Shell tool — execute commands via platform shell with streaming output and timeout.
use crate::core::tool::{Tool, ToolExecution};
use crate::core::types::ToolSchema;
use crate::tool::bash_safety;
use crate::tool::shell;
use anyhow::{Result, bail};
use std::future::Future;
use std::pin::Pin;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_OUTPUT: usize = 32_000;
const HEAD_BYTES: usize = 8_000; // keep first 8K
const TAIL_BYTES: usize = 20_000; // keep last 20K — errors/results are at the end
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Execute shell commands with streaming output.
pub struct BashTool {
    name: &'static str,
}

impl BashTool {
    /// Create a BashTool with Claude-style naming.
    pub fn claude() -> Self {
        Self { name: "Bash" }
    }
    /// Create a BashTool with Codex-style naming.
    pub fn codex() -> Self {
        Self {
            name: "exec_command",
        }
    }
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        self.name
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.into(),
            description: concat!(
                "Execute a shell command. Returns stdout + stderr.\n",
                "Use for: builds, tests, git operations, running scripts, installing packages.\n",
                "Do NOT use for file operations — use dedicated tools instead:\n",
                "  read files: `read` (not cat/head/tail)\n",
                "  edit files: `edit` (not sed/awk)\n",
                "  create files: `write` (not echo/cat)\n",
                "  find files: `glob` (not find/fd)\n",
                "  search contents: `grep` (not grep/rg)\n",
                "Rules:\n",
                "- Use absolute paths. Quote paths with spaces.\n",
                "- Do NOT use interactive commands (editors, REPLs, password prompts).\n",
                "- Independent commands: use separate parallel tool calls.\n",
                "- Dependent commands: chain with && in a single call.\n",
                "- Only run git commit/push if explicitly instructed.\n",
                "- Timeout default 30s, max 120s.",
            )
            .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "timeout": { "type": "number", "description": "Timeout in ms (default 30000)" }
                },
                "required": ["command"]
            }),
            streamable_arg: Some("command".into()),
        }
    }

    fn execute(
        &self,
        args: serde_json::Value,
        output_tx: mpsc::Sender<String>,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecution>> + Send + '_>> {
        Box::pin(async move {
            let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if command.is_empty() {
                bail!("missing command argument");
            }

            let timeout_ms = args
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_TIMEOUT_MS);

            if bash_safety::contains_dangerous_substr(command)
                || bash_safety::is_dangerous_cmd(command)
            {
                bail!("blocked dangerous command — {command}");
            }

            let mut child = shell::spawn(command)?;

            let mut stdout = child.stdout.take().expect("stdout piped");
            let mut stderr = child.stderr.take().expect("stderr piped");

            let (output, exit_code) = read_output(
                &mut stdout,
                &mut stderr,
                &output_tx,
                &cancel,
                &mut child,
                timeout_ms,
            )
            .await?;

            let mut result_str = output;
            if exit_code != 0 && !result_str.contains("[exit code:") {
                result_str.push_str(&format!("\n[exit code: {exit_code}]"));
            }

            Ok(ToolExecution {
                result: if result_str.trim().is_empty() {
                    "(no output)".into()
                } else {
                    result_str
                },
                artifact: None,
            })
        })
    }
}

/// Read stdout + stderr interleaved with cancel/deadline support.
async fn read_output(
    stdout: &mut tokio::process::ChildStdout,
    stderr: &mut tokio::process::ChildStderr,
    output_tx: &mpsc::Sender<String>,
    cancel: &CancellationToken,
    child: &mut tokio::process::Child,
    timeout_ms: u64,
) -> Result<(String, i32)> {
    let mut out = String::new();
    let mut total_bytes = 0usize;
    let mut tail = String::new();
    let mut truncated = false;
    let mut buf = [0u8; 4096];
    let mut stderr_buf = [0u8; 4096];
    let mut aborted = false;
    let mut timed_out = false;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

    loop {
        if stdout_done && stderr_done {
            break;
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => { aborted = true; break; }
            _ = tokio::time::sleep_until(deadline) => { timed_out = true; break; }
            n = stdout.read(&mut buf), if !stdout_done => {
                let n = n?;
                if n == 0 { stdout_done = true; continue; }
                let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                total_bytes += n;
                let _ = output_tx.send(chunk.clone()).await;
                accumulate(&mut out, &mut tail, &mut truncated, &chunk);
            }
            n = stderr.read(&mut stderr_buf), if !stderr_done => {
                let n = n?;
                if n == 0 { stderr_done = true; continue; }
                let chunk = String::from_utf8_lossy(&stderr_buf[..n]).to_string();
                total_bytes += n;
                let _ = output_tx.send(chunk.clone()).await;
                accumulate(&mut out, &mut tail, &mut truncated, &chunk);
            }
        }
    }

    if truncated {
        out.truncate(HEAD_BYTES);
        out.push_str(&format!(
            "\n\n[... {total_bytes} bytes total, middle truncated ...]\n\n"
        ));
        out.push_str(&tail);
    }

    if aborted || timed_out {
        child.kill().await.ok();
        if aborted {
            out.push_str("\n[aborted]");
        }
        if timed_out {
            out.push_str("\n[timeout]");
        }
        Ok((out, if aborted { 130 } else { 124 }))
    } else {
        let status = child.wait().await?;
        Ok((out, status.code().unwrap_or(1)))
    }
}

/// Accumulate output: head in `out`, tail as rolling window.
fn accumulate(out: &mut String, tail: &mut String, truncated: &mut bool, chunk: &str) {
    if !*truncated {
        out.push_str(chunk);
        if out.len() > MAX_OUTPUT {
            *truncated = true;
            let split = floor_char_boundary(out, out.len().saturating_sub(TAIL_BYTES));
            *tail = out.split_off(split);
        }
    } else {
        tail.push_str(chunk);
        if tail.len() > TAIL_BYTES * 2 {
            let start = floor_char_boundary(tail, tail.len() - TAIL_BYTES);
            *tail = tail[start..].to_owned();
        }
    }
}

/// Find the largest byte index `<= idx` that is a char boundary.
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bash_echo() {
        let tool = BashTool::claude();
        let (tx, mut rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(serde_json::json!({"command": "echo hello"}), tx, cancel)
            .await
            .unwrap();

        assert!(result.result.contains("hello"));
        let chunk = rx.try_recv();
        assert!(chunk.is_ok());
    }

    #[tokio::test]
    async fn bash_exit_code() {
        let tool = BashTool::claude();
        let (tx, _rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(serde_json::json!({"command": "exit 42"}), tx, cancel)
            .await
            .unwrap();

        assert!(result.result.contains("[exit code: 42]"));
    }

    #[tokio::test]
    async fn bash_dangerous_blocked() {
        let tool = BashTool::claude();
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(serde_json::json!({"command": "rm -rf /"}), tx, cancel)
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn cancel() {
        let tool = BashTool::claude();
        let (tx, _rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });

        let result = tool
            .execute(serde_json::json!({"command": "sleep 10"}), tx, cancel)
            .await
            .unwrap();

        assert!(result.result.contains("[aborted]"));
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn cancel() {
        let tool = BashTool::claude();
        let (tx, _rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });

        let result = tool
            .execute(
                serde_json::json!({"command": "ping -n 100 127.0.0.1"}),
                tx,
                cancel,
            )
            .await
            .unwrap();

        assert!(result.result.contains("[aborted]"));
    }

    #[test]
    fn accumulate_multibyte_no_panic() {
        // '│' is 3 bytes (E2 94 82). Fill output so the truncation point
        // lands inside a multi-byte char.
        let mut out = String::new();
        let mut tail = String::new();
        let mut truncated = false;

        // Build a string of 3-byte chars that will exceed MAX_OUTPUT.
        let chunk: String = "│".repeat(MAX_OUTPUT);
        accumulate(&mut out, &mut tail, &mut truncated, &chunk);
        assert!(truncated);
        // Both out and tail must be valid UTF-8 (no panic on split).
        assert!(out.is_char_boundary(out.len()));
        assert!(tail.is_char_boundary(tail.len()));

        // Trigger the tail-trimming path.
        let big_tail: String = "│".repeat(TAIL_BYTES * 3);
        accumulate(&mut out, &mut tail, &mut truncated, &big_tail);
        assert!(tail.len() <= TAIL_BYTES * 2 + 3); // +3 for rounding to char boundary
    }
}
