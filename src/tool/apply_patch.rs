/// apply_patch tool — Codex-compatible patch format for Deep mode.
mod parse;

use parse::{Hunk, seek_context, seek_sequence};

use crate::core::tool::{Tool, ToolExecution};
use crate::core::types::{FileArtifact, FileChangeArtifact, ToolSchema, ToolStatus};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Codex apply_patch tool.
pub struct ApplyPatchTool;

impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_owned(),
            description: concat!(
                "Apply a patch to create, update, move, or delete files.\n",
                "You MUST read the file before applying a patch, even if you read it earlier.\n",
                "Format:\n",
                "*** Begin Patch\n",
                "*** Add File: <path>     — new file, lines start with +\n",
                "*** Delete File: <path>  — remove file\n",
                "*** Update File: <path>  — edit with hunks\n",
                "  *** Move to: <new_path> (optional rename)\n",
                "  @@ [context]           — hunk header\n",
                "  ' ' context / '-' remove / '+' add\n",
                "*** End Patch\n",
                "Use 3 lines of context before/after changes. ",
                "Use @@ class/function for disambiguation.",
            )
            .to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "The patch text in apply_patch format"
                    }
                },
                "required": ["patch"]
            }),
            streamable_arg: Some("patch".into()),
        }
    }

    fn execute(
        &self,
        args: serde_json::Value,
        output_tx: mpsc::Sender<String>,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<ToolExecution>> + Send + '_>> {
        Box::pin(async move {
            let patch_text = args["patch"].as_str().unwrap_or("");
            let hunks = parse::parse_patch(patch_text)?;
            let result = apply_hunks(&hunks, &output_tx).await?;
            Ok(ToolExecution {
                artifact: Some(FileChangeArtifact {
                    files: hunks.iter().map(hunk_to_artifact).collect(),
                    raw_input: Some(patch_text.to_owned()),
                    error: None,
                    status: ToolStatus::Done,
                }),
                result,
            })
        })
    }
}

fn hunk_to_artifact(hunk: &Hunk) -> FileArtifact {
    let path = match hunk {
        Hunk::Update {
            move_to: Some(move_to),
            ..
        } => move_to.display().to_string(),
        _ => hunk.path().display().to_string(),
    };
    let diff = match hunk {
        Hunk::Add { contents, .. } => Some(crate::tool::diff::make_diff("", contents).join("\n")),
        Hunk::Delete { path } => std::fs::read_to_string(resolve_path(path))
            .ok()
            .map(|old| crate::tool::diff::make_diff(&old, "").join("\n")),
        Hunk::Update {
            path,
            move_to,
            chunks,
        } => build_update_diff(path, move_to.as_ref(), chunks),
    };

    FileArtifact {
        path,
        operation: hunk.operation(),
        diff,
        preview: None,
    }
}

fn build_update_diff(
    path: &Path,
    move_to: Option<&PathBuf>,
    chunks: &[parse::Chunk],
) -> Option<String> {
    let target = move_to.map_or(path, PathBuf::as_path);
    let current = std::fs::read_to_string(resolve_path(target)).ok()?;
    let mut original = current.clone();
    for chunk in chunks.iter().rev() {
        let new_block = chunk.new_lines.join("\n");
        let old_block = chunk.old_lines.join("\n");
        if !new_block.is_empty() {
            original = original.replacen(&new_block, &old_block, 1);
        }
    }
    Some(crate::tool::diff::make_diff(&original, &current).join("\n"))
}

/// Apply parsed hunks to the filesystem.
async fn apply_hunks(hunks: &[Hunk], tx: &mpsc::Sender<String>) -> Result<String> {
    let mut summary = Vec::new();

    for hunk in hunks {
        match hunk {
            Hunk::Add { path, contents } => {
                let abs = resolve_path(path);
                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&abs, contents)?;
                let msg = format!("A {}", path.display());
                let _ = tx.send(msg.clone()).await;
                summary.push(msg);
            }
            Hunk::Delete { path } => {
                let abs = resolve_path(path);
                std::fs::remove_file(&abs)?;
                let msg = format!("D {}", path.display());
                let _ = tx.send(msg.clone()).await;
                summary.push(msg);
            }
            Hunk::Update {
                path,
                move_to,
                chunks,
            } => {
                let abs = resolve_path(path);
                let content = std::fs::read_to_string(&abs)?;
                let mut lines: Vec<String> = content.lines().map(|l| l.to_owned()).collect();

                let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
                let mut line_idx = 0;

                for chunk in chunks {
                    // Use @@ context hint to jump to the right scope
                    let search_from = if let Some(ctx) = &chunk.context {
                        seek_context(&lines, ctx, line_idx).ok_or_else(|| {
                            anyhow::anyhow!(
                                "Failed to find context '{}' in {}",
                                ctx,
                                path.display()
                            )
                        })?
                    } else {
                        line_idx
                    };
                    let found = seek_sequence(&lines, &chunk.old_lines, search_from, chunk.is_eof);
                    if let Some(start) = found {
                        replacements.push((start, chunk.old_lines.len(), chunk.new_lines.clone()));
                        line_idx = start + chunk.old_lines.len();
                    } else {
                        anyhow::bail!(
                            "Failed to find sequence in {}:\n{}",
                            path.display(),
                            chunk.old_lines.join("\n")
                        );
                    }
                }

                replacements.sort_by_key(|r| std::cmp::Reverse(r.0));
                for (start, old_len, new_lines) in &replacements {
                    for _ in 0..*old_len {
                        if *start < lines.len() {
                            lines.remove(*start);
                        }
                    }
                    for (offset, line) in new_lines.iter().enumerate() {
                        lines.insert(start + offset, line.clone());
                    }
                }

                let new_content = lines.join("\n") + "\n";
                let target = move_to
                    .as_ref()
                    .map(|p| resolve_path(p))
                    .unwrap_or(abs.clone());

                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&target, &new_content)?;

                if move_to.is_some() && target != abs {
                    std::fs::remove_file(&abs)?;
                }

                let prefix = if move_to.is_some() { "R" } else { "M" };
                let msg = format!("{prefix} {}", path.display());
                let _ = tx.send(msg.clone()).await;
                summary.push(msg);
            }
        }
    }

    Ok(format!(
        "Success. Updated the following files:\n{}",
        summary.join("\n")
    ))
}

fn resolve_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn apply_add_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");

        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+hello\n*** End Patch",
            file.display()
        );
        let hunks = parse::parse_patch(&patch).unwrap();
        let (tx, _rx) = mpsc::channel(64);
        let result = apply_hunks(&hunks, &tx).await.unwrap();
        assert!(result.contains("A "));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello\n");

        let patch = format!(
            "*** Begin Patch\n*** Delete File: {}\n*** End Patch",
            file.display()
        );
        let hunks = parse::parse_patch(&patch).unwrap();
        let result = apply_hunks(&hunks, &tx).await.unwrap();
        assert!(result.contains("D "));
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn apply_update() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("code.py");
        std::fs::write(&file, "foo\nbar\nbaz\n").unwrap();

        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n foo\n-bar\n+BAR\n*** End Patch",
            file.display()
        );
        let hunks = parse::parse_patch(&patch).unwrap();
        let (tx, _rx) = mpsc::channel(64);
        let result = apply_hunks(&hunks, &tx).await.unwrap();
        assert!(result.contains("M "));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "foo\nBAR\nbaz\n");
    }

    #[tokio::test]
    async fn tool_execute_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("e2e.rs");
        std::fs::write(&file, "fn main() {\n    println!(\"old\");\n}\n").unwrap();

        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@ fn main()\n fn main() {{\n-    println!(\"old\");\n+    println!(\"new\");\n*** End Patch",
            file.display()
        );

        let tool = ApplyPatchTool;
        let args = serde_json::json!({"patch": patch});
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let result = tool.execute(args, tx, cancel).await.unwrap();
        assert!(result.result.contains("Success"));
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains("println!(\"new\")"));
    }

    #[tokio::test]
    async fn tool_execute_multi_hunk() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("lib.rs");
        std::fs::write(&existing, "use std::io;\n\nfn read() {}\n\nfn write() {}\n").unwrap();
        let new_file = dir.path().join("util.rs");

        let patch = format!(
            "*** Begin Patch\n*** Add File: {new}\n+pub fn helper() {{}}\n*** Update File: {lib}\n@@\n use std::io;\n+use std::fs;\n*** End Patch",
            new = new_file.display(),
            lib = existing.display(),
        );

        let tool = ApplyPatchTool;
        let args = serde_json::json!({"patch": patch});
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let result = tool.execute(args, tx, cancel).await.unwrap();
        assert!(result.result.contains("Success"));
        assert!(new_file.exists());
        let lib_content = std::fs::read_to_string(&existing).unwrap();
        assert!(lib_content.contains("use std::fs;"));
    }

    #[tokio::test]
    async fn apply_update_reports_missing_context_precisely() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("picker.rs");
        std::fs::write(&file, "#[test]\nfn lines_rendering() {\n}\n").unwrap();

        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@ fn missing_name() {{\n fn lines_rendering() {{\n+    let x = 1;\n*** End Patch",
            file.display()
        );
        let hunks = parse::parse_patch(&patch).unwrap();
        let (tx, _rx) = mpsc::channel(64);
        let err = apply_hunks(&hunks, &tx).await.unwrap_err().to_string();
        assert!(err.contains("Failed to find context 'fn missing_name() {'"));
    }
}
