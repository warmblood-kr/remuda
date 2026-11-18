//! Read-only VFS navigation, independent of session input and renderer choice.
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use remuda_core::{
    protocol::{Request, Response},
    vfs,
};
use serde_json::Value;
use std::{
    cell::Cell,
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

const MAX_BYTES: usize = 1024 * 1024;
const MAX_LINES: usize = 4096;
const MAX_HISTORY: usize = 32;
const MAX_HISTORY_SOURCE_BYTES: usize = 2 * MAX_BYTES;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    Directory(String),
    Document(String),
}
impl Location {
    pub fn path(&self) -> &str {
        match self {
            Self::Directory(path) | Self::Document(path) => path,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub directory: bool,
    /// Display override. The synthetic parent row shows `..` while its path
    /// stays the real parent address used for navigation.
    pub label: Option<String>,
}
impl Entry {
    /// The row label with its full identity: the override when present,
    /// otherwise the escaped path. The TUI listing uses this.
    pub fn display_label(&self) -> String {
        self.label
            .clone()
            .unwrap_or_else(|| path_label(&self.path))
    }
    /// The compact row label: the override when present, otherwise the
    /// escaped last path component. Selection and opening still use `path`.
    pub fn display_name(&self) -> String {
        self.label
            .clone()
            .unwrap_or_else(|| basename_label(&self.path))
    }
}
#[derive(Clone, Debug)]
pub struct Page {
    pub location: Location,
    pub entries: Vec<Entry>,
    pub text: Vec<String>,
    pub position: usize,
    pub identity: Option<(u64, String)>,
    pub error: Option<String>,
    /// Text/link projection for HTML and Markdown; `None` for plain text.
    pub projection: Option<crate::document_html::Document>,
    /// Original Markdown source for a styled GUI reader. Shared by history
    /// clones so a back stack does not copy the same 1 MiB source per entry.
    pub markdown_source: Option<std::sync::Arc<str>>,
    /// The bounded original UTF-8 document, shared by history snapshots.
    pub source_text: Option<std::sync::Arc<str>>,
    pub selected_link: Option<usize>,
}
impl Page {
    fn empty(location: Location) -> Self {
        Self {
            location,
            entries: Vec::new(),
            text: Vec::new(),
            position: 0,
            identity: None,
            error: None,
            projection: None,
            markdown_source: None,
            source_text: None,
            selected_link: None,
        }
    }
}
type Reply = Result<Page, String>;
/// One worker at a time; navigation replaces only the queued desired request.
pub struct Browser {
    pub page: Page,
    pub loading: bool,
    socket: PathBuf,
    history: Vec<Page>,
    generation: u64,
    pending: bool,
    flight: Option<(u64, mpsc::Receiver<Reply>)>,
    rendered_count: Cell<usize>,
    /// Opt-in `..` row for directory listings.
    parent_navigation: bool,
    address: Option<(String, usize)>,
}
impl Browser {
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Show a `..` row at the top of a non-root directory listing. Off by
    /// default so a document listing keeps its earlier shape until it opts in.
    pub fn enable_parent_navigation(&mut self) {
        self.parent_navigation = true;
        self.inject_parent_row();
    }
    /// Prepend the parent row after a listing arrives. A restored page keeps
    /// its own rows, so Back shows the same layout it left.
    fn inject_parent_row(&mut self) {
        if !self.parent_navigation {
            return;
        }
        let Location::Directory(path) = &self.page.location else {
            return;
        };
        let Some(parent) = parent_directory(path) else {
            return;
        };
        if self
            .page
            .entries
            .first()
            .is_some_and(|entry| entry.label.is_some())
        {
            return;
        }
        let had_entries = !self.page.entries.is_empty();
        self.page.entries.insert(
            0,
            Entry {
                path: parent,
                directory: true,
                label: Some("..".into()),
            },
        );
        // Keep the earlier behavior of selecting the first real entry.
        self.page.position = usize::from(had_entries);
    }
    pub fn new(socket: PathBuf) -> Self {
        let mut browser = Self {
            page: Page::empty(Location::Directory(String::new())),
            loading: false,
            socket,
            history: Vec::new(),
            generation: 0,
            pending: false,
            flight: None,
            rendered_count: Cell::new(0),
            parent_navigation: false,
            address: None,
        };
        browser.refresh();
        browser
    }
    pub fn cancel(&mut self) {
        self.address = None;
        self.generation = self.generation.wrapping_add(1);
        self.pending = false;
        self.loading = false;
    }
    pub fn refresh(&mut self) {
        if self.address.is_some() {
            return;
        }
        self.cancel();
        self.loading = true;
        self.pending = true;
        self.page.error = None;
        self.poll();
    }
    pub fn move_selection(&mut self, down: bool) {
        if let Some((_, position)) = &mut self.address {
            let last = self.rendered_count.get().saturating_sub(1);
            let current = (*position).min(last);
            *position = if down {
                current.saturating_add(1).min(last)
            } else {
                current.saturating_sub(1)
            };
            return;
        }
        let count = match self.page.location {
            Location::Directory(_) => self.page.entries.len(),
            Location::Document(_) => self.rendered_count.get(),
        };
        self.page.position = if down {
            self.page
                .position
                .saturating_add(1)
                .min(count.saturating_sub(1))
        } else {
            self.page.position.saturating_sub(1)
        };
    }
    pub fn open_selected(&mut self) {
        if self.loading || self.address.is_some() {
            return;
        }
        if let Some(link) = self.selected_link() {
            use crate::document_html::Target;
            match link.target.clone() {
                Target::Vfs(path) => self.navigate(if path.is_empty() || path.ends_with('/') {
                    Location::Directory(path)
                } else {
                    Location::Document(path)
                }),
                Target::Blocked(reason) => self.page.error = Some(reason),
            }
            return;
        }
        let Some(entry) = self.page.entries.get(self.page.position) else {
            return;
        };
        let location = if entry.directory {
            Location::Directory(entry.path.clone())
        } else {
            Location::Document(entry.path.clone())
        };
        self.navigate(location);
    }
    pub fn select_link(&mut self, next: bool) {
        if self.loading || self.address.is_some() {
            return;
        }
        let count = self
            .page
            .projection
            .as_ref()
            .map_or(0, |doc| doc.links.len());
        if count == 0 {
            return;
        }
        self.page.selected_link = Some(match self.page.selected_link {
            Some(index) if next => (index + 1) % count,
            Some(index) => (index + count - 1) % count,
            None if next => 0,
            None => count - 1,
        });
        self.page.error = None;
    }
    fn selected_link(&self) -> Option<&crate::document_html::Link> {
        self.page
            .projection
            .as_ref()?
            .links
            .get(self.page.selected_link?)
    }
    pub fn navigate(&mut self, location: Location) {
        let path = match &location {
            Location::Directory(path) => path.strip_suffix('/').unwrap_or(path),
            Location::Document(path) => path,
        };
        if !(path.is_empty() && matches!(location, Location::Directory(_)))
            && vfs::validate_path(path).is_err()
        {
            self.page.error = Some("Invalid VFS address".into());
            return;
        }
        let location = match location {
            Location::Directory(path) if !path.is_empty() => {
                Location::Directory(format!("{}/", path.trim_end_matches('/')))
            }
            other => other,
        };
        if self.history.len() == MAX_HISTORY {
            self.history.remove(0);
        }
        self.history.push(self.page.clone());
        // Retain small historical documents for rich Back navigation while
        // bounding raw source independently per pane. Oldest evictions retain
        // projection/identity and never silently fetch a newer version.
        let mut retained: usize = self.history.iter().map(|page| page.source_text.as_ref().map_or(0, |s| s.len())).sum();
        for historical in &mut self.history {
            if retained <= MAX_HISTORY_SOURCE_BYTES { break; }
            if let Some(source) = historical.source_text.take() {
                retained -= source.len();
                historical.markdown_source = None;
            }
        }
        self.page = Page::empty(location);
        self.refresh();
    }
    /// True when Back leaves the root and should restore session focus.
    pub fn back(&mut self) -> bool {
        self.back_with_move().0
    }

    /// `(root, moved)`: `root` keeps the existing focus behavior, while
    /// `moved` says a history page was actually restored. A cancelled address
    /// or empty history changes nothing.
    pub fn back_with_move(&mut self) -> (bool, bool) {
        if self.address.take().is_some() {
            return (false, false);
        }
        self.cancel();
        match self.history.pop() {
            Some(page) => {
                self.page = page;
                // Keep the historical projection and identity unchanged.
                // Refetch only on explicit Refresh, since the current VFS
                // version may differ or the daemon may be unavailable.
                if matches!(self.page.location, Location::Document(_))
                    && self.page.source_text.is_none()
                    && self.page.error.is_none()
                {
                    self.page.error = Some("Historical source evicted; Refresh loads the current version".into());
                }
                (false, true)
            }
            None => (true, false),
        }
    }
    pub fn lines(&self, width: usize, height: usize) -> Vec<String> {
        if height == 0 {
            return Vec::new();
        }
        if let Some((address, position)) = &self.address {
            return self.address_lines(address, *position, width, height);
        }
        let identity = self
            .page
            .identity
            .as_ref()
            .map(|(v, hash)| format!(" · v{v} {}", hash.get(..12).unwrap_or(hash)))
            .unwrap_or_default();
        let mut lines = vec![mark_clip(
            &format!(
                "Documents remuda:///{}{identity}",
                path_label(self.page.location.path())
            ),
            width,
        )];
        let room = body_room(height);
        match self.page.location {
            Location::Directory(_) => {
                if self.page.entries.is_empty() && !self.loading {
                    lines.push("(empty)".into());
                }
                let start = self.page.position.saturating_sub(room.saturating_sub(1));
                lines.extend(
                    self.page
                        .entries
                        .iter()
                        .enumerate()
                        .skip(start)
                        .take(room)
                        .map(|(i, entry)| {
                            mark_clip(
                                &format!(
                                    "{} {}",
                                    if i == self.page.position { ">" } else { " " },
                                    entry.display_label()
                                ),
                                width,
                            )
                        }),
                );
            }
            Location::Document(_) => {
                let wrapped = wrap_text(&self.page.text, width.max(1));
                self.rendered_count.set(wrapped.len());
                lines.extend(
                    wrapped
                        .into_iter()
                        .skip(
                            self.page
                                .position
                                .min(self.rendered_count.get().saturating_sub(1)),
                        )
                        .take(room),
                );
            }
        }
        lines.resize(height.saturating_sub(2).max(1), String::new());
        lines.push(mark_clip(&self.status(), width));
        lines.push(
            "a full address · ↑↓ scroll · Enter open · Esc back · r retry · q sessions".into(),
        );
        lines
            .into_iter()
            .take(height)
            .map(|line| clip(&line, width))
            .collect()
    }
    /// Inspect the final document frame, excluding its header/status/help rows.
    /// Address inspection and clipped/blank body rows are not document evidence.
    pub fn body_in_frame(&self, frame: &str) -> bool {
        if self.address.is_some() || !matches!(self.page.location, Location::Document(_)) {
            return false;
        }
        let rows: Vec<_> = frame.split('\n').collect();
        rows.iter()
            .skip(1)
            .take(body_room(rows.len()))
            .any(|row| unicode_width::UnicodeWidthStr::width(row.trim()) > 0)
    }

    fn status(&self) -> String {
        if self.loading {
            return "Loading…".into();
        }
        if let Some(error) = &self.page.error {
            return error.clone();
        }
        if let Some(link) = self.selected_link() {
            let target = match &link.target {
                crate::document_html::Target::Vfs(path) => {
                    format!("remuda:///{}", path_label(path))
                }
                crate::document_html::Target::Blocked(reason) => format!("blocked: {reason}"),
            };
            return format!(
                "[{}] {} → {target}",
                self.page.selected_link.unwrap_or(0) + 1,
                link.label
            );
        }
        String::new()
    }
    pub fn inspect_address(&mut self) {
        if self.address.take().is_some() {
            return;
        }
        let path = if let Some(link) = self.selected_link() {
            match &link.target {
                crate::document_html::Target::Vfs(path) => path.as_str(),
                crate::document_html::Target::Blocked(reason) => {
                    self.page.error = Some(reason.clone());
                    return;
                }
            }
        } else {
            self.page
                .entries
                .get(self.page.position)
                .filter(|_| matches!(self.page.location, Location::Directory(_)))
                .map_or(self.page.location.path(), |entry| entry.path.as_str())
        };
        // ASCII escapes make every code point inspectable even in a one-column
        // terminal, without changing the address used for navigation.
        let escaped: String = path.chars().flat_map(char::escape_default).collect();
        // Freeze the inspected selection as well as its displayed URI. A late
        // refresh must not make Enter act on another item after inspection.
        let paused = self.loading || self.pending;
        self.cancel();
        if paused {
            self.page.error = Some("Refresh paused for address inspection; r to refresh".into());
        }
        self.address = Some((format!("remuda:///{escaped}"), 0));
    }

    fn address_lines(
        &self,
        address: &str,
        position: usize,
        width: usize,
        height: usize,
    ) -> Vec<String> {
        let rows: Vec<_> = address
            .as_bytes()
            .chunks(width.max(1))
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect();
        self.rendered_count.set(rows.len());
        let position = position.min(rows.len().saturating_sub(1));
        let mut lines = Vec::new();
        if height >= 3 {
            lines.push(clip(
                &format!("VFS address (escaped) · {}/{}", position + 1, rows.len()),
                width,
            ));
        }
        let room = if height >= 3 { height - 2 } else { 1 };
        lines.extend(
            rows.into_iter()
                .skip(position)
                .take(room)
                .map(|line| clip(&line, width)),
        );
        lines.resize(height.saturating_sub(1).max(1), String::new());
        if height > 1 {
            lines.push(clip("↑↓ scroll · a/Esc back", width));
        }
        lines
    }
    pub fn poll(&mut self) {
        let _ = self.poll_changed();
    }

    /// Poll and report whether the visible page changed. Callers that repaint
    /// on a timer use this to skip a rebuild when nothing arrived.
    pub fn poll_changed(&mut self) -> bool {
        let mut changed = false;
        if let Some((generation, receiver)) = &self.flight {
            let reply = match receiver.try_recv() {
                Ok(reply) => Some(reply),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Document worker stopped".into()))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(reply) = reply {
                if *generation == self.generation {
                    self.loading = false;
                    match reply {
                        Ok(page) => {
                            self.page = page;
                            self.inject_parent_row();
                        }
                        Err(error) => self.page.error = Some(error),
                    }
                    changed = true;
                }
                self.flight = None;
            }
        }
        if self.flight.is_none() && std::mem::take(&mut self.pending) {
            let socket = self.socket.clone();
            let location = self.page.location.clone();
            let (sender, receiver) = mpsc::sync_channel(1);
            match std::thread::Builder::new()
                .name("vfs-document".into())
                .spawn(move || {
                    let _ = sender.send(fetch(&socket, &location));
                }) {
                Ok(_) => self.flight = Some((self.generation, receiver)),
                Err(error) => {
                    self.loading = false;
                    self.page.error = Some(error.to_string());
                    changed = true;
                }
            }
        }
        changed
    }
}

/// The last component of a VFS path, escaped for display. Directory paths end
/// with `/`; a root or empty path has no component.
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

fn body_room(height: usize) -> usize {
    height.saturating_sub(3)
}

fn wrap_text(lines: &[String], width: usize) -> Vec<String> {
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

fn clip(text: &str, width: usize) -> String {
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

fn mark_clip(text: &str, width: usize) -> String {
    if unicode_width::UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", clip(text, width - 1))
}

fn fetch(socket: &Path, location: &Location) -> Reply {
    let uri = format!("remuda:///{}", location.path());
    let args = match location {
        Location::Directory(_) => vec!["ls".into(), uri, "--json".into()],
        Location::Document(path) => vec![
            "read".into(),
            uri,
            "--max-bytes".into(),
            if html_path(path) {
                crate::document_html::MAX_HTML_BYTES
            } else {
                MAX_BYTES
            }
            .to_string(),
        ],
    };
    let response = crate::client::request_with_deadline(
        socket,
        &Request::Vfs {
            args,
            input_base64: None,
        },
        Duration::from_secs(3),
    )
    .map_err(|error| error.to_string())?;
    let Response::CommandResult {
        exit_code,
        stdout_base64,
        stderr_base64,
    } = response
    else {
        return Err("Daemon does not support document reads".into());
    };
    if exit_code != 0 {
        return Err(
            String::from_utf8_lossy(&BASE64.decode(stderr_base64).unwrap_or_default()).into_owned(),
        );
    }
    if stdout_base64.len() > 4 * MAX_BYTES {
        return Err("Document response is too large".into());
    }
    let bytes = BASE64
        .decode(stdout_base64)
        .map_err(|_| "Invalid document response")?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid document JSON")?;
    decode(location, &value)
}

/// Read bounded binary media (an image) from the VFS. The caller decides the
/// format and pixel limits; this function only enforces the byte bound and the
/// same identity/size checks as a text read.
pub fn read_media(socket: &Path, path: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
    vfs::validate_path(path).map_err(|_| "Invalid VFS address")?;
    let uri = format!("remuda:///{path}");
    let response = crate::client::request_with_deadline(
        socket,
        &Request::Vfs {
            args: vec![
                "read".into(),
                uri,
                "--max-bytes".into(),
                max_bytes.to_string(),
            ],
            input_base64: None,
        },
        Duration::from_secs(3),
    )
    .map_err(|error| error.to_string())?;
    let Response::CommandResult {
        exit_code,
        stdout_base64,
        stderr_base64,
    } = response
    else {
        return Err("Daemon does not support document reads".into());
    };
    if exit_code != 0 {
        return Err(
            String::from_utf8_lossy(&BASE64.decode(stderr_base64).unwrap_or_default()).into_owned(),
        );
    }
    // The VFS reply is JSON, its `content` is base64 of the binary, and the
    // whole JSON is base64 again on stdout. Size the outer bound for both
    // encodings plus metadata instead of comparing it to the binary limit.
    let inner_base64 = max_bytes.div_ceil(3).saturating_mul(4);
    let json = inner_base64.saturating_add(64 * 1024);
    let outer_base64 = json.div_ceil(3).saturating_mul(4).saturating_add(64 * 1024);
    if stdout_base64.len() > outer_base64 {
        return Err("Media response is too large".into());
    }
    let bytes = BASE64
        .decode(stdout_base64)
        .map_err(|_| "Invalid media response")?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid media JSON")?;
    if value["ok"].as_bool() != Some(true) {
        return Err(format!(
            "{}: {}",
            value["code"].as_str().unwrap_or("io"),
            value["detail"].as_str().unwrap_or("Read failed")
        ));
    }
    if value["path"].as_str() != Some(path) {
        return Err("Media identity changed".into());
    }
    let hash = value["sha256"].as_str().ok_or("Missing media hash")?;
    vfs::validate_sha256(hash).map_err(|_| "Invalid media hash")?;
    let content = value["content"].as_str().ok_or("Missing media content")?;
    let decoded = BASE64
        .decode(content)
        .map_err(|_| "Invalid media encoding")?;
    if decoded.len() > max_bytes || value["size"].as_u64() != Some(decoded.len() as u64) {
        return Err("Invalid media size".into());
    }
    Ok(decoded)
}

fn decode(location: &Location, value: &Value) -> Reply {
    let mut page = Page::empty(location.clone());
    match location {
        Location::Directory(prefix) => {
            let rows = value.as_array().ok_or("Invalid directory listing")?;
            if rows.len() > 10_000 {
                return Err("Directory listing is too large".into());
            }
            for row in rows {
                let path = row["path"].as_str().ok_or("Missing VFS path")?;
                let directory = match row["kind"].as_str() {
                    Some("dir") => true,
                    Some("file") => false,
                    _ => return Err("Invalid VFS entry kind".into()),
                };
                vfs::validate_path(path.trim_end_matches('/')).map_err(|_| "Invalid VFS path")?;
                let child = path
                    .strip_prefix(prefix)
                    .ok_or("Entry outside directory")?
                    .trim_end_matches('/');
                if child.is_empty() || child.contains('/') || directory != path.ends_with('/') {
                    return Err("Entry is not an immediate child".into());
                }
                page.entries.push(Entry {
                    path: path.to_owned(),
                    directory, label: None,
                });
            }
        }
        Location::Document(path) => {
            if value["ok"].as_bool() != Some(true) {
                return Err(format!(
                    "{}: {}",
                    value["code"].as_str().unwrap_or("io"),
                    value["detail"].as_str().unwrap_or("Read failed")
                ));
            }
            if value["path"].as_str() != Some(path) {
                return Err("Document identity changed".into());
            }
            let hash = value["sha256"].as_str().ok_or("Missing document hash")?;
            vfs::validate_sha256(hash).map_err(|_| "Invalid document hash")?;
            let version = value["version"]
                .as_u64()
                .ok_or("Missing document version")?;
            let content = value["content"]
                .as_str()
                .ok_or("Missing document content")?;
            if content.len() > MAX_BYTES.div_ceil(3) * 4 {
                return Err("Document exceeds 1 MiB".into());
            }
            let bytes = BASE64
                .decode(content)
                .map_err(|_| "Invalid document encoding")?;
            if bytes.len() > MAX_BYTES || value["size"].as_u64() != Some(bytes.len() as u64) {
                return Err("Invalid document size".into());
            }
            let text = String::from_utf8(bytes).map_err(|_| "Document is not UTF-8 text")?;
            if text.contains('\0') {
                return Err("Document is binary".into());
            }
            page.source_text = Some(text.as_str().into());
            if html_path(path) {
                let html = crate::document_html::parse(path, &text)?;
                page.text = html.lines.clone();
                page.projection = Some(html);
            } else if markdown_path(path) {
                let markdown = crate::document_markdown::parse(path, &text)?;
                page.text = markdown.lines.clone();
                page.projection = Some(markdown);
                page.markdown_source = page.source_text.clone();
            } else {
                page.text = text.lines().take(MAX_LINES).map(str::to_owned).collect();
                if text.lines().count() > MAX_LINES {
                    page.text.push("[… line limit reached]".into());
                }
            }
            page.identity = Some((version, hash.to_owned()));
        }
    }
    Ok(page)
}

fn html_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".html") || path.ends_with(".htm")
}

fn markdown_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".md") || path.ends_with(".markdown")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn basename_label_keeps_the_last_component_only() {
        assert_eq!(basename_label(""), "");
        assert_eq!(basename_label("docs/"), "docs");
        assert_eq!(basename_label("docs/readme.md"), "readme.md");
        assert_eq!(basename_label("readme.md"), "readme.md");
        assert_eq!(basename_label("docs/한글 폴더/노트.md"), "노트.md");
        assert_eq!(basename_label(r"docs/a\b.md"), r"a\\b.md");
        assert_eq!(basename_label("docs/a\u{202e}b.md"), r"a\u{202e}b.md");
    }
    #[test]
    fn entry_display_name_is_compact_but_keeps_the_full_path() {
        let entry = Entry {
            path: "docs/guide/note.md".into(),
            directory: false,
            label: None,
        };
        assert_eq!(entry.display_name(), "note.md");
        assert_eq!(entry.display_label(), "docs/guide/note.md");
        assert_eq!(entry.path, "docs/guide/note.md");
        let parent = Entry {
            path: "docs/".into(),
            directory: true,
            label: Some("..".into()),
        };
        assert_eq!(parent.display_name(), "..");
        assert_eq!(parent.display_label(), "..");
    }
    #[test]
    fn parent_directory_stops_at_the_root() {
        assert_eq!(parent_directory(""), None);
        assert_eq!(parent_directory("/"), None);
        assert_eq!(parent_directory("docs"), Some(String::new()));
        assert_eq!(parent_directory("docs/"), Some(String::new()));
        assert_eq!(parent_directory("docs/guide/"), Some("docs/".into()));
        assert_eq!(parent_directory("a/b/c/"), Some("a/b/".into()));
    }
    fn html_page(path: &str, text: &str) -> Page {
        decode(
            &Location::Document(path.into()),
            &json!({
                "ok":true,"path":path,"sha256":"a".repeat(64),"version":1,
                "size":text.len(),"content":BASE64.encode(text)
            }),
        )
        .unwrap()
    }

    #[test]
    fn small_markdown_history_restores_source_and_links_across_two_back_steps() {
        let (_send, receive) = mpsc::sync_channel(1);
        let original = "# Original\n[guide](guide.md) ![diagram](image.png)\n";
        let mut browser = Browser {
            page: html_page("a.md", original), loading: false,
            socket: PathBuf::from("/unavailable-test-only"), history: Vec::new(),
            generation: 1, pending: false, flight: Some((1, receive)),
            rendered_count: Cell::new(0), address: None, parent_navigation: false,
        };
        browser.navigate(Location::Document("b.md".into()));
        browser.page = html_page("b.md", "# B");
        browser.navigate(Location::Document("c.md".into()));
        browser.page = html_page("c.md", "# C");
        assert!(!browser.back());
        assert!(!browser.back());
        assert_eq!(browser.page.markdown_source.as_deref(), Some(original));
        assert_eq!(browser.page.source_text.as_deref(), Some(original));
        assert!(browser.page.projection.as_ref().unwrap().links.iter().any(|link| matches!(&link.target, crate::document_html::Target::Vfs(path) if path == "guide.md")));
        assert!(browser.page.error.is_none());
        assert!(!browser.loading && !browser.pending);
    }

    #[test]
    fn history_bounds_sources_and_never_silently_refetches_evicted_entries() {
        for stale_reply in [Ok(html_page("a.md", "# changed")), Err("daemon unavailable".into())] {
            let (send, receive) = mpsc::sync_channel(1);
            let mut browser = Browser {
                page: html_page("a.md", "# original"), loading: false,
                socket: PathBuf::from("/unavailable-test-only"), history: Vec::new(),
                generation: 1, pending: false, flight: Some((1, receive)),
                rendered_count: Cell::new(0), address: None, parent_navigation: false,
            };
            let raw: std::sync::Arc<str> = "a".repeat(MAX_BYTES).into();
            browser.page.source_text = Some(raw.clone());
            browser.page.markdown_source = Some(raw);
            browser.navigate(Location::Document("b.md".into()));
            assert!(browser.history[0].source_text.is_some());
            browser.page = html_page("b.md", "# second");
            browser.navigate(Location::Document("c.md".into()));
            browser.page = html_page("c.md", "# third");
            let raw: std::sync::Arc<str> = "c".repeat(MAX_BYTES).into();
            browser.page.source_text = Some(raw.clone());
            browser.page.markdown_source = Some(raw);
            browser.navigate(Location::Document("d.md".into()));
            assert_eq!(browser.history.iter().filter(|p| p.source_text.is_some()).count(), 2);
            assert!(browser.history[0].markdown_source.is_none());
            browser.page = html_page("d.md", "# fourth");
            assert!(!browser.back());
            assert_eq!(browser.page.location, Location::Document("c.md".into()));
            assert!(!browser.back());
            assert_eq!(browser.page.source_text.as_deref(), Some("# second"));
            assert!(!browser.back());
            assert_eq!(browser.page.location, Location::Document("a.md".into()));
            assert_eq!(browser.page.identity, Some((1, "a".repeat(64))));
            assert!(browser.page.source_text.is_none());
            assert!(browser.page.error.as_deref().unwrap().contains("Refresh loads the current version"));
            assert!(browser.page.text.join(" ").contains("original"));
            assert!(!browser.loading && !browser.pending);
            send.send(stale_reply).unwrap();
            browser.poll();
            assert_eq!(browser.page.identity, Some((1, "a".repeat(64))));
            assert!(browser.page.text.join(" ").contains("original"));
            browser.refresh(); // Only an explicit user refresh starts I/O.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while browser.loading && std::time::Instant::now() < deadline {
                browser.poll();
                std::thread::yield_now();
            }
            assert!(!browser.loading);
            assert!(browser.page.error.is_some());
            assert_eq!(browser.page.identity, Some((1, "a".repeat(64))));
            assert!(browser.page.text.join(" ").contains("original"));
            assert!(browser.page.source_text.is_none());
        }
    }

    #[test]
    fn sixteen_panes_retain_at_most_three_megabytes_of_raw_source_each() {
        const TEST_PANE_COUNT: usize = 16;
        const RETAINED_SOURCES_PER_PANE: usize = 3;
        let mut browsers = Vec::new();
        let mut senders = Vec::new();
        for _ in 0..TEST_PANE_COUNT {
            let (send, receive) = mpsc::sync_channel(1);
            let mut page = Page::empty(Location::Document("a.md".into()));
            let raw: std::sync::Arc<str> = "a".repeat(MAX_BYTES).into();
            page.source_text = Some(raw.clone());
            page.markdown_source = Some(raw);
            let mut browser = Browser {
                page, loading: false, socket: PathBuf::from("/unavailable-test-only"),
                history: Vec::new(), generation: 1, pending: false,
                flight: Some((1, receive)), rendered_count: Cell::new(0), address: None, parent_navigation: false,
            };
            browser.navigate(Location::Document("b.md".into()));
            browser.page.source_text = Some("b".repeat(MAX_BYTES).into());
            browser.navigate(Location::Document("c.md".into()));
            browser.page.source_text = Some("c".repeat(MAX_BYTES).into());
            browser.navigate(Location::Document("d.md".into()));
            browser.page.source_text = Some("d".repeat(MAX_BYTES).into());
            senders.push(send);
            browsers.push(browser);
        }
        let retained: usize = browsers.iter().map(|browser| {
            browser.page.source_text.as_ref().map_or(0, |s| s.len()) +
                browser.history.iter().map(|page| page.source_text.as_ref().map_or(0, |s| s.len())).sum::<usize>()
        }).sum();
        assert_eq!(retained, TEST_PANE_COUNT * RETAINED_SOURCES_PER_PANE * MAX_BYTES);
        assert!(browsers.iter().all(|browser| browser.history[0].markdown_source.is_none()));
        drop(senders);
    }

    #[test]
    fn presentation_requires_a_visible_body_glyph_in_the_final_frame() {
        let mut browser = Browser {
            page: html_page("docs/small.html", "<p>界</p>"),
            loading: false,
            socket: PathBuf::new(),
            history: Vec::new(),
            generation: 1,
            pending: false,
            flight: None,
            rendered_count: Cell::new(0),
            parent_navigation: false,
            address: None,
        };
        for (width, height, expected) in [
            (100, 0, false),
            (100, 3, false),
            (100, 4, true),
            (0, 4, false),
            (1, 4, true), // The existing renderer shows '?' for an over-wide glyph.
            (2, 4, true),
        ] {
            let frame = browser.lines(width, height).join("\n");
            assert_eq!(
                browser.body_in_frame(&frame),
                expected,
                "{width}×{height}: {frame:?}"
            );
        }
        browser.inspect_address();
        assert!(!browser.body_in_frame(&browser.lines(100, 4).join("\n")));
        browser.address = None;
        browser.page.text = vec![String::new()];
        assert!(!browser.body_in_frame(&browser.lines(100, 4).join("\n")));
    }

    #[test]
    fn html_links_inspection_and_back_share_the_same_target_and_history() {
        let (_send, receive) = mpsc::sync_channel(1);
        let mut browser = Browser {
            page: html_page(
                "docs/index.HTML",
                "<title>Guide</title><p>Intro</p><a href='sub/next.html'>Next</a>",
            ),
            loading: false,
            socket: PathBuf::new(),
            history: Vec::new(),
            generation: 1,
            pending: false,
            flight: Some((0, receive)),
            rendered_count: Cell::new(0),
            parent_navigation: false,
            address: None,
        };
        browser.page.position = 2;
        browser.select_link(true);
        assert!(browser.status().contains("remuda:///docs/sub/next.html"));
        browser.inspect_address();
        assert_eq!(
            browser.address.as_ref().unwrap().0,
            "remuda:///docs/sub/next.html"
        );
        browser.open_selected();
        assert_eq!(
            browser.page.location,
            Location::Document("docs/index.HTML".into())
        );
        assert!(!browser.back());
        browser.open_selected();
        assert_eq!(
            browser.page.location,
            Location::Document("docs/sub/next.html".into())
        );
        assert!(!browser.back());
        assert_eq!(browser.page.position, 2);
        assert_eq!(browser.page.selected_link, Some(0));
        assert_eq!(browser.page.projection.as_ref().unwrap().title, "Guide");
    }

    #[test]
    fn markdown_pages_read_as_prose_and_their_local_links_navigate() {
        let mut browser = Browser {
            page: html_page(
                "docs/guide.md",
                "# Guide\n\nRead **this** [next](sub/next.md).\n\n- one\n",
            ),
            loading: false,
            socket: PathBuf::new(),
            history: Vec::new(),
            generation: 1,
            pending: false,
            flight: None,
            rendered_count: Cell::new(0),
            parent_navigation: false,
            address: None,
        };
        let text = browser.page.text.join("\n");
        assert!(!text.contains('#') && !text.contains("**"), "{text}");
        assert!(text.contains("Guide") && text.contains("Read this next [1]."));
        let frame = browser.lines(80, 12).join("\n");
        assert!(frame.contains("Read this next [1]."), "{frame}");
        assert!(browser.body_in_frame(&frame));
        browser.select_link(true);
        assert!(browser.status().contains("remuda:///docs/sub/next.md"));
        browser.open_selected();
        assert_eq!(
            browser.page.location,
            Location::Document("docs/sub/next.md".into())
        );
        assert!(!browser.back());
        assert_eq!(
            browser.page.location,
            Location::Document("docs/guide.md".into())
        );
        assert_eq!(browser.page.selected_link, Some(0));
    }

    #[test]
    fn unsupported_html_links_are_visible_but_do_not_navigate_or_fetch() {
        let mut browser = Browser {
            page: html_page(
                "docs/index.html",
                "<a href='#part'>Fragment</a><a href='https://example.com'>External</a>",
            ),
            loading: false,
            socket: PathBuf::new(),
            history: Vec::new(),
            generation: 1,
            pending: false,
            flight: None,
            rendered_count: Cell::new(0),
            parent_navigation: false,
            address: None,
        };
        browser.select_link(false);
        assert_eq!(browser.page.selected_link, Some(1));
        browser.select_link(true);
        assert_eq!(browser.page.selected_link, Some(0));
        assert!(browser.status().contains("blocked: Fragment"));
        browser.open_selected();
        browser.inspect_address();
        assert!(browser.page.error.as_ref().unwrap().contains("Fragment"));
        assert!(browser.address.is_none() && browser.flight.is_none() && !browser.pending);
        assert!(browser.history.is_empty());
        assert_eq!(browser.generation, 1);
        assert_eq!(
            browser.page.location,
            Location::Document("docs/index.html".into())
        );
    }
    #[test]
    fn html_refresh_cannot_replace_the_link_that_was_inspected() {
        let (send, receive) = mpsc::sync_channel(1);
        let mut page = html_page("docs/index.html", "<a href='A.html'>A</a>");
        page.selected_link = Some(0);
        let mut browser = Browser {
            page,
            loading: true,
            socket: PathBuf::new(),
            history: Vec::new(),
            generation: 1,
            pending: false,
            flight: Some((1, receive)),
            rendered_count: Cell::new(0),
            parent_navigation: false,
            address: None,
        };
        browser.inspect_address();
        assert_eq!(browser.address.as_ref().unwrap().0, "remuda:///docs/A.html");
        send.send(Ok(html_page("docs/index.html", "<a href='B.html'>B</a>")))
            .unwrap();
        browser.poll();
        assert!(!browser.back());
        browser.open_selected();
        assert_eq!(
            browser.page.location,
            Location::Document("docs/A.html".into())
        );
    }
    #[test]
    fn inspection_fences_refresh_reordering_or_removal_before_enter() {
        for (reply_before_close, queued) in
            [(true, false), (false, false), (true, true), (false, true)]
        {
            for new_paths in [vec!["docs/B", "docs/A"], vec!["docs/B"]] {
                let (send, receive) = mpsc::sync_channel(1);
                let mut page = Page::empty(Location::Directory("docs/".into()));
                page.entries = ["docs/A", "docs/B"]
                    .into_iter()
                    .map(|path| Entry {
                        path: path.into(),
                        directory: false,

                        label: None,
                    })
                    .collect();
                let mut browser = Browser {
                    page,
                    loading: true,
                    socket: PathBuf::new(),
                    history: Vec::new(),
                    generation: if queued { 2 } else { 1 },
                    pending: queued,
                    flight: Some((1, receive)),
                    rendered_count: Cell::new(0),
                    parent_navigation: false,
                    address: None,
                };
                browser.inspect_address();
                assert_eq!(browser.address.as_ref().unwrap().0, "remuda:///docs/A");
                let mut replacement = Page::empty(Location::Directory("docs/".into()));
                replacement.entries = new_paths
                    .into_iter()
                    .map(|path| Entry {
                        path: path.into(),
                        directory: false,

                        label: None,
                    })
                    .collect();
                if !reply_before_close {
                    assert!(!browser.back());
                }
                send.send(Ok(replacement)).unwrap();
                browser.poll();
                if reply_before_close {
                    assert!(!browser.back());
                }
                assert_eq!(browser.page.entries[browser.page.position].path, "docs/A");
                assert!(!browser.loading);
                assert!(browser
                    .page
                    .error
                    .as_ref()
                    .unwrap()
                    .contains("Refresh paused"));
                assert!(browser.flight.is_none() && !browser.pending);
                browser.open_selected();
                assert_eq!(browser.page.location, Location::Document("docs/A".into()));
            }
        }
    }
    #[test]
    fn narrow_address_inspection_reveals_the_full_selected_identity() {
        let prefix = "docs/".to_owned() + &"long-name-".repeat(20);
        let paths = [format!("{prefix}A"), format!("{prefix}界\u{202e}B")];
        let mut browser = Browser {
            page: Page::empty(Location::Directory("docs/".into())),
            loading: false,
            socket: PathBuf::new(),
            history: Vec::new(),
            generation: 0,
            pending: false,
            flight: None,
            rendered_count: Cell::new(0),
            parent_navigation: false,
            address: None,
        };
        browser.page.entries = paths
            .iter()
            .map(|path| Entry {
                path: path.clone(),
                directory: false,

                label: None,
            })
            .collect();
        let listing = browser.lines(12, 8);
        assert!(listing[1].ends_with('…') && listing[2].ends_with('…'));
        for (selected, path) in paths.iter().enumerate() {
            browser.page.position = selected;
            let expected = format!(
                "remuda:///{}",
                path.chars()
                    .flat_map(char::escape_default)
                    .collect::<String>()
            );
            for width in [1, 2, 8, 20] {
                for height in [1, 2, 3] {
                    browser.inspect_address();
                    let mut reconstructed = String::new();
                    for _ in 0..expected.len().div_ceil(width) {
                        let rows = browser.lines(width, height);
                        reconstructed.push_str(&rows[usize::from(height >= 3)]);
                        browser.move_selection(true);
                    }
                    assert_eq!(reconstructed, expected, "width={width}, height={height}");
                    browser.open_selected();
                    browser.refresh();
                    assert!(!browser.back());
                    assert_eq!(browser.page.position, selected);
                    assert_eq!(browser.page.entries[selected].path, *path);
                    assert!(
                        browser.flight.is_none() && !browser.pending && browser.history.is_empty()
                    );
                }
            }
        }
        browser.page = Page::empty(Location::Document(paths[1].clone()));
        browser.inspect_address();
        assert!(browser
            .address
            .as_ref()
            .unwrap()
            .0
            .ends_with(r"\u{754c}\u{202e}B"));
        assert!(browser.lines(0, 0).is_empty());
        assert!(browser.lines(0, 3).iter().all(String::is_empty));
    }
    #[test]
    fn retries_coalesce_behind_one_flight_and_recover_after_it_settles() {
        const RETRY_STRESS_ITERATIONS: usize = 32;
        let (send, receive) = mpsc::sync_channel(1);
        let mut browser = Browser {
            page: Page::empty(Location::Directory(String::new())),
            loading: true,
            socket: PathBuf::from("/missing-document-test-socket"),
            history: Vec::new(),
            generation: 1,
            pending: false,
            flight: Some((1, receive)),
            rendered_count: Cell::new(0),
            parent_navigation: false,
            address: None,
        };
        for _ in 0..RETRY_STRESS_ITERATIONS {
            browser.refresh();
            assert_eq!(browser.flight.as_ref().unwrap().0, 1);
            assert!(browser.pending);
            browser.cancel();
            browser.poll();
            assert_eq!(browser.flight.as_ref().unwrap().0, 1);
        }
        browser.refresh();
        let generation = browser.generation;
        send.send(Err("obsolete timeout".into())).unwrap();
        browser.poll();
        assert_eq!(browser.flight.as_ref().unwrap().0, generation);
        assert!(!browser.pending);
        assert!(browser.page.error.is_none());
        let limit = std::time::Instant::now() + Duration::from_secs(1);
        while browser.flight.is_some() && std::time::Instant::now() < limit {
            browser.poll();
            std::thread::yield_now();
        }
        assert!(browser.flight.is_none());
        assert!(!browser.loading);
        assert!(browser.page.error.is_some());
    }
    #[test]
    fn path_labels_escape_controls_without_changing_navigation_identity() {
        let raw = "docs/a\u{202e}b";
        let page = decode(
            &Location::Directory("docs/".into()),
            &json!([
                {"kind":"file","path":"docs/ab"},
                {"kind":"file","path":raw},
                {"kind":"dir","path":"docs/a\u{2066}b/"}
            ]),
        )
        .unwrap();
        assert!(page.entries.iter().any(|entry| entry.path == raw));
        let labels: Vec<_> = page.entries.iter().map(|e| path_label(&e.path)).collect();
        assert!(labels.contains(&"docs/ab".to_owned()));
        assert!(labels.contains(&r"docs/a\u{202e}b".to_owned()));
        assert!(labels.contains(&r"docs/a\u{2066}b/".to_owned()));
        assert_eq!(path_label("a\nb\x1b\u{009b}\\"), r"a\nb\u{1b}\u{9b}\\");
        assert_ne!(path_label(raw), path_label(r"docs/a\u{202e}b"));
        assert_eq!(clip(raw, 100), "docs/ab");
    }
    #[test]
    fn back_fences_a_late_reply_and_restores_selection() {
        let root = Page::empty(Location::Directory(String::new()));
        let (send, receive) = mpsc::sync_channel(1);
        let mut browser = Browser {
            page: Page::empty(Location::Document("x".into())),
            loading: true,
            socket: PathBuf::new(),
            history: vec![root],
            generation: 1,
            pending: false,
            flight: Some((1, receive)),
            rendered_count: Cell::new(0),
            parent_navigation: false,
            address: None,
        };
        assert!(!browser.back());
        send.send(Ok(Page::empty(Location::Document("x".into()))))
            .unwrap();
        browser.poll();
        assert_eq!(browser.page.location, Location::Directory(String::new()));
        assert!(!browser.loading);
        assert!(browser.back());
    }
    #[test]
    fn read_refusal_binary_and_large_content_are_errors() {
        let location = Location::Document("x".into());
        assert!(decode(&location, &json!({"ok":false,"code":"too_large"}))
            .unwrap_err()
            .contains("too_large"));
        for bytes in [vec![0], vec![255], vec![b'x'; MAX_BYTES + 1]] {
            let value = json!({"ok":true,"path":"x","sha256":"a".repeat(64),"version":1,"size":bytes.len(),"content":BASE64.encode(bytes)});
            assert!(decode(&location, &value).is_err());
        }
    }
    #[test]
    fn markdown_pages_carry_their_source_for_a_styled_reader() {
        let page = html_page("docs/guide.md", "# Title\n\nUse `code`.\n");
        assert_eq!(
            page.markdown_source.as_deref(),
            Some("# Title\n\nUse `code`.\n")
        );
        assert_eq!(page.projection.as_ref().unwrap().title, "Title");
        let plain = html_page("docs/notes.txt", "plain text");
        assert!(plain.markdown_source.is_none());
    }

    #[test]
    fn listing_and_text_display_cannot_escape_their_boundaries() {
        let location = Location::Directory("docs/".into());
        assert!(decode(&location, &json!([{"kind":"file","path":"docs/sub/x"}])).is_err());
        assert!(decode(&location, &json!([{"kind":"file","path":"other/x"}])).is_err());
        assert_eq!(clip("ab\x1b[2J界z", 6), "ab[2J");
        assert_eq!(clip("界界z", 4), "界界");
        assert_eq!(wrap_text(&["abcdefgh".into()], 3), ["abc", "def", "gh"]);
    }
}
