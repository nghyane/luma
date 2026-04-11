/// GhLs tool — list files in a GitHub repository via `gh` CLI.
use crate::core::tool::{Tool, ToolExecution};
use crate::core::types::ToolSchema;
use crate::tool::gh_file::{normalize_repo, resolve_default_branch, run_gh_api};
use anyhow::{Result, bail};
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 500;

/// List files in a GitHub repository.
pub struct GhLsTool;

impl Tool for GhLsTool {
    fn name(&self) -> &str {
        "GhLs"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "GhLs".into(),
            description: concat!(
                "List files in a GitHub repository using the gh CLI.\n",
                "- Requires `gh` to be installed and authenticated.\n",
                "- Returns file tree with type (dir/file), path, and URL.\n",
                "- Use path prefix to filter results to a subdirectory.",
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
                        "description": "Optional path prefix to filter results"
                    },
                    "ref": {
                        "type": "string",
                        "description": "Optional git ref, branch, or commit SHA"
                    },
                    "limit": {
                        "type": "number",
                        "description": "Maximum entries to return (default 200, max 500)"
                    }
                },
                "required": ["repo"]
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
            if repo.is_empty() {
                bail!("missing repo");
            }

            let git_ref = match args.get("ref").and_then(|v| v.as_str()) {
                Some(r) if !r.is_empty() => r.to_owned(),
                _ => resolve_default_branch(&repo, &cancel).await?,
            };
            let prefix = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim_matches('/');
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_LIMIT as u64)
                .min(MAX_LIMIT as u64) as usize;

            let tree_sha = resolve_tree_sha(&repo, &git_ref, &cancel).await?;
            let api_path = format!("repos/{repo}/git/trees/{tree_sha}?recursive=1");

            let output = tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("aborted"),
                r = run_gh_api(&api_path) => r?,
            };

            let json: serde_json::Value = serde_json::from_str(&output)?;
            let tree = json["tree"].as_array();

            let entries: Vec<&serde_json::Value> = match tree {
                Some(arr) => arr
                    .iter()
                    .filter(|e| {
                        if prefix.is_empty() {
                            return true;
                        }
                        let p = e["path"].as_str().unwrap_or("");
                        p == prefix || p.starts_with(&format!("{prefix}/"))
                    })
                    .take(limit)
                    .collect(),
                None => {
                    return Ok(ToolExecution {
                        result: format!("Could not resolve tree for {repo}@{git_ref}"),
                        artifact: None,
                    });
                }
            };

            if entries.is_empty() {
                return Ok(ToolExecution {
                    result: format!("No entries found for {repo}@{git_ref}"),
                    artifact: None,
                });
            }

            let mut result = String::new();
            for entry in &entries {
                let path = entry["path"].as_str().unwrap_or("");
                let etype = entry["type"].as_str().unwrap_or("blob");
                let kind = if etype == "tree" { "dir" } else { "file" };
                let url_type = if etype == "tree" { "tree" } else { "blob" };
                let url = format!("https://github.com/{repo}/{url_type}/{git_ref}/{path}");
                result.push_str(&format!("{kind} {path} {url}\n"));
            }

            Ok(ToolExecution {
                result,
                artifact: None,
            })
        })
    }
}

/// Resolve the tree SHA for a given ref.
async fn resolve_tree_sha(repo: &str, git_ref: &str, cancel: &CancellationToken) -> Result<String> {
    let api_path = format!("repos/{repo}/branches/{git_ref}");
    let output = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("aborted"),
        r = run_gh_api(&api_path) => r?,
    };
    let json: serde_json::Value = serde_json::from_str(&output)?;
    json["commit"]["commit"]["tree"]["sha"]
        .as_str()
        .map(|s| s.to_owned())
        .ok_or_else(|| anyhow::anyhow!("could not resolve tree SHA for {repo}@{git_ref}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_works() {
        assert_eq!(normalize_repo("owner/repo"), "owner/repo");
        assert_eq!(normalize_repo("https://github.com/a/b.git"), "a/b");
    }
}
