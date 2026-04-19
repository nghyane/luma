use super::Region;
/// Region painting — fill bg, padding, content lines into ScreenBuffer.
use crate::tui::text::{Line, ScreenBuffer};

/// Paint lines into buffer for a region.
pub fn paint_region<'a>(
    buf: &mut ScreenBuffer,
    region: &Region,
    lines: impl Iterator<Item = &'a Line>,
) {
    let r = region.row.saturating_sub(1);
    let c = region.col.saturating_sub(1);

    buf.fill_bg(r, c, region.width, region.height, region.bg);
    fill_region_padding(buf, region, r, c);

    let content_col = c + region.padding.left;
    let content_row = r + region.padding.top;
    let content_w = region.content_width();
    let content_h = region.content_height();

    for (j, line) in lines.enumerate() {
        if j as u16 >= content_h {
            break;
        }
        let row = content_row + j as u16;
        apply_line_bg(buf, line, row, c, region);
        write_line_content(buf, line, row, content_col, content_w, region.bg);
    }
}

fn fill_region_padding(buf: &mut ScreenBuffer, region: &Region, r: u16, c: u16) {
    if region.padding.left > 0 {
        buf.fill_padding(r, c, region.padding.left, region.height, region.bg);
    }
    if region.padding.right > 0 {
        let right_col = c + region.width - region.padding.right;
        buf.fill_padding(r, right_col, region.padding.right, region.height, region.bg);
    }
    if region.padding.top > 0 {
        let cc = c + region.padding.left;
        let cw = region.content_width();
        buf.fill_padding(r, cc, cw, region.padding.top, region.bg);
    }
    if region.padding.bottom > 0 {
        let cc = c + region.padding.left;
        let cw = region.content_width();
        let br = r + region.height - region.padding.bottom;
        buf.fill_padding(br, cc, cw, region.padding.bottom, region.bg);
    }
}

fn apply_line_bg(buf: &mut ScreenBuffer, line: &Line, row: u16, c: u16, region: &Region) {
    let Some(line_bg) = line.bg else { return };
    let content_w = region.content_width();
    let content_col = c + region.padding.left;

    if line.margin {
        buf.fill_bg(row, content_col, content_w, 1, line_bg);
    } else {
        buf.fill_bg(row, c, region.width, 1, line_bg);
        if region.padding.left > 0 {
            buf.fill_padding(row, c, region.padding.left, 1, line_bg);
        }
        if region.padding.right > 0 {
            let rc = c + region.width - region.padding.right;
            buf.fill_padding(row, rc, region.padding.right, 1, line_bg);
        }
    }
}

fn write_line_content(
    buf: &mut ScreenBuffer,
    line: &Line,
    row: u16,
    content_col: u16,
    content_w: u16,
    region_bg: crate::tui::theme::Rgb,
) {
    let indent = line.indent;
    if line.margin && line.bg.is_some() {
        const MARGIN: u16 = 2;
        let inner_col = content_col + MARGIN;
        let inner_w = content_w.saturating_sub(MARGIN * 2);
        if inner_w > 0 {
            buf.write_line(line, row, inner_col, inner_w);
        }
    } else if line.bleed > 0 {
        let bleed_col = content_col.saturating_sub(line.bleed);
        let bleed_w = content_w + line.bleed;
        buf.write_line(line, row, bleed_col, bleed_w);
    } else if indent > 0 {
        let line_bg = line.bg.unwrap_or(region_bg);
        buf.fill_padding(row, content_col, indent, 1, line_bg);
        let text_col = content_col + indent;
        let text_w = content_w.saturating_sub(indent);
        buf.write_line(line, row, text_col, text_w);
    } else {
        buf.write_line_decorated(line, row, content_col, content_w, line.deco);
    }
}

/// Paint a floating layer at absolute screen position.
pub fn paint_floating(buf: &mut ScreenBuffer, layer: &super::FloatingLayer) {
    let col = layer.col.saturating_sub(1);
    let start_row = layer.row.saturating_sub(1);

    for (j, line) in layer.lines.iter().enumerate() {
        let row = start_row + j as u16;
        if row >= buf.height {
            break;
        }
        let bg = line.bg.unwrap_or(layer.bg);
        buf.fill_bg(row, col, layer.width, 1, bg);
        buf.write_line(line, row, col, layer.width);
    }
}
