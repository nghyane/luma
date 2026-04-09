use super::TextBlock;
/// TextCache — incremental markdown render cache for TextBlock.
use super::render::RenderState;
use crate::tui::markdown::BlockState;
use crate::tui::markdown::{is_table_line, parse_inline_streaming, parse_line, render_table};
use crate::tui::text::Line;

/// Incremental markdown render cache.
#[derive(Debug, Clone)]
pub struct TextCache {
    parsed_count: usize,
    lines: Vec<Line>,
    md_state: BlockState,
    width: usize,
    trailing_table_lines: usize,
    trailing_table_rows: usize,
}

impl TextCache {
    fn new() -> Self {
        Self {
            parsed_count: 0,
            lines: Vec::new(),
            md_state: BlockState::new(),
            width: 0,
            trailing_table_lines: 0,
            trailing_table_rows: 0,
        }
    }

    fn invalidate(&mut self) {
        self.parsed_count = 0;
        self.lines.clear();
        self.md_state = BlockState::new();
        self.trailing_table_lines = 0;
        self.trailing_table_rows = 0;
    }
}

/// Render text block using incremental cache from RenderState.
pub fn render_text(tb: &TextBlock, state: &mut RenderState, width: usize) -> Vec<Line> {
    let cache = ensure_text_cache(state);
    let committed = &tb.stream.committed;

    if cache.width != width {
        cache.invalidate();
        cache.width = width;
    }

    let new_start = compute_start(cache, committed);
    parse_committed(cache, committed, new_start, width);
    cache.parsed_count = committed.len();

    let mut result = cache.lines.clone();
    append_partial(&mut result, tb.stream.partial());
    strip_empty_edges(&mut result);
    result
}

fn ensure_text_cache(state: &mut RenderState) -> &mut TextCache {
    if !matches!(state, RenderState::Text(_)) {
        *state = RenderState::Text(TextCache::new());
    }
    match state {
        RenderState::Text(c) => c,
        _ => unreachable!(),
    }
}

fn compute_start(cache: &mut TextCache, committed: &[String]) -> usize {
    if cache.parsed_count > 0
        && cache.parsed_count <= committed.len()
        && cache.trailing_table_rows > 0
        && committed
            .get(cache.parsed_count)
            .is_some_and(|l| is_table_line(l))
    {
        for _ in 0..cache.trailing_table_lines {
            cache.lines.pop();
        }
        cache.parsed_count -= cache.trailing_table_rows;
        cache.md_state = BlockState::Normal;
        cache.trailing_table_lines = 0;
        cache.trailing_table_rows = 0;
        cache.parsed_count
    } else if cache.parsed_count <= committed.len() {
        cache.parsed_count
    } else {
        cache.invalidate();
        0
    }
}

fn parse_committed(cache: &mut TextCache, committed: &[String], start: usize, width: usize) {
    let mut table_rows: Vec<String> = Vec::new();

    for text in &committed[start..] {
        let is_table = is_table_line(text);

        if !table_rows.is_empty() && !is_table {
            flush_table(&mut cache.lines, &table_rows, width);
            cache.trailing_table_lines = 0;
            cache.trailing_table_rows = 0;
            table_rows.clear();
        }

        if is_table {
            table_rows.push(text.clone());
            let (_, new_state) = parse_line(text, &cache.md_state);
            cache.md_state = new_state;
        } else {
            let (lines, new_state) = parse_line(text, &cache.md_state);
            for l in lines {
                push_wrapped(&mut cache.lines, &l, width);
            }
            cache.md_state = new_state;
            cache.trailing_table_lines = 0;
            cache.trailing_table_rows = 0;
        }
    }

    if !table_rows.is_empty() {
        let before = cache.lines.len();
        flush_table(&mut cache.lines, &table_rows, width);
        cache.trailing_table_lines = cache.lines.len() - before;
        cache.trailing_table_rows = table_rows.len();
    }
}

fn flush_table(lines: &mut Vec<Line>, rows: &[String], width: usize) {
    for rl in render_table(rows) {
        for wl in crate::tui::text::wrap_line(&rl, width, None) {
            let is_empty = wl.visible_width() == 0;
            let prev_empty = lines.last().is_none_or(|p: &Line| p.visible_width() == 0);
            if is_empty && prev_empty {
                continue;
            }
            lines.push(wl);
        }
    }
}

fn push_wrapped(lines: &mut Vec<Line>, line: &Line, width: usize) {
    for wl in crate::tui::text::wrap_line(line, width, None) {
        let is_empty = wl.visible_width() == 0;
        let prev_empty = lines.last().is_none_or(|p: &Line| p.visible_width() == 0);
        if is_empty && prev_empty {
            continue;
        }
        lines.push(wl);
    }
}

fn append_partial(result: &mut Vec<Line>, partial: &str) {
    if partial.is_empty() || is_table_line(partial) {
        return;
    }
    let spans = parse_inline_streaming(partial);
    result.push(Line::new(spans));
}

fn strip_empty_edges(result: &mut Vec<Line>) {
    let leading = result.iter().take_while(|l| l.visible_width() == 0).count();
    if leading > 0 {
        result.drain(..leading);
    }
    while result.last().is_some_and(|l| l.visible_width() == 0) {
        result.pop();
    }
}
