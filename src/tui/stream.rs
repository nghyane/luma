/// Accumulates streaming tokens into committed lines.
#[derive(Debug, Clone)]
pub struct StreamBuf {
    pub committed: Vec<String>,
    partial: String,
}

impl StreamBuf {
    /// Create a new stream buffer.
    pub fn new() -> Self {
        Self {
            committed: Vec::new(),
            partial: String::new(),
        }
    }

    /// Create a buffer pre-loaded with finished content (no partial).
    pub fn finished(text: &str) -> Self {
        let mut buf = Self::new();
        buf.feed(text);
        buf.flush();
        buf
    }

    /// Feed a token chunk. Newlines split into committed lines.
    pub fn feed(&mut self, token: &str) {
        for (i, part) in token.split('\n').enumerate() {
            if i > 0 {
                self.committed.push(std::mem::take(&mut self.partial));
            }
            self.partial.push_str(part);
        }
    }

    /// Flush any remaining partial text into committed.
    pub fn flush(&mut self) {
        if !self.partial.is_empty() {
            self.committed.push(std::mem::take(&mut self.partial));
        }
    }

    /// Whether there's any content (committed or partial).
    pub fn is_empty(&self) -> bool {
        self.committed.is_empty() && self.partial.is_empty()
    }

    /// Access partial text (for streaming display).
    pub fn partial(&self) -> &str {
        &self.partial
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_no_newline() {
        let mut s = StreamBuf::new();
        s.feed("hello");
        s.feed(" world");
        assert!(s.committed.is_empty());
        assert_eq!(s.partial, "hello world");
    }

    #[test]
    fn feed_with_newline() {
        let mut s = StreamBuf::new();
        s.feed("line1\nline2\nline3");
        assert_eq!(s.committed, vec!["line1", "line2"]);
        assert_eq!(s.partial, "line3");
    }

    #[test]
    fn flush_commits_partial() {
        let mut s = StreamBuf::new();
        s.feed("partial");
        s.flush();
        assert_eq!(s.committed, vec!["partial"]);
        assert!(s.partial.is_empty());
    }

    #[test]
    fn empty_on_new() {
        let s = StreamBuf::new();
        assert!(s.is_empty());
    }

    #[test]
    fn preserves_blank_lines() {
        let mut s = StreamBuf::new();
        s.feed("hello\n\nworld\n");
        assert_eq!(s.committed, vec!["hello", "", "world"]);
        assert_eq!(s.partial, "");
    }

    #[test]
    fn consecutive_newlines() {
        let mut s = StreamBuf::new();
        s.feed("a\n");
        s.feed("\n");
        s.feed("b");
        assert_eq!(s.committed, vec!["a", ""]);
        assert_eq!(s.partial, "b");
    }
}
