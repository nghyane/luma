/// Low-level terminal: clipboard, buffered stdout.
use std::io::{self, BufWriter, Stdout, Write};

/// Copy text to system clipboard.
///
/// On Unix, uses OSC 52 escape sequence (widely supported).
/// On Windows, pipes to `clip.exe` since OSC 52 is not supported by conhost.
#[cfg(not(windows))]
pub fn copy_to_clipboard(out: &mut impl Write, text: &str) -> io::Result<()> {
    let b64 = base64_encode(text.as_bytes());
    write!(out, "\x1b]52;c;{b64}\x1b\\")
}

#[cfg(windows)]
pub fn copy_to_clipboard(_out: &mut impl Write, text: &str) -> io::Result<()> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("clip")
        .stdin(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    child.wait()?;
    Ok(())
}

/// Create a buffered stdout writer.
pub fn buffered_stdout() -> BufWriter<Stdout> {
    BufWriter::with_capacity(8192, io::stdout())
}

/// Minimal base64 encode (no padding needed for OSC 52).
fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((n >> 18) & 63) as usize] as char);
        out.push(CHARS[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encode_hello() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn base64_encode_empty() {
        assert_eq!(base64_encode(b""), "");
    }
}
