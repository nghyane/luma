/// Edit tool — find-and-replace in files with safety checks.
use crate::core::tool::{Tool, ToolExecution};
use crate::core::types::{FileArtifact, FileChangeArtifact, FileOp, ToolSchema, ToolStatus};
use anyhow::{Result, bail};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_EDIT_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10 MB

/// Normalize curly quotes to straight quotes for fuzzy matching.
fn normalize_quotes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{2018}' | '\u{2019}' => out.push('\''),
            '\u{201C}' | '\u{201D}' => out.push('"'),
            _ => out.push(c),
        }
    }
    out
}

/// Find actual string in file, accounting for curly-quote normalization.
/// Returns the file's version of the string if found via normalization.
///
/// Exposed to the `multi_edit` tool so sequential edits inside one call
/// share the same fuzzy-match behaviour as a single Edit.
pub(crate) fn find_actual_string(content: &str, search: &str) -> Option<String> {
    // Exact match — no normalization needed
    if content.contains(search) {
        return None; // caller uses original
    }
    // Try normalized quotes
    let norm_search = normalize_quotes(search);
    let norm_content = normalize_quotes(content);
    let idx = norm_content.find(&norm_search)?;
    // Extract the actual substring from the original content at the same position
    Some(content[idx..idx + search.len()].to_owned())
}

/// Edits files by exact string replacement.
pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "Edit".into(),
            description: concat!(
                "Performs exact string replacement in an existing file.\n",
                "- old_string must match exactly one location, or set replace_all=true.\n",
                "- old_string and new_string must be different.\n",
                "- Preserve exact indentation from the file when specifying old_string.\n",
                "- Use replace_all for renaming variables/strings across the file.\n",
                "- To create new files, use the `Write` tool instead.",
            )
            .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to the file. ALWAYS generate this argument first." },
                    "old_string": { "type": "string", "description": "Exact string to find" },
                    "new_string": { "type": "string", "description": "Replacement string" },
                    "replace_all": { "type": "boolean", "description": "Replace all occurrences" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
            streamable_arg: Some("new_string".into()),
        }
    }

    fn execute(
        &self,
        args: serde_json::Value,
        output_tx: mpsc::Sender<String>,
        _cancel: CancellationToken,
        _caps: crate::core::tool::ModelCaps,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecution>> + Send + '_>> {
        Box::pin(async move {
            let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let old = args
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = args
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let replace_all = args
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if path_str.is_empty() {
                bail!("missing path argument");
            }
            if old == new {
                bail!("old_string and new_string are identical");
            }

            let path = PathBuf::from(path_str);

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) if old.is_empty() => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, new)?;
                    let diff = crate::tool::diff::make_diff("", new);
                    for line in &diff {
                        let _ = output_tx.send(format!("{line}\n")).await;
                    }
                    return Ok(ToolExecution {
                        result: (format!("Created {}", path.display())).into(),
                        artifact: Some(FileChangeArtifact {
                            files: vec![FileArtifact {
                                path: path.display().to_string(),
                                operation: FileOp::Add,
                                diff: Some(diff.join("\n")),
                                preview: Some(new.to_owned()),
                            }],
                            raw_input: None,
                            error: None,
                            status: ToolStatus::Done,
                        }),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let suggestion = crate::tool::read::suggest_similar_file(&path);
                    let msg = format!("File not found: {}", path.display());
                    if let Some(s) = suggestion {
                        bail!("{msg}. Did you mean {s}?");
                    }
                    bail!("{msg}");
                }
                Err(e) => bail!(e),
            };

            // File size guard
            if let Ok(meta) = std::fs::metadata(&path)
                && meta.len() > MAX_EDIT_FILE_SIZE
            {
                bail!(
                    "File too large ({:.1} MB). Use Bash with sed for large files.",
                    meta.len() as f64 / 1_048_576.0
                );
            }

            // Try exact match first, then curly-quote normalized match
            let actual_old = find_actual_string(&content, old);
            let search = actual_old.as_deref().unwrap_or(old);

            let count = content.matches(search).count();
            if count == 0 {
                bail!("old_string not found in file");
            }
            if count > 1 && !replace_all {
                bail!("found {count} matches. Set replace_all=true or provide more context.");
            }

            let updated = if replace_all {
                content.replace(search, new)
            } else {
                content.replacen(search, new, 1)
            };

            // Skip write if nothing changed
            if updated == content {
                return Ok(ToolExecution {
                    result: (format!("{} is unchanged", path.display())).into(),
                    artifact: Some(FileChangeArtifact {
                        files: vec![FileArtifact {
                            path: path.display().to_string(),
                            operation: FileOp::Update,
                            diff: None,
                            preview: Some(updated),
                        }],
                        raw_input: None,
                        error: None,
                        status: ToolStatus::Done,
                    }),
                });
            }

            std::fs::write(&path, &updated)?;

            // Send context-based diff to UI (no LCS needed — we know exact position)
            let diff = crate::tool::diff::make_edit_diff(&content, search, new, replace_all);
            for line in &diff {
                let _ = output_tx.send(format!("{line}\n")).await;
            }

            let (adds, dels) = crate::tool::diff::diff_stats(&diff);

            Ok(ToolExecution {
                result: (format!(
                    "Edited {} ({} replacement{}, +{adds} -{dels})",
                    path.display(),
                    count,
                    if count > 1 { "s" } else { "" }
                ))
                .into(),
                artifact: Some(FileChangeArtifact {
                    files: vec![FileArtifact {
                        path: path.display().to_string(),
                        operation: FileOp::Update,
                        diff: Some(diff.join("\n")),
                        preview: Some(updated),
                    }],
                    raw_input: None,
                    error: None,
                    status: ToolStatus::Done,
                }),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn edit_single_replace() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello world").unwrap();

        let tool = EditTool;
        let (tx, _rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let result = tool.execute(serde_json::json!({"path": file.to_str().unwrap(), "old_string": "hello", "new_string": "hi"}), tx, cancel, Default::default()).await.unwrap();

        assert!(result.result.as_text().contains("Edited"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hi world");
    }

    #[tokio::test]
    async fn edit_multiple_without_flag_fails() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "aaa bbb aaa").unwrap();

        let tool = EditTool;
        let (tx, _rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let result = tool.execute(serde_json::json!({"path": file.to_str().unwrap(), "old_string": "aaa", "new_string": "x"}), tx, cancel, Default::default()).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn edit_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "aaa bbb aaa").unwrap();

        let tool = EditTool;
        let (tx, _rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let result = tool.execute(serde_json::json!({"path": file.to_str().unwrap(), "old_string": "aaa", "new_string": "x", "replace_all": true}), tx, cancel, Default::default()).await.unwrap();

        assert!(result.result.as_text().contains("2 replacements"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x bbb x");
    }

    #[tokio::test]
    async fn edit_create_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("new.txt");

        let tool = EditTool;
        let (tx, _rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let result = tool.execute(serde_json::json!({"path": file.to_str().unwrap(), "old_string": "", "new_string": "content"}), tx, cancel, Default::default()).await.unwrap();

        assert!(result.result.as_text().contains("Created"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "content");
    }
}
