//! A bounded, inert presentation model for Markdown source.
//!
//! The native `document_markdown` projection deliberately flattens styling so
//! every adapter can read the same text. A GUI reader needs the structure back,
//! so this module turns already-bounded Markdown source into blocks, using the
//! inline spans from `markdown_inline`. It never loads a link, image or script: a link's destination is
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

pub use crate::markdown_inline::{inline, InlineStyle, Span};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Heading(u8),
    Paragraph,
    List { ordered: bool, level: usize },
    Quote,
    Code { language: Option<String> },
    Rule,
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
    fn oversized_and_excess_blocks_stay_bounded() {
        let long = "*".repeat(MAX_SOURCE_BYTES + 1);
        assert_eq!(parse(&long).len(), 1);
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

}