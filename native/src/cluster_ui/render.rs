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
        // Keep a minimum pre-status budget so a long error/status cannot erase
        // the name. The status keeps its leading live/ended/stale token.
        let minimum_before = (available / 2).min(12);
        let status_cap = available.saturating_sub(minimum_before);
        let status_width = span_width(status).min(status_cap);
        let before = render_spans(
            &row.spans[..index],
            available.saturating_sub(status_width),
            ellipsis,
        );
        let before_width = visible_width(&before);
        out.push_str(&before);
        let remaining = available.saturating_sub(before_width);
        let status = render_spans(std::slice::from_ref(status), remaining, true);
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

/// One window into a flat row list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowWindow {
    pub start: usize,
    pub end: usize,
    /// Whether a `… more` line should follow `start..end`. A selection always
    /// wins over this marker, so the window plus marker never exceeds the
    /// leaf's capacity.
    pub more: bool,
}

/// Pick a leaf window that never drops the selected row. Selection wins over
/// `… more`; without one, keep the historical top window and overflow marker.
pub fn row_window(total: usize, capacity: usize, selected: Option<usize>) -> RowWindow {
    if total == 0 || capacity == 0 {
        return RowWindow {
            start: 0,
            end: 0,
            more: false,
        };
    }
    if total <= capacity {
        return RowWindow {
            start: 0,
            end: total,
            more: false,
        };
    }
    let selected = selected.filter(|index| *index < total);
    match selected {
        Some(index) if index < capacity => {
            // The selection is in the first leaf. Keep the marker only when it
            // does not take the selection's line; otherwise the selection wins.
            if index + 1 < capacity {
                RowWindow {
                    start: 0,
                    end: capacity - 1,
                    more: true,
                }
            } else {
                RowWindow {
                    start: 0,
                    end: capacity,
                    more: false,
                }
            }
        }
        Some(index) => {
            // Bottom-follow, matching the peer list: the selected row is the
            // last content line once the roster scrolls.
            let start = index + 1 - capacity;
            RowWindow {
                start,
                end: (start + capacity).min(total),
                more: false,
            }
        }
        None => RowWindow {
            start: 0,
            end: capacity - 1,
            more: true,
        },
    }
}

/// Render a leaf through a selection-following window, so the selected target's
/// first row stays visible through navigation, resize and filtering. Targets
/// remain in the composition and navigation, so no route is silently dropped.
pub fn render_rows(
    rows: &[Row],
    width: usize,
    leaf_rows: usize,
    gutter_cells: usize,
) -> Vec<String> {
    if leaf_rows == 0 || width == 0 {
        return Vec::new();
    }
    let selected = rows
        .iter()
        .position(|row| row.selected && row.target.is_some());
    let window = row_window(rows.len(), leaf_rows, selected);
    let mut out = Vec::with_capacity((window.end - window.start) + usize::from(window.more));
    for row in rows.iter().take(window.end).skip(window.start) {
        out.push(render_row(row, width, gutter_cells));
    }
    if window.more {
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

    fn rows(count: usize) -> Vec<Row> {
        (0..count)
            .map(|index| Row::plain(format!("row{index}")))
            .collect()
    }

    fn select(rows: &mut [Row], index: usize) {
        rows[index].target = Some(90 + index as u32);
        rows[index].selected = true;
    }

    #[test]
    fn selected_row_scrolls_into_a_short_leaf() {
        let mut rows = rows(8);
        select(&mut rows, 6);
        let rendered = render_rows(&rows, 20, 3, 0);
        assert_eq!(rendered, vec!["row4", "row5", "row6"]);
        assert!(rendered.last().is_some_and(|row| row == "row6"));
        assert!(!rendered.iter().any(|row| row.contains("more")));
    }

    #[test]
    fn height_one_renders_the_selected_row_not_the_more_marker() {
        let mut rows = rows(8);
        select(&mut rows, 5);
        assert_eq!(render_rows(&rows, 20, 1, 0), vec!["row5"]);
    }

    #[test]
    fn a_selection_in_the_last_slot_is_not_replaced_by_the_more_marker() {
        let mut rows = rows(5);
        select(&mut rows, 2);
        let rendered = render_rows(&rows, 20, 3, 0);
        assert_eq!(rendered, vec!["row0", "row1", "row2"]);
        assert!(!rendered.iter().any(|row| row.contains("more")));
    }

    #[test]
    fn without_a_selection_the_overflow_marker_is_preserved() {
        assert_eq!(
            render_rows(&rows(5), 20, 3, 0),
            vec!["row0", "row1", "… more"]
        );
    }

    #[test]
    fn a_full_leaf_renders_every_row_for_the_peer_list_path() {
        let mut rows = rows(5);
        select(&mut rows, 4);
        let rendered = render_rows(&rows, 20, 5, 0);
        assert_eq!(rendered.len(), 5);
        assert_eq!(rendered.last().map(String::as_str), Some("row4"));
        assert!(!rendered.iter().any(|row| row.contains("more")));
    }

    #[test]
    fn a_multirow_target_keeps_its_selected_first_row_visible() {
        let mut rows = rows(7);
        select(&mut rows, 2);
        // The rest of the target block keeps `target = None`; only its first
        // row is navigable and carries the selection.
        rows[3].spans = vec![Span::plain("block-body")];
        let rendered = render_rows(&rows, 20, 5, 0);
        assert_eq!(
            rendered,
            vec!["row0", "row1", "row2", "block-body", "… more"]
        );
    }

    fn session_row(name: &str, detail: Option<&str>, status: &str) -> Row {
        let mut spans = vec![Span::plain("    "), Span::plain(name)];
        if let Some(detail) = detail {
            spans.push(Span::plain(format!(" · {detail}")));
        }
        spans.push(Span::plain(" "));
        let status_index = spans.len();
        spans.push(Span::plain(status));
        Row {
            spans,
            target: Some(7),
            selected: true,
            gutter: true,
            status: Some(status_index),
        }
    }

    #[test]
    fn normal_width_fitting_status_is_unchanged() {
        let row = session_row("dev", None, "live");
        assert_eq!(render_row(&row, 30, 1), ">    dev live");
    }

    #[test]
    fn narrow_status_keeps_name_and_leading_token() {
        for token in ["live", "ended", "stale"] {
            let status = format!("{token} · {}", "remote error ".repeat(20));
            let row = session_row("build", None, &status);
            let rendered = render_row(&row, 24, 1);
            assert!(rendered.starts_with(">    build"), "{rendered}");
            assert!(rendered.contains(token), "{rendered}");
            assert!(rendered.contains('…'), "{rendered}");
            assert!(super::visible_width(&rendered) <= 24, "{rendered}");
        }
    }

    #[test]
    fn narrow_selected_row_with_detail_label_keeps_name_and_leading_status() {
        let status = "live · stale · remote session error detail";
        let row = session_row("build", Some("worker"), status);
        let narrow = render_row(&row, 32, 1);
        assert!(narrow.starts_with(">    build"), "{narrow}");
        assert!(narrow.contains("live"), "{narrow}");
        assert!(super::visible_width(&narrow) <= 32, "{narrow}");

        let wide = render_row(&row, 80, 1);
        assert!(wide.contains("build · worker"), "{wide}");
        assert!(wide.contains(status), "{wide}");
    }
}
