/// Glob tool — fast file pattern matching using `ignore` crate.
///
/// Respects `.gitignore`, `.ignore`, skips hidden files by default.
/// Same engine as ripgrep for consistent, fast directory walking.
use crate::core::tool::{Tool, ToolExecution};
use crate::core::types::ToolSchema;
use anyhow::{Result, bail};
use std::path::Path;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_RESULTS: usize = 200;

/// Native glob tool using `ignore` crate (ripgrep's walker) + globset matcher.
pub struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_owned(),
            description: concat!(
                "Find files by name pattern. Returns paths sorted by modification time (newest first).\n",
                "- Patterns: **/*.rs, src/**/*.{ts,tsx}, *test*, src/[a-z]*/*.ts\n",
                "- Respects .gitignore rules. Hidden files are excluded by default.\n",
                "- Not for searching file contents — use `Grep` instead.",
            ).to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to match files (e.g. \"**/*.rs\", \"src/**/*.{ts,tsx}\")"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in. Defaults to current working directory."
                    }
                },
                "required": ["pattern"]
            }),
            streamable_arg: None,
        }
    }

    fn execute(
        &self,
        args: serde_json::Value,
        output_tx: mpsc::Sender<String>,
        _cancel: CancellationToken,
        _caps: crate::core::tool::ModelCaps,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<ToolExecution>> + Send + '_>> {
        Box::pin(async move {
            let pattern = args["pattern"].as_str().unwrap_or("**/*");
            let path = args["path"].as_str().unwrap_or(".");
            let root = if Path::new(path).is_absolute() {
                path.to_owned()
            } else {
                std::env::current_dir()?
                    .join(path)
                    .to_string_lossy()
                    .into_owned()
            };

            if !Path::new(&root).is_dir() {
                bail!("Directory does not exist: {path}");
            }

            let matcher = globset::GlobBuilder::new(pattern)
                .literal_separator(false)
                .build()
                .map_err(|e| anyhow::anyhow!("Invalid glob pattern: {e}"))?
                .compile_matcher();

            let mut matches: Vec<(String, std::time::SystemTime)> = Vec::new();
            let root_path = Path::new(&root);

            // Walk using `ignore` crate — respects .gitignore, skips hidden,
            // handles symlinks, parallel-ready.
            let walker = ignore::WalkBuilder::new(&root)
                .hidden(true) // skip hidden files
                .git_ignore(true) // respect .gitignore
                .git_global(true) // respect global gitignore
                .git_exclude(true) // respect .git/info/exclude
                .build();

            for entry in walker {
                if matches.len() >= MAX_RESULTS {
                    break;
                }
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                // Skip directories — only match files
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) {
                    continue;
                }
                let entry_path = entry.path();
                let rel = entry_path.strip_prefix(root_path).unwrap_or(entry_path);
                if matcher.is_match(rel) {
                    let mtime = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    matches.push((rel.to_string_lossy().into_owned(), mtime));
                }
            }

            // Sort by mtime descending (newest first)
            matches.sort_by_key(|e| std::cmp::Reverse(e.1));

            let truncated = matches.len() >= MAX_RESULTS;
            let files: Vec<String> = matches.into_iter().map(|(p, _)| p).collect();

            for f in &files {
                let _ = output_tx.send(f.clone()).await;
            }

            let mut result = files.join("\n");
            if truncated {
                result.push_str("\n(Results truncated. Use a more specific pattern or path.)");
            }
            if files.is_empty() {
                result = "No files found".to_owned();
            }
            Ok(ToolExecution {
                result: result.into(),
                artifact: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn glob_finds_rust_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("readme.md"), "# hi\n").unwrap();

        let (tx, _rx) = mpsc::channel(64);
        let tool = GlobTool;
        let args = serde_json::json!({"pattern": "**/*.rs", "path": root.to_str().unwrap()});
        let result = tool
            .execute(args, tx, CancellationToken::new(), Default::default())
            .await
            .unwrap();
        assert!(result.result.as_text().contains(".rs"));
        assert!(!result.result.as_text().contains("readme.md"));
    }

    #[tokio::test]
    async fn glob_no_match() {
        let (tx, _rx) = mpsc::channel(64);
        let tool = GlobTool;
        let args = serde_json::json!({"pattern": "**/*.nonexistent_ext"});
        let result = tool
            .execute(args, tx, CancellationToken::new(), Default::default())
            .await
            .unwrap();
        assert_eq!(result.result.as_text(), "No files found");
    }

    #[tokio::test]
    async fn glob_invalid_dir() {
        let (tx, _rx) = mpsc::channel(64);
        let tool = GlobTool;
        let args = serde_json::json!({"pattern": "**/*", "path": "/nonexistent_dir_xyz"});
        let result = tool.execute(args, tx, CancellationToken::new(), Default::default()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn glob_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // ignore crate needs .git dir to recognize .gitignore
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(root.join("keep.rs"), "").unwrap();
        std::fs::write(root.join("skip.log"), "").unwrap();

        let (tx, _rx) = mpsc::channel(64);
        let tool = GlobTool;
        let args = serde_json::json!({"pattern": "**/*", "path": root.to_str().unwrap()});
        let result = tool
            .execute(args, tx, CancellationToken::new(), Default::default())
            .await
            .unwrap();

        assert!(
            result.result.as_text().contains("keep.rs"),
            "should include keep.rs: {result:?}"
        );
        assert!(
            !result.result.as_text().contains("skip.log"),
            "should exclude skip.log: {result:?}"
        );
    }
}
