/// GhSearch tool — search code in a GitHub repository via `gh` CLI.
use crate::core::tool::{Tool, ToolExecution};
use crate::core::types::ToolSchema;
use crate::tool::gh_file::normalize_repo;
use anyhow::{Result, bail};
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const DEFAULT_LIMIT: u64 = 20;
const MAX_LIMIT: u64 = 100;

/// Search code in a GitHub repository.
pub struct GhSearchTool;

impl Tool for GhSearchTool {
    fn name(&self) -> &str {
        "GhSearch"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "GhSearch".into(),
            description: concat!(
                "Search code in a GitHub repository using the gh CLI.\n",
                "- Requires `gh` to be installed and authenticated.\n",
                "- Returns matching file paths and URLs.\n",
                "- Use to find specific code, functions, or patterns in a remote repo.\n",
                "- Call in parallel for multiple independent queries.",
            )
            .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "GitHub repo (owner/name or URL)"
                    },
                    "query": {
                        "type": "string",
                        "description": "Search query for code"
                    },
                    "path": {
                        "type": "string",
                        "description": "Optional path qualifier inside the repo"
                    },
                    "limit": {
                        "type": "number",
                        "description": "Maximum results to return (default 20, max 100)"
                    }
                },
                "required": ["repo", "query"]
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
            let query = args["query"].as_str().unwrap_or("");
            if repo.is_empty() {
                bail!("missing repo");
            }
            if query.is_empty() {
                bail!("missing query");
            }

            let path_filter = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_LIMIT)
                .min(MAX_LIMIT);

            let scoped_query = if path_filter.is_empty() {
                query.to_owned()
            } else {
                format!("{query} path:{path_filter}")
            };

            let output = tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("aborted"),
                r = run_gh_search(&scoped_query, &repo, limit) => r?,
            };

            let items: Vec<serde_json::Value> = serde_json::from_str(&output)?;

            if items.is_empty() {
                return Ok(ToolExecution {
                    result: format!("No results for \"{query}\" in {repo}"),
                    artifact: None,
                });
            }

            let mut result = format!("{} results in {repo}:\n\n", items.len());
            for (i, item) in items.iter().enumerate() {
                let path = item["path"].as_str().unwrap_or("");
                let url = item["url"].as_str().unwrap_or("");
                result.push_str(&format!("{}. {path}\n   {url}\n\n", i + 1));
            }

            Ok(ToolExecution {
                result,
                artifact: None,
            })
        })
    }
}

/// Run `gh search code` and return stdout.
async fn run_gh_search(query: &str, repo: &str, limit: u64) -> Result<String> {
    let output = tokio::process::Command::new("gh")
        .args([
            "search",
            "code",
            query,
            "--repo",
            repo,
            "--limit",
            &limit.to_string(),
            "--json",
            "path,repository,url,sha",
        ])
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
        bail!("gh search code failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_repo_url() {
        assert_eq!(
            normalize_repo("https://github.com/tokio-rs/tokio.git"),
            "tokio-rs/tokio"
        );
    }

    #[test]
    fn normalize_repo_plain() {
        assert_eq!(normalize_repo("owner/repo"), "owner/repo");
    }
}
