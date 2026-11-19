//! A bounded, inert presentation model for Markdown source.
//!
//! The native `document_markdown` projection deliberately flattens styling so
//! every adapter can read the same text. A GUI reader needs the structure back,
//! so this module turns already-bounded Markdown source into blocks and inline
//! spans. It never loads a link, image or script: a link's destination is
//! discarded and an image becomes a text placeholder, exactly like the text
//! projection.
//!
//! Scope is the subset a reader needs: ATX headings, fenced code, block quotes,
//! list items with their marker and indentation, thematic breaks, paragraphs,
//! and inline bold/italic/code/link/image. Tables, setext headings, nested
//! block structure and raw HTML stay literal. Every loop is bounded by the
//! source size plus a fixed budget, so malformed delimiters cannot turn a line
//! into quadratic work.

/// A source larger than the browser read bound is not styled.
pub const MAX_SOURCE_BYTES: usize = 1024 * 1024;
/// Most blocks one document may produce.
pub const MAX_BLOCKS: usize = 4096;
/// Most inline spans one line may produce.
const MAX_SPANS: usize = 256;
/// Recursion through emphasis while parsing one line.
const MAX_DEPTH: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Heading(u8),
    Paragraph,
    List { ordered: bool, level: usize },
    Quote,
    Code { language: Option<String> },
    Rule,
}

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    pub spans: Vec<Span>,
    /// Literal lines of a fenced code block, without the fence.
    pub code: Vec<String>,
    /// A list item's display marker (`•` or `1.`), empty for other blocks.
    pub marker: String,
}

impl Block {
    /// Text as the reader presents it, markers excluded.
    pub fn text(&self) -> String {
        if !self.code.is_empty() {
            return self.code.join("\n");
        }
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

/// Parse bounded Markdown source. A document over [`MAX_SOURCE_BYTES`] is
/// returned as one plain paragraph so the caller still has something to show.
pub fn parse(source: &str) -> Vec<Block> {
    if source.len() > MAX_SOURCE_BYTES {
        return plain(&[source.to_owned()]);
    }
    let mut blocks = Vec::new();
    let mut lines = source.lines().peekable();
    while let Some(line) = lines.next() {
        if blocks.len() >= MAX_BLOCKS {
            break;
        }
        if let Some((fence, language)) = open_fence(line) {
            let mut code = Vec::new();
            for body in lines.by_ref() {
                if closes_fence(body, fence) {
                    break;
                }
                // Preserve source indentation while still filtering terminal and bidi controls.
                code.push(body.split('\t').map(|part| remuda_native::text::strip_terminal_controls(part).into_owned()).collect::<Vec<_>>().join("\t"));
                if code.len() >= MAX_BLOCKS {
                    break;
                }
            }
            blocks.push(Block {
                kind: BlockKind::Code { language },
                spans: Vec::new(),
                code,
                marker: String::new(),
            });
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Some((level, text)) = heading(line) {
            blocks.push(text_block(BlockKind::Heading(level), text));
            continue;
        }
        if rule(line) {
            blocks.push(Block {
                kind: BlockKind::Rule,
                spans: Vec::new(),
                code: Vec::new(),
                marker: String::new(),
            });
            continue;
        }
        if let Some(rest) = quote(line) {
            blocks.push(text_block(BlockKind::Quote, rest));
            continue;
        }
        if let Some((marker, level, ordered, text)) = list_item(line) {
            let mut block = text_block(BlockKind::List { ordered, level }, text);
            block.marker = marker;
            blocks.push(block);
            continue;
        }
        blocks.push(text_block(BlockKind::Paragraph, line));
    }
    blocks
}

/// A last-resort model when no Markdown source is available: one paragraph per
/// projected line. This is the old plain projection, not styled Markdown.
pub fn plain(lines: &[String]) -> Vec<Block> {
    lines
        .iter()
        .take(MAX_BLOCKS)
        .map(|line| text_block(BlockKind::Paragraph, line))
        .collect()
}

fn text_block(kind: BlockKind, text: &str) -> Block {
    Block {
        kind,
        spans: inline(text),
        code: Vec::new(),
        marker: String::new(),
    }
}

fn open_fence(line: &str) -> Option<(char, Option<String>)> {
    let rest = line.trim_start_matches(' ');
    if line.len() - rest.len() > 3 {
        return None;
    }
    let fence = rest.chars().next()?;
    if fence != '`' && fence != '~' {
        return None;
    }
    let width = rest.chars().take_while(|ch| *ch == fence).count();
    if width < 3 {
        return None;
    }
    let info = rest[width..].trim();
    let language = info.split_whitespace().next().map(sanitize_language);
    Some((fence, language))
}

fn closes_fence(line: &str, fence: char) -> bool {
    let rest = line.trim_start_matches(' ');
    if line.len() - rest.len() > 3 {
        return false;
    }
    let width = rest.chars().take_while(|ch| *ch == fence).count();
    width >= 3 && rest[width..].trim().is_empty()
}

fn sanitize_language(info: &str) -> String {
    info.chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '#' | '.'))
        .take(24)
        .collect::<String>()
        .to_ascii_lowercase()
}

fn heading(line: &str) -> Option<(u8, &str)> {
    let rest = line.trim_start_matches(' ');
    if line.len() - rest.len() > 3 {
        return None;
    }
    let level = rest.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let after = &rest[level..];
    if !after.is_empty() && !after.starts_with(' ') {
        return None;
    }
    let text = after.trim();
    let text = text.trim_end_matches('#').trim_end();
    Some((level as u8, text))
}

fn rule(line: &str) -> bool {
    let rest = line.trim();
    let first = match rest.chars().next() {
        Some(ch @ ('-' | '*' | '_')) => ch,
        _ => return false,
    };
    let mut count = 0;
    for ch in rest.chars() {
        if ch == first {
            count += 1;
        } else if !ch.is_whitespace() {
            return false;
        }
    }
    count >= 3
}

fn quote(line: &str) -> Option<&str> {
    let rest = line.trim_start_matches(' ');
    let quoted = rest.strip_prefix('>')?;
    Some(quoted.strip_prefix(' ').unwrap_or(quoted))
}

/// Returns `(marker, level, ordered, text)` for a list item.
fn list_item(line: &str) -> Option<(String, usize, bool, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let level = (indent / 2).min(6);
    let first = rest.chars().next()?;
    if matches!(first, '-' | '*' | '+') {
        let text = rest[1..].strip_prefix(' ')?;
        return Some(("•".to_owned(), level, false, text));
    }
    let digits = rest.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let after = &rest[digits..];
    let delimiter = after.chars().next()?;
    if !matches!(delimiter, '.' | ')') {
        return None;
    }
    let text = after[1..].strip_prefix(' ')?;
    Some((format!("{}.", &rest[..digits]), level, true, text))
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
    fn headings_lists_quotes_and_rules_keep_their_structure() {
        let blocks = parse(
            "# Title\n\nIntro **bold** and `code`.\n\n## Sub\n\n- one\n  - two\n1. first\n> quoted\n\n---\n",
        );
        assert_eq!(blocks[0].kind, BlockKind::Heading(1));
        assert_eq!(blocks[0].text(), "Title");
        assert_eq!(blocks[1].kind, BlockKind::Paragraph);
        assert_eq!(blocks[2].kind, BlockKind::Heading(2));
        assert_eq!(
            blocks[3].kind,
            BlockKind::List {
                ordered: false,
                level: 0
            }
        );
        assert_eq!(blocks[3].marker, "•");
        assert_eq!(
            blocks[4].kind,
            BlockKind::List {
                ordered: false,
                level: 1
            }
        );
        assert_eq!(
            blocks[5].kind,
            BlockKind::List {
                ordered: true,
                level: 0
            }
        );
        assert_eq!(blocks[5].marker, "1.");
        assert_eq!(blocks[6].kind, BlockKind::Quote);
        assert_eq!(blocks[7].kind, BlockKind::Rule);
    }

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
    fn fenced_code_stays_literal_and_names_its_language() {
        let blocks = parse("```python\ndef f(x):\n    return `x`\n```\n");
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].kind,
            BlockKind::Code {
                language: Some("python".into())
            }
        );
        assert_eq!(blocks[0].code, ["def f(x):", "    return `x`"]);
    }

    #[test]
    fn malformed_markup_stays_literal_and_bounds_hold() {
        let spans = inline("**open [x]( y `z");
        let text: String = spans.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(text, "**open [x]( y `z");
        let long = "*".repeat(MAX_SOURCE_BYTES + 1);
        assert_eq!(parse(&long).len(), 1);
        let hostile = "**a".repeat(100_000);
        let spans = inline(&hostile);
        assert!(!spans.is_empty());
        let many = "x\n".repeat(MAX_BLOCKS + 100);
        assert!(parse(&many).len() <= MAX_BLOCKS);
    }

    #[test]
    fn fenced_code_preserves_tabs_but_filters_other_controls() {
        let blocks = parse("```rs\n\tlet x = \"한\";\u{1b}\u{202e}\n```\n");
        assert_eq!(blocks[0].code[0], "\tlet x = \"한\";");
    }

    #[test]
    fn controls_are_stripped_from_blocks_code_and_link_labels() {
        let source = "# Ti\u{1b}tle\u{202e}\n\nA [la\u{1b}bel](next.md) end\n\n```py\nx = \"\u{1b}]0;x\"\n```\n";
        let blocks = parse(source);
        assert_eq!(blocks[0].text(), "Title");
        assert!(blocks.iter().all(|block| {
            !block
                .text()
                .chars()
                .any(|ch| ch.is_control() || ch == '\u{202e}')
        }));
        let link = blocks[1].spans.iter().find(|span| span.style.link).unwrap();
        assert_eq!(link.text, "label");
        assert_eq!(link.target.as_deref(), Some("next.md"));
        assert_eq!(blocks[2].code[0], "x = \"]0;x\"");
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
