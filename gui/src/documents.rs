//! GUI document pane: a bounded, inert view over the shared VFS document model.
//!
//! The projection, link resolution and 1 MiB read bound live in
//! `remuda_native::documents`; this adapter only decides what the GUI shows and
//! keeps every dimension the pane can render bounded.

use remuda_native::documents::{Browser, Location};
use std::path::PathBuf;

/// Widest frame the pane will request; the browser clips and wraps to it.
pub const MAX_COLUMNS: usize = 120;
/// Most body rows the pane will request.
pub const MAX_ROWS: usize = 48;
/// A GUI caller must bound a source before handing it to a parser; the daemon
/// read request is capped at this many bytes by `documents::MAX_BYTES`.
pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;

pub struct Pane {
    browser: Browser,
    key_events: usize,
}

impl Pane {
    /// Start at the VFS root of `socket`. No document is read until selected.
    pub fn new(socket: PathBuf) -> Self {
        Self {
            browser: Browser::new(socket),
            key_events: 0,
        }
    }

    /// Record that one key event reached this pane (focus evidence for tests).
    pub fn note_key(&mut self) {
        self.key_events = self.key_events.saturating_add(1);
    }

    pub fn key_events(&self) -> usize {
        self.key_events
    }

    pub fn poll(&mut self) {
        self.browser.poll();
    }

    /// The exact rows the GUI renders: the same bounded projection the TUI uses.
    pub fn frame(&self, columns: usize, rows: usize) -> Vec<String> {
        self.browser
            .lines(columns.clamp(1, MAX_COLUMNS), rows.clamp(4, MAX_ROWS))
    }

    pub fn status(&self) -> String {
        if self.browser.loading {
            return "LOADING · reading from the local daemon".into();
        }
        match &self.browser.page.error {
            Some(error)
                if error.contains("binary") || error.contains("not UTF-8 text") =>
            {
                format!(
                "UNSUPPORTED · {error} — this pane reads text, Markdown and HTML; \
                 image preview is not implemented"
                )
            }
            Some(error) => format!("ERROR · {error}"),
            None => format!("ready · {}", self.browser.page.location.path()),
        }
    }

    pub fn move_selection(&mut self, down: bool) {
        self.browser.move_selection(down);
    }

    pub fn open_selected(&mut self) {
        self.browser.open_selected();
    }

    pub fn select_link(&mut self, next: bool) {
        self.browser.select_link(next);
    }

    pub fn back(&mut self) -> bool {
        self.browser.back()
    }

    pub fn refresh(&mut self) {
        self.browser.refresh();
    }

    /// Open one local VFS document by address (`docs/guide.md`).
    pub fn open_document(&mut self, path: &str) {
        self.browser.navigate(Location::Document(path.to_owned()));
    }

    pub fn location(&self) -> &str {
        self.browser.page.location.path()
    }
}
