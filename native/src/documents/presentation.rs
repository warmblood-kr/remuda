//! Display labels and bounded text layout for document panes.

use super::MAX_LINES;

pub fn basename_label(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    path_label(name)
}

/// The lexical parent of a VFS directory path, or `None` at the root. The
/// root is the empty path; every answer is a path `navigate` re-validates.
pub fn parent_directory(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    Some(match trimmed.rfind('/') {
        Some(index) => format!("{}/", &trimmed[..index]),
        None => String::new(),
    })
}

#[allow(dead_code)] // used by tests and the browser layer that follows.
pub(crate) fn wrap_text(lines: &[String], width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in lines {
        let mut row = String::new();
        let mut used = 0;
        for ch in crate::text::strip_terminal_controls(line).chars() {
            let size = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + size > width && !row.is_empty() {
                out.push(std::mem::take(&mut row));
                used = 0;
            }
            if out.len() == MAX_LINES {
                out.push("[… display limit reached]".into());
                return out;
            }
            row.push(if size > width { '?' } else { ch });
            used += size.min(width);
        }
        out.push(row);
        if out.len() >= MAX_LINES {
            out.push("[… display limit reached]".into());
            break;
        }
    }
    out
}

/// Escape controls and backslashes while preserving the original navigation identity.
pub fn path_label(path: &str) -> String {
    path.chars()
        .flat_map(|ch| {
            if crate::text::is_terminal_control(ch) || ch == '\\' {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}

#[allow(dead_code)] // used by tests and the browser layer that follows.
pub(crate) fn clip(text: &str, width: usize) -> String {
    let clean = crate::text::strip_terminal_controls(text);
    let mut used = 0;
    clean
        .chars()
        .take_while(|ch| {
            used += unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0);
            used <= width
        })
        .collect()
}
