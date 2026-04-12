/// Grep tool — fast content search using `ignore` crate + regex.
///
/// Respects `.gitignore`, skips binary files, hidden files.
/// Same walker as ripgrep for consistent behavior.
use crate::core::tool::{Tool, ToolExecution};
use crate::core::types::ToolSchema;
use anyhow::Result;
use std::path::Path;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_RESULTS: usize = 250;
const MAX_LINE_LEN: usize = 500;
const MAX_FILE_SIZE: u64 = 1_000_000;

/// Native grep tool using `ignore` crate walker + regex matching.
pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_owned(),
            description: concat!(
                "Search file contents using regex. Returns file paths with line numbers and matching lines.\n",
                "- Regex syntax: \"log.*Error\", \"fn\\s+\\w+\", \"TODO|FIXME\"\n",
                "- Filter by file type with include param: \"*.rs\", \"*.{ts,tsx}\"\n",
                "- Results sorted by modification time (newest first).\n",
                "- Respects .gitignore rules. Skips binary files.\n",
                "- Call in parallel for multiple independent patterns or paths.\n",
                "- Not for finding files by name — use `Glob` instead.",
            ).to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to search for in file contents"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in. Defaults to current working directory."
                    },
                    "include": {
                        "type": "string",
                        "description": "File glob pattern to filter (e.g. \"*.rs\", \"*.{ts,tsx}\")"
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
    ) -> Pin<Box<dyn std::future::Future<Output = Result<ToolExecution>> + Send + '_>> {
        Box::pin(async move {
            let pattern_str = args["pattern"].as_str().unwrap_or("");
            let path = args["path"].as_str().unwrap_or(".");
            let include = args["include"].as_str();

            let root = if Path::new(path).is_absolute() {
                path.to_owned()
            } else {
                std::env::current_dir()?
                    .join(path)
                    .to_string_lossy()
                    .into_owned()
            };

            let re = regex::Regex::new(pattern_str)
                .map_err(|e| anyhow::anyhow!("Invalid regex: {e}"))?;

            let file_filter = include.and_then(|g| {
                globset::GlobBuilder::new(g)
                    .literal_separator(false)
                    .build()
                    .ok()
                    .map(|g| g.compile_matcher())
            });

            let root_path = Path::new(&root);
            let mut matches: Vec<FileMatch> = Vec::new();

            // Walk using `ignore` crate — respects .gitignore, skips hidden/binary.
            let walker = ignore::WalkBuilder::new(&root)
                .hidden(true)
                .git_ignore(true)
                .git_global(true)
                .git_exclude(true)
                .build();

            for entry in walker {
                if matches.len() >= MAX_RESULTS {
                    break;
                }
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) {
                    continue;
                }
                let entry_path = entry.path();
                let rel = entry_path.strip_prefix(root_path).unwrap_or(entry_path);

                // Apply file filter
                if let Some(filter) = &file_filter
                    && !filter.is_match(rel)
                {
                    continue;
                }

                // Skip large files
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.len() > MAX_FILE_SIZE {
                    continue;
                }

                let content = match std::fs::read_to_string(entry_path) {
                    Ok(c) => c,
                    Err(_) => continue, // skip binary/unreadable
                };

                let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                let rel_str = rel.to_string_lossy().into_owned();

                for (i, line) in content.lines().enumerate() {
                    if matches.len() >= MAX_RESULTS {
                        break;
                    }
                    if re.is_match(line) {
                        matches.push(FileMatch {
                            path: rel_str.clone(),
                            line_num: i + 1,
                            line: line.to_owned(),
                            mtime,
                        });
                    }
                }
            }

            // Sort by mtime descending
            matches.sort_by_key(|e| std::cmp::Reverse(e.mtime));

            let mut lines = Vec::new();
            for m in &matches {
                let _ = output_tx.send(format!("{}:{}", m.path, m.line_num)).await;
                let line = if m.line.len() > MAX_LINE_LEN {
                    format!("{}:{}:{}...", m.path, m.line_num, &m.line[..MAX_LINE_LEN])
                } else {
                    format!("{}:{}:{}", m.path, m.line_num, m.line)
                };
                lines.push(line);
            }

            if lines.is_empty() {
                return Ok(ToolExecution {
                    result: "No matches found".to_owned(),
                    artifact: None,
                });
            }

            let truncated = lines.len() >= MAX_RESULTS;
            let mut result = lines.join("\n");
            if truncated {
                result.push_str("\n(Results truncated. Use a more specific pattern or path.)");
            }
            Ok(ToolExecution {
                result,
                artifact: None,
            })
        })
    }
}

struct FileMatch {
    path: String,
    line_num: usize,
    line: String,
    mtime: std::time::SystemTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn grep_finds_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();

        let (tx, _rx) = mpsc::channel(64);
        let tool = GrepTool;
        let args = serde_json::json!({"pattern": "fn main", "path": root.to_str().unwrap()});
        let result = tool
            .execute(args, tx, CancellationToken::new())
            .await
            .unwrap();
        assert!(result.result.contains("fn main"));
    }

    #[tokio::test]
    async fn grep_no_match() {
        let dir = std::env::temp_dir().join("luma_grep_test_empty");
        let _ = std::fs::create_dir_all(&dir);
        let (tx, _rx) = mpsc::channel(64);
        let tool = GrepTool;
        let args =
            serde_json::json!({"pattern": "ZZZZZ_NONEXISTENT", "path": dir.to_str().unwrap()});
        let result = tool
            .execute(args, tx, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.result, "No matches found");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn grep_with_include() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("lib.rs"), "pub struct Foo;\nfn bar() {}\n").unwrap();
        std::fs::write(root.join("main.py"), "class Foo: pass\n").unwrap();

        let (tx, _rx) = mpsc::channel(64);
        let tool = GrepTool;
        let args = serde_json::json!({"pattern": "pub struct", "include": "*.rs", "path": root.to_str().unwrap()});
        let result = tool
            .execute(args, tx, CancellationToken::new())
            .await
            .unwrap();
        assert!(result.result.contains("pub struct"));
        assert!(!result.result.contains("main.py"));
    }

    #[tokio::test]
    async fn grep_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(root.join("keep.txt"), "findme\n").unwrap();
        std::fs::write(root.join("ignored.txt"), "findme\n").unwrap();

        let (tx, _rx) = mpsc::channel(64);
        let tool = GrepTool;
        let args = serde_json::json!({"pattern": "findme", "path": root.to_str().unwrap()});
        let result = tool
            .execute(args, tx, CancellationToken::new())
            .await
            .unwrap();

        assert!(
            result.result.contains("keep.txt"),
            "should include keep.txt: {result:?}"
        );
        assert!(
            !result.result.contains("ignored.txt"),
            "should exclude ignored.txt: {result:?}"
        );
    }
}
