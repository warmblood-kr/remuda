use super::{pad_to, truncate};
use crate::net::browser::{list_local_drive, DriveEntry, DriveListing, DriveReadError};
use crossterm::event::KeyCode;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;

type ListingReply = mpsc::Receiver<Result<DriveListing, DriveReadError>>;

#[derive(Default)]
pub(super) struct Sidebar {
    pub(super) visible: bool,
    listings: HashMap<String, DriveListing>,
    expanded: HashSet<String>,
    selected: Option<(String, bool)>,
    pending: Option<(String, ListingReply)>,
    error: Option<String>,
}

enum Row<'a> {
    Entry(&'a DriveEntry, usize),
    More(&'a str, &'a str, usize),
}

impl Row<'_> {
    fn key(&self) -> (&str, bool) {
        match self {
            Self::Entry(entry, _) => (&entry.uri, false),
            Self::More(directory, _, _) => (directory, true),
        }
    }
}

pub(super) enum Action {
    Handled,
    Open { uri: String, size: Option<u64> },
}

impl Sidebar {
    pub(super) fn toggle(&mut self) {
        self.visible = !self.visible;
        if self.visible && !self.listings.contains_key("") {
            self.request(String::new(), None);
        }
    }

    fn request(&mut self, directory: String, after: Option<String>) -> bool {
        if self.pending.is_some() {
            return false;
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        let scope = directory.clone();
        match std::thread::Builder::new()
            .name("cluster-drive-list".into())
            .spawn(move || {
                let scope = (!scope.is_empty()).then_some(scope.as_str());
                let _ = reply.send(list_local_drive(scope, after.as_deref()));
            }) {
            Ok(_) => {
                self.error = None;
                self.pending = Some((directory, receiver));
                true
            }
            Err(error) => {
                self.error = Some(error.to_string());
                false
            }
        }
    }

    pub(super) fn poll(&mut self) {
        let Some((_, receiver)) = self.pending.as_ref() else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err(DriveReadError::Unavailable),
        };
        let (directory, _) = self.pending.take().expect("polled a pending listing");
        match result {
            Ok(page) => match self.listings.entry(directory) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(page);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let listing = entry.get_mut();
                    listing.entries.extend(page.entries);
                    listing.complete = page.complete;
                    listing.next = page.next;
                }
            },
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    fn append_rows<'a>(&'a self, directory: &'a str, depth: usize, rows: &mut Vec<Row<'a>>) {
        let Some(listing) = self.listings.get(directory) else {
            return;
        };
        for entry in &listing.entries {
            rows.push(Row::Entry(entry, depth));
            if entry.is_directory && self.expanded.contains(&entry.uri) {
                self.append_rows(&entry.uri, depth + 1, rows);
            }
        }
        if let Some(after) = &listing.next {
            rows.push(Row::More(directory, after, depth));
        }
    }

    fn rows(&self) -> Vec<Row<'_>> {
        let mut rows = Vec::new();
        self.append_rows("", 0, &mut rows);
        rows
    }

    fn selected_index(&self, rows: &[Row<'_>]) -> usize {
        self.selected
            .as_ref()
            .and_then(|(key, more)| {
                rows.iter()
                    .position(|row| row.key() == (key.as_str(), *more))
            })
            .unwrap_or(0)
    }

    pub(super) fn key(&mut self, key: KeyCode) -> Action {
        let rows = self.rows();
        let index = self.selected_index(&rows);
        let row = match key {
            KeyCode::Up => rows.get(index.saturating_sub(1)),
            KeyCode::Down => rows.get((index + 1).min(rows.len().saturating_sub(1))),
            KeyCode::Enter if rows.is_empty() && !self.listings.contains_key("") => {
                self.request(String::new(), None);
                return Action::Handled;
            }
            KeyCode::Enter | KeyCode::Left | KeyCode::Right => rows.get(index),
            _ => None,
        };
        let Some(row) = row else {
            return Action::Handled;
        };
        if matches!(key, KeyCode::Up | KeyCode::Down) {
            let (uri, more) = row.key();
            self.selected = Some((uri.to_owned(), more));
            return Action::Handled;
        }
        match row {
            Row::Entry(entry, _) if entry.is_directory => {
                let uri = entry.uri.clone();
                if self.expanded.contains(&uri) && key != KeyCode::Right {
                    self.expanded.remove(&uri);
                } else if key != KeyCode::Left
                    && (self.listings.contains_key(&uri) || self.request(uri.clone(), None))
                {
                    self.expanded.insert(uri);
                }
            }
            Row::Entry(entry, _) if key == KeyCode::Enter => {
                return Action::Open {
                    uri: entry.uri.clone(),
                    size: entry.size,
                };
            }
            Row::More(directory, after, _) if key == KeyCode::Enter => {
                let (directory, after) = (directory.to_string(), after.to_string());
                self.request(directory, Some(after));
            }
            _ => {}
        }
        Action::Handled
    }

    pub(super) fn lines(&self, width: usize) -> Vec<String> {
        let status = if self.pending.is_some() {
            "Loading…"
        } else if let Some(error) = &self.error {
            error
        } else {
            "d Sessions"
        };
        let mut lines = vec!["Drive".to_owned(), truncate(status, width)];
        let rows = self.rows();
        let selected = self.selected_index(&rows);
        for (index, row) in rows.into_iter().enumerate() {
            let marker = if index == selected { '>' } else { ' ' };
            let line = match row {
                Row::Entry(entry, depth) => {
                    let glyph = if !entry.is_directory {
                        "·"
                    } else if self.expanded.contains(&entry.uri) {
                        "▾"
                    } else {
                        "▸"
                    };
                    let indent = " ".repeat((depth * 2).min(width.saturating_sub(6)));
                    let name = crate::text::strip_terminal_controls(&entry.name);
                    let label = format!("{marker}{indent} {glyph} {name}");
                    if let Some(size) = entry.size.filter(|_| width >= 14) {
                        let size = size_label(size);
                        let name_width = width.saturating_sub(size.len() + 1);
                        format!(
                            "{} {size}",
                            pad_to(&truncate(&label, name_width), name_width)
                        )
                    } else {
                        label
                    }
                }
                Row::More(_, _, depth) => {
                    let indent = " ".repeat((depth * 2).min(width.saturating_sub(6)));
                    format!("{marker}{indent} … More files")
                }
            };
            lines.push(truncate(&line, width));
        }
        if lines.len() == 2 && self.pending.is_none() && self.error.is_none() {
            lines.push(truncate("No files", width));
        }
        lines
    }
}

fn size_label(size: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * KIB;
    if size >= MIB {
        format!("{:.1} MiB", size as f64 / MIB as f64)
    } else if size >= KIB {
        format!("{:.1} KiB", size as f64 / KIB as f64)
    } else {
        format!("{size} B")
    }
}
