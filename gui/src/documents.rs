//! GUI document pane: a bounded, inert view over the shared VFS document model.
//!
//! The projection, link resolution and 1 MiB read bound live in
//! `remuda_native::documents`; this adapter only decides what the GUI shows and
//! keeps every dimension the pane can render bounded.

use crate::{markdown, media};
use remuda_native::document_html::Target;
use remuda_native::documents::{Browser, Location};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{mpsc, Arc},
};

/// Widest frame the pane will request; the browser clips and wraps to it.
pub const MAX_COLUMNS: usize = 120;
/// Most body rows the pane will request.
pub const MAX_ROWS: usize = 48;
/// A GUI caller must bound a source before handing it to a parser; the daemon
/// read request is capped at this many bytes by `documents::MAX_BYTES`.
pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
/// Most image entries one pane will keep; the workspace budget is the real cap.
pub const MAX_IMAGES_PER_PANE: usize = 8;
/// Decoded image memory shared by every pane in a workspace.
pub const MAX_WORKSPACE_IMAGE_BYTES: usize = 128 * 1024 * 1024;
/// Image entries (loaded or failed) shared by every pane in a workspace.
pub const MAX_WORKSPACE_IMAGE_ENTRIES: usize = 32;
/// Why an image request was refused, kept separate so the GUI can name the
/// actual limit and the action that releases it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageLimit {
    PaneEntries,
    WorkspaceEntries,
    WorkspaceBytes,
}

impl ImageLimit {
    pub fn message(self) -> String {
        match self {
            Self::PaneEntries => format!(
                "this tab already holds {MAX_IMAGES_PER_PANE} images; open a document with fewer images"
            ),
            Self::WorkspaceEntries => format!(
                "workspace image cache is full ({MAX_WORKSPACE_IMAGE_ENTRIES} entries); close another image tab or refresh this document"
            ),
            Self::WorkspaceBytes => format!(
                "workspace decoded-image memory is full ({} MiB); close another image tab or refresh this document",
                MAX_WORKSPACE_IMAGE_BYTES / (1024 * 1024)
            ),
        }
    }
}
/// Image reads/decode in flight per pane; the budget serializes actual decodes.
const MAX_CONCURRENT_IMAGE_LOADS: usize = 2;

#[derive(Debug, Default)]
struct BudgetState {
    bytes: usize,
    entries: usize,
}

/// A workspace-wide admission budget for decoded images. Entries are reserved
/// before a read starts, so a page with many failed paths cannot grow the map
/// without bound. Decodes are serialized by `decode` to keep the transient
/// RGBA/BGRA peak to one image across all tabs.
#[derive(Debug)]
pub struct ImageBudget {
    max_bytes: usize,
    max_entries: usize,
    state: std::sync::Mutex<BudgetState>,
    decode: std::sync::Mutex<()>,
}

impl ImageBudget {
    pub fn new() -> Self {
        Self::with_limits(MAX_WORKSPACE_IMAGE_BYTES, MAX_WORKSPACE_IMAGE_ENTRIES)
    }

    pub fn with_limits(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            max_bytes,
            max_entries,
            state: std::sync::Mutex::new(BudgetState::default()),
            decode: std::sync::Mutex::new(()),
        }
    }

    fn reserve_entry(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.entries >= self.max_entries {
            return false;
        }
        state.entries += 1;
        true
    }

    fn reserve_bytes(&self, bytes: usize) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.bytes.saturating_add(bytes) > self.max_bytes {
            return false;
        }
        state.bytes = state.bytes.saturating_add(bytes);
        true
    }

    fn release(&self, bytes: usize, entries: usize) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.bytes = state.bytes.saturating_sub(bytes);
        state.entries = state.entries.saturating_sub(entries);
    }

    fn decode_guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.decode
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// `(decoded bytes, entries)` currently reserved. Test/diagnostic evidence.
    pub fn usage(&self) -> (usize, usize) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        (state.bytes, state.entries)
    }
}

impl Default for ImageBudget {
    fn default() -> Self {
        Self::new()
    }
}

/// One bounded image load per path. A refresh releases the entry and its bytes;
/// an in-flight result from before the refresh is ignored by revision.
#[derive(Clone, Debug)]
pub enum ImageState {
    Loading,
    Ready {
        image: Arc<gpui::RenderImage>,
        width: u32,
        height: u32,
    },
    Failed(String),
}

type DecodedResult = Result<(media::Decoded, ImageReservation), String>;
type ImageFlight = (u64, String, mpsc::Receiver<DecodedResult>);

/// Holds decoded bytes against the workspace budget until the state is dropped.
struct ImageReservation {
    budget: Arc<ImageBudget>,
    bytes: usize,
}

impl Drop for ImageReservation {
    fn drop(&mut self) {
        self.budget.release(self.bytes, 0);
    }
}

pub struct Pane {
    selection_id: u64,
    selection_epoch: u64,
    directory_scroll: gpui::ScrollHandle,
    browser: Browser,
    socket: PathBuf,
    budget: Arc<ImageBudget>,
    key_events: usize,
    images: HashMap<String, ImageState>,
    reservations: HashMap<String, ImageReservation>,
    image_flights: Vec<ImageFlight>,
    image_queue: std::collections::VecDeque<String>,
    image_revision: u64,
    image_limit: Option<ImageLimit>,
    image_scan: Option<String>,
}

impl Pane {
    /// Start at the VFS root of `socket`. No document is read until selected.
    pub fn new(socket: PathBuf) -> Self {
        Self::new_with_budget(socket, Arc::new(ImageBudget::new()))
    }

    /// A pane that shares one workspace image budget.
    pub fn new_with_budget(socket: PathBuf, budget: Arc<ImageBudget>) -> Self {
        static NEXT_PANE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            selection_id: NEXT_PANE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            selection_epoch: 0,
            directory_scroll: gpui::ScrollHandle::new(),
            browser: Browser::new(socket.clone()),
            socket,
            budget,
            key_events: 0,
            images: HashMap::new(),
            reservations: HashMap::new(),
            image_flights: Vec::new(),
            image_queue: std::collections::VecDeque::new(),
            image_revision: 0,
            image_limit: None,
            image_scan: None,
        }
    }

    pub fn selection_id(&self) -> u64 { self.selection_id }
    pub fn selection_epoch(&self) -> u64 { self.selection_epoch }
    fn invalidate_selection(&mut self) { self.selection_epoch = self.selection_epoch.wrapping_add(1); }

    /// Record that one key event reached this pane (focus evidence for tests).
    pub fn note_key(&mut self) {
        self.key_events = self.key_events.saturating_add(1);
    }

    pub fn key_events(&self) -> usize {
        self.key_events
    }

    pub fn poll(&mut self) {
        let _ = self.poll_changed();
    }

    /// Poll document and image workers and report visible state changes.
    pub fn poll_changed(&mut self) -> bool {
        let document_changed = self.browser.poll_changed();
        if document_changed { self.invalidate_selection(); }
        let before = (
            self.images.len(),
            self.image_limited(),
            self.images.values().filter(|state| !matches!(state, ImageState::Loading)).count(),
        );
        self.poll_images();
        self.schedule_images();
        let after = (
            self.images.len(),
            self.image_limited(),
            self.images.values().filter(|state| !matches!(state, ImageState::Loading)).count(),
        );
        document_changed || before != after
    }

    /// Cancel this pane's document work and ignore old image results.
    pub fn cancel(&mut self) {
        self.browser.cancel();
        self.release_images();
    }

    fn poll_images(&mut self) {
        let mut results = Vec::new();
        for (index, (revision, path, receiver)) in self.image_flights.iter().enumerate() {
            match receiver.try_recv() {
                Ok(result) => results.push((index, *revision, path.clone(), result)),
                Err(mpsc::TryRecvError::Disconnected) => results.push((
                    index,
                    *revision,
                    path.clone(),
                    Err("image worker stopped".into()),
                )),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        for (index, revision, path, result) in results.into_iter().rev() {
            self.image_flights.remove(index);
            if revision != self.image_revision {
                continue;
            }
            let state = match result {
                Ok((decoded, reservation)) => match media::render_image(decoded) {
                    Some((image, width, height)) => {
                        self.reservations.insert(path.clone(), reservation);
                        ImageState::Ready {
                            image,
                            width,
                            height,
                        }
                    }
                    None => ImageState::Failed("image could not be uploaded".into()),
                },
                Err(error) => ImageState::Failed(error),
            };
            self.images.insert(path, state);
        }
        self.pump_images();
    }

    /// Queue images for the current location once. Direct image documents load
    /// themselves; Markdown loads every local VFS image it references.
    fn schedule_images(&mut self) {
        let location = self.location().to_owned();
        if self.image_scan.as_deref() == Some(location.as_str()) {
            return;
        }
        if !self.is_image() && self.markdown_source().is_none() && self.browser.loading {
            return;
        }
        self.image_scan = Some(location.clone());
        let mut targets = Vec::new();
        if self.is_image() {
            targets.push(location);
        } else if let Some(source) = self.markdown_source() {
            for block in markdown::parse(source) {
                for span in block.spans {
                    if !span.style.image {
                        continue;
                    }
                    let Some(destination) = span.target else {
                        continue;
                    };
                    if let Target::Vfs(path) =
                        remuda_native::document_html::resolve(self.location(), &destination)
                    {
                        if media::extension_is_image(&path) {
                            targets.push(path);
                        }
                    }
                }
            }
        }
        for path in targets {
            self.request_image(&path);
        }
    }

    fn request_image(&mut self, path: &str) {
        if self.images.contains_key(path) {
            return;
        }
        if self.images.len() >= MAX_IMAGES_PER_PANE {
            self.image_limit = Some(ImageLimit::PaneEntries);
            return;
        }
        if !self.budget.reserve_entry() {
            self.image_limit = Some(ImageLimit::WorkspaceEntries);
            return;
        }
        self.image_limit = None;
        self.images.insert(path.to_owned(), ImageState::Loading);
        self.image_queue.push_back(path.to_owned());
        self.pump_images();
    }

    fn pump_images(&mut self) {
        while self.image_flights.len() < MAX_CONCURRENT_IMAGE_LOADS {
            let Some(path) = self.image_queue.pop_front() else {
                return;
            };
            self.spawn_image(path);
        }
    }

    fn spawn_image(&mut self, path: String) {
        let socket = self.socket.clone();
        let worker_path = path.clone();
        let budget = self.budget.clone();
        let revision = self.image_revision;
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("vfs-image".into())
            .spawn(move || {
                let _guard = budget.decode_guard();
                let result = remuda_native::documents::read_media(
                    &socket,
                    &worker_path,
                    media::MAX_MEDIA_BYTES,
                )
                .and_then(|bytes| media::decode(&bytes));
                match result {
                    Ok(decoded) => {
                        let bytes = (decoded.width as usize)
                            .saturating_mul(decoded.height as usize)
                            .saturating_mul(4);
                        if !budget.reserve_bytes(bytes) {
                            let _ = sender.send(Err(ImageLimit::WorkspaceBytes.message()));
                        } else {
                            let reservation = ImageReservation {
                                budget: budget.clone(),
                                bytes,
                            };
                            let _ = sender.send(Ok((decoded, reservation)));
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                    }
                }
            });
        match worker {
            Ok(_) => self.image_flights.push((revision, path, receiver)),
            Err(error) => {
                self.images.insert(
                    path,
                    ImageState::Failed(format!("image worker unavailable: {error}")),
                );
            }
        }
    }

    /// Drop cached image state and ignore results from older reads. Called by an
    /// explicit refresh so a changed file cannot keep its old pixels.
    fn release_images(&mut self) {
        self.budget.release(0, self.images.len());
        self.images.clear();
        self.reservations.clear();
        self.image_queue.clear();
        self.image_limit = None;
        self.image_revision = self.image_revision.wrapping_add(1);
        self.image_scan = None;
    }

    /// The current state for one image path, if it was requested.
    pub fn image(&self, path: &str) -> Option<&ImageState> {
        self.images.get(path)
    }

    /// True when a request was refused by the pane/workspace image budget.
    pub fn image_limited(&self) -> bool {
        self.image_limit.is_some()
    }

    /// The exact refusal, for an actionable message and tests.
    pub fn image_limit(&self) -> Option<ImageLimit> {
        self.image_limit
    }

    /// Number of cached image entries; bounded by pane and workspace budgets.
    pub fn image_entry_count(&self) -> usize {
        self.images.len()
    }

    /// True when the location names a PNG/JPEG document.
    pub fn is_image(&self) -> bool {
        media::extension_is_image(self.location())
    }

    /// The exact rows the GUI renders: the same bounded projection the TUI uses.
    pub fn frame(&self, columns: usize, rows: usize) -> Vec<String> {
        self.browser
            .lines(columns.clamp(1, MAX_COLUMNS), rows.clamp(4, MAX_ROWS))
    }

    pub fn status(&self) -> String {
        if self.is_image() {
            return match self.image(self.location()) {
                Some(ImageState::Ready { width, height, .. }) => {
                    format!("IMAGE · {width}×{height}")
                }
                Some(ImageState::Failed(error)) => format!("IMAGE · {error}"),
                Some(ImageState::Loading) | None => "IMAGE · loading".into(),
            };
        }
        if self.browser.loading {
            return "LOADING · reading from the local daemon".into();
        }
        match &self.browser.page.error {
            Some(error) if error.contains("binary") || error.contains("not UTF-8 text") => {
                format!(
                    "UNSUPPORTED · {error} — this pane reads text, Markdown and HTML; \
                 image preview is not implemented"
                )
            }
            Some(error) => format!("ERROR · {error}"),
            None => format!("ready · {}", self.browser.page.location.path()),
        }
    }

    /// Snapshot of the current directory, guarded by its request generation.
    pub fn directory_entries(&self) -> Option<(u64, &str, &[remuda_native::documents::Entry], usize)> {
        match &self.browser.page.location {
            Location::Directory(path) if !self.browser.loading && self.browser.page.error.is_none() =>
                Some((self.browser.generation(), path, &self.browser.page.entries, self.browser.page.position)),
            _ => None,
        }
    }

    /// Ignore clicks from old layouts or changed directory listings.
    pub fn open_directory_entry(&mut self, generation: u64, directory: &str, index: usize, path: &str, is_directory: bool) -> bool {
        let Some((current, current_path, entries, _)) = self.directory_entries() else { return false; };
        if current != generation || current_path != directory || !entries.get(index).is_some_and(|entry| entry.path == path && entry.directory == is_directory) { return false; }
        self.browser.page.position = index;
        self.open_selected();
        true
    }

    pub fn directory_scroll(&self) -> gpui::ScrollHandle { self.directory_scroll.clone() }

    pub fn move_selection(&mut self, down: bool) {
        self.browser.move_selection(down);
        if self.directory_entries().is_some() { self.directory_scroll.scroll_to_item(self.browser.page.position); }
    }

    pub fn open_selected(&mut self) {
        self.invalidate_selection();
        self.release_images();
        self.browser.open_selected();
    }

    pub fn select_link(&mut self, next: bool) {
        self.browser.select_link(next);
    }

    pub fn back(&mut self) -> bool {
        let changed = self.browser.back();
        if changed {
            self.invalidate_selection();
            self.release_images();
        }
        changed
    }

    pub fn refresh(&mut self) {
        self.invalidate_selection();
        self.release_images();
        self.browser.refresh();
    }

    /// Open one local VFS document by address (`docs/guide.md`).
    pub fn open_document(&mut self, path: &str) {
        self.invalidate_selection();
        self.release_images();
        self.browser.navigate(Location::Document(path.to_owned()));
    }

    /// Follow a resolved VFS link. A trailing slash means directory.
    pub fn open_path(&mut self, path: &str) {
        if path.is_empty() || path.ends_with('/') {
            self.invalidate_selection();
            self.release_images();
            self.browser.navigate(Location::Directory(path.to_owned()));
        } else {
            self.open_document(path);
        }
    }

    pub fn location(&self) -> &str {
        self.browser.page.location.path()
    }

    /// The original Markdown source for a styled reader, when the current
    /// document has one. Bounded by the browser's 1 MiB read limit.
    pub fn markdown_source(&self) -> Option<&str> {
        self.browser.page.markdown_source.as_deref()
    }

    /// Original bounded document text; directories and images have no text.
    pub fn source_text(&self) -> Option<&str> {
        self.browser.page.source_text.as_deref()
    }

    /// Identity and freshness of the captured source, not a claim that it is current.
    pub fn source_provenance(&self) -> String {
        let identity = self.browser.page.identity.as_ref()
            .map(|(version, hash)| format!("v{version} sha256:{}", hash.get(..12).unwrap_or(hash)))
            .unwrap_or_else(|| "identity unavailable".into());
        let state = if self.browser.loading {
            "refresh pending"
        } else if self.browser.page.error.is_some() {
            "last refresh failed"
        } else {
            "last load succeeded"
        };
        format!("last loaded {identity} · {state}")
    }

    /// Full source hash for the snapshot's explicit identity-details action.
    pub fn source_identity(&self) -> Option<String> {
        self.browser.page.identity.as_ref().map(|(version, hash)| format!("Version {version} · SHA-256 {hash}"))
    }

    /// Whether this pane currently names a document rather than a directory.
    pub fn is_document(&self) -> bool {
        matches!(self.browser.page.location, Location::Document(_))
    }

    /// A tab label: the Markdown/HTML projection title when one exists,
    /// otherwise the VFS address; an empty address is the VFS root.
    pub fn title(&self) -> String {
        if let Some(projection) = &self.browser.page.projection {
            if !projection.title.trim().is_empty() {
                return projection.title.clone();
            }
        }
        let path = self.location();
        if path.is_empty() {
            "VFS root".into()
        } else {
            path.to_owned()
        }
    }

    /// The syntax family for the displayed file, if it has one. Markdown uses
    /// the existing inert projection, not a code highlighter.
    pub fn language(&self) -> Option<&'static str> {
        let path = std::path::Path::new(self.location());
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match extension.as_str() {
            "py" => "python",
            "rs" => "rust",
            "js" | "mjs" | "cjs" => "javascript",
            "ts" | "tsx" | "jsx" => "javascript",
            "sh" | "bash" | "zsh" => "shell",
            "json" => "json",
            "toml" => "toml",
            "yaml" | "yml" => "yaml",
            "md" | "markdown" => "markdown",
            "html" | "htm" => "markup",
            _ => return None,
        })
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.budget.release(0, self.images.len());
    }
}

/// Bound the number of concurrently open documents; each owns a browser thread.
pub const MAX_WORKSPACE_TABS: usize = 16;

/// One workspace owns independent [`Pane`]s: each tab has its own browser
/// history, location and selection, so switching tabs never resets navigation.
/// The active tab is always drawn; `split` may point at a second tab for a
/// side-by-side comparison.
pub struct Workspace {
    socket: PathBuf,
    budget: Arc<ImageBudget>,
    tabs: Vec<Pane>,
    active: usize,
    split: Option<usize>,
}

impl Workspace {
    pub fn new(socket: PathBuf) -> Self {
        Self::with_budget(socket, Arc::new(ImageBudget::new()))
    }

    /// A workspace whose tabs share one image budget. Tests use a smaller limit.
    pub fn with_budget(socket: PathBuf, budget: Arc<ImageBudget>) -> Self {
        Self {
            socket: socket.clone(),
            budget: budget.clone(),
            tabs: vec![Pane::new_with_budget(socket, budget)],
            active: 0,
            split: None,
        }
    }

    /// The shared image budget, for diagnostics and tests.
    pub fn image_budget(&self) -> Arc<ImageBudget> {
        self.budget.clone()
    }

    pub fn open_tab(&mut self) -> usize {
        if self.tabs.len() >= MAX_WORKSPACE_TABS {
            return self.active;
        }
        self.tabs.push(Pane::new_with_budget(
            self.socket.clone(),
            self.budget.clone(),
        ));
        self.active = self.tabs.len() - 1;
        self.active
    }

    pub fn open_in_new_tab(&mut self, path: &str) -> usize {
        if self.tabs.len() >= MAX_WORKSPACE_TABS {
            self.open_in_active(path);
            return self.active;
        }
        let mut pane = Pane::new_with_budget(self.socket.clone(), self.budget.clone());
        pane.open_document(path);
        self.tabs.push(pane);
        self.active = self.tabs.len() - 1;
        self.active
    }

    pub fn open_in_active(&mut self, path: &str) {
        self.tabs[self.active].open_document(path);
    }

    /// Close one tab. The workspace always keeps at least one empty tab.
    pub fn close(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() {
            return false;
        }
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.tabs.push(Pane::new_with_budget(
                self.socket.clone(),
                self.budget.clone(),
            ));
            self.active = 0;
        } else if self.active == index {
            self.active = index.min(self.tabs.len() - 1);
        } else if self.active > index {
            self.active -= 1;
        }
        self.split = match self.split {
            Some(value) if value == index => None,
            Some(value) if value > index => Some(value - 1),
            other => other,
        };
        if self.split == Some(self.active) {
            self.split = None;
        }
        true
    }

    pub fn activate(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() {
            return false;
        }
        self.active = index;
        if self.split == Some(index) {
            self.split = None;
        }
        true
    }

    /// Focus a pane by clicking it. Unlike [`Self::activate`], focusing the
    /// split pane keeps it beside the other one and swaps which side is active,
    /// so a side-by-side comparison survives a click.
    pub fn focus(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() {
            return false;
        }
        if self.active != index {
            let previous = self.active;
            self.active = index;
            if self.split == Some(index) {
                self.split = Some(previous);
            }
        }
        true
    }

    /// Split the active tab with another open tab. Returns false when there is
    /// no other tab or the index is the active one.
    pub fn split_with(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() || index == self.active {
            return false;
        }
        self.split = Some(index);
        true
    }

    pub fn unsplit(&mut self) {
        self.split = None;
    }

    pub fn toggle_split(&mut self) -> bool {
        if self.split.is_some() {
            self.unsplit();
            return false;
        }
        if self.tabs.len() < 2 {
            return false;
        }
        let other = (self.active + 1) % self.tabs.len();
        self.split_with(other)
    }

    pub fn poll(&mut self) {
        let _ = self.poll_changed();
    }

    /// Poll each tab even when an earlier tab has already changed.
    pub fn poll_changed(&mut self) -> bool {
        let mut changed = false;
        for pane in &mut self.tabs {
            changed |= pane.poll_changed();
        }
        changed
    }

    pub fn tabs(&self) -> &[Pane] {
        &self.tabs
    }

    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn split_index(&self) -> Option<usize> {
        self.split
    }

    pub fn pane(&self, index: usize) -> Option<&Pane> {
        self.tabs.get(index)
    }

    pub fn pane_mut(&mut self, index: usize) -> Option<&mut Pane> {
        self.tabs.get_mut(index)
    }

    pub fn active(&self) -> &Pane {
        &self.tabs[self.active]
    }

    pub fn active_mut(&mut self) -> &mut Pane {
        &mut self.tabs[self.active]
    }

    pub fn frame(&self, index: usize, columns: usize, rows: usize) -> Vec<String> {
        self.tabs
            .get(index)
            .map(|pane| pane.frame(columns, rows))
            .unwrap_or_default()
    }

    pub fn title(&self, index: usize) -> String {
        self.tabs
            .get(index)
            .map(Pane::title)
            .unwrap_or_else(|| "VFS root".into())
    }

    pub fn language(&self, index: usize) -> Option<&'static str> {
        self.tabs.get(index).and_then(Pane::language)
    }

    /// Key events received by any pane; a test uses this as focus evidence.
    pub fn key_events(&self) -> usize {
        self.tabs.iter().map(Pane::key_events).sum()
    }
}

#[cfg(test)]
mod polling_tests {
    use super::*;

    #[test]
    fn source_snapshot_provenance_names_version_and_refresh_state() {
        let mut pane = Pane::new(PathBuf::from("/nonexistent-vfs.sock"));
        pane.cancel();
        pane.browser.page.source_text = Some("loaded".into());
        pane.browser.page.identity = Some((7, "a".repeat(64)));
        assert!(pane.source_provenance().contains("last loaded v7 sha256:"));
        assert!(pane.source_provenance().contains("last load succeeded"));
        pane.browser.loading = true;
        assert!(pane.source_provenance().contains("refresh pending"));
        pane.browser.loading = false;
        pane.browser.page.error = Some("private detail".into());
        let failed = pane.source_provenance();
        assert!(failed.contains("last refresh failed"));
        assert!(failed.contains("v7"));
        assert!(!failed.contains("private detail"));
    }

    #[test]
    fn navigation_releases_cached_images_and_clears_the_limit() {
        let budget = Arc::new(ImageBudget::with_limits(4096, 2));
        let mut pane =
            Pane::new_with_budget(PathBuf::from("/nonexistent-vfs.sock"), budget.clone());
        assert!(budget.reserve_entry());
        pane.images.insert(
            "old.png".into(),
            ImageState::Failed("old failure".into()),
        );
        pane.image_limit = Some(ImageLimit::PaneEntries);
        pane.image_scan = Some("old.png".into());
        pane.open_document("new.png");
        assert_eq!(pane.image_entry_count(), 0);
        assert!(!pane.image_limited());
        assert_eq!(pane.image_limit(), None);
        assert_eq!(budget.usage(), (0, 0));
        pane.cancel();
    }

    #[test]
    fn image_limit_messages_name_the_limit_and_an_action() {
        let pane = ImageLimit::PaneEntries.message();
        assert!(pane.contains("8 images") && pane.contains("fewer images"), "{pane}");
        let entries = ImageLimit::WorkspaceEntries.message();
        assert!(entries.contains("32 entries") && entries.contains("close another image tab"), "{entries}");
        let bytes = ImageLimit::WorkspaceBytes.message();
        assert!(bytes.contains("128 MiB") && bytes.contains("close another image tab"), "{bytes}");
    }

    #[test]
    fn workspace_repaints_when_each_image_finishes_and_is_idle_afterwards() {
        let mut workspace = Workspace::new(PathBuf::from("/nonexistent-vfs.sock"));
        workspace.open_tab();
        for (index, pane) in workspace.tabs.iter_mut().enumerate() {
            pane.cancel();
            let path = format!("image-{index}.png");
            pane.images.insert(path.clone(), ImageState::Loading);
            let (sender, receiver) = mpsc::sync_channel(1);
            sender.send(Err("fixture error".into())).unwrap();
            pane.image_flights.push((pane.image_revision, path, receiver));
        }

        assert!(workspace.poll_changed(), "image completion must repaint");
        for (index, pane) in workspace.tabs.iter().enumerate() {
            let path = format!("image-{index}.png");
            assert!(matches!(pane.image(&path), Some(ImageState::Failed(error)) if error == "fixture error"));
        }
        assert!(!workspace.poll_changed(), "idle panes should not repaint");
    }
}
