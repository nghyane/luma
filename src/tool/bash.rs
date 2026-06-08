/// Shell tool — execute commands via platform shell with streaming output and timeout.
use crate::core::session::{Resolved, resolve_resource_path};
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

/// Maximum combined stdout+stderr bytes retained in the tool result.
///
/// When output exceeds this cap, `bash` uses head+tail truncation (see
/// [`HEAD_BYTES`], [`TAIL_BYTES`]) with a self-describing middle marker —
/// intentionally richer than the generic
/// [`crate::core::tool::TRUNCATION_MARKER`], because the tail of a build
/// log is usually the part that matters. `bash` therefore does *not*
/// rely on the agent-level safety cap in `core::agent::turn` and bounds
/// itself.
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
                "Do NOT use for file operations — use dedicated tools instead (Read/Write/Edit/Glob/Grep).\n",
                "- Do NOT use interactive commands (editors, REPLs, password prompts).\n",
                "- Dependent commands: chain with && in a single call.\n",
                "- Only run git commit/push if explicitly instructed.\n",
                "- Timeout default 30s, max 120s.\n",
                "- `artifact://ev/{id}` references expand to the evidence blob's local path before the shell runs, so tools like `jq`, `grep`, `sed` can read promoted tool outputs directly (e.g. `jq '.heart' artifact://ev/ev_abc`).",
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
        _caps: crate::core::tool::ModelCaps,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecution>> + Send + '_>> {
        Box::pin(async move {
            let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if command.is_empty() {
                bail!("missing command argument");
            }

            // Expand `artifact://ev/{id}` references to shell-quoted
            // absolute paths before anything else sees the command. Done
            // pre-safety so bash_safety inspects the literal path the
            // shell will execute — not the URI that might otherwise mask
            // a dangerous target after the shell parses it.
            let command = expand_artifact_refs(command)?;
            let command = command.as_str();

            let timeout_ms = args
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_TIMEOUT_MS);

            if bash_safety::contains_dangerous_substr(command)
                || bash_safety::is_dangerous_cmd(command)
            {
                bail!("blocked dangerous command — {command}");
            }

            // Child inherits Luma's cwd — callers no longer need to
            // prefix `cd /abs/path && …` when the project root is
            // already the working directory. Audit of real sessions
            // showed 61% of Bash calls starting with that exact
            // prefix; inherited cwd makes it a no-op.
            let mut child = shell::spawn(command, None)?;

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
                result: (if result_str.trim().is_empty() {
                    "(no output)".into()
                } else {
                    result_str
                })
                .into(),
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
        crate::util::truncate_string_at_boundary(&mut out, HEAD_BYTES);
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
            let split = crate::util::floor_char_boundary(out, out.len().saturating_sub(TAIL_BYTES));
            *tail = out.split_off(split);
        }
    } else {
        tail.push_str(chunk);
        if tail.len() > TAIL_BYTES * 2 {
            let start = crate::util::floor_char_boundary(tail, tail.len() - TAIL_BYTES);
            *tail = tail[start..].to_owned();
        }
    }
}

/// Expand every `artifact://ev/{id}` token in `command` to its
/// session-local absolute path, shell-quoted.
///
/// Expansion runs **before** safety screening so `bash_safety` sees the
/// real path the shell will execute. Invalid ids, non-`ev` artifact
/// types, and unresolved blobs all fail loudly instead of silently
/// leaving the URI in the command (where the shell would treat it as a
/// literal filename and fail with a confusing "No such file" error).
///
/// Token boundary: the id consists of ASCII alphanumerics and `_`, which
/// matches `next_evidence_id` in `core::evidence`. Anything outside that
/// set terminates the id — whitespace, punctuation, end-of-string all
/// work naturally.
fn expand_artifact_refs(command: &str) -> Result<String> {
    const PREFIX: &str = "artifact://ev/";
    let mut out = String::with_capacity(command.len());
    let mut rest = command;
    while let Some(start) = rest.find(PREFIX) {
        out.push_str(&rest[..start]);
        let after = &rest[start + PREFIX.len()..];
        let id_end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        let id = &after[..id_end];
        if id.is_empty() {
            bail!("empty evidence id in `artifact://ev/` reference");
        }
        let uri = format!("{PREFIX}{id}");
        let path = match resolve_resource_path(&uri) {
            Ok(Resolved::Path(p)) => p,
            Ok(Resolved::PathStripFrontmatter(_)) => {
                bail!("unsupported artifact type in Bash: {uri}");
            }
            Err(e) => bail!("cannot resolve {uri}: {e}"),
        };
        let path_str = path.to_string_lossy();
        out.push_str(&shell_single_quote(&path_str));
        rest = &after[id_end..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Wrap `s` in single quotes for POSIX shells, escaping any embedded
/// single quote via the classic `'\''` dance. Works for every byte
/// sequence the shell accepts — we never inspect the bytes themselves.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
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
            .execute(
                serde_json::json!({"command": "echo hello"}),
                tx,
                cancel,
                Default::default(),
            )
            .await
            .unwrap();

        assert!(result.result.as_text().contains("hello"));
        let chunk = rx.try_recv();
        assert!(chunk.is_ok());
    }

    #[tokio::test]
    async fn bash_exit_code() {
        let tool = BashTool::claude();
        let (tx, _rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(
                serde_json::json!({"command": "exit 42"}),
                tx,
                cancel,
                Default::default(),
            )
            .await
            .unwrap();

        assert!(result.result.as_text().contains("[exit code: 42]"));
    }

    #[tokio::test]
    async fn bash_dangerous_blocked() {
        let tool = BashTool::claude();
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(
                serde_json::json!({"command": "rm -rf /"}),
                tx,
                cancel,
                Default::default(),
            )
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
            .execute(
                serde_json::json!({"command": "sleep 10"}),
                tx,
                cancel,
                Default::default(),
            )
            .await
            .unwrap();

        assert!(result.result.as_text().contains("[aborted]"));
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
                Default::default(),
            )
            .await
            .unwrap();

        assert!(result.result.as_text().contains("[aborted]"));
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

    #[test]
    fn shell_quote_wraps_and_escapes_quotes() {
        assert_eq!(shell_single_quote("hello"), "'hello'");
        assert_eq!(shell_single_quote("a b"), "'a b'");
        assert_eq!(shell_single_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_single_quote(""), "''");
    }

    #[test]
    fn expand_noop_when_no_artifact_ref() {
        let cmd = "jq '.foo' /tmp/x.json";
        assert_eq!(expand_artifact_refs(cmd).unwrap(), cmd);
    }

    #[test]
    fn expand_empty_id_is_rejected() {
        // A bare `artifact://ev/` with nothing after must not silently
        // produce a quoted empty path — that would crash jq/grep with a
        // cryptic error instead of pointing at the real problem.
        let err = expand_artifact_refs("cat artifact://ev/ here").unwrap_err();
        assert!(err.to_string().contains("empty evidence id"));
    }

    #[test]
    fn expand_unknown_id_surfaces_error() {
        // No active session scope → resolver returns an error. Bash must
        // surface it instead of leaving `artifact://ev/...` literal in
        // the command for the shell to misread.
        let err = expand_artifact_refs("cat artifact://ev/ev_nonexistent").unwrap_err();
        assert!(err.to_string().contains("artifact://ev/ev_nonexistent"));
    }

    #[tokio::test]
    async fn expand_resolves_real_blob_in_scoped_session() {
        use crate::core::evidence::{EvidenceDraft, EvidenceKind, EvidenceStore};
        use crate::core::session::{scope_current_session, session_evidence_dir};

        let session_id = format!("test_{}", std::process::id());
        scope_current_session(&session_id, async {
            let dir = session_evidence_dir(&session_id);
            let mut store = EvidenceStore::default();
            let draft = EvidenceDraft {
                kind: EvidenceKind::Other,
                summary: String::new(),
                preview: String::new(),
                blob: "payload".into(),
                related_files: Vec::new(),
            };
            let id = store.ingest(&dir, 0, "tc_test", draft).unwrap();

            let cmd = format!("cat artifact://ev/{id}");
            let expanded = expand_artifact_refs(&cmd).unwrap();
            let expected_tail = format!("{id}.txt'");
            assert!(
                expanded.starts_with("cat '") && expanded.ends_with(&expected_tail),
                "unexpected expansion: {expanded}"
            );

            std::fs::remove_dir_all(dir.parent().unwrap()).ok();
        })
        .await;
    }
}
