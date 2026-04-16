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

/// Hard file-size gate before we even try to decode. Files larger than this
/// are almost certainly not raster images we can usefully resize.
const MAX_IMAGE_READ_BYTES: u64 = 50 * 1024 * 1024; // 50 MB

/// Target raw byte size so that base64-encoding stays under the 5 MB API
/// limit (base64 overhead ≈ 4/3, so 3.75 MB × 4/3 ≈ 5 MB).
const IMAGE_TARGET_RAW_BYTES: usize = 3 * 1024 * 1024 + 768 * 1024; // 3.75 MB

/// Maximum dimension (width or height) sent to any provider.
const IMAGE_MAX_DIMENSION: u32 = 2000;

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
                    result: (format!(
                        "(file has {total_lines} lines, offset {offset} is past end)"
                    ))
                    .into(),
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
/// - **Vision model**: run preprocessing pipeline (resize + compress if needed),
///   save bytes into the session image store, return metadata `Text` + `Image`.
/// - **Text-only model**: return metadata text only.
fn read_image(
    path: &std::path::Path,
    ext: &str,
    meta: &std::fs::Metadata,
    caps: crate::core::tool::ModelCaps,
) -> Result<ToolExecution> {
    use crate::core::types::{ToolResultBody, ToolResultItem};

    if meta.len() > MAX_IMAGE_READ_BYTES {
        bail!(
            "Image too large ({:.1} MB). Maximum readable size is {} MB.",
            meta.len() as f64 / 1_048_576.0,
            MAX_IMAGE_READ_BYTES / 1_048_576,
        );
    }

    let data = fs::read(path)?;
    if data.is_empty() {
        bail!("Image file is empty: {}", path.display());
    }

    let media_type = media_type_from_ext(ext);

    if !caps.vision {
        let dims = parse_png_dimensions(&data);
        let dim_txt = dims.map(|(w, h)| format!("{w}×{h} ")).unwrap_or_default();
        let size_kb = data.len().div_ceil(1024);
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

    let (processed, out_ext, meta_text) = preprocess_image(data, ext)?;
    let id = crate::core::session::save_image(&session_id, &processed, out_ext);

    Ok(ToolExecution {
        result: ToolResultBody::Items(vec![
            ToolResultItem::Text { text: meta_text },
            ToolResultItem::Image {
                media_type: media_type_from_ext(out_ext).to_owned(),
                id,
            },
        ]),
        artifact: None,
    })
}

/// Preprocessing pipeline: resize if over dimension limit, compress if over
/// payload limit. Returns `(bytes, ext, metadata_text)`.
///
/// Pipeline order (stops at first passing step):
/// 1. Pass through if already within both limits.
/// 2. Resize to fit 2000×2000 if over dimension limit.
/// 3. Compress with format-native settings.
/// 4. Progressive resize: 75% → 50% → 25%.
/// 5. Convert to JPEG quality 75 (last resort).
fn preprocess_image(data: Vec<u8>, ext: &str) -> Result<(Vec<u8>, &'static str, String)> {
    use image::{ImageFormat, ImageReader, imageops::FilterType};
    use std::io::Cursor;

    let fmt = ImageFormat::from_extension(ext).unwrap_or(ImageFormat::Png);

    let img = ImageReader::with_format(Cursor::new(&data), fmt)
        .decode()
        .map_err(|e| anyhow::anyhow!("Cannot decode image: {e}"))?;

    let orig_w = img.width();
    let orig_h = img.height();
    let orig_bytes = data.len();

    // Step 1: pass through if within both limits.
    let needs_resize = orig_w > IMAGE_MAX_DIMENSION || orig_h > IMAGE_MAX_DIMENSION;
    if !needs_resize && orig_bytes <= IMAGE_TARGET_RAW_BYTES {
        let size_kb = orig_bytes.div_ceil(1024);
        let text = format!(
            "{}: {orig_w}×{orig_h} {size_kb} KB (attached)",
            media_type_from_ext(ext)
        );
        return Ok((data, ext_for_format(fmt), text));
    }

    // Compute target dimensions (fit inside 2000×2000, preserve aspect ratio).
    let (target_w, target_h) = if needs_resize {
        let scale = (IMAGE_MAX_DIMENSION as f32 / orig_w.max(orig_h) as f32).min(1.0);
        (
            ((orig_w as f32 * scale).round() as u32).max(1),
            ((orig_h as f32 * scale).round() as u32).max(1),
        )
    } else {
        (orig_w, orig_h)
    };

    let resized = if (target_w, target_h) != (orig_w, orig_h) {
        img.resize(target_w, target_h, FilterType::Lanczos3)
    } else {
        img
    };

    // Try encoding in original format first, then fallback strategies.
    let (final_bytes, out_ext) = encode_within_limit(&resized, fmt)
        .or_else(|_| encode_jpeg(&resized, 75))
        .map_err(|_| {
            anyhow::anyhow!(
                "Image cannot be compressed to fit within the 5 MB API limit. \
             Please use a smaller image."
            )
        })?;

    let display_w = resized.width();
    let display_h = resized.height();
    let size_kb = final_bytes.len().div_ceil(1024);
    let media_type = media_type_from_ext(out_ext);

    let text = if (display_w, display_h) != (orig_w, orig_h) {
        let scale = orig_w as f32 / display_w as f32;
        format!(
            "{media_type}: {orig_w}×{orig_h} → {display_w}×{display_h} \
             (scale {scale:.2}×), {size_kb} KB (attached)"
        )
    } else {
        format!("{media_type}: {display_w}×{display_h} {size_kb} KB (attached)")
    };

    Ok((final_bytes, out_ext, text))
}

/// Encode `img` in `fmt` and return bytes if they fit within the target size.
/// For PNG uses compression level 8; for JPEG uses quality 85; WebP quality 85.
fn encode_within_limit(
    img: &image::DynamicImage,
    fmt: image::ImageFormat,
) -> Result<(Vec<u8>, &'static str)> {
    use image::ImageFormat;
    let (bytes, ext) = match fmt {
        ImageFormat::Png => {
            let mut buf = Vec::new();
            let encoder = image::codecs::png::PngEncoder::new_with_quality(
                &mut buf,
                image::codecs::png::CompressionType::Best,
                image::codecs::png::FilterType::Adaptive,
            );
            img.write_with_encoder(encoder)?;
            (buf, "png")
        }
        ImageFormat::Gif => {
            // GIF: encode first frame as PNG (GIF encoder in `image` requires
            // palette quantization; PNG is simpler and universally supported).
            let mut buf = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)?;
            (buf, "png")
        }
        _ => {
            let (b, e) = encode_jpeg(img, 85)?;
            (b, e)
        }
    };
    if bytes.len() <= IMAGE_TARGET_RAW_BYTES {
        Ok((bytes, ext))
    } else {
        // Try progressive resize at 75% / 50% / 25% of current dimensions.
        for factor in [75u32, 50, 25] {
            let w = ((img.width() * factor) / 100).max(1);
            let h = ((img.height() * factor) / 100).max(1);
            let smaller = img.resize(w, h, image::imageops::FilterType::Lanczos3);
            let (b, e) = encode_within_limit(&smaller, fmt)?;
            if b.len() <= IMAGE_TARGET_RAW_BYTES {
                return Ok((b, e));
            }
        }
        anyhow::bail!("still over limit after progressive resize")
    }
}

fn encode_jpeg(img: &image::DynamicImage, quality: u8) -> Result<(Vec<u8>, &'static str)> {
    let mut buf = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    img.write_with_encoder(encoder)?;
    Ok((buf, "jpeg"))
}

/// Map `ImageFormat` back to the file extension we store in the session.
fn ext_for_format(fmt: image::ImageFormat) -> &'static str {
    use image::ImageFormat;
    match fmt {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpeg",
        ImageFormat::Gif => "gif",
        ImageFormat::WebP => "webp",
        ImageFormat::Bmp => "bmp",
        _ => "png",
    }
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
/// Used only for the text-only metadata path where we skip full decode.
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
            .execute(
                serde_json::json!({"path": file.to_str().unwrap()}),
                tx,
                cancel,
                Default::default(),
            )
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
            .execute(
                serde_json::json!({"path": file.to_str().unwrap(), "limit": 10}),
                tx,
                cancel,
                Default::default(),
            )
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
            .execute(
                serde_json::json!({"path": file.to_str().unwrap(), "limit": 200}),
                tx,
                cancel,
                Default::default(),
            )
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
            .execute(
                serde_json::json!({"path": dir.path().to_str().unwrap()}),
                tx,
                cancel,
                Default::default(),
            )
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
            .execute(
                serde_json::json!({"path": file.to_str().unwrap(), "offset": 50, "limit": 5}),
                tx,
                cancel,
                Default::default(),
            )
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
        let file = dir.path().join("huge.png");
        // > 50 MB hard gate — triggers MAX_IMAGE_READ_BYTES bail before decode.
        std::fs::write(&file, vec![0u8; 51 * 1024 * 1024]).unwrap();

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
    async fn read_image_corrupt_bails_with_vision() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("corrupt.png");
        // Invalid PNG bytes — decode should fail when vision is enabled.
        std::fs::write(&file, vec![0u8; 6 * 1024 * 1024]).unwrap();

        let tool = ReadTool;
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let caps = crate::core::tool::ModelCaps { vision: true };
        let result = tool
            .execute(
                serde_json::json!({"path": file.to_str().unwrap()}),
                tx,
                cancel,
                caps,
            )
            .await;

        // Either decode error or session error (no active session in test).
        assert!(result.is_err(), "corrupt image should fail");
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
                Default::default(),
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
                Default::default(),
            )
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

    /// Build a minimal valid 1×1 white PNG in memory.
    fn make_png(width: u32, height: u32) -> Vec<u8> {
        use image::{ImageBuffer, Rgb};
        let img: ImageBuffer<Rgb<u8>, _> =
            ImageBuffer::from_fn(width, height, |_, _| Rgb([255u8, 255, 255]));
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn preprocess_small_image_passes_through() {
        let data = make_png(100, 100);
        let orig_len = data.len();
        let (out, ext, text) = preprocess_image(data, "png").unwrap();
        assert_eq!(ext, "png");
        assert_eq!(out.len(), orig_len);
        assert!(text.contains("100×100"));
        assert!(text.contains("attached"));
        assert!(!text.contains("→")); // no resize indicator
    }

    #[test]
    fn preprocess_oversized_image_resizes() {
        let data = make_png(3000, 2000);
        let (out, _ext, text) = preprocess_image(data, "png").unwrap();
        // Decode output to verify dimensions are within limit.
        let decoded = image::load_from_memory(&out).unwrap();
        assert!(decoded.width() <= IMAGE_MAX_DIMENSION);
        assert!(decoded.height() <= IMAGE_MAX_DIMENSION);
        assert!(text.contains("→"), "metadata should show resize: {text}");
        assert!(text.contains("scale"), "metadata should show scale: {text}");
    }
}
