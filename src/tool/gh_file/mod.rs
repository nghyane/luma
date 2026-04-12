/// GhFile tool — fetch a file from a GitHub repository via `gh` CLI.
mod scoring;

use crate::core::tool::{Tool, ToolExecution};
use crate::core::types::ToolSchema;
use anyhow::{Result, bail};
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_OUTPUT_BYTES: usize = 262_144;

/// Fetch a file from a GitHub repository.
pub struct GhFileTool;

impl Tool for GhFileTool {
    fn name(&self) -> &str {
        "GhFile"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "GhFile".into(),
            description: concat!(
                "Fetch a file from a GitHub repository using the gh CLI.\n",
                "- Requires `gh` to be installed and authenticated.\n",
                "- Repo format: owner/name or full GitHub URL.\n",
                "- When objective is given, returns only relevant code blocks.\n",
                "- Output clipped to 256KB.\n",
                "- Call in parallel for multiple files you need to fetch.",
            )
            .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "GitHub repo (owner/name or URL)"
                    },
                    "path": {
                        "type": "string",
                        "description": "Path to the file in the repository"
                    },
                    "ref": {
                        "type": "string",
                        "description": "Optional git ref, branch, or commit SHA"
                    },
                    "objective": {
                        "type": "string",
                        "description": "Optional objective — returns only relevant code blocks"
                    }
                },
                "required": ["repo", "path"]
            }),
            streamable_arg: None,
        }
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _output_tx: mpsc::Sender<String>,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecution>> + Send + '_>> {
        Box::pin(async move {
            let repo = normalize_repo(args["repo"].as_str().unwrap_or(""));
            let path = args["path"].as_str().unwrap_or("");
            if repo.is_empty() {
                bail!("missing repo");
            }
            if path.is_empty() {
                bail!("missing path");
            }

            let git_ref = match args.get("ref").and_then(|v| v.as_str()) {
                Some(r) if !r.is_empty() => r.to_owned(),
                _ => resolve_default_branch(&repo, &cancel).await?,
            };
            let objective = args.get("objective").and_then(|v| v.as_str()).unwrap_or("");

            let encoded_path = path
                .split('/')
                .map(urlencoding)
                .collect::<Vec<_>>()
                .join("/");
            let api_path = format!("repos/{repo}/contents/{encoded_path}?ref={git_ref}");

            let output = tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("aborted"),
                r = run_gh_api(&api_path) => r?,
            };

            let json: serde_json::Value = serde_json::from_str(&output)?;
            if json["type"].as_str().unwrap_or("") != "file" {
                bail!("{path} is not a file");
            }

            let encoded = json["content"].as_str().unwrap_or("");
            let cleaned: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
            let bytes =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &cleaned)?;
            let content = String::from_utf8_lossy(&bytes);
            let html_url = json["html_url"].as_str().unwrap_or("");
            let header = format!("{repo} {path} {html_url}");

            if !objective.is_empty() {
                let excerpts = scoring::format_blocks(&content, objective, path);
                let mut result = header;
                for e in excerpts {
                    result.push_str("\n\n");
                    result.push_str(&e);
                }
                Ok(ToolExecution {
                    result,
                    artifact: None,
                })
            } else {
                let mut result = format!("{header}\n\n");
                for (i, line) in content.lines().enumerate() {
                    result.push_str(&format!("{}: {line}\n", i + 1));
                    if result.len() > MAX_OUTPUT_BYTES {
                        result.push_str(crate::core::tool::TRUNCATION_MARKER);
                        break;
                    }
                }
                Ok(ToolExecution {
                    result,
                    artifact: None,
                })
            }
        })
    }
}

/// Resolve the default branch of a repository.
pub async fn resolve_default_branch(repo: &str, cancel: &CancellationToken) -> Result<String> {
    let api_path = format!("repos/{repo}");
    let output = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("aborted"),
        r = run_gh_api(&api_path) => r?,
    };
    let json: serde_json::Value = serde_json::from_str(&output)?;
    Ok(json["default_branch"].as_str().unwrap_or("main").to_owned())
}

/// Run `gh api <path>` and return stdout.
pub async fn run_gh_api(api_path: &str) -> Result<String> {
    let output = tokio::process::Command::new("gh")
        .args(["api", api_path])
        .output()
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("gh CLI not found. Install from https://cli.github.com/")
            } else {
                e.into()
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Normalize a GitHub repo identifier to owner/name format.
pub fn normalize_repo(input: &str) -> String {
    input
        .trim()
        .trim_start_matches("https://github.com/")
        .trim_start_matches("http://github.com/")
        .trim_end_matches(".git")
        .trim_matches('/')
        .to_owned()
}

/// Simple percent-encoding for path segments.
fn urlencoding(segment: &str) -> String {
    let mut result = String::with_capacity(segment.len());
    for &byte in segment.as_bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_repo_url() {
        assert_eq!(
            normalize_repo("https://github.com/owner/repo.git"),
            "owner/repo"
        );
    }

    #[test]
    fn normalize_repo_plain() {
        assert_eq!(normalize_repo("owner/repo"), "owner/repo");
    }

    #[test]
    fn urlencoding_basic() {
        assert_eq!(urlencoding("hello world"), "hello%20world");
        assert_eq!(urlencoding("file.rs"), "file.rs");
    }
}
