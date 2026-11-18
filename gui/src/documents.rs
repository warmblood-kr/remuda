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
    stale_flights: usize,
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

    #[cfg(test)]
    fn reserve_bytes(&self, bytes: usize) -> bool {
        self.reserve_bytes_or_cause(bytes).is_ok()
    }

    /// Reserve decoded bytes or report the cause at refusal time. Both the
    /// stale count and the live bytes are read under one lock, so a poll that
    /// runs after the stale reservation drains cannot lose that evidence.
    fn reserve_bytes_or_cause(&self, bytes: usize) -> Result<(), RetryCause> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.bytes.saturating_add(bytes) <= self.max_bytes {
            state.bytes = state.bytes.saturating_add(bytes);
            return Ok(());
        }
        if state.stale_flights > 0 {
            Err(RetryCause::Stale)
        } else {
            Err(RetryCause::Bytes)
        }
    }

    /// Whether `bytes` would fit once the current stale reservations drain.
    fn could_reserve(&self, bytes: usize) -> bool {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.bytes.saturating_add(bytes) <= self.max_bytes
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

    fn mark_stale(&self, count: usize) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.stale_flights = state.stale_flights.saturating_add(count);
    }

    fn clear_stale(&self, count: usize) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.stale_flights = state.stale_flights.saturating_sub(count);
    }

    fn stale_flights(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stale_flights
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

/// Why an image could not reserve workspace memory. Captured with the
/// refusal so a later poll cannot misclassify a stale-caused race.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetryCause {
    /// A stale flight still held bytes when the reservation was refused.
    Stale,
    /// The live image set left no room; only a later release can help.
    Bytes,
}

enum ImageLoadResult {
    Ready(media::Decoded, ImageReservation),
    Retry { bytes: usize, cause: RetryCause },
    Failed(String),
}

type DecodedResult = ImageLoadResult;

/// Test-only observation point between dropping a stale result and opening the
/// stale gate. It proves the ordering without injecting a retry cause.
#[cfg(test)]
thread_local! {
    static STALE_DRAIN_PROBE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

struct ImageFlight {
    revision: u64,
    path: String,
    receiver: mpsc::Receiver<DecodedResult>,
    stale: bool,
}

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
    image_retries: std::collections::HashSet<String>,
}

impl Pane {
    /// Start at the VFS root of `socket`. No document is read until selected.
    pub fn new(socket: PathBuf) -> Self {
        Self::new_with_budget(socket, Arc::new(ImageBudget::new()))
    }

    /// A pane that shares one workspace image budget.
    pub fn new_with_budget(socket: PathBuf, budget: Arc<ImageBudget>) -> Self {
        static NEXT_PANE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut browser = Browser::new(socket.clone());
        browser.enable_parent_navigation();
        Self {
            selection_id: NEXT_PANE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            selection_epoch: 0,
            directory_scroll: gpui::ScrollHandle::new(),
            browser,
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
            image_retries: std::collections::HashSet::new(),
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
        for (index, flight) in self.image_flights.iter().enumerate() {
            match flight.receiver.try_recv() {
                Ok(result) => results.push((index, flight.revision, flight.path.clone(), result)),
                Err(mpsc::TryRecvError::Disconnected) => results.push((
                    index,
                    flight.revision,
                    flight.path.clone(),
                    ImageLoadResult::Failed("image worker stopped".into()),
                )),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        for (index, revision, path, result) in results.into_iter().rev() {
            let flight = self.image_flights.remove(index);
            if revision != self.image_revision {
                // Drop the stale result first: a Ready reservation releases
                // its bytes here. Only then open the stale gate, so another
                // worker cannot observe stale == 0 while these bytes are
                // still reserved and misclassify them as a live cap.
                drop(result);
                #[cfg(test)]
                STALE_DRAIN_PROBE.with(|probe| {
                    if let Some(probe) = probe.borrow_mut().take() {
                        probe();
                    }
                });
                if flight.stale {
                    self.budget.clear_stale(1);
                }
                continue;
            }
            if flight.stale {
                self.budget.clear_stale(1);
            }
            let state = match result {
                ImageLoadResult::Ready(decoded, reservation) => match media::render_image(decoded) {
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
                ImageLoadResult::Retry { bytes, cause } => {
                    // `bytes == 0` means the worker exited before decoding, so
                    // this is a scheduling retry, not a memory refusal. It
                    // must not consume the one transient memory retry.
                    if bytes == 0 {
                        self.image_queue.push_back(path.clone());
                        continue;
                    }
                    // A refusal caused by a stale reservation stays temporary
                    // even if that reservation drained before this poll. The
                    // pump gate keeps the requeue asleep until it is safe, so
                    // this is a state transition, not a busy loop.
                    if cause == RetryCause::Stale || self.budget.stale_flights() > 0 {
                        self.image_queue.push_back(path.clone());
                        continue;
                    }
                    // A live-bytes refusal retries once only if memory is free
                    // now. Otherwise this is a persistent cap and the exact
                    // 128 MiB failure is final for this document.
                    if self.image_retries.insert(path.clone()) && self.budget.could_reserve(bytes) {
                        self.image_queue.push_back(path.clone());
                        continue;
                    }
                    ImageState::Failed(ImageLimit::WorkspaceBytes.message())
                }
                ImageLoadResult::Failed(error) => ImageState::Failed(error),
            };
            if let ImageState::Ready { .. } = &state {
                self.image_retries.remove(&path);
            }
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
        // A stale worker may still reserve decoded bytes before its result is
        // polled. Do not start a new decode beside it: that can make the new
        // image fail on a budget the stale reservation is about to return.
        if self
            .image_flights
            .iter()
            .any(|flight| flight.revision != self.image_revision)
            || self.budget.stale_flights() > 0
        {
            return;
        }
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
                Self::run_image_worker(
                    budget,
                    || {
                        remuda_native::documents::read_media(
                            &socket,
                            &worker_path,
                            media::MAX_MEDIA_BYTES,
                        )
                        .and_then(|bytes| media::decode(&bytes))
                    },
                    sender,
                )
            });
        match worker {
            Ok(_) => self.image_flights.push(ImageFlight {
                revision,
                path,
                receiver,
                stale: false,
            }),
            Err(error) => {
                self.images.insert(
                    path,
                    ImageState::Failed(format!("image worker unavailable: {error}")),
                );
            }
        }
    }

    /// Run one image read/decode and decide whether its memory may be kept.
    /// The decode lock covers the read, the reservation decision and the
    /// result send, so a closed pane drops its buffered reservation before
    /// another worker can decode or reserve. The send is non-blocking (one
    /// result per one-slot channel); nothing waits for another tab's stale
    /// reservation while holding the lock.
    fn run_image_worker(
        budget: Arc<ImageBudget>,
        read: impl FnOnce() -> Result<media::Decoded, String>,
        sender: mpsc::SyncSender<ImageLoadResult>,
    ) {
        // A stale reservation has not been polled yet. Decode nothing: the
        // image will be requeued once that reservation drains, which also
        // keeps a released large image from spending decode time.
        if budget.stale_flights() > 0 {
            let _ = sender.send(ImageLoadResult::Retry { bytes: 0, cause: RetryCause::Stale });
            return;
        }
        // Keep the one-decoded-buffer peak and close the closed-pane race: the
        // read, the reservation decision, dropping a refused buffer and the
        // send all happen under the decode lock. The send cannot block (one
        // result fits the one-slot channel), so this still never waits for
        // another tab's stale reservation.
        {
            let _guard = budget.decode_guard();
            let result = match read() {
                Ok(decoded) => {
                    let bytes = (decoded.width as usize)
                        .saturating_mul(decoded.height as usize)
                        .saturating_mul(4);
                    match budget.reserve_bytes_or_cause(bytes) {
                        Ok(()) => {
                            let reservation = ImageReservation {
                                budget: budget.clone(),
                                bytes,
                            };
                            ImageLoadResult::Ready(decoded, reservation)
                        }
                        Err(cause) => {
                            // `decoded` is dropped here, still under the lock,
                            // so a refused buffer cannot overlap the next
                            // decode. The cause travels with the result.
                            ImageLoadResult::Retry { bytes, cause }
                        }
                    }
                }
                Err(error) => ImageLoadResult::Failed(error),
            };
            // If the pane closed and dropped the receiver, the failed send
            // drops a Ready reservation here, still under the decode lock.
            let _ = sender.send(result);
        }
    }

    /// Drop cached image state and ignore results from older reads. Called by an
    /// explicit refresh so a changed file cannot keep its old pixels.
    fn release_images(&mut self) {
        let mut newly_stale = 0;
        for flight in &mut self.image_flights {
            if flight.revision == self.image_revision && !flight.stale {
                flight.stale = true;
                newly_stale += 1;
            }
        }
        self.budget.mark_stale(newly_stale);
        self.budget.release(0, self.images.len());
        self.images.clear();
        self.reservations.clear();
        self.image_queue.clear();
        self.image_limit = None;
        self.image_retries.clear();
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
                    "UNSUPPORTED · {error} — this pane reads text, Markdown and HTML, \
                 plus PNG/JPEG within the {} MiB, {} px and {} pixel limits",
                    media::MAX_MEDIA_BYTES / (1024 * 1024),
                    media::MAX_DIMENSION,
                    media::MAX_PIXELS
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
        let generation = self.browser.generation();
        self.invalidate_selection();
        self.browser.open_selected();
        if self.browser.generation() != generation {
            self.release_images();
        }
    }

    pub fn select_link(&mut self, next: bool) {
        self.browser.select_link(next);
    }

    pub fn back(&mut self) -> bool {
        let (root, moved) = self.browser.back_with_move();
        if moved {
            self.invalidate_selection();
            self.release_images();
        }
        root
    }

    pub fn refresh(&mut self) {
        let generation = self.browser.generation();
        self.invalidate_selection();
        self.browser.refresh();
        if self.browser.generation() != generation {
            self.release_images();
        }
    }

    /// Open one local VFS document by address (`docs/guide.md`).
    pub fn open_document(&mut self, path: &str) {
        let generation = self.browser.generation();
        self.invalidate_selection();
        self.browser.navigate(Location::Document(path.to_owned()));
        if self.browser.generation() != generation {
            self.release_images();
        }
    }

    /// Follow a resolved VFS link. A trailing slash means directory.
    pub fn open_path(&mut self, path: &str) {
        if path.is_empty() || path.ends_with('/') {
            let generation = self.browser.generation();
            self.invalidate_selection();
            self.browser.navigate(Location::Directory(path.to_owned()));
            if self.browser.generation() != generation {
                self.release_images();
            }
        } else {
            self.open_document(path);
        }
    }

    pub fn location(&self) -> &str {
        self.browser.page.location.path()
    }

    /// True when the pane shows a directory listing.
    pub fn is_directory(&self) -> bool {
        matches!(
            self.browser.page.location,
            remuda_native::documents::Location::Directory(_)
        )
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
        let stale = self.image_flights.iter().filter(|flight| flight.stale).count();
        // Drop receivers and buffered results while the stale count still
        // gates other panes. Any reservation in a channel is released before
        // another worker can see stale == 0 and fail against those bytes.
        self.image_flights.clear();
        self.reservations.clear();
        self.budget.release(0, self.images.len());
        self.budget.clear_stale(stale);
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
    fn no_op_navigation_and_cancelled_address_keep_the_current_images() {
        let budget = Arc::new(ImageBudget::with_limits(4096, 4));
        let mut pane =
            Pane::new_with_budget(PathBuf::from("/nonexistent-vfs.sock"), budget.clone());
        assert!(budget.reserve_entry());
        pane.images.insert(
            "old.png".into(),
            ImageState::Failed("old failure".into()),
        );
        pane.image_limit = Some(ImageLimit::PaneEntries);
        pane.open_document("../invalid");
        assert_eq!(pane.image_entry_count(), 1);
        assert_eq!(budget.usage(), (0, 1));
        assert!(pane.image_limited());
        pane.browser.loading = true;
        pane.open_selected();
        assert_eq!(pane.image_entry_count(), 1);
        pane.browser.inspect_address();
        pane.refresh();
        assert_eq!(pane.image_entry_count(), 1);
        let _ = pane.back();
        assert_eq!(pane.image_entry_count(), 1);
        assert_eq!(budget.usage(), (0, 1));
        pane.cancel();
    }

    #[test]
    fn history_back_releases_the_previous_document_images() {
        let budget = Arc::new(ImageBudget::with_limits(4096, 4));
        let mut pane =
            Pane::new_with_budget(PathBuf::from("/nonexistent-vfs.sock"), budget.clone());
        pane.open_document("a.png");
        pane.open_document("b.png");
        assert!(budget.reserve_entry());
        pane.images.insert(
            "b.png".into(),
            ImageState::Failed("old failure".into()),
        );
        assert!(!pane.back(), "back should restore history without leaving the root");
        assert_eq!(pane.image_entry_count(), 0);
        assert_eq!(budget.usage(), (0, 0));
        pane.cancel();
    }

    #[test]
    fn stale_flight_blocks_new_decode_until_it_is_polled() {
        let budget = Arc::new(ImageBudget::with_limits(4096, 4));
        let mut pane =
            Pane::new_with_budget(PathBuf::from("/nonexistent-vfs.sock"), budget.clone());
        pane.cancel();
        pane.image_revision = 7;
        let (_sender, receiver) = mpsc::sync_channel(1);
        pane.image_flights.push(ImageFlight {
            revision: 6,
            path: "stale.png".into(),
            receiver,
            stale: false,
        });
        pane.image_queue.push_back("new.png".into());
        pane.pump_images();
        assert_eq!(pane.image_queue.len(), 1, "new decode started beside a stale flight");
        assert_eq!(pane.image_flights.len(), 1);
        pane.image_flights.clear();
        pane.pump_images();
        assert!(pane.image_queue.is_empty());
        pane.cancel();
    }

    #[test]
    fn stale_flight_in_one_tab_blocks_decodes_in_another_tab() {
        let budget = Arc::new(ImageBudget::with_limits(4096, 4));
        let mut first =
            Pane::new_with_budget(PathBuf::from("/nonexistent-vfs.sock"), budget.clone());
        let mut second =
            Pane::new_with_budget(PathBuf::from("/nonexistent-vfs.sock"), budget.clone());
        first.cancel();
        second.cancel();
        let (sender, receiver) = mpsc::sync_channel(1);
        first.image_flights.push(ImageFlight {
            revision: first.image_revision,
            path: "old.png".into(),
            receiver,
            stale: false,
        });
        first.release_images();
        assert_eq!(budget.stale_flights(), 1);
        second.image_queue.push_back("new.png".into());
        second.pump_images();
        assert_eq!(second.image_queue.len(), 1, "another tab decoded beside a stale reservation");
        sender.send(ImageLoadResult::Failed("stale".into())).unwrap();
        first.poll_images();
        assert_eq!(budget.stale_flights(), 0);
        second.pump_images();
        assert!(second.image_queue.is_empty());
        first.cancel();
        second.cancel();
    }

    #[test]
    fn reservation_refusal_keeps_its_failure_time_cause() {
        let budget = ImageBudget::with_limits(100, 4);
        assert!(budget.reserve_bytes(97));
        assert_eq!(budget.reserve_bytes_or_cause(4), Err(RetryCause::Bytes));
        budget.mark_stale(1);
        assert_eq!(budget.reserve_bytes_or_cause(4), Err(RetryCause::Stale));
        budget.clear_stale(1);
        budget.release(97, 0);
        assert_eq!(budget.reserve_bytes_or_cause(4), Ok(()));
        assert_eq!(budget.usage(), (4, 0));
    }

    #[test]
    fn stale_caused_retry_survives_the_stale_drain_before_poll_twice() {
        let budget = Arc::new(ImageBudget::with_limits(100, 4));
        let mut pane =
            Pane::new_with_budget(PathBuf::from("/nonexistent-vfs.sock"), budget.clone());
        pane.cancel();
        pane.images
            .insert("new.png".into(), ImageState::Loading);
        // Keep the two-flight limit full so the requeued image remains visible
        // in the queue instead of spawning another worker in this test.
        let mut dummy_senders: Vec<mpsc::SyncSender<ImageLoadResult>> = Vec::new();
        for _ in 0..MAX_CONCURRENT_IMAGE_LOADS {
            let (sender, receiver) = mpsc::sync_channel(1);
            dummy_senders.push(sender);
            pane.image_flights.push(ImageFlight {
                revision: pane.image_revision,
                path: "dummy.png".into(),
                receiver,
                stale: false,
            });
        }
        for round in 0..2 {
            assert_eq!(budget.stale_flights(), 0, "round {round}");
            let (sender, receiver) = mpsc::sync_channel(1);
            sender
                .send(ImageLoadResult::Retry {
                    bytes: 50,
                    cause: RetryCause::Stale,
                })
                .unwrap();
            pane.image_flights.push(ImageFlight {
                revision: pane.image_revision,
                path: "new.png".into(),
                receiver,
                stale: false,
            });
            pane.poll_images();
            assert!(
                matches!(pane.image("new.png"), Some(ImageState::Loading)),
                "round {round}: stale-caused retry became a failure"
            );
            assert_eq!(pane.image_queue.len(), 1, "round {round}");
            assert!(
                !pane.image_retries.contains("new.png"),
                "round {round}: a drained stale race consumed the memory retry"
            );
            pane.image_queue.clear();
        }
        assert_eq!(pane.image_flights.len(), MAX_CONCURRENT_IMAGE_LOADS);
        drop(dummy_senders);
        pane.cancel();
    }

    #[test]
    fn retry_result_is_requeued_and_admitted_after_stale_clears() {
        let budget = Arc::new(ImageBudget::with_limits(100, 2));
        assert!(budget.reserve_entry());
        assert!(budget.reserve_bytes(80));
        budget.mark_stale(1);
        let mut pane =
            Pane::new_with_budget(PathBuf::from("/nonexistent-vfs.sock"), budget.clone());
        pane.cancel();
        pane.images
            .insert("new.png".into(), ImageState::Loading);
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .send(ImageLoadResult::Retry {
                bytes: 50,
                cause: RetryCause::Stale,
            })
            .unwrap();
        pane.image_flights.push(ImageFlight {
            revision: pane.image_revision,
            path: "new.png".into(),
            receiver,
            stale: false,
        });
        pane.poll_images();
        assert!(matches!(pane.image("new.png"), Some(ImageState::Loading)));
        assert_eq!(pane.image_queue.len(), 1, "the retry was not queued");
        assert!(
            !pane.image_retries.contains("new.png"),
            "a stale-dependent retry consumed the one transient retry"
        );
        // A later attempt that meets another tab's new stale reservation must
        // requeue again instead of turning into a permanent failure.
        pane.image_queue.clear();
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .send(ImageLoadResult::Retry {
                bytes: 50,
                cause: RetryCause::Stale,
            })
            .unwrap();
        pane.image_flights.push(ImageFlight {
            revision: pane.image_revision,
            path: "new.png".into(),
            receiver,
            stale: false,
        });
        pane.poll_images();
        assert_eq!(pane.image_queue.len(), 1, "a second stale retry was dropped");
        budget.release(80, 0);
        budget.clear_stale(1);
        pane.pump_images();
        assert!(pane.image_queue.is_empty(), "the retry was never admitted");
        assert_eq!(pane.image_flights.len(), 1);
        pane.cancel();
    }

    #[test]
    fn started_worker_returns_retry_without_waiting_for_its_own_stale_count() {
        let budget = Arc::new(ImageBudget::with_limits(100, 4));
        assert!(budget.reserve_entry());
        // 97 used + a 1x1 RGBA (4 bytes) exceeds 100, so the worker takes the
        // Retry branch instead of reserving successfully.
        assert!(budget.reserve_bytes(97));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker_budget = budget.clone();
        let worker = std::thread::spawn(move || {
            Pane::run_image_worker(
                worker_budget,
                move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(media::Decoded {
                        width: 1,
                        height: 1,
                        bgra: vec![0; 4],
                    })
                },
                sender,
            )
        });
        started_rx.recv().unwrap();
        // The worker is already decoding under the lock; its tab becomes stale
        // now. It must finish instead of waiting for its own stale count.
        budget.mark_stale(1);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        match receiver.try_recv() {
            Ok(ImageLoadResult::Retry { bytes, cause }) => {
                assert_eq!(bytes, 4);
                assert_eq!(cause, RetryCause::Stale);
            }
            _ => panic!("a started worker waited for its own stale reservation"),
        }
        assert_eq!(budget.usage(), (97, 1));
        assert_eq!(budget.stale_flights(), 1);
        budget.release(97, 0);
        budget.clear_stale(1);
        assert_eq!(budget.stale_flights(), 0);
        let (sender, receiver) = mpsc::sync_channel(1);
        Pane::run_image_worker(
            budget.clone(),
            || {
                Ok(media::Decoded {
                    width: 1,
                    height: 1,
                    bgra: vec![0; 4],
                })
            },
            sender,
        );
        match receiver.try_recv() {
            Ok(ImageLoadResult::Ready(_, reservation)) => {
                assert_eq!(reservation.bytes, 4);
                assert_eq!(budget.usage(), (4, 1));
            }
            _ => panic!("the retry was not admitted after the stale reservation drained"),
        }
    }

    #[test]
    fn stale_worker_exits_before_decoding() {
        let budget = Arc::new(ImageBudget::with_limits(100, 4));
        budget.mark_stale(1);
        let decoded = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = decoded.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        Pane::run_image_worker(
            budget.clone(),
            move || {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(media::Decoded {
                    width: 1,
                    height: 1,
                    bgra: vec![0; 4],
                })
            },
            sender,
        );
        assert!(
            !decoded.load(std::sync::atomic::Ordering::SeqCst),
            "a stale worker spent time decoding an image that will be dropped"
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(ImageLoadResult::Retry { bytes: 0, .. })
        ));
        assert_eq!(budget.usage(), (0, 0));
    }

    #[test]
    fn reservation_is_taken_under_the_decode_lock_before_the_next_read() {
        let budget = Arc::new(ImageBudget::with_limits(100, 4));
        let (first_read_tx, first_read_rx) = mpsc::channel();
        let (first_done_tx, first_done_rx) = mpsc::channel();
        let (second_read_tx, second_read_rx) = mpsc::channel();
        let (second_seen_tx, second_seen_rx) = mpsc::channel();
        let (first_sender, first_receiver) = mpsc::sync_channel(1);
        let first_budget = budget.clone();
        let first = std::thread::spawn(move || {
            Pane::run_image_worker(
                first_budget,
                move || {
                    first_read_tx.send(()).unwrap();
                    first_done_rx.recv().unwrap();
                    Ok(media::Decoded {
                        width: 1,
                        height: 1,
                        bgra: vec![0; 4],
                    })
                },
                first_sender,
            )
        });
        first_read_rx.recv().unwrap();
        let (second_sender, second_receiver) = mpsc::sync_channel(1);
        let second_budget = budget.clone();
        let seen_budget = budget.clone();
        let second = std::thread::spawn(move || {
            Pane::run_image_worker(
                second_budget,
                move || {
                    second_read_tx.send(()).unwrap();
                    // The first worker must have reserved its bytes before
                    // this second read starts, even though it has not sent its
                    // result yet.
                    second_seen_tx.send(seen_budget.usage()).unwrap();
                    Ok(media::Decoded {
                        width: 1,
                        height: 1,
                        bgra: vec![0; 4],
                    })
                },
                second_sender,
            )
        });
        assert!(
            second_read_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "a second read entered while the first held the decode lock"
        );
        first_done_tx.send(()).unwrap();
        second_read_rx.recv().unwrap();
        assert_eq!(second_seen_rx.recv().unwrap(), (4, 0));
        first.join().unwrap();
        second.join().unwrap();
        let first_result = first_receiver.try_recv().unwrap();
        let second_result = second_receiver.try_recv().unwrap();
        assert!(matches!(&first_result, ImageLoadResult::Ready(_, _)));
        assert!(matches!(&second_result, ImageLoadResult::Ready(_, _)));
        assert_eq!(budget.usage(), (8, 0));
        drop((first_result, second_result));
        assert_eq!(budget.usage(), (0, 0));
    }

    #[test]
    fn pane_drop_releases_a_buffered_reservation_and_the_stale_gate() {
        let budget = Arc::new(ImageBudget::with_limits(4096, 4));
        let mut pane =
            Pane::new_with_budget(PathBuf::from("/nonexistent-vfs.sock"), budget.clone());
        pane.cancel();
        assert!(budget.reserve_bytes(128));
        let (sender, receiver) = mpsc::sync_channel(1);
        let reservation = ImageReservation {
            budget: budget.clone(),
            bytes: 128,
        };
        sender
            .send(ImageLoadResult::Ready(
                media::Decoded {
                    width: 1,
                    height: 1,
                    bgra: vec![0; 4],
                },
                reservation,
            ))
            .unwrap();
        pane.image_flights.push(ImageFlight {
            revision: pane.image_revision,
            path: "closing.png".into(),
            receiver,
            stale: true,
        });
        budget.mark_stale(1);
        drop(pane);
        assert_eq!(budget.stale_flights(), 0);
        assert_eq!(budget.usage(), (0, 0));
    }

    #[test]
    fn stale_drain_releases_bytes_before_clearing_the_gate() {
        let budget = Arc::new(ImageBudget::with_limits(100, 4));
        let mut pane =
            Pane::new_with_budget(PathBuf::from("/nonexistent-vfs.sock"), budget.clone());
        pane.cancel();
        assert!(budget.reserve_bytes(80));
        let (sender, receiver) = mpsc::sync_channel(1);
        pane.image_flights.push(ImageFlight {
            revision: pane.image_revision,
            path: "old.png".into(),
            receiver,
            stale: false,
        });
        pane.release_images();
        assert_eq!(budget.stale_flights(), 1);
        sender
            .send(ImageLoadResult::Ready(
                media::Decoded {
                    width: 1,
                    height: 1,
                    bgra: vec![0; 4],
                },
                ImageReservation {
                    budget: budget.clone(),
                    bytes: 80,
                },
            ))
            .unwrap();
        let checked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe_budget = budget.clone();
        let probe_checked = checked.clone();
        STALE_DRAIN_PROBE.with(|probe| {
            *probe.borrow_mut() = Some(Box::new(move || {
                // This runs in poll_images after the stale result (and its
                // reservation) is dropped but before clear_stale. The real
                // classifier must be admitted, not told Bytes.
                assert_eq!(probe_budget.usage().0, 0, "stale bytes were still reserved");
                assert_eq!(probe_budget.stale_flights(), 1, "gate cleared before the drop");
                assert_eq!(probe_budget.reserve_bytes_or_cause(50), Ok(()));
                probe_budget.release(50, 0);
                probe_checked.store(true, std::sync::atomic::Ordering::SeqCst);
            }));
        });
        pane.poll_images();
        STALE_DRAIN_PROBE.with(|probe| {
            probe.borrow_mut().take();
        });
        assert!(
            checked.load(std::sync::atomic::Ordering::SeqCst),
            "the stale drain probe did not run"
        );
        assert_eq!(budget.stale_flights(), 0);
        assert_eq!(budget.usage().0, 0);
    }

    #[test]
    fn stale_reservation_is_released_when_its_result_is_polled() {
        let budget = Arc::new(ImageBudget::with_limits(4096, 4));
        let mut pane =
            Pane::new_with_budget(PathBuf::from("/nonexistent-vfs.sock"), budget.clone());
        pane.cancel();
        assert!(budget.reserve_bytes(128));
        let (sender, receiver) = mpsc::sync_channel(1);
        let reservation = ImageReservation {
            budget: budget.clone(),
            bytes: 128,
        };
        sender
            .send(ImageLoadResult::Ready(
                media::Decoded {
                    width: 1,
                    height: 1,
                    bgra: vec![0; 4],
                },
                reservation,
            ))
            .unwrap();
        pane.image_revision = 3;
        pane.image_flights.push(ImageFlight {
            revision: 2,
            path: "stale.png".into(),
            receiver,
            stale: false,
        });
        pane.poll_images();
        assert!(pane.image_flights.is_empty());
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
            sender.send(ImageLoadResult::Failed("fixture error".into())).unwrap();
            pane.image_flights.push(ImageFlight {
                revision: pane.image_revision,
                path,
                receiver,
                stale: false,
            });
        }

        assert!(workspace.poll_changed(), "image completion must repaint");
        for (index, pane) in workspace.tabs.iter().enumerate() {
            let path = format!("image-{index}.png");
            assert!(matches!(pane.image(&path), Some(ImageState::Failed(error)) if error == "fixture error"));
        }
        assert!(!workspace.poll_changed(), "idle panes should not repaint");
    }
}
