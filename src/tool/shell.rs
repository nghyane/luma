//! Platform shell — spawn the appropriate shell for the current OS.

use std::path::Path;

/// Spawn a shell process to execute a command.
///
/// When `cwd` is `Some`, the child process runs with that directory as
/// its working directory (no `cd X && …` prefix needed in `command`).
/// When `None`, the child inherits the parent process's cwd — identical
/// to the pre-`cwd` behaviour.
pub fn spawn(command: &str, cwd: Option<&Path>) -> std::io::Result<tokio::process::Child> {
    let mut cmd = platform::command(command);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.spawn()
}

#[cfg(unix)]
mod platform {
    /// Build a shell command using bash.
    pub fn command(command: &str) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(command);
        cmd
    }
}

#[cfg(windows)]
mod platform {
    /// Build a shell command using PowerShell on Windows.
    pub fn command(command: &str) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("powershell");
        cmd.arg("-NoProfile").arg("-Command").arg(command);
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo() {
        // "echo hello" works on both bash and cmd.
        let output = spawn("echo hello", None)
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains("hello"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn exit_code() {
        let output = spawn("exit 42", None)
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(42));
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn exit_code() {
        let output = spawn("exit 42", None)
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(42));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn spawn_with_cwd_runs_in_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let output = spawn("pwd", Some(tmp.path()))
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();
        let out = String::from_utf8_lossy(&output.stdout);
        // pwd resolves symlinks on macOS (/var → /private/var); compare
        // via canonicalize on both sides.
        let expected = std::fs::canonicalize(tmp.path()).unwrap();
        let got = std::fs::canonicalize(out.trim()).unwrap();
        assert_eq!(got, expected);
    }
}
