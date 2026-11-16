//! A bounded text/link projection of untrusted Markdown, shared by UI adapters.
//!
//! This is a reading projection, not a CommonMark implementation. It renders the
//! constructs a reader needs while keeping the source's line shape: headings,
//! fenced code verbatim, block quotes, list items, code spans, emphasis, inline
//! images as an explicit placeholder, and local links with the same `Target`
//! semantics as the HTML projection. Anything it does not recognise stays
//! literal, and no embedded HTML, script or resource load is executed.

use crate::document_html::{resolve, Document, Link};
use std::collections::HashMap;

const MAX_LINES: usize = 4092;
const MAX_LINKS: usize = 128;
const MAX_DEPTH: usize = 8;

/// Project Markdown source into inert title, text lines and typed local links.
pub fn parse(path: &str, source: &str) -> Result<Document, String> {
    let definitions = definitions(source);
    let mut document = Document::default();
    let mut fence: Option<(char, usize)> = None;
    for line in source.lines() {
        if let Some((character, size)) = fence {
            if closes_fence(line, character, size) {
                fence = None;
            } else {
                push(&mut document, line, false)?;
            }
            continue;
        }
        if let Some(open) = opens_fence(line) {
            fence = Some(open);
        } else if let Some(projected) = project_line(path, line, &definitions, &mut document)? {
            push(&mut document, &projected, true)?;
        }
    }
    Ok(document)
}

fn push(document: &mut Document, line: &str, collapse: bool) -> Result<(), String> {
    let clean = crate::text::strip_terminal_controls(line);
    if collapse && clean.trim().is_empty() && document.lines.last().is_some_and(|l| l.is_empty()) {
        return Ok(());
    }
    if document.lines.len() >= MAX_LINES {
        return Err("Markdown has too many text blocks".into());
    }
    document.lines.push(if collapse {
        clean.trim_end().to_owned()
    } else {
        clean.into_owned()
    });
    Ok(())
}

/// One source line, or `None` for a line that is markup only (fence, definition).
fn project_line(
    path: &str,
    line: &str,
    definitions: &HashMap<String, String>,
    document: &mut Document,
) -> Result<Option<String>, String> {
    if definition(line).is_some() {
        return Ok(None);
    }
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = line.trim_start_matches(' ');
    if indent <= 3 {
        if let Some((level, text)) = heading(rest) {
            if level == 1 && document.title.is_empty() {
                document.title = crate::text::strip_terminal_controls(text.trim()).into_owned();
            }
            return Ok(Some(inline(path, text, definitions, document)?));
        }
        if is_thematic_break(rest) {
            return Ok(Some(String::new()));
        }
        if let Some(quoted) = rest.strip_prefix('>') {
            let quoted = quoted.strip_prefix(' ').unwrap_or(quoted);
            return Ok(Some(format!(
                "| {}",
                inline(path, quoted, definitions, document)?
            )));
        }
    }
    if let Some((prefix, text)) = list_item(line) {
        return Ok(Some(format!(
            "{prefix}{}",
            inline(path, text, definitions, document)?
        )));
    }
    Ok(Some(inline(path, line, definitions, document)?))
}

fn heading(line: &str) -> Option<(usize, &str)> {
    if line.starts_with("```") || line.starts_with("~~~") {
        return None;
    }
    let level = line.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let text = rest.trim_end_matches(' ');
    let text = match text.strip_suffix('#') {
        Some(before) if before.ends_with(' ') || before.is_empty() => before.trim_end_matches(' '),
        _ => text,
    };
    Some((level, text.trim_start()))
}

fn is_thematic_break(line: &str) -> bool {
    let mut marks = line.chars().filter(|ch| !ch.is_whitespace());
    let Some(first) = marks.next() else {
        return false;
    };
    if !matches!(first, '-' | '*' | '_') {
        return false;
    }
    let mut count = 1;
    for mark in marks {
        if mark != first {
            return false;
        }
        count += 1;
    }
    count >= 3
}

/// Keep the item marker and its indentation so nesting stays visible; a `*`/`+`
/// bullet becomes `-` so it cannot be mistaken for an emphasis delimiter.
fn list_item(line: &str) -> Option<(String, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let (marker, width) = match rest.chars().next()? {
        '-' | '*' | '+' => ("-".to_owned(), 1),
        ch if ch.is_ascii_digit() => {
            let width = rest.chars().take_while(|c| c.is_ascii_digit()).count();
            match &rest[width..] {
                after if after.starts_with(". ") => (format!("{}.", &rest[..width]), width + 1),
                after if after.starts_with(") ") => (format!("{})", &rest[..width]), width + 1),
                _ => return None,
            }
        }
        _ => return None,
    };
    let text = rest[width..].strip_prefix(' ')?;
    Some((format!("{}{marker} ", " ".repeat(indent.min(8))), text))
}

fn inline(
    path: &str,
    text: &str,
    definitions: &HashMap<String, String>,
    document: &mut Document,
) -> Result<String, String> {
    let mut out = String::new();
    Inline {
        path,
        definitions,
        document,
    }
    .render(text, &mut out, 0)?;
    Ok(out)
}

struct Inline<'a> {
    path: &'a str,
    definitions: &'a HashMap<String, String>,
    document: &'a mut Document,
}

impl Inline<'_> {
    fn render(&mut self, text: &str, out: &mut String, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            out.push_str(text);
            return Ok(());
        }
        let chars: Vec<char> = text.chars().collect();
        let mut index = 0;
        while index < chars.len() {
            let ch = chars[index];
            if ch == '\\'
                && chars
                    .get(index + 1)
                    .is_some_and(|c| c.is_ascii_punctuation())
            {
                out.push(chars[index + 1]);
                index += 2;
            } else if ch == '`' {
                index = code_span(&chars, index, out);
            } else if ch == '!' && chars.get(index + 1) == Some(&'[') {
                index = image(&chars, index, out);
            } else if ch == '[' {
                index = self.link(&chars, index, out, depth)?;
            } else if ch == '*' || ch == '_' {
                index = self.emphasis(&chars, index, out, depth)?;
            } else {
                out.push(ch);
                index += 1;
            }
        }
        Ok(())
    }

    /// `![alt](src)` becomes a placeholder: this projection never loads or
    /// decodes image data.
    fn link(
        &mut self,
        chars: &[char],
        index: usize,
        out: &mut String,
        depth: usize,
    ) -> Result<usize, String> {
        let Some(label_end) = bracket(chars, index) else {
            out.push('[');
            return Ok(index + 1);
        };
        let raw: String = chars[index + 1..label_end].iter().collect();
        let found = self.destination(chars, label_end, &raw);
        let Some((destination, consumed)) = found else {
            out.push('[');
            out.push_str(&raw);
            out.push(']');
            return Ok(label_end + 1);
        };
        if self.document.links.len() == MAX_LINKS {
            return Err("Markdown has too many links".into());
        }
        let mut label = String::new();
        self.render(&raw, &mut label, depth + 1)?;
        let label = if label.trim().is_empty() {
            "(link)".to_owned()
        } else {
            label
        };
        self.document.links.push(Link {
            label: label.chars().take(512).collect(),
            target: resolve(self.path, &destination),
        });
        out.push_str(&format!("{label} [{}]", self.document.links.len()));
        Ok(consumed)
    }

    fn destination(&self, chars: &[char], label_end: usize, raw: &str) -> Option<(String, usize)> {
        if let Some((target_end, destination)) = parenthesized(chars, label_end + 1) {
            let destination = destination
                .split_whitespace()
                .next()?
                .trim_matches(['<', '>'])
                .to_owned();
            return Some((destination, target_end + 1));
        }
        if chars.get(label_end + 1) != Some(&'[') {
            let destination = self.definitions.get(&raw.trim().to_ascii_lowercase())?;
            return Some((destination.clone(), label_end + 1));
        }
        let end = bracket(chars, label_end + 1)?;
        let id: String = chars[label_end + 2..end].iter().collect();
        let id = if id.is_empty() { raw } else { &id };
        let destination = self.definitions.get(&id.trim().to_ascii_lowercase())?;
        Some((destination.clone(), end + 1))
    }

    fn emphasis(
        &mut self,
        chars: &[char],
        index: usize,
        out: &mut String,
        depth: usize,
    ) -> Result<usize, String> {
        let ch = chars[index];
        let open = run(chars, index, ch);
        let intraword = ch == '_' && index > 0 && chars[index - 1].is_alphanumeric();
        if open > 3 || intraword || chars.get(index + open).is_none_or(|c| c.is_whitespace()) {
            repeat(out, ch, open);
            return Ok(index + open);
        }
        let mut cursor = index + open;
        while cursor < chars.len() {
            if chars[cursor] != ch {
                cursor += 1;
                continue;
            }
            let close = run(chars, cursor, ch);
            let closes = close == open
                && !chars[cursor - 1].is_whitespace()
                && (ch != '_'
                    || chars
                        .get(cursor + close)
                        .is_none_or(|c| !c.is_alphanumeric()));
            if closes {
                let inner: String = chars[index + open..cursor].iter().collect();
                self.render(&inner, out, depth + 1)?;
                return Ok(cursor + close);
            }
            cursor += close;
        }
        repeat(out, ch, open);
        Ok(index + open)
    }
}

fn repeat(out: &mut String, ch: char, count: usize) {
    out.extend(std::iter::repeat_n(ch, count));
}

fn run(chars: &[char], index: usize, ch: char) -> usize {
    chars[index..].iter().take_while(|c| **c == ch).count()
}

fn code_span(chars: &[char], index: usize, out: &mut String) -> usize {
    let open = run(chars, index, '`');
    let mut cursor = index + open;
    while cursor < chars.len() {
        if chars[cursor] != '`' {
            cursor += 1;
            continue;
        }
        let close = run(chars, cursor, '`');
        if close == open {
            out.extend(&chars[index + open..cursor]);
            return cursor + close;
        }
        cursor += close;
    }
    repeat(out, '`', open);
    index + open
}

fn bracket(chars: &[char], index: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, ch) in chars[index..].iter().enumerate() {
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

fn parenthesized(chars: &[char], index: usize) -> Option<(usize, String)> {
    if chars.get(index) != Some(&'(') {
        return None;
    }
    let mut depth = 0usize;
    for (offset, ch) in chars[index..].iter().enumerate() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let inner = chars[index + 1..index + offset].iter().collect();
                    return Some((index + offset, inner));
                }
            }
            _ => {}
        }
    }
    None
}

fn image(chars: &[char], index: usize, out: &mut String) -> usize {
    let label_start = index + 1;
    let Some(label_end) = bracket(chars, label_start) else {
        out.push('!');
        return index + 1;
    };
    let Some((target_end, _)) = parenthesized(chars, label_end + 1) else {
        out.push('!');
        return index + 1;
    };
    let alt: String = chars[label_start + 1..label_end].iter().collect();
    let alt = alt.trim();
    if alt.is_empty() {
        out.push_str("[image]");
    } else {
        out.push_str(&format!("[image: {alt}]"));
    }
    target_end + 1
}

fn opens_fence(line: &str) -> Option<(char, usize)> {
    let rest = line.trim_start_matches(' ');
    let ch = rest.chars().next()?;
    if !matches!(ch, '`' | '~') {
        return None;
    }
    let size = rest.chars().take_while(|c| *c == ch).count();
    (size >= 3).then_some((ch, size))
}

fn closes_fence(line: &str, ch: char, size: usize) -> bool {
    let rest = line.trim_start_matches(' ');
    let count = rest.chars().take_while(|c| *c == ch).count();
    count >= size && rest.chars().skip(count).all(char::is_whitespace)
}

fn definition(line: &str) -> Option<(String, String)> {
    if line.len() - line.trim_start_matches(' ').len() > 3 {
        return None;
    }
    let rest = line.trim_start_matches(' ').strip_prefix('[')?;
    let close = rest.find(']')?;
    let id = rest[..close].trim().to_ascii_lowercase();
    let after = rest[close + 1..].strip_prefix(':')?;
    let destination = after.split_whitespace().next()?.trim_matches(['<', '>']);
    (!id.is_empty() && !destination.is_empty()).then(|| (id, destination.to_owned()))
}

fn definitions(source: &str) -> HashMap<String, String> {
    source.lines().filter_map(definition).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_quotes_lists_and_fences_keep_the_reading_shape() {
        let source = "# Guide #\n\nIntro with **bold** and `code`.\n\n> quoted *text*\n\n\
                      - one\n* two\n  1. sub\n\n```sh\n# not a heading\n**not bold**\n```\n\n---\n";
        let document = parse("docs/guide.md", source).unwrap();
        assert_eq!(document.title, "Guide");
        assert_eq!(
            document.lines,
            vec![
                "Guide",
                "",
                "Intro with bold and code.",
                "",
                "| quoted text",
                "",
                "- one",
                "- two",
                "  1. sub",
                "",
                "# not a heading",
                "**not bold**",
                "",
            ]
        );
        assert!(document.links.is_empty());
    }

    #[test]
    fn inline_and_reference_links_resolve_against_the_origin_and_images_stay_explicit() {
        let source = "[Next](sub/next.md) and ![diagram](img/d.png) \
                      and [bad](javascript:alert(1)) and [the guide][g] or [guide](/guide.md)\n\n\
                      [g]: ../guide.md\n";
        let document = parse("docs/index.md", source).unwrap();
        assert_eq!(
            document.lines,
            vec![
                "Next [1] and [image: diagram] and bad [2] and the guide [3] or guide [4]",
                ""
            ]
        );
        let targets: Vec<_> = document.links.iter().map(|link| &link.target).collect();
        assert_eq!(
            targets,
            vec![
                &crate::document_html::Target::Vfs("docs/sub/next.md".into()),
                &crate::document_html::Target::Blocked("Only local VFS links are supported".into()),
                &crate::document_html::Target::Vfs("guide.md".into()),
                &crate::document_html::Target::Vfs("guide.md".into()),
            ]
        );
        assert_eq!(document.links[0].label, "Next");
    }

    #[test]
    fn unclosed_and_unknown_markup_stays_literal_and_controls_never_reach_the_screen() {
        let source =
            "[unclosed text\n\n<script>alert(1)</script>\n\n\u{1b}[31msnake_case_name\u{202e}\n";
        let document = parse("docs/raw.md", source).unwrap();
        assert!(document.lines.contains(&"[unclosed text".to_owned()));
        assert!(document
            .lines
            .contains(&"<script>alert(1)</script>".to_owned()));
        assert!(document.lines.contains(&"[31msnake_case_name".to_owned()));
        assert!(document.links.is_empty());
    }

    #[test]
    fn link_and_line_limits_are_explicit_errors() {
        assert!(parse("x.md", &"[a](b) ".repeat(MAX_LINKS + 1)).is_err());
        assert!(parse("x.md", &"line\n".repeat(MAX_LINES + 1))
            .unwrap_err()
            .contains("too many"));
        assert!(parse("x.md", "```\nunclosed fence to the end\n").is_ok());
    }
}
