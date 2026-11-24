//! A bounded text/link projection of untrusted Markdown, shared by UI adapters.
//!
//! This is a reading projection, not a CommonMark implementation. It renders the
//! constructs a reader needs while keeping the source's line shape: headings,
//! fenced code verbatim, block quotes, list items, code spans, emphasis, inline
//! images as an explicit placeholder, and local links with the same `Target`
//! semantics as the HTML projection. Anything it does not recognise stays
//! literal, and no embedded HTML, script or resource load is executed.
//!
//! Input size is the caller's responsibility: [`parse`] has no inherent byte
//! cap. The TUI read path (`documents::fetch`) enforces the 1 MiB document
//! limit; a GUI or any other adapter must bound the source before calling.

use crate::document_html::{resolve, Document, Image, Link, MAX_IMAGES};
use std::collections::HashMap;

const MAX_LINES: usize = 4092;
const MAX_LINKS: usize = 128;
const MAX_DEPTH: usize = 8;
/// Scan work allowed per input character on one line.
const INLINE_WORK: usize = 8;

/// Project Markdown source into inert title, text lines and typed local links.
/// Callers must bound `source`; this parser has no inherent byte cap.
pub fn parse(path: &str, source: &str) -> Result<Document, String> {
    project(path, source, None)
}

pub fn parse_with_images(path: &str, source: &str) -> Result<(Document, Vec<Image>), String> {
    let mut images = Vec::new();
    let document = project(path, source, Some(&mut images))?;
    Ok((document, images))
}

fn project(
    path: &str,
    source: &str,
    mut images: Option<&mut Vec<Image>>,
) -> Result<Document, String> {
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
        } else if let Some(projected) = project_line(
            path,
            line,
            &definitions,
            &mut document,
            images.as_deref_mut(),
        )? {
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
    images: Option<&mut Vec<Image>>,
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
            return Ok(Some(inline(path, text, definitions, document, images)?));
        }
        if is_thematic_break(rest) {
            return Ok(Some(String::new()));
        }
        if let Some(quoted) = rest.strip_prefix('>') {
            let quoted = quoted.strip_prefix(' ').unwrap_or(quoted);
            return Ok(Some(format!(
                "| {}",
                inline(path, quoted, definitions, document, images)?
            )));
        }
    }
    if let Some((prefix, text)) = list_item(line) {
        return Ok(Some(format!(
            "{prefix}{}",
            inline(path, text, definitions, document, images)?
        )));
    }
    Ok(Some(inline(path, line, definitions, document, images)?))
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
    images: Option<&mut Vec<Image>>,
) -> Result<String, String> {
    let mut out = String::new();
    Inline {
        path,
        definitions,
        document,
        images,
        budget: text
            .chars()
            .count()
            .saturating_mul(INLINE_WORK)
            .saturating_add(1024),
        exhausted: false,
    }
    .render(text, &mut out, 0)?;
    Ok(out)
}

struct Inline<'a> {
    path: &'a str,
    definitions: &'a HashMap<String, String>,
    document: &'a mut Document,
    images: Option<&'a mut Vec<Image>>,
    /// Characters this line may still inspect. Malformed input must not turn
    /// repeated delimiter scans into quadratic work, so the total scan work per
    /// line is bounded; when it runs out the rest of the line stays literal.
    budget: usize,
    exhausted: bool,
}

impl Inline<'_> {
    fn tick(&mut self, step: usize) -> bool {
        if self.exhausted || self.budget < step {
            self.exhausted = true;
            return false;
        }
        self.budget -= step;
        true
    }

    fn run_len(&mut self, chars: &[char], index: usize, ch: char) -> usize {
        let mut count = 0;
        while index + count < chars.len() && chars[index + count] == ch {
            if !self.tick(1) {
                break;
            }
            count += 1;
        }
        count
    }

    fn bracket(&mut self, chars: &[char], index: usize) -> Option<usize> {
        let mut depth = 0usize;
        for (offset, ch) in chars[index..].iter().enumerate() {
            if !self.tick(1) {
                return None;
            }
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

    fn parenthesized(&mut self, chars: &[char], index: usize) -> Option<(usize, String)> {
        if chars.get(index) != Some(&'(') {
            return None;
        }
        let mut depth = 0usize;
        for (offset, ch) in chars[index..].iter().enumerate() {
            if !self.tick(1) {
                return None;
            }
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

    fn render(&mut self, text: &str, out: &mut String, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            out.push_str(text);
            return Ok(());
        }
        let chars: Vec<char> = text.chars().collect();
        let mut index = 0;
        while index < chars.len() {
            if self.exhausted {
                out.extend(&chars[index..]);
                return Ok(());
            }
            let ch = chars[index];
            if ch == '\\'
                && chars
                    .get(index + 1)
                    .is_some_and(|c| c.is_ascii_punctuation())
            {
                out.push(chars[index + 1]);
                index += 2;
            } else if ch == '`' {
                index = self.code_span(&chars, index, out);
            } else if ch == '!' && chars.get(index + 1) == Some(&'[') {
                index = self.image(&chars, index, out);
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
        let Some(label_end) = self.bracket(chars, index) else {
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
        let label = shared_label(&label);
        self.document.links.push(Link {
            label: label.clone(),
            target: resolve(self.path, &destination),
        });
        out.push_str(&format!("{label} [{}]", self.document.links.len()));
        Ok(consumed)
    }

    fn destination(
        &mut self,
        chars: &[char],
        label_end: usize,
        raw: &str,
    ) -> Option<(String, usize)> {
        if let Some((target_end, destination)) = self.parenthesized(chars, label_end + 1) {
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
        let end = self.bracket(chars, label_end + 1)?;
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
        let open = self.run_len(chars, index, ch);
        if open == 0 {
            out.push(ch);
            return Ok(index + 1);
        }
        let intraword = ch == '_' && index > 0 && chars[index - 1].is_alphanumeric();
        if open > 3 || intraword || chars.get(index + open).is_none_or(|c| c.is_whitespace()) {
            repeat(out, ch, open);
            return Ok(index + open);
        }
        let mut cursor = index + open;
        while cursor < chars.len() {
            if !self.tick(1) {
                break;
            }
            if chars[cursor] != ch {
                cursor += 1;
                continue;
            }
            let close = self.run_len(chars, cursor, ch);
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

    fn code_span(&mut self, chars: &[char], index: usize, out: &mut String) -> usize {
        let open = self.run_len(chars, index, '`');
        if open == 0 {
            out.push('`');
            return index + 1;
        }
        let mut cursor = index + open;
        while cursor < chars.len() {
            if !self.tick(1) {
                break;
            }
            if chars[cursor] != '`' {
                cursor += 1;
                continue;
            }
            let close = self.run_len(chars, cursor, '`');
            if close == open {
                out.extend(&chars[index + open..cursor]);
                return cursor + close;
            }
            cursor += close.max(1);
        }
        repeat(out, '`', open);
        index + open
    }

    fn image(&mut self, chars: &[char], index: usize, out: &mut String) -> usize {
        let label_start = index + 1;
        let Some(label_end) = self.bracket(chars, label_start) else {
            out.push('!');
            return index + 1;
        };
        let Some((target_end, raw)) = self.parenthesized(chars, label_end + 1) else {
            out.push('!');
            return index + 1;
        };
        let alt: String = chars[label_start + 1..label_end].iter().collect();
        if let (Some(images), Some(src)) =
            (self.images.as_deref_mut(), raw.split_whitespace().next())
        {
            if images.len() < MAX_IMAGES {
                images.push(Image {
                    label: shared_label(&alt),
                    target: resolve(self.path, src.trim_matches(['<', '>'])),
                    line: self.document.lines.len(),
                });
            }
        }
        image_placeholder(&alt, out);
        target_end + 1
    }
}

fn repeat(out: &mut String, ch: char, count: usize) {
    out.extend(std::iter::repeat_n(ch, count));
}

/// `Dispatch` and `Browser` sanitize their own frames, but a label is shared
/// data: strip controls/bidi and collapse whitespace before any adapter,
/// including a GUI, can read it directly.
fn shared_label(label: &str) -> String {
    let clean = crate::text::strip_terminal_controls(label);
    let collapsed = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        "(link)".to_owned()
    } else {
        collapsed.chars().take(512).collect()
    }
}

fn image_placeholder(alt: &str, out: &mut String) {
    let alt = alt.trim();
    if alt.is_empty() {
        out.push_str("[image]");
    } else {
        out.push_str(&format!("[image: {alt}]"));
    }
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

/// Definitions inside a fenced block are code, not document markup: they must
/// neither resolve a reference outside the fence nor disappear from the block.
fn definitions(source: &str) -> HashMap<String, String> {
    let mut fence: Option<(char, usize)> = None;
    let mut map = HashMap::new();
    for line in source.lines() {
        if let Some((ch, size)) = fence {
            if closes_fence(line, ch, size) {
                fence = None;
            }
            continue;
        }
        match opens_fence(line) {
            Some(open) => fence = Some(open),
            None => {
                if let Some((id, destination)) = definition(line) {
                    // CommonMark: the first definition of an id is the one used.
                    map.entry(id).or_insert(destination);
                }
            }
        }
    }
    map
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
    fn adversarial_lines_do_not_turn_delimiter_scans_into_quadratic_work() {
        let started = std::time::Instant::now();
        for source in [
            "*a ".repeat(200_000),
            "[ ".repeat(200_000),
            "`a ".repeat(200_000),
            "**a [".repeat(100_000),
        ] {
            let document = parse("x.md", &source).unwrap();
            assert!(!document.lines.is_empty());
            assert!(document.links.is_empty());
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "elapsed {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn thematic_breaks_accept_only_markers_and_whitespace() {
        let document = parse("x.md", "- keep --- text\n* * *\n___\n- - -\n").unwrap();
        assert_eq!(
            document.lines,
            vec!["- keep --- text".to_owned(), String::new()]
        );
    }

    #[test]
    fn definitions_inside_fenced_code_do_not_resolve_outside_references() {
        let document = parse("docs/x.md", "```\n[id]: ../evil.md\n```\n\nSee [id].\n").unwrap();
        assert!(document.lines.contains(&"[id]: ../evil.md".to_owned()));
        assert!(document.lines.contains(&"See [id].".to_owned()));
        assert!(document.links.is_empty());
    }

    #[test]
    fn malformed_and_nested_syntax_falls_back_without_losing_text() {
        for (source, expected) in [
            ("[a](b\n", "[a](b"),
            ("**a\n", "**a"),
            ("`a\n", "`a"),
            ("[[a]](b)\n", "[a] [1]"),
        ] {
            let document = parse("docs/x.md", source).unwrap();
            assert_eq!(document.lines[0], expected, "{source}");
        }
    }

    #[test]
    fn duplicate_reference_definitions_keep_the_first_destination() {
        let document = parse(
            "docs/x.md",
            "Read [a][id] and [b][id].\n\n[id]: one.md\n[id]: two.md\n",
        )
        .unwrap();
        assert_eq!(document.lines[0], "Read a [1] and b [2].");
        assert_eq!(
            document
                .links
                .iter()
                .map(|link| &link.target)
                .collect::<Vec<_>>(),
            vec![
                &crate::document_html::Target::Vfs("docs/one.md".into()),
                &crate::document_html::Target::Vfs("docs/one.md".into()),
            ]
        );
    }

    #[test]
    fn shared_link_labels_carry_no_terminal_controls_or_bidi_overrides() {
        let document = parse(
            "docs/x.md",
            "[bad\u{202e}\u{1b}label](next.md) and [two\u{2066}](next.md)\n",
        )
        .unwrap();
        assert_eq!(document.links[0].label, "badlabel");
        assert_eq!(document.links[1].label, "two");
        assert!(document
            .links
            .iter()
            .all(|link| !link.label.chars().any(crate::text::is_terminal_control)));
        assert!(document.lines[0].contains("badlabel [1]"));
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
