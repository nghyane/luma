/// Prompt rendering — input line with inline segment chips, dropdown.
use super::buffer::Seg;
use super::completion::{dropdown_line, highlight_at_refs};
use crate::tui::text::{Line, Span};
use crate::tui::theme::palette;
use smallvec::smallvec;

impl super::PromptState {
    /// Render the prompt input line(s). Emits one `Line` per logical newline
    /// in the buffer; inline chips render in place.
    pub fn lines(&self) -> Vec<Line> {
        let mut lines: Vec<Line> = Vec::new();
        let mut cur: smallvec::SmallVec<[Span; 4]> = smallvec![];
        let mut img_n = 0;

        for seg in &self.buf.segs {
            match seg {
                Seg::Text(t) => {
                    let mut parts = t.split('\n');
                    if let Some(first) = parts.next()
                        && !first.is_empty()
                    {
                        cur.extend(highlight_at_refs(first));
                    }
                    for part in parts {
                        lines.push(Line::new(std::mem::take(&mut cur)));
                        if !part.is_empty() {
                            cur.extend(highlight_at_refs(part));
                        }
                    }
                }
                Seg::Image { .. } => {
                    img_n += 1;
                    cur.push(Span::with_bg(
                        format!(" Image {img_n} "),
                        palette::BG,
                        palette::FILE_REF,
                    ));
                    cur.push(Span::new(" ".to_owned(), palette::FG));
                }
                Seg::Paste(text) => {
                    let n = text.lines().count();
                    cur.push(Span::with_bg(
                        format!(" Pasted ~{n} lines "),
                        palette::BG,
                        palette::WARN,
                    ));
                    cur.push(Span::new(" ".to_owned(), palette::FG));
                }
            }
        }

        let ghost = self.ghost();
        if !ghost.is_empty() {
            cur.push(Span::new(ghost, palette::MUTED));
        }
        if cur.is_empty() && lines.is_empty() {
            cur.push(Span::new(String::new(), palette::FG));
        }
        lines.push(Line::new(cur));
        lines
    }

    /// Render dropdown for commands or @file autocomplete.
    pub fn dropdown(&self) -> Vec<Line> {
        use crate::tui::theme::icon;
        let bar = icon::PROMPT;

        if let Some(query) = self.at_file_query() {
            let matches = self.comp.file_matches(&query);
            if matches.is_empty() {
                return Vec::new();
            }
            return matches
                .iter()
                .enumerate()
                .take(8)
                .map(|(i, path)| {
                    dropdown_line(
                        bar,
                        &format!("@{path}"),
                        "",
                        i == self.comp.dropdown_idx,
                        palette::FILE_REF,
                    )
                })
                .collect();
        }

        let matches = self.get_matches();
        if matches.is_empty() {
            return Vec::new();
        }
        let max_name = matches.iter().map(|c| c.name.len()).max().unwrap_or(0);
        matches
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let pad = max_name - c.name.len();
                dropdown_line(
                    bar,
                    &format!("/{}", c.name),
                    &format!("{}  {}", " ".repeat(pad), c.desc),
                    i == self.comp.dropdown_idx,
                    palette::ACCENT,
                )
            })
            .collect()
    }
}

