/// Read tool — read files or list directories with line numbers.
use crate::core::tool::Tool;
use crate::core::types::ToolSchema;
use anyhow::{Result, bail};
use std::fs;
use std::future::Future;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_LEN: usize = 2000;
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10 MB — reject larger files without offset/limit

/// Common binary file extensions — skip these entirely.
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "avif", "mp3", "mp4", "wav", "ogg", "flac",
    "avi", "mkv", "mov", "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "wasm", "pyc", "class", "o",
    "so", "dylib", "dll", "exe", "ttf", "otf", "woff", "woff2", "eot", "sqlite", "db",
];

/// Reads files with line numbers or lists directory contents.
pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "Read".into(),
            description: concat!(
                "Read a file or list a directory. Returns content with line numbers (e.g. '1: content').\n",
                "- Path must be absolute.\n",
                "- Default reads up to 2000 lines. Use offset/limit for large files.\n",
                "- Files larger than 10MB require offset and limit parameters.\n",
                "- Avoid tiny repeated slices (e.g. 30-line chunks). Read a larger window instead.\n",
                "- Call in parallel for multiple files you need to read.\n",
                "- For directories, returns entries with trailing / for subdirectories.\n",
                "- Not for searching — use `Grep` for content search, `Glob` for file search.",
            ).into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to read" },
                    "offset": { "type": "number", "description": "Start line (1-indexed)" },
                    "limit": { "type": "number", "description": "Max lines (default 2000)" }
                },
                "required": ["path"]
            }),
            streamable_arg: None,
        }
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _output_tx: mpsc::Sender<String>,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        Box::pin(async move {
            let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path_str.is_empty() {
                bail!("missing path argument");
            }

            let path = PathBuf::from(path_str)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(path_str));

            let meta = match fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => {
                    // File not found — suggest similar files
                    let suggestion = suggest_similar(&path);
                    let msg = format!("File not found: {}", path.display());
                    if let Some(s) = suggestion {
                        bail!("{msg}. Did you mean {s}?");
                    }
                    bail!("{msg}");
                }
            };

            if meta.is_dir() {
                let mut entries: Vec<String> = fs::read_dir(&path)?
                    .filter_map(|e| e.ok())
                    .map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                            format!("{name}/")
                        } else {
                            name
                        }
                    })
                    .collect();
                entries.sort();
                return Ok(entries.join("\n"));
            }

            // Binary file check
            if let Some(ext) = path.extension().and_then(|e| e.to_str())
                && BINARY_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
            {
                bail!(
                    "Cannot read binary file ({}). Use appropriate tools for binary analysis.",
                    ext
                );
            }

            let offset = args
                .get("offset")
                .and_then(|v| v.as_u64())
                .unwrap_or(1)
                .max(1) as usize;
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_LIMIT as u64) as usize;
            let has_explicit_range = args.get("offset").is_some() || args.get("limit").is_some();

            // File size guard — reject large files without explicit range
            if !has_explicit_range && meta.len() > MAX_FILE_SIZE {
                bail!(
                    "File too large ({:.1} MB). Use offset and limit to read specific portions.",
                    meta.len() as f64 / 1_048_576.0
                );
            }

            let file = fs::File::open(&path)?;
            let mut reader = BufReader::new(file);

            // Strip UTF-8 BOM
            let mut bom = [0u8; 3];
            let bom_len = reader.read(&mut bom)?;
            if bom_len < 3 || bom != [0xEF, 0xBB, 0xBF] {
                // Not a BOM — seek back (re-open since BufReader doesn't support seek easily)
                drop(reader);
                let file = fs::File::open(&path)?;
                reader = BufReader::new(file);
            }

            let mut result = String::new();
            let mut count = 0;
            let mut total_lines = 0;

            for (i, line) in reader.lines().enumerate() {
                let line = line?;
                let line_num = i + 1;
                total_lines = line_num;
                if line_num < offset {
                    continue;
                }
                if count >= limit {
                    continue;
                } // keep counting total_lines
                if line.len() > MAX_LINE_LEN {
                    result.push_str(&format!("{line_num}: {}...\n", &line[..MAX_LINE_LEN]));
                } else {
                    result.push_str(&format!("{line_num}: {line}\n"));
                }
                count += 1;
            }

            if result.is_empty() {
                if total_lines == 0 {
                    return Ok("(empty file)".into());
                }
                return Ok(format!(
                    "(file has {total_lines} lines, offset {offset} is past end)"
                ));
            }

            // Append total line count hint for model context
            if total_lines > count + offset.saturating_sub(1) {
                result.push_str(&format!("\n({total_lines} lines total)\n"));
            }

            Ok(result)
        })
    }
}

/// Public wrapper for edit tool "did you mean?" suggestions.
pub fn suggest_similar_file(path: &std::path::Path) -> Option<String> {
    suggest_similar(path)
}

/// Suggest a similar filename in the same directory.
fn suggest_similar(path: &std::path::Path) -> Option<String> {
    let parent = path.parent()?;
    let target = path.file_name()?.to_string_lossy().to_lowercase();
    let entries = fs::read_dir(parent).ok()?;

    let mut best: Option<(usize, String)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let dist = str_distance(&target, &name.to_lowercase());
        if dist <= 3 && (best.is_none() || dist < best.as_ref().unwrap().0) {
            let full = parent.join(&name).to_string_lossy().into_owned();
            best = Some((dist, full));
        }
    }
    best.map(|(_, p)| p)
}

/// Simple edit distance (Levenshtein), capped for performance.
fn str_distance(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (n, m) = (a.len(), b.len());
    if n.abs_diff(m) > 3 {
        return 4;
    } // early exit
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn read_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "line1\nline2\nline3\n").unwrap();

        let tool = ReadTool;
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(
                serde_json::json!({"path": file.to_str().unwrap()}),
                tx,
                cancel,
            )
            .await
            .unwrap();

        assert!(result.contains("1: line1"));
        assert!(result.contains("3: line3"));
    }

    #[tokio::test]
    async fn read_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        let tool = ReadTool;
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(
                serde_json::json!({"path": dir.path().to_str().unwrap()}),
                tx,
                cancel,
            )
            .await
            .unwrap();

        assert!(result.contains("a.txt"));
        assert!(result.contains("sub/"));
    }

    #[tokio::test]
    async fn read_with_offset_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.txt");
        let mut f = std::fs::File::create(&file).unwrap();
        for i in 1..=100 {
            writeln!(f, "line {i}").unwrap();
        }

        let tool = ReadTool;
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(
                serde_json::json!({"path": file.to_str().unwrap(), "offset": 50, "limit": 5}),
                tx,
                cancel,
            )
            .await
            .unwrap();

        assert!(result.contains("50: line 50"));
        assert!(result.contains("54: line 54"));
        assert!(result.contains("100 lines total"));
    }

    #[tokio::test]
    async fn read_binary_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("image.png");
        std::fs::write(&file, b"\x89PNG\r\n").unwrap();

        let tool = ReadTool;
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(
                serde_json::json!({"path": file.to_str().unwrap()}),
                tx,
                cancel,
            )
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("binary"));
    }

    #[tokio::test]
    async fn read_not_found_suggests() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main(){}").unwrap();

        let tool = ReadTool;
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(
                serde_json::json!({"path": dir.path().join("mian.rs").to_str().unwrap()}),
                tx,
                cancel,
            )
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Did you mean"), "should suggest: {err}");
        assert!(err.contains("main.rs"), "should suggest main.rs: {err}");
    }

    #[test]
    fn edit_distance() {
        assert_eq!(str_distance("main", "mian"), 2);
        assert_eq!(str_distance("test", "test"), 0);
        assert_eq!(str_distance("abc", "xyz"), 3);
    }

    #[tokio::test]
    async fn read_shows_total_lines() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lines.txt");
        let mut f = std::fs::File::create(&file).unwrap();
        for i in 1..=50 {
            writeln!(f, "line {i}").unwrap();
        }

        let tool = ReadTool;
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(
                serde_json::json!({"path": file.to_str().unwrap(), "limit": 5}),
                tx,
                cancel,
            )
            .await
            .unwrap();

        assert!(
            result.contains("50 lines total"),
            "should show total: {result}"
        );
    }
}
