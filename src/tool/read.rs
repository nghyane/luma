/// Read tool — read files or list directories with line numbers.
use crate::core::tool::{Tool, ToolExecution};
use crate::core::types::ToolSchema;
use anyhow::{Result, bail};
use std::fs;
use std::future::Future;
use std::io::{BufRead, BufReader, Read};
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_LEN: usize = 2000;
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10 MB — reject larger files without offset/limit

/// Common binary file extensions — skip these entirely.
/// Image extensions (png/jpg/jpeg/gif/webp/bmp/ico/avif) are handled by
/// the image branch below, not rejected outright.
const BINARY_EXTENSIONS: &[&str] = &[
    "mp3", "mp4", "wav", "ogg", "flac", "avi", "mkv", "mov", "zip", "tar", "gz", "bz2", "xz", "7z",
    "rar", "wasm", "pyc", "class", "o", "so", "dylib", "dll", "exe", "ttf", "otf", "woff", "woff2",
    "eot", "sqlite", "db",
];

/// Image extensions this tool can attach to the model or describe as metadata.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// Maximum size for image reads. Anthropic caps uploads at 5 MB; going
/// larger triggers provider-side reject. Resize is deferred to a future
/// RFC — for now the tool bails with guidance.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

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
                "- `path` can be an absolute filesystem path OR an `artifact://` URI:\n",
                "  - `artifact://ev/{id}` — re-read a stored evidence blob from this session\n",
                "    (ids appear in prior tool summaries, e.g. 'stored as artifact://ev/ev_abc').\n",
                "  - `artifact://skill/{name}` — load a skill's instructions\n",
                "    (names come from the `<available_skills>` catalog; frontmatter is stripped).\n",
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
        caps: crate::core::tool::ModelCaps,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecution>> + Send + '_>> {
        Box::pin(async move {
            let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path_str.is_empty() {
                bail!("missing path argument");
            }

            // URI schemes (artifact://ev/, artifact://skill/) resolve to
            // a concrete path plus a post-read transformation flag.
            let resolved = match crate::core::session::resolve_resource_path(path_str) {
                Ok(r) => r,
                Err(e) => bail!("{e}"),
            };
            let (raw_path, strip_frontmatter) = match resolved {
                crate::core::session::Resolved::Path(p) => (p, false),
                crate::core::session::Resolved::PathStripFrontmatter(p) => (p, true),
            };
            let path = raw_path.canonicalize().unwrap_or(raw_path);

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
                return Ok(ToolExecution {
                    result: (entries.join("\n")).into(),
                    artifact: None,
                });
            }

            // Image branch — returns an image attachment (vision-capable
            // model) or text metadata (text-only model). Sidesteps the
            // BINARY_EXTENSIONS gate entirely.
            if let Some(ext) = path.extension().and_then(|e| e.to_str())
                && IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
            {
                return read_image(&path, ext, &meta, caps);
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

            // For skill artifacts, skip past the leading `---…---` YAML
            // frontmatter so line 1 lines up with the first body line
            // (the frontmatter is already advertised in the skill
            // catalog inside the system prompt).
            let skip_lines = if strip_frontmatter {
                count_frontmatter_lines(&path)?
            } else {
                0
            };

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
                if i < skip_lines {
                    continue;
                }
                let line_num = i - skip_lines + 1;
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
                    return Ok(ToolExecution {
                        result: "(empty file)".into(),
                        artifact: None,
                    });
                }
                return Ok(ToolExecution {
                    result: (format!("(file has {total_lines} lines, offset {offset} is past end)")).into(),
                    artifact: None,
                });
            }

            // Append total line count hint for model context
            if total_lines > count + offset.saturating_sub(1) {
                result.push_str(&format!("\n({total_lines} lines total)\n"));
            }

            // Tiny-window pushback: the agent pays a full round trip per
            // Read, so a 30-line slice out of a 1000-line file is almost
            // always a loss versus one 300-500 line read. Audit of 9
            // real sessions counted 57 tiny reads, 30 on a single file
            // — nudge the model to widen the window next time.
            const TINY_LIMIT_THRESHOLD: usize = 30;
            if args.get("limit").is_some() && limit < TINY_LIMIT_THRESHOLD && total_lines > limit {
                result.push_str(&format!(
                    "\n[hint: read {limit} lines; consider a wider window (100-500) next time — one call beats multiple round trips.]\n"
                ));
            }

            Ok(ToolExecution {
                result: result.into(),
                artifact: None,
            })
        })
    }
}

/// Public wrapper for edit tool "did you mean?" suggestions.
pub fn suggest_similar_file(path: &std::path::Path) -> Option<String> {
    suggest_similar(path)
}

/// Read an image file into a `ToolExecution`.
///
/// Branches on `caps.vision`:
/// - **Vision model**: save bytes into the session image store and return
///   a `ToolResultBody::Items` with a short metadata `Text` item followed
///   by an `Image` item referencing the saved id. The provider's
///   `ImageResolver` pulls the bytes back as base64 at send time.
/// - **Text-only model**: return metadata text only. The model sees file
///   type/size/dimensions (when cheap to parse) and is explicitly told
///   the visual content cannot be included, so it can describe to the
///   user or fall back to OCR via Bash instead of hallucinating.
///
/// Oversize files bail with a clear message rather than silently
/// truncating — a malformed/huge image would exceed provider limits and
/// produce a 4xx far from the tool call site.
fn read_image(
    path: &std::path::Path,
    ext: &str,
    meta: &std::fs::Metadata,
    caps: crate::core::tool::ModelCaps,
) -> Result<ToolExecution> {
    use crate::core::types::{ToolResultBody, ToolResultItem};

    if meta.len() > MAX_IMAGE_BYTES {
        bail!(
            "Image too large ({:.1} MB, max {} MB). Resize or crop before reading.",
            meta.len() as f64 / 1_048_576.0,
            MAX_IMAGE_BYTES / 1_048_576,
        );
    }

    let data = fs::read(path)?;
    if data.is_empty() {
        bail!("Image file is empty: {}", path.display());
    }

    let media_type = media_type_from_ext(ext);
    let size_kb = data.len().div_ceil(1024);
    let dims = parse_png_dimensions(&data);
    let dim_txt = dims
        .map(|(w, h)| format!("{w}×{h} "))
        .unwrap_or_default();

    if !caps.vision {
        // Drop the bytes — no need to save to the session store when
        // nothing will reference them. Metadata alone goes back.
        let text = format!(
            "{media_type} image: {dim_txt}{size_kb} KB. \
             This model does not support image input — describe the contents \
             to the user or use Bash/OCR tools for text extraction.",
        );
        return Ok(ToolExecution {
            result: text.into(),
            artifact: None,
        });
    }

    let session_id = crate::core::session::current_session_id()
        .ok_or_else(|| anyhow::anyhow!("no active session — image cannot be attached"))?;
    let id = crate::core::session::save_image(&session_id, &data, ext);

    let text = format!("{media_type}: {dim_txt}{size_kb} KB (attached)");
    Ok(ToolExecution {
        result: ToolResultBody::Items(vec![
            ToolResultItem::Text { text },
            ToolResultItem::Image {
                media_type: media_type.to_owned(),
                id,
            },
        ]),
        artifact: None,
    })
}

/// Map a file extension (lowercase or not) to the closest image media type
/// the providers understand. Unknown falls back to `image/png` — safer
/// than refusing, since all modern gateways treat PNG as lingua franca.
fn media_type_from_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/png",
    }
}

/// Parse width/height from PNG header (IHDR chunk at offset 16-23).
/// Returns `None` for any other format — JPEG/WebP require walking
/// markers/RIFF chunks, which isn't worth pulling in an image crate for
/// a nice-to-have metadata field.
fn parse_png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if !data.starts_with(&[0x89, b'P', b'N', b'G']) || data.len() < 24 {
        return None;
    }
    let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
    Some((w, h))
}

/// Count the lines taken up by a leading `---…---` YAML frontmatter
/// block — inclusive of both fence lines. Returns 0 when the file
/// doesn't start with one, so callers can use the value as an offset
/// unconditionally.
fn count_frontmatter_lines(path: &std::path::Path) -> std::io::Result<usize> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();
    match lines.next() {
        Some(Ok(first)) if first.trim() == "---" => {
            let mut skipped = 1;
            for line in lines {
                skipped += 1;
                if line?.trim() == "---" {
                    return Ok(skipped);
                }
            }
            // Unterminated frontmatter — treat as no frontmatter to
            // avoid swallowing the whole file.
            Ok(0)
        }
        _ => Ok(0),
    }
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

    #[test]
    fn count_frontmatter_counts_delimiter_lines_inclusive() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("skill.md");
        std::fs::write(&file, "---\nname: x\ndesc: y\n---\nbody\nmore\n").unwrap();
        assert_eq!(count_frontmatter_lines(&file).unwrap(), 4);
    }

    #[test]
    fn count_frontmatter_returns_zero_without_delimiter() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain.md");
        std::fs::write(&file, "# title\nbody\n").unwrap();
        assert_eq!(count_frontmatter_lines(&file).unwrap(), 0);
    }

    #[test]
    fn count_frontmatter_returns_zero_on_unterminated_fence() {
        // An opening `---` with no closing fence: treat as no
        // frontmatter so the Read tool doesn't swallow the whole file.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("broken.md");
        std::fs::write(&file, "---\nname: x\nno close\n").unwrap();
        assert_eq!(count_frontmatter_lines(&file).unwrap(), 0);
    }

    #[tokio::test]
    async fn read_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "line1\nline2\nline3\n").unwrap();

        let tool = ReadTool;
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(serde_json::json!({"path": file.to_str().unwrap()}), tx, cancel, Default::default())
            .await
            .unwrap();

        assert!(result.result.as_text().contains("1: line1"));
        assert!(result.result.as_text().contains("3: line3"));
    }

    #[tokio::test]
    async fn tiny_limit_appends_hint() {
        // Audit found 57 Read calls with limit < 30 across 9 sessions;
        // surface a hint to widen the window so the agent stops paying
        // a full round trip per 20-line slice.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.txt");
        let body: String = (1..=500).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&file, body).unwrap();

        let tool = ReadTool;
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(serde_json::json!({"path": file.to_str().unwrap(), "limit": 10}), tx, cancel, Default::default())
            .await
            .unwrap();

        assert!(result.result.as_text().contains("hint: read 10 lines"));
        assert!(result.result.as_text().contains("wider window"));
    }

    #[tokio::test]
    async fn normal_limit_no_hint() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.txt");
        let body: String = (1..=500).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&file, body).unwrap();

        let tool = ReadTool;
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(serde_json::json!({"path": file.to_str().unwrap(), "limit": 200}), tx, cancel, Default::default())
            .await
            .unwrap();

        assert!(!result.result.as_text().contains("hint:"));
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
            .execute(serde_json::json!({"path": dir.path().to_str().unwrap()}), tx, cancel, Default::default())
            .await
            .unwrap();

        assert!(result.result.as_text().contains("a.txt"));
        assert!(result.result.as_text().contains("sub/"));
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
            .execute(serde_json::json!({"path": file.to_str().unwrap(), "offset": 50, "limit": 5}), tx, cancel, Default::default())
            .await
            .unwrap();

        assert!(result.result.as_text().contains("50: line 50"));
        assert!(result.result.as_text().contains("54: line 54"));
        assert!(result.result.as_text().contains("100 lines total"));
    }

    #[tokio::test]
    async fn read_binary_rejected() {
        // Non-image binary extensions remain rejected (images now have their
        // own capability-aware branch; see `read_image_without_vision_returns_metadata`).
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("libfoo.dylib");
        std::fs::write(&file, b"\x7fELF").unwrap();

        let tool = ReadTool;
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(
                serde_json::json!({"path": file.to_str().unwrap()}),
                tx,
                cancel,
                Default::default(),
            )
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("binary"));
    }

    #[tokio::test]
    async fn read_image_without_vision_returns_metadata_only() {
        // Minimal valid PNG: 8-byte signature + IHDR with 1×1 dimensions.
        // Dimensions live at bytes 16-23 (big-endian u32 each), so parser
        // picks them up and the metadata string mentions "1×1".
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tiny.png");
        let mut png = Vec::new();
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        png.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1u32.to_be_bytes()); // width
        png.extend_from_slice(&1u32.to_be_bytes()); // height
        png.extend_from_slice(&[8, 6, 0, 0, 0]); // bit depth / color type / rest
        std::fs::write(&file, &png).unwrap();

        let tool = ReadTool;
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let caps = crate::core::tool::ModelCaps { vision: false };
        let exec = tool
            .execute(
                serde_json::json!({"path": file.to_str().unwrap()}),
                tx,
                cancel,
                caps,
            )
            .await
            .expect("image read succeeds even without vision");

        let text = exec.result.as_text();
        assert!(text.contains("image/png"), "media type reported: {text}");
        assert!(text.contains("1×1"), "dimensions reported: {text}");
        assert!(
            text.contains("does not support image input"),
            "caller learns why bytes are not attached: {text}"
        );
        assert!(!exec.result.has_images());
    }

    #[tokio::test]
    async fn read_image_too_large_bails() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.png");
        // 6 MB > 5 MB cap. Contents don't need to be a valid PNG — the
        // size check runs before any parsing.
        std::fs::write(&file, vec![0u8; 6 * 1024 * 1024]).unwrap();

        let tool = ReadTool;
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(
                serde_json::json!({"path": file.to_str().unwrap()}),
                tx,
                cancel,
                Default::default(),
            )
            .await;

        let err = result.unwrap_err().to_string();
        assert!(err.contains("too large"), "msg: {err}");
    }

    #[tokio::test]
    async fn read_not_found_suggests() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main(){}").unwrap();

        let tool = ReadTool;
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let result = tool
            .execute(serde_json::json!({"path": dir.path().join("mian.rs").to_str().unwrap()}), tx, cancel, Default::default())
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
            .execute(serde_json::json!({"path": file.to_str().unwrap(), "limit": 5}), tx, cancel, Default::default())
            .await
            .unwrap();

        assert!(
            result.result.as_text().contains("50 lines total"),
            "should show total: {result:?}"
        );
    }

    #[test]
    fn parse_png_dimensions_reads_ihdr() {
        let mut png = Vec::new();
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1920u32.to_be_bytes());
        png.extend_from_slice(&1080u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        assert_eq!(parse_png_dimensions(&png), Some((1920, 1080)));
    }

    #[test]
    fn parse_png_dimensions_rejects_non_png() {
        assert_eq!(parse_png_dimensions(b"\xff\xd8jpeg..."), None);
        assert_eq!(parse_png_dimensions(b"short"), None);
    }

    #[test]
    fn media_type_from_ext_covers_known_extensions() {
        assert_eq!(media_type_from_ext("png"), "image/png");
        assert_eq!(media_type_from_ext("PNG"), "image/png");
        assert_eq!(media_type_from_ext("jpg"), "image/jpeg");
        assert_eq!(media_type_from_ext("jpeg"), "image/jpeg");
        assert_eq!(media_type_from_ext("gif"), "image/gif");
        assert_eq!(media_type_from_ext("webp"), "image/webp");
        assert_eq!(media_type_from_ext("bmp"), "image/bmp");
        // Unknown falls back to PNG — safe default, every gateway understands it.
        assert_eq!(media_type_from_ext("xyz"), "image/png");
    }
}
