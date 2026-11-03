//! Rendering validated rows to terminal cells. Rust owns the authoritative
//! cursor gutter; the canonical status text was substituted into the row by
//! validation, so the style and position of that span belong to Lua.

use super::{Color, Row, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Render one row exactly `width` cells wide at most.
pub fn render_row(row: &Row, width: usize, gutter_cells: usize) -> String {
    let gutter = gutter(row, width.min(gutter_cells));
    let available = width.saturating_sub(gutter.len());
    // Title/local rows were historically plain text: clip them without an
    // ellipsis, while navigable labels keep the readable ellipsis.
    let ellipsis = row.target.is_some();
    let mut out = String::with_capacity(width);
    out.push_str(&gutter);
    if let Some(index) = row.status.filter(|index| *index < row.spans.len()) {
        let status = &row.spans[index];
        let status_width = span_width(status);
        let before = render_spans(
            &row.spans[..index],
            available.saturating_sub(status_width),
            ellipsis,
        );
        let before_width = visible_width(&before);
        out.push_str(&before);
        let remaining = available.saturating_sub(before_width);
        let status = render_spans(std::slice::from_ref(status), remaining, false);
        let status_width = visible_width(&status);
        out.push_str(&status);
        let remaining = available.saturating_sub(before_width + status_width);
        if index + 1 < row.spans.len() {
            out.push_str(&render_spans(&row.spans[index + 1..], remaining, ellipsis));
        }
    } else {
        out.push_str(&render_spans(&row.spans, available, ellipsis));
    }
    out
}

/// Render a leaf of rows. If the composed roster is longer than the leaf, the
/// last visible line is `… more`; targets remain in the composition (and in
/// navigation), so this never silently drops a route.
pub fn render_rows(
    rows: &[Row],
    width: usize,
    leaf_rows: usize,
    gutter_cells: usize,
) -> Vec<String> {
    if leaf_rows == 0 || width == 0 {
        return Vec::new();
    }
    let overflow = rows.len() > leaf_rows;
    let take = if overflow {
        leaf_rows.saturating_sub(1)
    } else {
        rows.len()
    };
    let mut out = Vec::with_capacity(leaf_rows.min(rows.len()) + usize::from(overflow));
    for row in rows.iter().take(take) {
        out.push(render_row(row, width, gutter_cells));
    }
    if overflow {
        out.push("… more".to_string());
    }
    out
}

fn gutter(row: &Row, cells: usize) -> String {
    if !row.gutter || cells == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(cells);
    if row.target.is_some() && row.selected {
        out.push('>');
    } else {
        out.push(' ');
    }
    for _ in 1..cells {
        out.push(' ');
    }
    out
}

fn render_spans(spans: &[Span], budget: usize, ellipsis: bool) -> String {
    if budget == 0 {
        return String::new();
    }
    let total: usize = spans.iter().map(span_width).sum();
    if total <= budget {
        let mut out = String::new();
        for span in spans {
            out.push_str(&render_span(span, &span.text));
        }
        return out;
    }
    let cut = if ellipsis { 1 } else { 0 };
    let mut out = String::new();
    let mut used = 0usize;
    let mut stopped = false;
    for span in spans {
        if stopped {
            break;
        }
        let mut kept = String::new();
        for grapheme in span.text.graphemes(true) {
            let width = UnicodeWidthStr::width(grapheme);
            if used + width + cut > budget {
                stopped = true;
                break;
            }
            kept.push_str(grapheme);
            used += width;
        }
        if !kept.is_empty() {
            out.push_str(&render_span(span, &kept));
        }
        if stopped {
            if ellipsis && used < budget {
                out.push_str(&render_span(span, "…"));
            }
            break;
        }
    }
    if !stopped && ellipsis && used < budget {
        if let Some(span) = spans.last() {
            out.push_str(&render_span(span, "…"));
        }
    }
    out
}

fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            width += UnicodeWidthStr::width(&text[start..index]);
            index += 1;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if byte == b'm' {
                    break;
                }
            }
            start = index;
            continue;
        }
        index += 1;
    }
    width + UnicodeWidthStr::width(&text[start..])
}

fn span_width(span: &Span) -> usize {
    UnicodeWidthStr::width(span.text.as_str())
}

fn render_span(span: &Span, text: &str) -> String {
    let codes = codes(span);
    if codes.is_empty() {
        return text.to_string();
    }
    format!("\x1b[{codes}m{text}\x1b[0m")
}

fn codes(span: &Span) -> String {
    let mut codes = Vec::new();
    if span.bold {
        codes.push("1".to_string());
    }
    if span.dim {
        codes.push("2".to_string());
    }
    if span.italic {
        codes.push("3".to_string());
    }
    if span.underline {
        codes.push("4".to_string());
    }
    if span.reverse {
        codes.push("7".to_string());
    }
    if let Some(color) = span.fg {
        codes.push(fg_code(color));
    }
    if let Some(color) = span.bg {
        codes.push(bg_code(color));
    }
    codes.join(";")
}

fn fg_code(color: Color) -> String {
    match color {
        Color::Index(index @ 0..=7) => format!("{}", 30 + index),
        Color::Index(index @ 8..=15) => format!("{}", 90 + (index - 8)),
        Color::Index(index) => format!("38;5;{index}"),
        Color::Rgb(r, g, b) => format!("38;2;{r};{g};{b}"),
    }
}

fn bg_code(color: Color) -> String {
    match color {
        Color::Index(index @ 0..=7) => format!("{}", 40 + index),
        Color::Index(index @ 8..=15) => format!("{}", 100 + (index - 8)),
        Color::Index(index) => format!("48;5;{index}"),
        Color::Rgb(r, g, b) => format!("48;2;{r};{g};{b}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_never_splits_a_grapheme() {
        let row = Row::plain("👩‍💻xyz");
        let rendered = render_row(&row, 2, 0);
        assert_eq!(rendered, "👩‍💻");
        assert!(!rendered.ends_with('\u{200d}'));
        let fitted = super::super::fit_text("👩‍💻xyz", 2);
        assert!(!fitted.ends_with('\u{200d}'));
    }
}
