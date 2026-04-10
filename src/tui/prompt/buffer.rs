/// Segment-based prompt buffer — single source of truth for editing.
use crate::core::types::ContentBlock;

/// A segment in the prompt buffer.
#[derive(Clone)]
pub enum Seg {
    Text(String),
    Image { media_type: String, data: Vec<u8> },
    Paste(String),
}

/// Prompt buffer with segment-aware cursor.
pub struct PromptBuffer {
    pub segs: Vec<Seg>,
    pub seg: usize,
    pub pos: usize,
}

impl PromptBuffer {
    pub fn new() -> Self {
        Self {
            segs: vec![Seg::Text(String::new())],
            seg: 0,
            pos: 0,
        }
    }

    /// Insert a char at cursor.
    pub fn insert(&mut self, ch: char) {
        self.ensure_text();
        if let Seg::Text(t) = &mut self.segs[self.seg] {
            let byte = char_to_byte(t, self.pos);
            t.insert(byte, ch);
            self.pos += 1;
        }
    }

    /// Insert a string at cursor.
    pub fn insert_str(&mut self, s: &str) {
        self.ensure_text();
        if let Seg::Text(t) = &mut self.segs[self.seg] {
            let byte = char_to_byte(t, self.pos);
            t.insert_str(byte, s);
            self.pos += s.chars().count();
        }
    }

    /// Delete char before cursor. Returns true if something was deleted.
    pub fn backspace(&mut self) -> bool {
        if self.pos > 0
            && let Seg::Text(t) = &mut self.segs[self.seg]
        {
            let start = char_to_byte(t, self.pos - 1);
            let end = char_to_byte(t, self.pos);
            t.replace_range(start..end, "");
            self.pos -= 1;
            return true;
        }
        // pos=0: delete previous non-text segment
        if self.seg > 0 {
            let prev = self.seg - 1;
            if !matches!(&self.segs[prev], Seg::Text(_)) {
                self.segs.remove(prev);
                self.seg -= 1;
                self.merge_adjacent_text();
                return true;
            }
        }
        false
    }

    /// Move cursor left.
    pub fn left(&mut self) {
        if self.pos > 0 {
            self.pos -= 1;
        } else if self.seg > 0 {
            // Skip over non-text segments
            self.seg -= 1;
            if let Seg::Text(t) = &self.segs[self.seg] {
                self.pos = t.chars().count();
            } else if self.seg > 0 {
                self.seg -= 1;
                if let Seg::Text(t) = &self.segs[self.seg] {
                    self.pos = t.chars().count();
                }
            }
        }
    }

    /// Move cursor right.
    pub fn right(&mut self) {
        let len = self.cur_text_len();
        if self.pos < len {
            self.pos += 1;
        } else if self.seg + 1 < self.segs.len() {
            self.seg += 1;
            self.pos = 0;
            // Skip over non-text to next text
            if !matches!(&self.segs[self.seg], Seg::Text(_)) && self.seg + 1 < self.segs.len() {
                self.seg += 1;
                self.pos = 0;
            }
        }
    }

    /// Move cursor to beginning.
    pub fn home(&mut self) {
        self.seg = 0;
        self.pos = 0;
    }

    /// Move cursor to end.
    pub fn end(&mut self) {
        self.seg = self.segs.len().saturating_sub(1);
        if let Seg::Text(t) = &self.segs[self.seg] {
            self.pos = t.chars().count();
        } else {
            self.pos = 0;
        }
    }

    /// Insert an image at cursor position.
    pub fn attach_image(&mut self, media_type: String, data: Vec<u8>) {
        let img = Seg::Image { media_type, data };
        self.split_and_insert(img);
    }

    /// Insert a paste block at cursor position.
    pub fn attach_paste(&mut self, text: String) {
        self.split_and_insert(Seg::Paste(text));
    }

    /// Clear everything.
    pub fn clear(&mut self) {
        self.segs = vec![Seg::Text(String::new())];
        self.seg = 0;
        self.pos = 0;
    }

    /// Replace buffer with text (for history recall).
    pub fn set_text(&mut self, text: &str) {
        self.segs = vec![Seg::Text(text.to_owned())];
        self.seg = 0;
        self.pos = text.chars().count();
    }

    /// Delete text before cursor on current line (Ctrl+U).
    pub fn kill_before(&mut self) {
        self.ensure_text();
        if let Seg::Text(t) = &mut self.segs[self.seg] {
            let byte = char_to_byte(t, self.pos);
            *t = t[byte..].to_owned();
            self.pos = 0;
        }
    }

    /// Whether buffer has any content.
    pub fn is_empty(&self) -> bool {
        self.segs.iter().all(|s| match s {
            Seg::Text(t) => t.is_empty(),
            _ => false,
        })
    }

    /// Global char position across all text segments (for @query, command detection).
    pub fn text_pos(&self) -> usize {
        let mut total = 0;
        for (i, s) in self.segs.iter().enumerate() {
            if i == self.seg {
                return total + self.pos;
            }
            if let Seg::Text(t) = s {
                total += t.chars().count();
            }
        }
        total + self.pos
    }

    /// Full text content (text segments only, joined).
    pub fn text(&self) -> String {
        let mut out = String::new();
        for s in &self.segs {
            if let Seg::Text(t) = s {
                out.push_str(t);
            }
        }
        out
    }

    /// Trimmed text content.
    pub fn trimmed_text(&self) -> String {
        self.text().trim().to_owned()
    }

    /// Whether buffer starts with '/'.
    pub fn is_command(&self) -> bool {
        self.text().starts_with('/')
    }

    /// Line count of text content.
    pub fn line_count(&self) -> usize {
        self.text().lines().count().max(1)
    }

    /// Last line of text content.
    pub fn last_line(&self) -> String {
        self.text().lines().last().unwrap_or("").to_owned()
    }

    /// Snapshot content blocks (read-only, for render/display).
    #[cfg(test)]
    pub fn to_content(&self) -> Vec<ContentBlock> {
        let mut blocks = Vec::new();
        for s in &self.segs {
            match s {
                Seg::Text(t) => {
                    let trimmed = t.trim();
                    if !trimmed.is_empty() {
                        blocks.push(ContentBlock::Text {
                            text: trimmed.to_owned(),
                        });
                    }
                }
                Seg::Image { media_type, .. } => {
                    blocks.push(ContentBlock::Image {
                        media_type: media_type.clone(),
                        id: String::new(),
                    });
                }
                Seg::Paste(text) => {
                    blocks.push(ContentBlock::Paste { text: text.clone() });
                }
            }
        }
        blocks
    }

    /// Consume buffer content for submission.
    ///
    /// Returns content blocks and extracted image data atomically.
    /// Image segments become `ContentBlock::Image { id: "" }` (agent fills real
    /// id after saving) and their binary data is returned separately.
    /// Buffer is cleared after this call.
    pub fn take_content(&mut self) -> (Vec<ContentBlock>, Vec<(String, Vec<u8>)>) {
        let mut blocks = Vec::new();
        let mut images = Vec::new();
        for s in &mut self.segs {
            match s {
                Seg::Text(t) => {
                    let trimmed = t.trim();
                    if !trimmed.is_empty() {
                        blocks.push(ContentBlock::Text {
                            text: trimmed.to_owned(),
                        });
                    }
                }
                Seg::Image { media_type, data } => {
                    images.push((media_type.clone(), std::mem::take(data)));
                    blocks.push(ContentBlock::Image {
                        media_type: media_type.clone(),
                        id: String::new(),
                    });
                }
                Seg::Paste(text) => {
                    blocks.push(ContentBlock::Paste { text: text.clone() });
                }
            }
        }
        self.clear();
        (blocks, images)
    }

    /// Display width of visible content before cursor (for cursor positioning).
    pub fn cursor_display_col(&self) -> usize {
        use crate::tui::text::display_width;
        let mut col = 0;
        for (i, s) in self.segs.iter().enumerate() {
            if i > self.seg {
                break;
            }
            match s {
                Seg::Text(t) => {
                    let slice = if i == self.seg {
                        &t[..char_to_byte(t, self.pos)]
                    } else {
                        t.as_str()
                    };
                    // Only measure last line
                    let last = slice.rsplit_once('\n').map(|(_, a)| a).unwrap_or(slice);
                    col += display_width(last);
                }
                Seg::Image { .. } => {
                    let n = self.image_index_at(i);
                    col += display_width(&format!(" Image {n} ")) + 1;
                }
                Seg::Paste(text) => {
                    let n = text.lines().count();
                    col += display_width(&format!(" Pasted ~{n} lines ")) + 1;
                }
            }
        }
        col
    }

    // ── Private ──

    fn cur_text_len(&self) -> usize {
        match &self.segs[self.seg] {
            Seg::Text(t) => t.chars().count(),
            _ => 0,
        }
    }

    fn ensure_text(&mut self) {
        if !matches!(&self.segs[self.seg], Seg::Text(_)) {
            self.segs.insert(self.seg + 1, Seg::Text(String::new()));
            self.seg += 1;
            self.pos = 0;
        }
    }

    fn split_and_insert(&mut self, new_seg: Seg) {
        self.ensure_text();
        if let Seg::Text(t) = &self.segs[self.seg] {
            let byte = char_to_byte(t, self.pos);
            let after = t[byte..].to_owned();
            let before = t[..byte].to_owned();
            self.segs[self.seg] = Seg::Text(before);
            let idx = self.seg + 1;
            self.segs.insert(idx, new_seg);
            self.segs.insert(idx + 1, Seg::Text(after));
            self.seg = idx + 1;
            self.pos = 0;
        }
    }

    fn merge_adjacent_text(&mut self) {
        if self.seg > 0
            && matches!(&self.segs[self.seg], Seg::Text(_))
            && matches!(&self.segs[self.seg - 1], Seg::Text(_))
        {
            let cur = if let Seg::Text(t) = &self.segs[self.seg] {
                t.clone()
            } else {
                return;
            };
            if let Seg::Text(prev) = &mut self.segs[self.seg - 1] {
                let new_pos = prev.chars().count();
                prev.push_str(&cur);
                self.segs.remove(self.seg);
                self.seg -= 1;
                self.pos = new_pos;
            }
        }
    }

    fn image_index_at(&self, seg_idx: usize) -> usize {
        self.segs[..=seg_idx]
            .iter()
            .filter(|s| matches!(s, Seg::Image { .. }))
            .count()
    }
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_text() {
        let mut b = PromptBuffer::new();
        b.insert('h');
        b.insert('i');
        assert_eq!(b.text(), "hi");
        assert_eq!(b.pos, 2);
    }

    #[test]
    fn backspace_text() {
        let mut b = PromptBuffer::new();
        b.insert_str("abc");
        b.backspace();
        assert_eq!(b.text(), "ab");
    }

    #[test]
    fn attach_image_splits() {
        let mut b = PromptBuffer::new();
        b.insert_str("before");
        b.attach_image("image/png".into(), vec![1]);
        b.insert_str("after");
        assert_eq!(b.segs.len(), 3);
        assert_eq!(b.text(), "beforeafter");
        let content = b.to_content();
        assert_eq!(content.len(), 3);
        assert!(matches!(&content[1], ContentBlock::Image { .. }));
    }

    #[test]
    fn backspace_removes_image() {
        let mut b = PromptBuffer::new();
        b.insert_str("a");
        b.attach_image("image/png".into(), vec![1]);
        // Cursor at start of text after image
        assert_eq!(b.pos, 0);
        b.backspace();
        assert_eq!(b.segs.len(), 1);
        assert_eq!(b.text(), "a");
    }

    #[test]
    fn attach_paste_at_cursor() {
        let mut b = PromptBuffer::new();
        b.insert_str("fix: ");
        b.attach_paste("line1\nline2\nline3".into());
        b.insert_str(" done");
        let content = b.to_content();
        assert!(matches!(&content[0], ContentBlock::Text { text } if text == "fix:"));
        assert!(matches!(&content[1], ContentBlock::Paste { .. }));
        assert!(matches!(&content[2], ContentBlock::Text { text } if text == "done"));
    }

    #[test]
    fn clear_resets() {
        let mut b = PromptBuffer::new();
        b.insert_str("hello");
        b.attach_image("image/png".into(), vec![1]);
        b.clear();
        assert!(b.is_empty());
        assert_eq!(b.segs.len(), 1);
    }

    #[test]
    fn take_content_extracts_images() {
        let mut b = PromptBuffer::new();
        b.insert_str("a");
        b.attach_image("image/png".into(), vec![42]);
        b.insert_str("b");
        let (content, imgs) = b.take_content();
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].1, vec![42]);
        assert_eq!(content.len(), 3);
        assert!(matches!(&content[1], ContentBlock::Image { .. }));
        // Buffer cleared after take_content
        assert!(b.is_empty());
    }

    #[test]
    fn cursor_movement() {
        let mut b = PromptBuffer::new();
        b.insert_str("ab");
        b.attach_image("image/png".into(), vec![1]);
        b.insert_str("cd");
        b.home();
        assert_eq!(b.seg, 0);
        assert_eq!(b.pos, 0);
        b.end();
        assert_eq!(b.text(), "abcd");
    }
}
