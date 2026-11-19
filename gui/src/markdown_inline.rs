//! Bounded, inert inline spans for the GUI Markdown reader.
//!
//! One display line becomes styled text runs. A link keeps only its label and
//! target; an image becomes a text placeholder. Nothing here loads a target,
//! runs a fence or fetches a resource, and every loop is bounded by the line
//! length plus a fixed span budget.
//!
//! The block model in `markdown` consumes these spans; the split keeps the
//! inline contract testable without a document.

/// Most inline spans one line may produce.
const MAX_SPANS: usize = 256;
/// Recursion through emphasis while parsing one line.
const MAX_DEPTH: usize = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link: bool,
    pub image: bool,
}


#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: InlineStyle,
    /// Raw destination of an inline `[label](target)` link. The view resolves
    /// it with the shared VFS link rules; `None` for ordinary text.
    pub target: Option<String>,
}

/// Parse inline spans. A malformed delimiter stays literal.
pub fn inline(text: &str) -> Vec<Span> {
    let mut budget = text.chars().count().saturating_mul(8).saturating_add(1024);
    let mut out = Vec::new();
    parse_inline(text, InlineStyle::default(), 0, &mut budget, &mut out);
    out
}

fn parse_inline(
    text: &str,
    style: InlineStyle,
    depth: usize,
    budget: &mut usize,
    out: &mut Vec<Span>,
) {
    let chars: Vec<char> = text.chars().collect();
    let mut literal = String::new();
    let mut index = 0;
    while index < chars.len() {
        if out.len() >= MAX_SPANS || *budget == 0 {
            literal.extend(&chars[index..]);
            break;
        }
        *budget -= 1;
        let ch = chars[index];
        let escaped = ch == '\\'
            && chars
                .get(index + 1)
                .is_some_and(|next| next.is_ascii_punctuation());
        if escaped {
            literal.push(chars[index + 1]);
            index += 2;
            continue;
        }
        if ch == '`' {
            if let Some((close, content)) = code_span(&chars, index, budget) {
                flush(&mut literal, style, out);
                let mut code = style;
                code.code = true;
                push_span(out, content, code, None);
                index = close;
                continue;
            }
        }
        if let Some((width, marker)) = emphasis_marker(&chars, index) {
            if depth < MAX_DEPTH && !literal.is_empty() {
                flush(&mut literal, style, out);
            }
            if depth < MAX_DEPTH {
                if let Some(close) = find_emphasis(&chars, index + width, marker, width, budget) {
                    let inner: String = chars[index + width..close].iter().collect();
                    let mut nested = style;
                    if width == 2 {
                        nested.bold = true;
                    } else {
                        nested.italic = true;
                    }
                    parse_inline(&inner, nested, depth + 1, budget, out);
                    index = close + width;
                    continue;
                }
            }
        }
        if ch == '!' && chars.get(index + 1) == Some(&'[') {
            if let Some((close, label, target)) = link(&chars, index + 1, budget) {
                flush(&mut literal, style, out);
                let mut image = style;
                image.image = true;
                push_span(out, format!("[image: {label}]"), image, target);
                index = close;
                continue;
            }
        }
        if ch == '[' {
            if let Some((close, label, Some(target))) = link(&chars, index, budget) {
                flush(&mut literal, style, out);
                let start = out.len();
                let mut nested = style;
                nested.link = true;
                append_inline(&label, nested, depth, budget, out);
                for span in &mut out[start..] {
                    span.target = Some(target.clone());
                }
                index = close;
                continue;
            }
        }
        literal.push(ch);
        index += 1;
    }
    flush(&mut literal, style, out);
}

fn append_inline(
    text: &str,
    style: InlineStyle,
    depth: usize,
    budget: &mut usize,
    out: &mut Vec<Span>,
) {
    if out.len() >= MAX_SPANS {
        push_span(out, text.to_owned(), style, None);
        return;
    }
    parse_inline(text, style, depth + 1, budget, out);
}

fn flush(literal: &mut String, style: InlineStyle, out: &mut Vec<Span>) {
    if !literal.is_empty() {
        push_span(out, std::mem::take(literal), style, None);
    }
}

fn push_span(out: &mut Vec<Span>, text: String, style: InlineStyle, target: Option<String>) {
    let text = remuda_native::text::strip_terminal_controls(&text).into_owned();
    if text.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut() {
        if last.style == style && last.target == target {
            last.text.push_str(&text);
            return;
        }
    }
    out.push(Span {
        text,
        style,
        target,
    });
}

fn code_span(chars: &[char], index: usize, budget: &mut usize) -> Option<(usize, String)> {
    let width = chars[index..]
        .iter()
        .take(8)
        .take_while(|ch| **ch == '`')
        .count();
    let mut cursor = index + width;
    while cursor < chars.len() {
        if *budget == 0 {
            return None;
        }
        *budget = budget.saturating_sub(1);
        if chars[cursor] == '`' {
            let close_width = chars[cursor..]
                .iter()
                .take(8)
                .take_while(|ch| **ch == '`')
                .count();
            if close_width == width {
                let content: String = chars[index + width..cursor].iter().collect();
                return Some((cursor + close_width, content));
            }
            cursor += close_width;
            continue;
        }
        cursor += 1;
    }
    None
}

/// Returns `(delimiter width, delimiter character)` when a markdown emphasis
/// delimiter starts at `index`.
fn emphasis_marker(chars: &[char], index: usize) -> Option<(usize, char)> {
    let ch = *chars.get(index)?;
    if ch != '*' && ch != '_' {
        return None;
    }
    // Only one or two delimiters matter; counting the whole run here turned a
    // long repeated delimiter into quadratic work.
    let width = chars[index..]
        .iter()
        .take(2)
        .take_while(|next| **next == ch)
        .count();
    if ch == '_' && index > 0 && chars[index - 1].is_alphanumeric() {
        return None;
    }
    Some((width, ch))
}

fn find_emphasis(
    chars: &[char],
    from: usize,
    marker: char,
    width: usize,
    budget: &mut usize,
) -> Option<usize> {
    let mut cursor = from;
    while cursor < chars.len() {
        if *budget == 0 {
            return None;
        }
        *budget = budget.saturating_sub(1);
        if chars[cursor] != marker {
            cursor += 1;
            continue;
        }
        let run = chars[cursor..]
            .iter()
            .take(width)
            .take_while(|next| **next == marker)
            .count();
        if run >= width {
            if marker == '_'
                && chars
                    .get(cursor + run)
                    .is_some_and(|next| next.is_alphanumeric())
            {
                cursor += run;
                continue;
            }
            return Some(cursor);
        }
        cursor += run;
    }
    None
}

/// Parse `[label](target)`, `[label][ref]` or `[label]`. The destination is
/// intentionally not returned for display; only the reference form is flagged.
fn link(
    chars: &[char],
    index: usize,
    budget: &mut usize,
) -> Option<(usize, String, Option<String>)> {
    if chars.get(index) != Some(&'[') {
        return None;
    }
    let close = bracket(chars, index, budget)?;
    let label: String = chars[index + 1..close].iter().collect();
    let next = chars.get(close + 1)?;
    if *next == '(' {
        let end = parenthesized(chars, close + 1, budget)?;
        let target: String = chars[close + 2..end].iter().collect();
        return Some((end + 1, label, Some(target)));
    }
    if *next == '[' {
        let end = bracket(chars, close + 1, budget)?;
        let id: String = chars[close + 2..end].iter().collect();
        return Some((end + 1, label, Some(id)));
    }
    Some((close + 1, label, None))
}

fn bracket(chars: &[char], index: usize, budget: &mut usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, ch) in chars[index..].iter().enumerate() {
        if *budget == 0 {
            return None;
        }
        *budget = budget.saturating_sub(1);
        match ch {
            '[' => depth += 1,
            ']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn parenthesized(chars: &[char], index: usize, budget: &mut usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, ch) in chars[index..].iter().enumerate() {
        if *budget == 0 {
            return None;
        }
        *budget = budget.saturating_sub(1);
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index + offset);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_bold_italic_code_and_links_produce_styled_spans() {
        let spans = inline("a **b** _c_ `d` [e](remuda:///x) ![f](x.png)");
        assert!(spans.iter().any(|span| span.text == "b" && span.style.bold));
        assert!(spans
            .iter()
            .any(|span| span.text == "c" && span.style.italic));
        assert!(spans.iter().any(|span| span.text == "d" && span.style.code));
        assert!(spans.iter().any(|span| span.text == "e" && span.style.link));
        assert!(spans.iter().any(|span| {
            span.text == "[image: f]" && span.style.image && span.target.as_deref() == Some("x.png")
        }));
        assert!(!spans.iter().any(|span| span.text.contains("remuda:///")));
    }

    #[test]
    fn malformed_inline_markup_stays_literal_and_bounds_hold() {
        let spans = inline("**open [x]( y `z");
        let text: String = spans.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(text, "**open [x]( y `z");
        let hostile = "**a".repeat(100_000);
        let spans = inline(&hostile);
        assert!(!spans.is_empty());
    }

    #[test]
    fn multibyte_text_and_escapes_stay_on_character_boundaries() {
        let spans = inline("한글 **굵게** \\*literal\\*");
        assert!(spans
            .iter()
            .any(|span| span.text == "굵게" && span.style.bold));
        let text: String = spans.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(text, "한글 굵게 *literal*");
    }
}
