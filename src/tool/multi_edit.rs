/// MultiEdit tool — apply several edits to a single file in one call.
///
/// Edit is round-trip-heavy: every single-location change costs a full
/// provider response cycle. Audit of real sessions showed 68 edits
/// against the same file with a p50 gap of 6 turns between them —
/// classic "patch one hunk at a time" behaviour. MultiEdit collapses
/// those into a single tool call so the model pays one round trip for
/// N atomic edits.
///
/// Semantics mirror a sequence of `Edit` calls on one file:
///
///   * Edits apply in order. Each edit sees the file content produced
///     by the previous edit in the same call.
///   * Fail-fast: the first mismatch (or ambiguous match without
///     `replace_all`) aborts the whole call — no partial writes.
///   * Curly-quote normalisation reuses `edit::find_actual_string` so
///     pasted-in snippets behave the same as with `Edit`.
use crate::core::tool::{Tool, ToolExecution};
use crate::core::types::{FileArtifact, FileChangeArtifact, FileOp, ToolSchema, ToolStatus};
use anyhow::{Result, bail};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_EDIT_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10 MB — matches Edit.

/// Apply multiple edits to one file in a single tool call.
pub struct MultiEditTool;

impl Tool for MultiEditTool {
    fn name(&self) -> &str {
        "MultiEdit"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "MultiEdit".into(),
            description: concat!(
                "Apply multiple edits to a single file in one call.\n",
                "- Prefer over `Edit` whenever you have more than one change to the same file — each Edit is a full round trip.\n",
                "- Edits apply sequentially in order; each edit sees the file after previous edits.\n",
                "- Fail-fast: first mismatch aborts the entire call (no partial writes).\n",
                "- Each edit has the same rules as Edit: old_string must match exactly once unless replace_all=true; old_string ≠ new_string.",
            )
            .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path to the file. ALWAYS generate this argument first."
                    },
                    "edits": {
                        "type": "array",
                        "description": "Ordered list of edits to apply to this file.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": { "type": "string", "description": "Exact string to find" },
                                "new_string": { "type": "string", "description": "Replacement string" },
                                "replace_all": {
                                    "type": "boolean",
                                    "description": "Replace all occurrences (default false)"
                                }
                            },
                            "required": ["old_string", "new_string"]
                        }
                    }
                },
                "required": ["path", "edits"]
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
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecution>> + Send + '_>> {
        Box::pin(async move {
            let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path_str.is_empty() {
                bail!("missing path argument");
            }

            let edits = args
                .get("edits")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow::anyhow!("missing or invalid edits array"))?;
            if edits.is_empty() {
                bail!("edits array is empty");
            }

            let path = PathBuf::from(path_str);
            let original = match std::fs::read_to_string(&path) {
                Ok(c) => c,
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

            if let Ok(meta) = std::fs::metadata(&path)
                && meta.len() > MAX_EDIT_FILE_SIZE
            {
                bail!(
                    "File too large ({:.1} MB). Read the file, apply changes in memory, and use Write to replace it.",
                    meta.len() as f64 / 1_048_576.0
                );
            }

            // Apply edits in order over an in-memory buffer. Any
            // mismatch aborts without touching disk.
            let mut content = original.clone();
            let mut total_replacements = 0usize;
            for (i, raw) in edits.iter().enumerate() {
                let old = raw.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
                let new = raw.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
                let replace_all = raw
                    .get("replace_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if old.is_empty() {
                    bail!("edit {i}: old_string is empty (use Write to create a file)");
                }
                if old == new {
                    bail!("edit {i}: old_string and new_string are identical");
                }

                // Reuse Edit's curly-quote fallback so pasted snippets
                // match the same way they do with the single-edit tool.
                let actual_old = crate::tool::edit::find_actual_string(&content, old);
                let search = actual_old.as_deref().unwrap_or(old);

                let count = content.matches(search).count();
                if count == 0 {
                    bail!("edit {i}: old_string not found in file");
                }
                if count > 1 && !replace_all {
                    bail!(
                        "edit {i}: found {count} matches. Set replace_all=true or provide more context."
                    );
                }
                content = if replace_all {
                    content.replace(search, new)
                } else {
                    content.replacen(search, new, 1)
                };
                total_replacements += if replace_all { count } else { 1 };
            }

            if content == original {
                return Ok(ToolExecution {
                    result: (format!("{} is unchanged", path.display())).into(),
                    artifact: Some(FileChangeArtifact {
                        files: vec![FileArtifact {
                            path: path.display().to_string(),
                            operation: FileOp::Update,
                            diff: None,
                            preview: Some(content),
                        }],
                        raw_input: None,
                        error: None,
                        status: ToolStatus::Done,
                    }),
                });
            }

            std::fs::write(&path, &content)?;

            // Diff against the original so the UI surfaces the net
            // change of the batch, not every intermediate step.
            let diff = crate::tool::diff::make_diff(&original, &content);
            for line in &diff {
                let _ = output_tx.send(format!("{line}\n")).await;
            }
            let (adds, dels) = crate::tool::diff::diff_stats(&diff);

            Ok(ToolExecution {
                result: (format!(
                    "Edited {} ({} edit{}, {} replacement{}, +{adds} -{dels})",
                    path.display(),
                    edits.len(),
                    if edits.len() > 1 { "s" } else { "" },
                    total_replacements,
                    if total_replacements > 1 { "s" } else { "" }
                ))
                .into(),
                artifact: Some(FileChangeArtifact {
                    files: vec![FileArtifact {
                        path: path.display().to_string(),
                        operation: FileOp::Update,
                        diff: Some(diff.join("\n")),
                        preview: Some(content),
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

    async fn run(args: serde_json::Value) -> Result<ToolExecution> {
        let tool = MultiEditTool;
        let (tx, _rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        tool.execute(args, tx, cancel, Default::default()).await
    }

    #[tokio::test]
    async fn applies_edits_sequentially() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one two three").unwrap();

        let result = run(serde_json::json!({
            "path": file.to_str().unwrap(),
            "edits": [
                { "old_string": "one", "new_string": "1" },
                { "old_string": "two", "new_string": "2" },
                { "old_string": "three", "new_string": "3" },
            ]
        }))
        .await
        .unwrap();

        assert!(result.result.as_text().contains("3 edits"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "1 2 3");
    }

    #[tokio::test]
    async fn later_edit_sees_earlier_edit_result() {
        // Edit 2 targets a string produced by Edit 1.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "alpha").unwrap();

        let result = run(serde_json::json!({
            "path": file.to_str().unwrap(),
            "edits": [
                { "old_string": "alpha", "new_string": "beta" },
                { "old_string": "beta",  "new_string": "gamma" },
            ]
        }))
        .await
        .unwrap();

        assert!(result.result.as_text().contains("Edited"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "gamma");
    }

    #[tokio::test]
    async fn first_mismatch_aborts_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "keep me\n").unwrap();

        let err = run(serde_json::json!({
            "path": file.to_str().unwrap(),
            "edits": [
                { "old_string": "keep me", "new_string": "ok" },
                { "old_string": "does not exist", "new_string": "x" },
                { "old_string": "ok", "new_string": "never runs" },
            ]
        }))
        .await
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("edit 1"),
            "error mentions the failing edit index: {err}"
        );
        assert!(err.contains("not found"));
        // File untouched.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep me\n");
    }

    #[tokio::test]
    async fn ambiguous_match_requires_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x x x\n").unwrap();

        let err = run(serde_json::json!({
            "path": file.to_str().unwrap(),
            "edits": [{ "old_string": "x", "new_string": "y" }]
        }))
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("3 matches"));

        // With replace_all the same input succeeds and reports 3
        // replacements inside a single edit.
        let ok = run(serde_json::json!({
            "path": file.to_str().unwrap(),
            "edits": [
                { "old_string": "x", "new_string": "y", "replace_all": true }
            ]
        }))
        .await
        .unwrap();
        assert!(ok.result.as_text().contains("3 replacement"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "y y y\n");
    }

    #[tokio::test]
    async fn empty_edits_array_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "content").unwrap();
        let err = run(serde_json::json!({
            "path": file.to_str().unwrap(),
            "edits": []
        }))
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("empty"));
    }

    #[tokio::test]
    async fn identical_strings_error_points_at_edit_index() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "abc").unwrap();
        let err = run(serde_json::json!({
            "path": file.to_str().unwrap(),
            "edits": [
                { "old_string": "abc", "new_string": "ABC" },
                { "old_string": "ABC", "new_string": "ABC" },
            ]
        }))
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("edit 1"));
        assert!(err.contains("identical"));
        // File rolled back (edit 0 was in-memory only).
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "abc");
    }
}
