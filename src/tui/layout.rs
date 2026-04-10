/// Layout — render blocks into cached lines, provide windowed access.
use crate::tui::block::{Block, RenderState, Snapshot, render_block};
use crate::tui::text::Line;

struct Slot {
    lines: Vec<Line>,
    snap: Option<Snapshot>,
    state: RenderState,
}

/// Render cache with offset index over blocks.
pub struct Layout {
    slots: Vec<Slot>,
    offsets: Vec<usize>,
    total: usize,
    width: usize,
    height: usize,
    spinner_frame: usize,
    cached_total: usize,
}

impl Layout {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            slots: Vec::new(),
            offsets: Vec::new(),
            total: 0,
            width,
            height,
            spinner_frame: 0,
            cached_total: 0,
        }
    }

    pub fn set_size(&mut self, width: usize, height: usize) {
        if self.width != width {
            for slot in &mut self.slots {
                slot.snap = None;
            }
        }
        self.width = width;
        self.height = height;
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn tick(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
    }

    /// Re-render visible blocks near `scroll_offset`, sync slot count.
    pub fn refresh(&mut self, blocks: &[Block], scroll_offset: usize) {
        // Shrink slots if document was cleared/truncated
        if self.slots.len() > blocks.len() {
            self.slots.truncate(blocks.len());
        }

        // New blocks: render immediately so line counts are accurate.
        while self.slots.len() < blocks.len() {
            let i = self.slots.len();
            let mut state = RenderState::new();
            if let Some(block) = blocks.get(i) {
                let snap = block.snapshot();
                let rendered = render_block(block, &mut state, self.width, self.spinner_frame);
                self.slots.push(Slot {
                    lines: rendered,
                    snap: Some(snap),
                    state,
                });
            } else {
                self.slots.push(Slot {
                    lines: vec![],
                    snap: None,
                    state,
                });
            }
        }
        self.rebuild_offsets();

        // Update existing blocks in visible range.
        let visible_end = scroll_offset + self.height * 2;
        let vis_start = if self.slots.is_empty() {
            0
        } else {
            self.block_at(scroll_offset.min(self.total.saturating_sub(1)))
        };
        let vis_end = if self.total == 0 {
            0
        } else {
            self.block_at(visible_end.min(self.total.saturating_sub(1))) + 1
        };

        let mut any_changed = false;
        for (i, slot) in self.slots.iter_mut().enumerate() {
            let in_range = i >= vis_start && i < vis_end;
            let was_rendered = !slot.lines.is_empty();
            if !in_range && !was_rendered {
                continue;
            }

            if let Some(block) = blocks.get(i) {
                let snap = block.snapshot();
                if slot.snap.as_ref() == Some(&snap) && was_rendered {
                    continue;
                }

                let new_lines =
                    render_block(block, &mut slot.state, self.width, self.spinner_frame);
                if new_lines.len() != slot.lines.len() {
                    any_changed = true;
                }
                slot.lines = new_lines;
                slot.snap = Some(snap);
            }
        }
        if any_changed {
            self.rebuild_offsets();
        }
        self.cached_total = self.total;
    }

    pub fn total_lines(&self) -> usize {
        self.cached_total
    }

    pub fn hit_test(&self, abs_line: usize) -> Option<usize> {
        if abs_line >= self.total {
            return None;
        }
        Some(self.block_at(abs_line))
    }

    /// Zero-copy iterator over visible lines.
    pub fn window_iter(&self, offset: usize, height: usize) -> LayoutIter<'_> {
        let end = (offset + height).min(self.total);
        if offset >= end || self.slots.is_empty() {
            return LayoutIter {
                slots: &self.slots,
                line_idx: 0,
                end: 0,
                block_idx: 0,
                local_idx: 0,
            };
        }
        let bi = self.block_at(offset);
        let local = offset - self.offsets[bi];
        LayoutIter {
            slots: &self.slots,
            line_idx: offset,
            end,
            block_idx: bi,
            local_idx: local,
        }
    }

    pub fn clear(&mut self) {
        self.slots.clear();
        self.offsets.clear();
        self.total = 0;
        self.cached_total = 0;
    }

    fn block_at(&self, abs_line: usize) -> usize {
        self.offsets
            .partition_point(|&o| o <= abs_line)
            .saturating_sub(1)
    }

    fn rebuild_offsets(&mut self) {
        self.offsets.clear();
        let mut acc = 0;
        for s in &self.slots {
            self.offsets.push(acc);
            acc += s.lines.len();
        }
        self.total = acc;
    }
}

pub struct LayoutIter<'a> {
    slots: &'a [Slot],
    line_idx: usize,
    end: usize,
    block_idx: usize,
    local_idx: usize,
}

impl<'a> Iterator for LayoutIter<'a> {
    type Item = &'a Line;

    fn next(&mut self) -> Option<Self::Item> {
        if self.line_idx >= self.end {
            return None;
        }
        while self.block_idx < self.slots.len()
            && self.local_idx >= self.slots[self.block_idx].lines.len()
        {
            self.block_idx += 1;
            self.local_idx = 0;
        }
        if self.block_idx >= self.slots.len() {
            return None;
        }
        let line = &self.slots[self.block_idx].lines[self.local_idx];
        self.line_idx += 1;
        self.local_idx += 1;
        Some(line)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let r = self.end - self.line_idx;
        (r, Some(r))
    }
}

impl ExactSizeIterator for LayoutIter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::block::TextBlock;

    fn text_blocks(groups: &[&[&str]]) -> Vec<Block> {
        groups
            .iter()
            .map(|labels| {
                let mut tb = TextBlock::new();
                for l in *labels {
                    tb.feed(l);
                    tb.feed("\n");
                }
                tb.flush();
                Block::Text(tb)
            })
            .collect()
    }

    #[test]
    fn refresh_and_total() {
        let blocks = text_blocks(&[&["a", "b", "c"], &["d", "e"]]);
        let mut layout = Layout::new(80, 100);
        layout.refresh(&blocks, 0);
        assert_eq!(layout.total_lines(), 5);
    }

    #[test]
    fn hit_test_finds_block() {
        let blocks = text_blocks(&[&["a", "b", "c"], &["d", "e"]]);
        let mut layout = Layout::new(80, 100);
        layout.refresh(&blocks, 0);
        assert_eq!(layout.hit_test(0), Some(0));
        assert_eq!(layout.hit_test(3), Some(1));
        assert_eq!(layout.hit_test(5), None);
    }

    #[test]
    fn window_iter_correct() {
        let blocks = text_blocks(&[&["a", "b"], &["c"]]);
        let mut layout = Layout::new(80, 100);
        layout.refresh(&blocks, 0);
        let texts: Vec<String> = layout
            .window_iter(0, 3)
            .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect())
            .collect();
        assert_eq!(texts.len(), 3);
    }

    #[test]
    fn clear_resets() {
        let blocks = text_blocks(&[&["a"]]);
        let mut layout = Layout::new(80, 100);
        layout.refresh(&blocks, 0);
        layout.clear();
        assert_eq!(layout.total_lines(), 0);
    }

    #[test]
    fn refresh_truncates_on_shrink() {
        let mut blocks = text_blocks(&[&["a", "b"], &["c", "d"]]);
        let mut layout = Layout::new(80, 100);
        layout.refresh(&blocks, 0);
        assert_eq!(layout.total_lines(), 4);

        // Simulate document clear + new content
        blocks.clear();
        blocks.push({
            let mut tb = TextBlock::new();
            tb.feed("x\n");
            tb.flush();
            Block::Text(tb)
        });
        layout.refresh(&blocks, 0);
        assert_eq!(layout.total_lines(), 1, "stale slots should be gone");
    }
}
