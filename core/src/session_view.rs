//! A rendered list and the exact session snapshot it describes.
//! Callers must validate before displaying or acting on a peer response.
use crate::SessionSummary;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const MAX_SESSIONS: usize = 256;
const MAX_VIEW_BYTES: usize = 32 * 1024;
const MAX_ROW_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewEntry {
    pub name: String,
    pub instance_id: String,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionView {
    sessions: Vec<SessionSummary>,
    height: usize,
    entries: Vec<ViewEntry>,
}

/// A view whose displayed targets and local selection were validated.
/// Its fields cannot be constructed from a peer's data without validation.
pub struct ValidatedView {
    sessions: Vec<SessionSummary>,
    entries: Vec<ViewEntry>,
    height: usize,
    width: u16,
    selected: Option<(String, String)>,
}
impl ValidatedView {
    pub fn sessions(&self) -> &[SessionSummary] {
        &self.sessions
    }
    pub fn entries(&self) -> &[ViewEntry] {
        &self.entries
    }
    pub fn height(&self) -> usize {
        self.height
    }
    /// Consumers must revalidate whenever the local selection changes.
    pub fn selected(&self) -> Option<(&str, &str)> {
        self.selected
            .as_ref()
            .map(|(name, instance)| (name.as_str(), instance.as_str()))
    }
    /// Consumers must revalidate when the pane width changes.
    pub fn width(&self) -> u16 {
        self.width
    }
}

impl SessionView {
    /// Construct raw untrusted data. It contains no accessible action targets.
    pub fn new(sessions: Vec<SessionSummary>, height: usize, entries: Vec<ViewEntry>) -> Self {
        Self {
            sessions,
            height,
            entries,
        }
    }

    /// Validate identity and actual fitted output, then apply local selection.
    /// The UI must paint these rows using `fit_session_row` at this same width.
    pub fn validate_for_display(
        &self,
        selected: Option<(&str, &str)>,
        width: u16,
    ) -> Result<ValidatedView, &'static str> {
        self.validate()?;
        if selected.is_some_and(|(name, instance)| {
            !self
                .entries
                .iter()
                .any(|entry| entry.name == name && entry.instance_id == instance)
        }) {
            return Err("selected instance not in snapshot");
        }
        let mut entries = Vec::with_capacity(self.entries.len());
        let mut sessions = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            // Selection is a local per-viewer fact. Remove the only allowed
            // peer reverse-video sequence before inspecting or showing a row.
            let mut clean = entry.clone();
            for line in &mut clean.lines {
                *line = line.replace("\x1b[1;7;36m", "\x1b[1m");
            }
            if !fitted_row_shows_name(&fit_session_row(&clean.lines[0], width), &clean.name) {
                return Err("session name is not fully visible");
            }
            if selected == Some((entry.name.as_str(), entry.instance_id.as_str())) {
                clean.lines[0] = highlight_name(&clean.lines[0], &clean.name)?;
            }
            sessions.push(
                self.sessions
                    .iter()
                    .find(|session| {
                        session.name == entry.name
                            && session.instance_id.as_deref() == Some(entry.instance_id.as_str())
                    })
                    .expect("validated view identity")
                    .clone(),
            );
            entries.push(clean);
        }
        Ok(ValidatedView {
            sessions,
            entries,
            height: self.height,
            width,
            selected: selected.map(|(name, instance)| (name.to_owned(), instance.to_owned())),
        })
    }

    /// Reject partial, stale, malformed or terminal-active views as a whole.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !matches!(self.height, 1 | 3)
            || self.sessions.len() > MAX_SESSIONS
            || self.entries.len() != self.sessions.len()
        {
            return Err("invalid view dimensions");
        }
        let mut expected = HashMap::new();
        for session in &self.sessions {
            let id = session
                .instance_id
                .as_deref()
                .filter(|id| valid_identity(id))
                .ok_or("unbound session")?;
            if session.unavailable
                || !valid_identity(&session.name)
                || expected.insert(session.name.as_str(), id).is_some()
            {
                return Err("invalid snapshot identity");
            }
        }
        let mut seen = HashSet::new();
        let mut bytes = 0usize;
        for entry in &self.entries {
            if expected.get(entry.name.as_str()).copied() != Some(entry.instance_id.as_str())
                || !seen.insert(entry.name.as_str())
                || entry.lines.len() != self.height
            {
                return Err("view identity mismatch");
            }
            for (index, line) in entry.lines.iter().enumerate() {
                bytes = bytes.checked_add(line.len()).ok_or("view too large")?;
                let plain = safe_line(line).ok_or("unsafe view")?;
                if line.len() > MAX_ROW_BYTES || bytes > MAX_VIEW_BYTES {
                    return Err("oversized view");
                }
                if index == 0 && !shows_name(&plain, &entry.name) {
                    return Err("displayed name does not match identity");
                }
            }
        }
        Ok(())
    }
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.trim() == value
        && !value.chars().any(|ch| ch.is_control() || (ch.is_whitespace() && ch != ' ') || unicode_width::UnicodeWidthChar::width(ch) == Some(0) || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
}

/// Rebuild the locally selected name as one uninterrupted trusted style span.
/// Validation has already bounded and checked every SGR sequence in `line`.
fn highlight_name(line: &str, name: &str) -> Result<String, &'static str> {
    let mut at = 0;
    let mut indent = String::new();
    loop {
        at = skip_sgr(line, at)?;
        if !line[at..].starts_with(' ') {
            break;
        }
        indent.push(' ');
        at += 1;
    }
    if let Some(highlighted) = select_name_at(&indent, line, at, name)? {
        return Ok(highlighted);
    }
    let start = name_after_status(line).ok_or("displayed name mismatch")?;
    select_name_at(&line[..start], line, start, name)?.ok_or("displayed name mismatch")
}

/// Rebuild `line` with the name at `start` selected. `None` when the name is
/// not there; SGR between the name's characters is skipped so the renderer's
/// own name style does not hide it.
fn select_name_at(
    prefix: &str,
    line: &str,
    start: usize,
    name: &str,
) -> Result<Option<String>, &'static str> {
    let mut at = start;
    for ch in name.chars() {
        at = skip_sgr(line, at)?;
        if !line[at..].starts_with(ch) {
            return Ok(None);
        }
        at += ch.len_utf8();
    }
    Ok(Some(format!(
        "{prefix}\x1b[1;7;36m{name}\x1b[0m{}",
        &line[at..]
    )))
}

fn skip_sgr(line: &str, mut at: usize) -> Result<usize, &'static str> {
    while line[at..].starts_with('\x1b') {
        let end = line[at..].find('m').ok_or("invalid style")?;
        at += end + 1;
    }
    Ok(at)
}

/// The status glyphs the list renderer puts before a name: the horse means a
/// working program, the hollow circle a fresh idle one, `!` a program waiting
/// for a human, and the filled dot the ordinary state. The cell is two
/// terminal cells wide, so one-cell glyphs are padded before the separator.
const STATUS_GLYPHS: [&str; 4] = ["🏇", "○", "●", "!"];
/// The status cell and its separator, in terminal cells.
const STATUS_PREFIX_CELLS: usize = 3;

fn glyph_width(glyph: &str) -> usize {
    glyph
        .chars()
        .map(|ch| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1))
        .sum()
}

/// Byte offset where the name starts on a row with a status cell:
/// indentation, a two-cell glyph, then the separator. `None` on a row without
/// one, so an older peer's row keeps its own shape.
fn name_after_status(line: &str) -> Option<usize> {
    let mut at = 0;
    loop {
        at = skip_sgr(line, at).ok()?;
        if !line[at..].starts_with(' ') {
            break;
        }
        at += 1;
    }
    let glyph = STATUS_GLYPHS
        .iter()
        .find(|glyph| line[at..].starts_with(**glyph))?;
    at = skip_sgr(line, at + glyph.len()).ok()?;
    // One-cell glyphs are padded; the renderer never styles inside the cell,
    // so the padding is plain spaces up to the separator.
    for _ in glyph_width(glyph)..STATUS_PREFIX_CELLS {
        if !line[at..].starts_with(' ') {
            return None;
        }
        at += 1;
    }
    Some(at)
}

fn shows_name(plain: &str, name: &str) -> bool {
    let text = plain.trim_start_matches(' ');
    name_at(text, name) || name_after_status(text).is_some_and(|at| name_at(&text[at..], name))
}

fn name_at(text: &str, name: &str) -> bool {
    text.strip_prefix(name)
        .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(' '))
}

/// Accept only the styles emitted by the Lua list renderer, each closed on the same row.
fn safe_line(line: &str) -> Option<String> {
    let mut plain = String::new();
    let mut chars = line.chars().peekable();
    let mut styled = false;
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.next()? != '[' {
                return None;
            }
            let mut params = String::new();
            while chars
                .peek()
                .is_some_and(|ch| ch.is_ascii_digit() || *ch == ';')
            {
                params.push(chars.next()?);
                if params.len() > 32 {
                    return None;
                }
            }
            if chars.next()? != 'm' {
                return None;
            }
            if !matches!(params.as_str(), "0" | "1" | "1;7;36" | "2" | "31" | "32") {
                return None;
            }
            styled = params != "0";
        } else if ch.is_control()
            || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            return None;
        } else {
            plain.push(ch);
        }
    }
    (!styled).then_some(plain)
}

/// Check a row AFTER applying the real UI fit function, not a width estimate.
/// Reject rows containing control effects, hidden characters or a clipped name.
pub fn fitted_row_shows_name(fitted: &str, name: &str) -> bool {
    valid_identity(name) && safe_line(fitted).is_some_and(|plain| shows_name(&plain, name))
}

/// Exactly `width` terminal cells, reserving the final cell for truncation.
pub fn fit(text: &str, width: u16) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    if visible_width(text) > width {
        let mut out = String::new();
        let mut used = 0usize;
        let mut chars = text.chars();
        let mut styled = false;
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                styled = true;
                out.push(ch);
                for escape in chars.by_ref() {
                    out.push(escape);
                    if escape == 'm' {
                        break;
                    }
                }
                continue;
            }
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
            if used + w > width.saturating_sub(1) {
                break;
            }
            out.push(ch);
            used += w;
        }
        if styled {
            out.push_str("\x1b[0m");
        }
        out.push('→');
        out.push_str(&" ".repeat(width.saturating_sub(used + 1)));
        out
    } else {
        let mut out = text.to_owned();
        out.push_str(&" ".repeat(width - visible_width(text)));
        out
    }
}
fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            for skip in chars.by_ref() {
                if skip == 'm' {
                    break;
                }
            }
        } else {
            width += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
        }
    }
    width
}
/// Fit a list row to `width`. The current renderer puts a two-cell status cell
/// and a separator before the name, so the name shrinks while the mark stays;
/// rows from an older peer keep the trailing dot and its reserved cells.
pub fn fit_session_row(content: &str, width: u16) -> String {
    const MARKER: &str = " 🏇";
    const PLAIN_PAD: &str = "   ";
    for state in [" \x1b[32m●\x1b[0m", " \x1b[31m●\x1b[0m"] {
        if let Some(name) = content.strip_suffix(state) {
            let suffix = format!("{state}{PLAIN_PAD}");
            let name_width = width.saturating_sub(visible_width(&suffix) as u16);
            return format!("{}{}", fit(name, name_width), suffix);
        }
        let attached = format!("{state}{MARKER}");
        if let Some(name) = content.strip_suffix(&attached) {
            let name_width = width.saturating_sub(visible_width(&attached) as u16);
            return format!("{}{}", fit(name, name_width), attached);
        }
    }
    if let Some(name) = content.strip_suffix(MARKER) {
        let name_width = width.saturating_sub(visible_width(MARKER) as u16);
        return format!("{}{}", fit(name, name_width), MARKER);
    }
    if let Some(at) = name_after_status(content) {
        let prefix = &content[..at];
        let name_width = width.saturating_sub(visible_width(prefix) as u16);
        // A compact row joins the detail with two spaces and a dim style. The
        // detail may be cut, but the name must survive whole when it can.
        if let Some(dim) = content[at..].find("\x1b[2m") {
            let head = &content[at..at + dim];
            if visible_width(head) < usize::from(name_width) {
                let rest = name_width - visible_width(head) as u16;
                return format!("{prefix}{head}{}", fit(&content[at + dim..], rest));
            }
            return format!("{prefix}{}", fit(head.trim_end_matches(' '), name_width));
        }
        return format!("{prefix}{}", fit(&content[at..], name_width));
    }
    fit(content, width)
}
