use gpui::{
    div, prelude::*, px, rgb, AnyElement, App, Bounds, Context, FocusHandle, KeyDownEvent, Window,
    WindowBounds, WindowOptions,
};
use gpui_kit::base::{Button, ScrollbarMode, Tab, Tabs};
use remuda_core::agent::{AgentProcess, Size};
use remuda_native::net::browser::{
    ssh_arguments, Client, DocumentRequest, DriveListing, Operation, Outcome, Reply,
};
use remuda_native::{CommandBuilder, PtyAgent};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{
    cell::Cell,
    rc::Rc,
    sync::{mpsc, Arc},
};

use crate::browser_media;
#[cfg(any(target_os = "macos", windows))]
use crate::native_document;
use crate::terminal_view::{self, Screen, Viewport};

const SPLIT_WIDTH: f32 = 1120.;
const SIDEBAR_WIDTH: f32 = 208.;

struct Document {
    uri: String,
    media: browser_media::Document,
    request: Option<DocumentRequest>,
    #[cfg(any(target_os = "macos", windows))]
    html: Option<Arc<[u8]>>,
}

enum DriveSource {
    Remote(Arc<Client>),
    Local,
}

impl DriveSource {
    fn read(&self, uri: &str) -> Result<Vec<u8>, String> {
        match self {
            Self::Remote(client) => client.read_file(uri),
            Self::Local => remuda_native::net::browser::read_local_drive(uri),
        }
        .map_err(|error| error.to_string())
    }
    fn list(
        &self,
        directory: Option<String>,
        after: Option<String>,
    ) -> Result<DriveListing, String> {
        match self {
            Self::Remote(client) => list(
                client,
                match after {
                    Some(after) => Operation::ListMore { directory, after },
                    None => Operation::List { directory },
                },
            ),
            Self::Local => remuda_native::net::browser::list_local_drive(
                directory.as_deref(),
                after.as_deref(),
            )
            .map_err(|error| error.to_string()),
        }
    }
}

#[derive(Clone)]
enum DriveAction {
    List(Option<String>),
    More(Option<String>, String),
    Open(String),
    Refresh(Vec<RefreshDirectory>),
}

#[derive(Clone)]
struct RefreshDirectory {
    directory: Option<String>,
    parent: Option<String>,
    loaded_entries: usize,
}

enum DriveResult {
    Directory(DriveListing, bool),
    Document(Document),
    Refreshed(HashMap<Option<String>, DriveListing>),
}

struct Drive {
    actions: mpsc::SyncSender<(u64, DriveAction)>,
    results: mpsc::Receiver<(u64, Result<DriveResult, String>)>,
    listings: HashMap<Option<String>, DriveListing>,
    expanded: HashSet<String>,
    selected: Option<String>,
    last_action: Option<DriveAction>,
    call: u64,
    pending: bool,
    error: Option<String>,
}

impl Drive {
    fn new(source: Arc<DriveSource>) -> Self {
        let (actions, incoming) = mpsc::sync_channel(2);
        let (return_result, results) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok((id, action)) = incoming.recv() {
                let result = match action {
                    DriveAction::List(directory) => source
                        .list(directory, None)
                        .map(|listing| DriveResult::Directory(listing, false)),
                    DriveAction::More(directory, after) => source
                        .list(directory, Some(after))
                        .map(|listing| DriveResult::Directory(listing, true)),
                    DriveAction::Refresh(directories) => {
                        refresh_drive(directories, |directory, after| {
                            source.list(directory, after)
                        })
                        .map(DriveResult::Refreshed)
                    }
                    DriveAction::Open(uri) => {
                        read(|uri| source.read(uri), uri, None).map(DriveResult::Document)
                    }
                };
                if return_result.send((id, result)).is_err() {
                    break;
                }
            }
        });
        Self {
            actions,
            results,
            listings: HashMap::new(),
            expanded: HashSet::new(),
            selected: None,
            last_action: None,
            call: 0,
            pending: false,
            error: None,
        }
    }

    fn send(&mut self, action: DriveAction) {
        self.call = self.call.wrapping_add(1);
        self.last_action = Some(action.clone());
        self.pending = self.actions.try_send((self.call, action)).is_ok();
        self.error = (!self.pending).then(|| "Drive is busy. Try again.".into());
    }

    fn request_listing(&mut self, directory: Option<String>, after: Option<String>) {
        if self.pending {
            return;
        }
        self.send(match after {
            Some(after) => DriveAction::More(directory, after),
            None => DriveAction::List(directory),
        });
    }

    fn refresh(&mut self) {
        if self.pending {
            return;
        }
        let mut directories = vec![RefreshDirectory {
            directory: None,
            parent: None,
            loaded_entries: self
                .listings
                .get(&None)
                .map_or(0, |page| page.entries.len()),
        }];
        directories.extend(self.rows().into_iter().filter_map(|(parent, _, entry)| {
            if !entry.is_directory || !self.expanded.contains(&entry.uri) {
                return None;
            }
            let directory = Some(entry.uri);
            let loaded_entries = self
                .listings
                .get(&directory)
                .map_or(0, |page| page.entries.len());
            Some(RefreshDirectory {
                directory,
                parent,
                loaded_entries,
            })
        }));
        self.send(DriveAction::Refresh(directories));
    }

    fn rows(
        &self,
    ) -> Vec<(
        Option<String>,
        usize,
        remuda_native::net::browser::DriveEntry,
    )> {
        fn append(
            drive: &Drive,
            directory: Option<String>,
            depth: usize,
            rows: &mut Vec<(
                Option<String>,
                usize,
                remuda_native::net::browser::DriveEntry,
            )>,
        ) {
            if depth >= 64 {
                return;
            }
            if let Some(listing) = drive.listings.get(&directory) {
                for entry in &listing.entries {
                    rows.push((directory.clone(), depth, entry.clone()));
                    if entry.is_directory && drive.expanded.contains(&entry.uri) {
                        append(drive, Some(entry.uri.clone()), depth + 1, rows);
                    }
                }
            }
        }
        let mut rows = Vec::new();
        append(self, None, 0, &mut rows);
        rows
    }
}

fn refresh_drive(
    directories: Vec<RefreshDirectory>,
    mut list: impl FnMut(Option<String>, Option<String>) -> Result<DriveListing, String>,
) -> Result<HashMap<Option<String>, DriveListing>, String> {
    let mut refreshed: HashMap<Option<String>, DriveListing> = HashMap::new();
    for RefreshDirectory {
        directory,
        parent,
        loaded_entries,
    } in directories
    {
        if let Some(uri) = &directory {
            let still_visible = refreshed.get(&parent).is_some_and(|page| {
                page.entries
                    .iter()
                    .any(|entry| entry.is_directory && &entry.uri == uri)
            });
            if !still_visible {
                continue;
            }
        }
        let mut page = list(directory.clone(), None)?;
        while page.entries.len() < loaded_entries {
            let Some(after) = page.next.take() else { break };
            let mut next = list(directory.clone(), Some(after.clone()))?;
            if next.next.as_ref() == Some(&after) || next.entries.is_empty() {
                return Err("Drive pagination did not advance".into());
            }
            page.entries.append(&mut next.entries);
            page.next = next.next;
            page.complete = next.complete;
        }
        refreshed.insert(directory, page);
    }
    Ok(refreshed)
}

fn list(client: &Client, operation: Operation) -> Result<DriveListing, String> {
    match client.send(&operation) {
        Ok(Reply::Directory(listing)) => Ok(listing),
        Ok(Reply::Denied) => Err("Drive access was denied".into()),
        _ => Err("Could not load this Drive folder".into()),
    }
}

type AssetJob = (u64, browser_media::Document);

struct Assets {
    requests: mpsc::SyncSender<AssetJob>,
    results: mpsc::Receiver<AssetJob>,
}

impl Assets {
    fn new(source: Arc<DriveSource>) -> Self {
        let (requests, input) = mpsc::sync_channel::<AssetJob>(1);
        let (output, results) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            while let Ok((generation, mut media)) = input.recv() {
                media.load_assets(|path| source.read(&format!("remuda:///{path}")).ok());
                if output.send((generation, media)).is_err() {
                    break;
                }
            }
        });
        Self { requests, results }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Content {
    Terminal,
    Document,
}

#[cfg(any(target_os = "macos", windows))]
#[derive(Default)]
struct NativeHtml {
    source: Option<Arc<DriveSource>>,
    selected: bool,
    failed: bool,
    opening: bool,
    preview: Option<native_document::Preview>,
    receipt_deadline: Option<Instant>,
}

#[cfg(any(target_os = "macos", windows))]
impl NativeHtml {
    fn clear(&mut self, cx: &App) {
        if let Some(mut preview) = self.preview.take() {
            preview.hide(cx);
        }
        self.selected = false;
        self.failed = false;
        self.opening = false;
        self.receipt_deadline = None;
    }
}

enum BrowserMode {
    Standalone,
    Local,
    Ssh,
}

pub(crate) struct Browser {
    destination: String,
    embedded: bool,
    documents: mpsc::Receiver<Document>,
    displayed: mpsc::Sender<DocumentRequest>,
    document: Option<Document>,
    pending_presentation: Option<DocumentRequest>,
    painted: Rc<Cell<bool>>,
    active: Content,
    show_drive: bool,
    drive: Option<Drive>,
    assets: Option<Assets>,
    #[cfg(any(target_os = "macos", windows))]
    native_html: NativeHtml,
    document_generation: u64,
    document_scroll: gpui::ScrollHandle,
    drive_scroll: gpui::ScrollHandle,
    zoom: f32,
    last_input: Option<Instant>,
    split_fraction: f32,
    dragging_split: bool,
    terminal: Option<PtyAgent>,
    screen: Screen,
    terminal_scroll: terminal_view::HistoryScroll,
    terminal_selection: terminal_view::TextSelection,
    panel_bounds: Rc<Cell<Option<Bounds<gpui::Pixels>>>>,
    viewport: Viewport,
    focus: FocusHandle,
    document_focus: FocusHandle,
    drive_focus: FocusHandle,
    split: bool,
}

pub fn open(
    app: &mut App,
    bounds: Bounds<gpui::Pixels>,
    destination: String,
    client: Option<Client>,
) {
    let mut command = CommandBuilder::new("ssh");
    command.args(ssh_arguments(&destination));
    command.env("TERM", "xterm-256color");
    command.env_remove("REMUDA_BROWSER_CLIENT");
    let (terminal, screen) = match PtyAgent::spawn(command, Size::new(100, 30)) {
        Ok(terminal) => (Some(terminal), Screen::default()),
        Err(error) => (
            None,
            Screen {
                text: format!("Could not connect to {destination}: {error}"),
                ..Screen::default()
            },
        ),
    };
    app.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |window, app| {
            window.set_window_title("Remuda Browser");
            #[cfg(windows)]
            crate::windows_icon::install(window);
            #[cfg(target_os = "macos")]
            crate::macos_lifecycle::install_window(
                window,
                app,
                crate::macos_lifecycle::WindowKind::Browser,
            );
            let focus = app.focus_handle();
            focus.focus(window, app);
            let handle = window.window_handle();
            app.new(|cx: &mut Context<Browser>| {
                cx.observe_window_activation(window, |_, _, cx| cx.notify())
                    .detach();
                cx.spawn(async move |view, cx| loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;
                    if handle
                        .update(cx, |_, window, cx| {
                            view.update(cx, |browser, cx| browser.tick(window, cx))
                        })
                        .is_err()
                    {
                        break;
                    }
                })
                .detach();
                Browser::new(
                    destination,
                    BrowserMode::Standalone,
                    client,
                    terminal,
                    screen,
                    focus,
                    cx,
                )
            })
        },
    )
    .expect("open native Browser window");
    app.activate(true);
}

impl Browser {
    fn new(
        destination: String,
        mode: BrowserMode,
        client: Option<Client>,
        terminal: Option<PtyAgent>,
        screen: Screen,
        focus: FocusHandle,
        cx: &mut App,
    ) -> Self {
        let (send_document, documents) = mpsc::sync_channel(1);
        let (displayed, results) = mpsc::channel();
        let remote = client.map(Arc::new);
        if let Some(poller) = remote.clone() {
            std::thread::spawn(move || serve(poller, send_document, results));
        }
        let source = remote
            .map(DriveSource::Remote)
            .or_else(|| matches!(mode, BrowserMode::Local).then_some(DriveSource::Local))
            .map(Arc::new);
        let drive = source.clone().map(Drive::new);
        let assets = source.clone().map(Assets::new);
        #[cfg(any(target_os = "macos", windows))]
        let native_html = NativeHtml {
            source: source.filter(|_| native_document::enabled()),
            ..NativeHtml::default()
        };
        Self {
            destination,
            embedded: !matches!(mode, BrowserMode::Standalone),
            documents,
            displayed,
            document: None,
            pending_presentation: None,
            painted: Default::default(),
            active: Content::Terminal,
            show_drive: false,
            drive,
            assets,
            #[cfg(any(target_os = "macos", windows))]
            native_html,
            document_generation: 0,
            document_scroll: Default::default(),
            drive_scroll: Default::default(),
            zoom: 1.,
            last_input: None,
            split_fraction: 0.5,
            dragging_split: false,
            terminal,
            screen,
            terminal_scroll: Default::default(),
            terminal_selection: Default::default(),
            panel_bounds: Default::default(),
            viewport: Default::default(),
            focus,
            document_focus: cx.focus_handle(),
            drive_focus: cx.focus_handle(),
            split: false,
        }
    }

    pub(crate) fn local(client: Option<Client>, focus: FocusHandle, cx: &mut App) -> Self {
        Self::new(
            String::new(),
            BrowserMode::Local,
            client,
            None,
            Screen::default(),
            focus,
            cx,
        )
    }

    pub(crate) fn ssh(client: Option<Client>, focus: FocusHandle, cx: &mut App) -> Self {
        Self::new(
            String::new(),
            BrowserMode::Ssh,
            client,
            None,
            Screen::default(),
            focus,
            cx,
        )
    }

    #[cfg(test)]
    pub(crate) fn seed_test_document(&mut self, window: &mut Window, cx: &mut App) {
        let uri = "remuda:///pilot/plan.md".to_owned();
        let media = browser_media::decode("pilot/plan.md", b"# Design plan").unwrap();
        self.open_document(
            Document {
                uri,
                media,
                request: None,
                #[cfg(any(target_os = "macos", windows))]
                html: None,
            },
            true,
            window,
            cx,
        );
    }

    pub(crate) fn mark_terminal_input(&mut self) {
        self.last_input = Some(Instant::now());
    }

    pub(crate) fn showing_document(&self) -> bool {
        self.active == Content::Document
    }
    pub(crate) fn has_document(&self) -> bool {
        self.document.is_some()
    }
    pub(crate) fn unseen_document(&self) -> bool {
        self.pending_presentation.is_some() && !self.painted.get()
    }
    pub(crate) fn owns_document_focus(&self, window: &Window, cx: &App) -> bool {
        self.document_focus.contains_focused(window, cx)
    }
    pub(crate) fn select_terminal(&mut self, cx: &App) {
        self.active = Content::Terminal;
        self.split = false;
        self.hide_native_document(cx);
    }
    pub(crate) fn select_document(&mut self, window: &mut Window, cx: &mut App) {
        self.active = Content::Document;
        self.split = false;
        self.document_focus.focus(window, cx);
    }
    pub(crate) fn select_drive(&mut self, window: &mut Window, cx: &mut App) {
        self.show_drive = true;
        self.drive_focus.focus(window, cx);
        if let Some(drive) = &mut self.drive {
            if !drive.listings.contains_key(&None) {
                drive.request_listing(None, None);
            }
        }
    }
    pub(crate) fn split_requested(&self) -> bool {
        self.split
    }
    pub(crate) fn select_split(&mut self) {
        self.split = true;
    }

    pub(crate) fn wide(window: &Window) -> bool {
        f32::from(window.viewport_size().width) >= SPLIT_WIDTH
    }

    pub(crate) fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.painted.get() && self.pending_presentation.take().is_some() {
            cx.notify();
        }
        #[cfg(any(target_os = "macos", windows))]
        if self.native_html.selected && self.pending_presentation.is_some() {
            if self
                .native_html
                .receipt_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.pending_presentation = None;
                cx.notify();
            } else if self.native_html.preview.is_some() {
                // Check the native view on the next real Document paint.
                window.refresh();
            }
        }
        if let Ok(document) = self.documents.try_recv() {
            let typing = (self.embedded || self.focus.is_focused(window))
                && self
                    .last_input
                    .is_some_and(|last| last.elapsed() < Duration::from_secs(2));
            if !typing && !window.is_window_active() {
                cx.activate(true);
                window.activate_window();
            }
            if typing && Self::wide(window) && !self.showing_document() {
                self.split = true;
            }
            self.open_document(document, !typing, window, cx);
            cx.notify();
            window.refresh();
        }
        if let Some(drive) = &mut self.drive {
            while let Ok((id, result)) = drive.results.try_recv() {
                if id != drive.call {
                    continue;
                }
                drive.pending = false;
                match result {
                    Ok(DriveResult::Directory(mut listing, append)) => {
                        let directory = match drive.last_action.take() {
                            Some(
                                DriveAction::List(directory) | DriveAction::More(directory, _),
                            ) => directory,
                            _ => continue,
                        };
                        if append {
                            if let Some(previous) = drive.listings.remove(&directory) {
                                let mut rows = previous.entries;
                                rows.append(&mut listing.entries);
                                listing.entries = rows;
                            }
                        }
                        drive.listings.insert(directory, listing);
                    }
                    Ok(DriveResult::Refreshed(listings)) => {
                        drive.last_action = None;
                        drive
                            .expanded
                            .retain(|uri| listings.contains_key(&Some(uri.clone())));
                        drive.listings = listings;
                        if let Some(selected) = &drive.selected {
                            if !drive
                                .listings
                                .values()
                                .any(|page| page.entries.iter().any(|entry| &entry.uri == selected))
                            {
                                drive.selected = None;
                            }
                        }
                    }
                    Ok(DriveResult::Document(document)) => {
                        drive.last_action = None;
                        let drive_had_focus = self.drive_focus.is_focused(window);
                        self.open_document(document, true, window, cx);
                        if drive_had_focus {
                            self.drive_focus.focus(window, cx);
                        }
                        cx.notify();
                        break;
                    }
                    Err(error) => drive.error = Some(error),
                }
                cx.notify();
            }
        }
        if let Some(assets) = &self.assets {
            while let Ok((generation, media)) = assets.results.try_recv() {
                if generation == self.document_generation {
                    if let Some(document) = &mut self.document {
                        document.media = media;
                        cx.notify();
                    }
                }
            }
        }
        if Self::wide(window) || self.active == Content::Terminal {
            self.refresh_terminal(cx);
        }
    }

    fn open_document(
        &mut self,
        document: Document,
        focus: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self
            .document
            .as_ref()
            .is_none_or(|current| current.uri != document.uri)
        {
            self.zoom = 1.;
            self.document_scroll.set_offset(gpui::point(px(0.), px(0.)));
        }
        if document.request.is_some()
            || self
                .document
                .as_ref()
                .is_none_or(|current| current.uri != document.uri)
        {
            self.pending_presentation = document.request.clone();
            self.painted = Default::default();
        }
        self.document_generation = self.document_generation.wrapping_add(1);
        #[cfg(any(target_os = "macos", windows))]
        {
            self.native_html.clear(cx);
            self.native_html.selected =
                document.html.is_some() && self.native_html.source.is_some();
            if self.native_html.selected && self.pending_presentation.is_some() {
                self.native_html.receipt_deadline =
                    Some(Instant::now() + remuda_native::net::browser::DELIVERY_TTL);
            }
        }
        if document.media.asset_paths().next().is_some() {
            if let Some(assets) = &self.assets {
                let _ = assets
                    .requests
                    .try_send((self.document_generation, document.media.clone()));
            }
        }
        if let Some(previous) = self.document.replace(document) {
            previous.media.release_images(window);
        }
        if focus {
            self.active = Content::Document;
            if !Self::wide(window) {
                self.document_focus.focus(window, cx);
            }
        }
    }

    fn close_document(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(document) = self.document.take() {
            document.media.release_images(window);
        }
        self.document_generation = self.document_generation.wrapping_add(1);
        #[cfg(any(target_os = "macos", windows))]
        self.native_html.clear(cx);
        self.pending_presentation = None;
        self.active = Content::Terminal;
        self.focus.focus(window, cx);
    }

    fn refresh_terminal(&mut self, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminal.as_mut() {
            if let Some(size) = self.viewport.get() {
                if terminal.size() != size && terminal.is_alive() {
                    let _ = terminal.resize(size);
                }
            }
            let scroll = self.terminal_scroll.offset;
            if let Ok(mut screen) = terminal
                .screen_cells_at(scroll)
                .map_err(|e| e.to_string())
                .and_then(Screen::from_cells)
            {
                screen.wrapped = terminal.row_wrapped_at(scroll).unwrap_or_default();
                if screen != self.screen {
                    self.screen = screen;
                    cx.notify();
                }
            }
        }
    }

    fn scroll_terminal(&mut self, event: &gpui::ScrollEvent, cx: &mut Context<Self>) {
        use remuda_core::agent::MouseMode;
        use remuda_native::mouse::{route_mouse_event, MouseAction, SgrMouse};

        let Some(terminal) = &mut self.terminal else {
            return;
        };
        let lines = self.terminal_scroll.wheel_lines(event.delta);
        if lines == 0 {
            return;
        }
        let mouse = terminal.mouse_state();
        if mouse.mode != MouseMode::None && self.terminal_scroll.offset == 0 {
            if let Some(bounds) = self.panel_bounds.get() {
                let size = terminal.size();
                let cell = |relative: gpui::Pixels, extent: gpui::Pixels, count: u16| {
                    (f32::from(relative) / f32::from(extent) * f32::from(count))
                        .clamp(0., f32::from(count.saturating_sub(1))) as u16
                        + 1
                };
                let wheel = SgrMouse {
                    button: if lines > 0 { 64 } else { 65 },
                    x: cell(
                        event.position.x - bounds.left(),
                        bounds.size.width,
                        size.cols(),
                    ),
                    y: cell(
                        event.position.y - bounds.top(),
                        bounds.size.height,
                        size.rows(),
                    ),
                    release: false,
                };
                if let MouseAction::Forward(input) = route_mouse_event(wheel, mouse, true, 0) {
                    for _ in 0..lines.unsigned_abs().min(8) {
                        let _ = terminal.write(&input);
                    }
                }
            }
        } else if self.terminal_scroll.shift(lines, terminal.scrollback_len()) {
            self.refresh_terminal(cx);
        }
        cx.stop_propagation();
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self
            .terminal_selection
            .copy(&event.keystroke, Some(&self.screen), cx)
        {
            cx.stop_propagation();
            return;
        }
        if let Some(input) = crate::live_session::key_bytes(&event.keystroke) {
            if let Some(terminal) = self.terminal.as_mut() {
                self.last_input = Some(Instant::now());
                if let Err(error) = terminal.write(&input) {
                    self.screen = Screen {
                        text: error.to_string(),
                        ..Screen::default()
                    };
                    cx.notify();
                }
                cx.stop_propagation();
            }
        }
    }

    fn visit(&mut self, uri: String, directory: bool) {
        if let Some(drive) = &mut self.drive {
            drive.selected = Some(uri.clone());
            if directory {
                if !drive.expanded.remove(&uri) {
                    let key = Some(uri.clone());
                    if !drive.listings.contains_key(&key) && drive.pending {
                        return;
                    }
                    drive.expanded.insert(uri);
                    if !drive.listings.contains_key(&key) {
                        drive.request_listing(key, None);
                    }
                }
            } else if !drive.pending {
                drive.send(DriveAction::Open(uri));
            }
        }
    }

    fn drive_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.drive_focus.is_focused(window) {
            return;
        }
        let Some(drive) = &mut self.drive else {
            return;
        };
        let rows = drive.rows();
        if rows.is_empty() {
            return;
        }
        let index = rows
            .iter()
            .position(|(_, _, row)| Some(&row.uri) == drive.selected.as_ref());
        let current = index.unwrap_or_default();
        match event.keystroke.key.as_str() {
            "up" | "down" | "home" | "end" => {
                let next = match event.keystroke.key.as_str() {
                    "up" => current.saturating_sub(1),
                    "down" => index.map_or(0, |i| (i + 1).min(rows.len() - 1)),
                    "end" => rows.len() - 1,
                    _ => 0,
                };
                drive.selected = Some(rows[next].2.uri.clone());
            }
            "left" => {
                let (parent, _, entry) = &rows[current];
                if !drive.expanded.remove(&entry.uri) {
                    drive.selected = parent.clone();
                }
            }
            "right" => {
                let (_, _, entry) = &rows[current];
                if entry.is_directory {
                    if drive.expanded.contains(&entry.uri) {
                        if let Some((Some(parent), _, child)) = rows.get(current + 1) {
                            if parent == &entry.uri {
                                drive.selected = Some(child.uri.clone());
                            }
                        }
                    } else {
                        self.visit(entry.uri.clone(), true);
                    }
                }
            }
            "enter" => {
                let entry = &rows[current].2;
                self.visit(entry.uri.clone(), entry.is_directory);
            }
            _ => return,
        }
        if let Some(next) = self.drive.as_ref().and_then(|drive| {
            rows.iter()
                .position(|(_, _, row)| Some(&row.uri) == drive.selected.as_ref())
        }) {
            self.drive_scroll.scroll_to_item(next);
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let switcher = Tabs::new("browser-sidebar-tabs")
            .flex()
            .gap_1()
            .p_2()
            .child(
                tab("sessions-mode", "Sessions", !self.show_drive).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.show_drive = false;
                        this.focus.focus(window, cx);
                        cx.notify();
                    },
                )),
            )
            .child(
                tab("drive-mode", "Drive", self.show_drive).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.select_drive(window, cx);
                        cx.notify();
                    },
                )),
            );
        let body = if self.show_drive {
            self.drive_listing(cx)
        } else {
            div()
                .p_2()
                .child(
                    button("connected-session", self.destination.clone(), true)
                        .truncate()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.active = Content::Terminal;
                            this.focus.focus(window, cx);
                            cx.notify();
                        })),
                )
                .into_any_element()
        };
        div()
            .id("browser-sidebar")
            .w(px(SIDEBAR_WIDTH))
            .flex_none()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(0x232834))
            .text_color(rgb(0xebeff8))
            .text_sm()
            .child(switcher)
            .child(body)
            .into_any_element()
    }

    pub(crate) fn drive_listing(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(drive) = &self.drive else {
            return div()
                .p_3()
                .child("Drive connection is not configured.")
                .into_any_element();
        };
        let rows = drive.rows();
        let pages = drive
            .listings
            .iter()
            .filter_map(|(directory, page)| {
                if directory.as_ref().is_some_and(|uri| {
                    !drive.expanded.contains(uri) || !rows.iter().any(|(_, _, row)| &row.uri == uri)
                }) {
                    return None;
                }
                Some((
                    directory.clone(),
                    page.next.clone()?,
                    page.directory.clone(),
                ))
            })
            .collect::<Vec<_>>();
        let entries = div()
            .id("drive-entries")
            .track_scroll(&self.drive_scroll)
            .track_focus(&self.drive_focus)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_2()
            .on_key_down(cx.listener(Self::drive_key))
            .children(rows.iter().map(|(_, depth, entry)| {
                let uri = entry.uri.clone();
                let directory = entry.is_directory;
                let label = if directory {
                    format!(
                        "{} {}",
                        if drive.expanded.contains(&uri) {
                            "▾"
                        } else {
                            "▸"
                        },
                        entry.name
                    )
                } else {
                    entry.name.clone()
                };
                button(
                    gpui::SharedString::from(format!("drive:{uri}")),
                    label,
                    drive.selected.as_ref() == Some(&uri),
                )
                .ml(px((*depth).min(12) as f32 * 14.))
                .truncate()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.drive_focus.focus(window, cx);
                    this.visit(uri.clone(), directory);
                    cx.notify();
                }))
            }))
            .children(pages.into_iter().map(|(directory, after, name)| {
                let id = gpui::SharedString::from(format!("drive-more:{after}:{directory:?}"));
                button(
                    id,
                    name.map_or_else(
                        || "Load more".into(),
                        |uri| format!("Load more · {}", uri.rsplit('/').next().unwrap_or_default()),
                    ),
                    false,
                )
                .truncate()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.drive_focus.focus(window, cx);
                    if let Some(drive) = &mut this.drive {
                        drive.request_listing(directory.clone(), Some(after.clone()));
                    }
                    cx.notify();
                }))
            }))
            .when(drive.pending, |d| d.child(div().p_2().child("Loading…")))
            .when(
                !drive.pending && drive.error.is_none() && rows.is_empty(),
                |d| d.child(div().p_2().child("No documents")),
            )
            .when_some(drive.error.clone(), |d, error| {
                d.child(
                    div().p_2().child(error).child(
                        action_button("drive-retry", "Retry")
                            .justify_start()
                            .disabled(drive.pending || drive.last_action.is_none())
                            .on_click(cx.listener(|this, event, window, cx| {
                                let Some(drive) =
                                    this.drive.as_mut().filter(|drive| !drive.pending)
                                else {
                                    return;
                                };
                                let Some(action) = drive.last_action.clone() else {
                                    return;
                                };
                                drive.send(action);
                                if drive.pending || !matches!(event, gpui::ClickEvent::Keyboard(_))
                                {
                                    this.drive_focus.focus(window, cx);
                                }
                                cx.notify();
                            })),
                    ),
                )
            });
        let refresh = action_button("drive-refresh", "Refresh")
            .disabled(drive.pending)
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(drive) = &mut this.drive {
                    drive.refresh();
                    cx.notify();
                }
            }));
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(div().flex_none().px_2().child(refresh))
            .child(crate::scroll_area(
                &self.drive_scroll,
                ScrollbarMode::Always,
                entries,
            ))
            .into_any_element()
    }

    pub(crate) fn terminal_link_handler(
        browser: gpui::WeakEntity<Self>,
    ) -> terminal_view::LinkHandler {
        std::rc::Rc::new(move |uri, _, cx| {
            use remuda_native::terminal_link::Target;
            match Target::parse(uri) {
                Some(Target::Drive(_)) => {
                    let _ = browser.update(cx, |this, cx| {
                        this.visit(uri.to_owned(), false);
                        cx.notify();
                    });
                }
                Some(Target::Web(url)) => cx.open_url(url.as_str()),
                None => {}
            }
        })
    }

    fn terminal_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let panel_bounds = self.panel_bounds.clone();
        let open = Self::terminal_link_handler(cx.entity().downgrade());
        div()
            .id("browser-terminal")
            .debug_selector(|| "browser-terminal".into())
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.active = Content::Terminal;
                    this.focus.focus(window, cx);
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(Self::key_down))
            .on_scroll(cx.listener(|this, event, _, cx| this.scroll_terminal(event, cx)))
            .child(
                gpui::canvas(
                    move |bounds, _, _| panel_bounds.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(terminal_view::measure(self.viewport.clone()))
            .child(self.screen.view(&self.terminal_selection, Some(open)))
            .into_any_element()
    }

    pub(crate) fn hide_native_document(&mut self, cx: &App) {
        #[cfg(any(target_os = "macos", windows))]
        {
            self.native_html.opening = false;
            if let Some(preview) = &mut self.native_html.preview {
                preview.hide(cx);
            }
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        let _ = cx;
    }

    #[cfg(any(target_os = "macos", windows))]
    fn native_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let view = cx.weak_entity();
        let generation = self.document_generation;
        let element = self
            .native_html
            .preview
            .as_ref()
            .map(|preview| preview.element(cx));
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .children(element)
            .child(
                gpui::canvas(
                    |_, _, _| (),
                    move |bounds, _, window, app| {
                        if bounds.intersect(&window.content_mask().bounds).is_empty() {
                            return;
                        }
                        let _ = view.update(app, |this, cx| {
                            if this.document_generation != generation || !this.native_html.selected
                            {
                                return;
                            }
                            if let Some(preview) = &this.native_html.preview {
                                if this.pending_presentation.is_some()
                                    && !this.painted.get()
                                    && preview.observe_presentation(cx)
                                {
                                    this.acknowledge_document(generation);
                                }
                                return;
                            }
                            if this.native_html.opening {
                                return;
                            }
                            this.native_html.opening = true;
                            let view = view.clone();
                            window.on_next_frame(move |window, app| {
                                let _ = view.update(app, |this, cx| {
                                    if this.document_generation != generation
                                        || !std::mem::take(&mut this.native_html.opening)
                                    {
                                        return;
                                    }
                                    let Some(document) = &this.document else {
                                        return;
                                    };
                                    let Some(html) = document.html.clone() else {
                                        return;
                                    };
                                    let Some(source) = this.native_html.source.clone() else {
                                        return;
                                    };
                                    let asset: native_document::ReadAsset =
                                        Arc::new(move |uri| source.read(uri).ok());
                                    match native_document::Preview::open(
                                        window,
                                        cx,
                                        &document.uri,
                                        html,
                                        asset,
                                    ) {
                                        Ok(preview) => this.native_html.preview = Some(preview),
                                        Err(error) => {
                                            eprintln!("remuda: native document preview: {error}");
                                            this.native_html.selected = false;
                                            this.native_html.failed = true;
                                            this.pending_presentation = None;
                                        }
                                    }
                                    cx.notify();
                                });
                            });
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
            .into_any_element()
    }

    fn acknowledge_document(&self, generation: u64) {
        if generation != self.document_generation {
            return;
        }
        if let Some(request) = self.pending_presentation.as_ref().filter(|request| {
            self.document
                .as_ref()
                .is_some_and(|document| request.uri == document.uri)
        }) {
            if !self.painted.replace(true) {
                let _ = self.displayed.send(request.clone());
            }
        }
    }

    pub(crate) fn document_panel(&mut self, pane_width: f32, cx: &mut Context<Self>) -> AnyElement {
        let Some(document) = self.document.as_ref() else {
            return div()
                .flex_1()
                .p_6()
                .child("Open a document from Drive")
                .into_any_element();
        };
        #[cfg(any(target_os = "macos", windows))]
        let reader_active = !self.native_html.selected;
        #[cfg(not(any(target_os = "macos", windows)))]
        let reader_active = true;
        let presentation = (reader_active
            && self.pending_presentation.is_some()
            && !self.painted.get())
        .then(|| {
            let view = cx.weak_entity();
            let generation = self.document_generation;
            gpui::canvas(
                |_, _, _| (),
                move |bounds, _, _, app| {
                    if bounds.size.width > px(0.) && bounds.size.height > px(0.) {
                        let _ = view.update(app, |this, _| this.acknowledge_document(generation));
                    }
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full()
        });
        let view = cx.weak_entity();
        let body = document.media.render(
            (pane_width - 48.).max(100.),
            self.zoom,
            move |path, _, app| {
                let _ = view.update(app, |this, cx| {
                    this.visit(format!("remuda:///{path}"), false);
                    cx.notify();
                });
            },
        );
        let fallback = || {
            crate::scroll_area(
                &self.document_scroll,
                ScrollbarMode::Always,
                div()
                    .id(gpui::SharedString::from(format!(
                        "document:{}",
                        document.uri
                    )))
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&self.document_scroll)
                    .debug_selector(|| "browser-document-scroll".into())
                    .p_6()
                    .child(body),
            )
            .into_any_element()
        };
        #[cfg(any(target_os = "macos", windows))]
        let (body, preview_button) = if self.native_html.source.is_some() && document.html.is_some()
        {
            let label = if self.native_html.selected {
                "Reader"
            } else {
                "Native preview"
            };
            let button = action_button("native-preview", label).on_click(cx.listener(
                |this, _, window, cx| {
                    this.native_html.selected = !this.native_html.selected;
                    this.native_html.failed = false;
                    if !this.native_html.selected {
                        this.pending_presentation = None;
                        this.hide_native_document(cx);
                    }
                    this.document_focus.focus(window, cx);
                    cx.notify();
                },
            ));
            let body = if self.native_html.selected {
                self.native_panel(cx)
            } else {
                fallback()
            };
            (body, Some(button))
        } else {
            (fallback(), None)
        };
        #[cfg(not(any(target_os = "macos", windows)))]
        let body = fallback();

        div()
            .id("browser-document")
            .debug_selector(|| "browser-document-body".into())
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(0xf7f7f5))
            .text_color(rgb(0x1c2327))
            .relative()
            .track_focus(&self.document_focus)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .p_2()
                    .bg(rgb(0xe5e8e8))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(document.media.title.clone()),
                    )
                    .when(document.media.is_image(), |d| {
                        d.child(
                            action_button("zoom-in", "+")
                                .accessibility_label("Zoom in")
                                .on_click(cx.listener(|this, event, window, cx| {
                                    this.zoom = (this.zoom * 1.25).min(8.);
                                    if !matches!(event, gpui::ClickEvent::Keyboard(_)) {
                                        this.document_focus.focus(window, cx);
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(action_button("zoom-fit", "Fit").on_click(cx.listener(
                            |this, event, window, cx| {
                                this.zoom = 1.;
                                if !matches!(event, gpui::ClickEvent::Keyboard(_)) {
                                    this.document_focus.focus(window, cx);
                                }
                                cx.notify();
                            },
                        )))
                    })
                    .when_some(
                        {
                            #[cfg(any(target_os = "macos", windows))]
                            {
                                preview_button
                            }
                            #[cfg(not(any(target_os = "macos", windows)))]
                            {
                                None::<Button>
                            }
                        },
                        |toolbar, button| toolbar.child(button),
                    )
                    .child(
                        action_button("close-document", "Close").on_click(cx.listener(
                            |this, _, window, cx| {
                                this.close_document(window, cx);
                                cx.notify();
                            },
                        )),
                    ),
            )
            .when(
                {
                    #[cfg(any(target_os = "macos", windows))]
                    {
                        self.native_html.failed
                    }
                    #[cfg(not(any(target_os = "macos", windows)))]
                    {
                        false
                    }
                },
                |panel| panel.child(div().px_3().py_2().child("Native preview unavailable")),
            )
            .child(body)
            .when_some(presentation, |d, marker| d.child(marker))
            .into_any_element()
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Some(terminal) = self.terminal.as_mut() {
            let _ = terminal.terminate();
        }
    }
}

fn serve(
    client: Arc<Client>,
    documents: mpsc::SyncSender<Document>,
    results: mpsc::Receiver<DocumentRequest>,
) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let instance = format!("{}-{nonce}", std::process::id());
    let (load, requests) = mpsc::sync_channel::<DocumentRequest>(1);
    let (loaded, documents_loaded) = mpsc::sync_channel(1);
    let reader = client.clone();
    // Keep the active subscription alive while a document downloads or decodes.
    std::thread::spawn(move || {
        while let Ok(request) = requests.recv() {
            let result = read(
                |uri| reader.read_file(uri).map_err(|error| error.to_string()),
                request.uri.clone(),
                Some(request.clone()),
            );

            if loaded.send((request, result)).is_err() {
                break;
            }
        }
    });
    let mut current = None;
    let mut outcome = None;
    loop {
        match results.try_recv() {
            Ok(request) if current.as_ref() == Some(&request) => outcome = Some(Outcome::Displayed),
            Err(mpsc::TryRecvError::Disconnected) => return,
            _ => {}
        }
        let poll = Operation::Poll {
            instance: instance.clone(),
        };
        let pending = match client.send(&poll) {
            Ok(Reply::Event(Some(request))) => Some(request),
            _ => None,
        };
        if let Some(request) = &pending {
            if current.as_ref() != Some(request) {
                current = Some(request.clone());
                outcome = load
                    .try_send(request.clone())
                    .err()
                    .map(|_| Outcome::ReadFailed);
            }
        }
        while let Ok((loaded_request, result)) = documents_loaded.try_recv() {
            if current.as_ref() != Some(&loaded_request) {
                continue;
            }
            match result {
                Ok(document) => match documents.try_send(document) {
                    Ok(()) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => return,
                    Err(mpsc::TrySendError::Full(_)) => outcome = Some(Outcome::ReadFailed),
                },
                Err(_) => outcome = Some(Outcome::ReadFailed),
            }
        }
        if let (Some(request), Some(outcome)) = (pending, outcome) {
            let _ = client.send(&Operation::Finish {
                instance: instance.clone(),
                request,
                outcome,
            });
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn read(
    load: impl FnOnce(&str) -> Result<Vec<u8>, String>,
    uri: String,
    request: Option<DocumentRequest>,
) -> Result<Document, String> {
    let path = remuda_core::vfs::VfsUri::parse(&uri)
        .map_err(|error| error.to_string())?
        .path;
    let bytes = load(&uri)?;
    let media = browser_media::decode(&path, &bytes)?;
    #[cfg(any(target_os = "macos", windows))]
    let html = (request.is_none()
        && path.rsplit_once('.').is_some_and(|(_, extension)| {
            extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm")
        }))
    .then(|| Arc::from(bytes));
    Ok(Document {
        uri,
        media,
        request,
        #[cfg(any(target_os = "macos", windows))]
        html,
    })
}

fn tab(id: &'static str, label: &'static str, selected: bool) -> Tab {
    Tab::new(id)
        .debug_selector(move || id.into())
        .selected(selected)
        .styles(|style| style.selected(|style| style.bg(rgb(0x365370)).text_color(rgb(0xf4f6fc))))
        .px_2()
        .py_1()
        .rounded_sm()
        .cursor_pointer()
        .hover(|d| d.bg(rgb(0x52647a)).text_color(rgb(0xf4f6fc)))
        .focus_visible(|d| d.bg(rgb(0x52647a)).text_color(rgb(0xf4f6fc)))
        .child(label)
}

fn action_button(id: &'static str, label: &'static str) -> Button {
    Button::new(id)
        .debug_selector(move || id.into())
        .px_2()
        .py_1()
        .rounded_sm()
        .line_height(gpui::TextStyle::default().line_height)
        .cursor_pointer()
        .styles(|style| style.disabled(|style| style.opacity(0.55)))
        .hover(|d| d.bg(rgb(0x52647a)).text_color(rgb(0xf4f6fc)))
        .focus_visible(|d| d.bg(rgb(0x52647a)).text_color(rgb(0xf4f6fc)))
        .child(label)
}

fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<gpui::SharedString>,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_sm()
        .cursor_pointer()
        .when(selected, |d| d.bg(rgb(0x365370)).text_color(rgb(0xf4f6fc)))
        .hover(|d| d.bg(rgb(0x52647a)).text_color(rgb(0xf4f6fc)))
        .child(label.into())
}

impl Render for Browser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let wide =
            Self::wide(window) && self.document.is_some() && self.terminal.is_some() && self.split;
        let has_unseen = self.pending_presentation.as_ref().is_some_and(|request| {
            self.document
                .as_ref()
                .is_some_and(|doc| doc.uri == request.uri)
        });
        let available = f32::from(window.viewport_size().width) - SIDEBAR_WIDTH;
        let document_width = if wide {
            (available - 5.) * (1. - self.split_fraction)
        } else {
            available
        };
        let mut content = div()
            .flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex_col()
            .child(
                Tabs::new("browser-content-tabs")
                    .flex()
                    .gap_2()
                    .p_2()
                    .bg(rgb(0xe5e8e8))
                    .child(
                        tab("terminal-tab", "Terminal", self.active == Content::Terminal)
                            .debug_selector(|| "browser-terminal-tab".into())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.select_terminal(cx);
                                this.focus.focus(window, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        tab(
                            "document-tab",
                            if has_unseen {
                                "Document •"
                            } else {
                                "Document"
                            },
                            self.active == Content::Document,
                        )
                        .debug_selector(|| "browser-document-tab".into())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.select_document(window, cx);
                            cx.notify();
                        })),
                    ),
            )
            .when(
                Self::wide(window) && self.document.is_some() && self.terminal.is_some(),
                |d| {
                    d.child(
                        button("browser-split-toggle", "Side by side", wide).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.select_split();
                                cx.notify();
                            },
                        )),
                    )
                },
            );
        if wide {
            let terminal = self.terminal_panel(cx);
            let document = self.document_panel(document_width, cx);
            let width = (f32::from(window.viewport_size().width) - SIDEBAR_WIDTH - 5.)
                * self.split_fraction;
            content = content.child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(
                        div()
                            .flex()
                            .min_h_0()
                            .w(px(width))
                            .flex_none()
                            .child(terminal),
                    )
                    .child(
                        div()
                            .id("browser-split-divider")
                            .w(px(5.))
                            .flex_none()
                            .cursor_col_resize()
                            .bg(rgb(0x354055))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|this, _, _, _| {
                                    this.dragging_split = true;
                                }),
                            ),
                    )
                    .child(document),
            );
        } else if self.active == Content::Document {
            content = content.child(self.document_panel(document_width, cx));
        } else {
            self.hide_native_document(cx);
            content = content.child(self.terminal_panel(cx));
        }
        div()
            .flex()
            .size_full()
            .text_color(rgb(0x1c2327))
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, cx| {
                    if !this.dragging_split {
                        return;
                    }
                    if event.pressed_button != Some(gpui::MouseButton::Left) {
                        this.dragging_split = false;
                        return;
                    }
                    let available = f32::from(window.viewport_size().width) - SIDEBAR_WIDTH;
                    if available > 0. {
                        this.split_fraction = ((f32::from(event.position.x) - SIDEBAR_WIDTH)
                            / available)
                            .clamp(0.34, 0.66);
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.dragging_split = false;
                }),
            )
            // Leave the PTY's plain Tab and Shift+Tab handling intact.
            .capture_key_down(|event, window, cx| {
                crate::workspace_tab_navigation(event, window, cx, true)
            })
            .on_key_down(|event, window, cx| {
                crate::workspace_tab_navigation(event, window, cx, false)
            })
            .child(self.sidebar(cx))
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_reloads_visible_folders_and_uses_fresh_pagination() {
        use remuda_native::net::browser::DriveEntry;

        let folder = "remuda:///folder".to_owned();
        let removed = "remuda:///gone".to_owned();
        let new_file = "remuda:///folder/new.md".to_owned();
        let entry = |uri: &str, is_directory| DriveEntry {
            name: uri.rsplit('/').next().unwrap().into(),
            uri: uri.into(),
            is_directory,
        };
        let plan = vec![
            RefreshDirectory {
                directory: None,
                parent: None,
                loaded_entries: 2,
            },
            RefreshDirectory {
                directory: Some(folder.clone()),
                parent: None,
                loaded_entries: 1,
            },
            RefreshDirectory {
                directory: Some(removed.clone()),
                parent: None,
                loaded_entries: 1,
            },
        ];
        let listings = refresh_drive(plan, |directory, after| {
            let (entries, next) = match (directory.as_deref(), after.as_deref()) {
                (None, None) => (vec![entry("remuda:///a.md", false)], Some("fresh".into())),
                (None, Some("fresh")) => (vec![entry(&folder, true)], None),
                (Some(uri), None) if uri == folder => (vec![entry(&new_file, false)], None),
                other => panic!("unexpected refresh request {other:?}"),
            };
            Ok(DriveListing {
                directory,
                entries,
                complete: next.is_none(),
                next,
            })
        })
        .unwrap();
        assert_eq!(listings[&None].entries.len(), 2);
        assert!(listings[&None].complete);
        assert_eq!(listings[&Some(folder)].entries[0].uri, new_file);
        assert!(!listings.contains_key(&Some(removed)));
    }

    #[gpui::test]
    #[ignore = "launched by the isolated two-listener document acceptance"]
    fn daemonless_receipt_acceptance(cx: &mut gpui::TestAppContext) {
        let client = Client::from_env()
            .unwrap()
            .expect("isolated Browser credentials");
        cx.update(|app| {
            let bounds = Bounds::centered(None, gpui::size(px(1000.), px(650.)), app);
            open(app, bounds, "fixture".into(), Some(client));
        });
        let end = Instant::now() + Duration::from_secs(45);
        while Instant::now() < end {
            cx.background_executor
                .advance_clock(Duration::from_millis(120));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn document(request: Option<DocumentRequest>) -> Document {
        Document {
            uri: "remuda:///pilot/plan.md".into(),
            media: browser_media::decode("pilot/plan.md", b"# Design plan\n\nMake it readable.")
                .unwrap(),
            request,
            #[cfg(any(target_os = "macos", windows))]
            html: None,
        }
    }

    #[gpui::test]
    fn drive_and_bus_open_the_same_reader_across_layouts(cx: &mut gpui::TestAppContext) {
        let (actions, drive_commands) = mpsc::sync_channel(2);
        let (send_drive_result, results) = mpsc::channel();
        let (send_document, documents) = mpsc::sync_channel(1);
        let (displayed, receipt) = mpsc::channel();
        let (view, cx) = cx.add_window_view(|window, cx| {
            let focus = cx.focus_handle();
            focus.focus(window, cx);
            Browser {
                destination: "pilot".into(),
                embedded: false,
                documents,
                displayed,
                document: None,
                pending_presentation: None,
                painted: Default::default(),
                active: Content::Terminal,
                show_drive: false,
                drive: Some(Drive {
                    actions,
                    results,
                    listings: HashMap::new(),
                    expanded: HashSet::new(),
                    selected: None,
                    last_action: None,
                    call: 0,
                    pending: false,
                    error: None,
                }),
                assets: None,
                #[cfg(any(target_os = "macos", windows))]
                native_html: NativeHtml::default(),
                document_generation: 0,
                document_scroll: Default::default(),
                drive_scroll: Default::default(),
                zoom: 1.,
                last_input: None,
                split_fraction: 0.5,
                dragging_split: false,
                terminal: None,
                screen: Screen::default(),
                terminal_scroll: Default::default(),
                terminal_selection: Default::default(),
                panel_bounds: Default::default(),
                viewport: Default::default(),
                focus,
                document_focus: cx.focus_handle(),
                drive_focus: cx.focus_handle(),
                split: false,
            }
        });
        cx.simulate_resize(gpui::size(px(800.), px(600.)));
        assert!(cx.debug_bounds("browser-terminal-tab").is_some());
        assert!(cx.debug_bounds("browser-document-tab").is_some());
        assert!(cx.debug_bounds("browser-document-body").is_none());
        cx.update(|window, app| window.draw(app).clear(app));
        cx.simulate_keystrokes("ctrl-tab end");
        assert!(view.read_with(cx, |browser, _| !browser.show_drive));
        cx.simulate_keystrokes("enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        assert!(view.read_with(cx, |browser, _| browser.show_drive));
        let (listing_id, listing) = drive_commands.try_recv().unwrap();
        assert!(matches!(listing, DriveAction::List(None)));
        send_drive_result
            .send((
                listing_id,
                Ok(DriveResult::Directory(
                    DriveListing {
                        directory: None,
                        entries: Vec::new(),
                        complete: true,
                        next: None,
                    },
                    false,
                )),
            ))
            .unwrap();
        view.update_in(cx, |browser, window, cx| browser.tick(window, cx));
        let sessions = cx.debug_bounds("sessions-mode").unwrap();
        cx.simulate_click(sessions.center(), gpui::Modifiers::none());
        assert!(view.read_with(cx, |browser, _| !browser.show_drive));

        let request = DocumentRequest {
            id: "requested-plan".into(),
            uri: document(None).uri,
        };
        send_document
            .send(document(Some(request.clone())))
            .ok()
            .unwrap();
        view.update_in(cx, |browser, window, cx| {
            browser.last_input = Some(Instant::now());
            browser.tick(window, cx);
            assert!(browser.active == Content::Terminal);
            assert_eq!(browser.pending_presentation, Some(request.clone()));
            browser.acknowledge_document(browser.document_generation.wrapping_sub(1));
            browser.pending_presentation.as_mut().unwrap().uri = "remuda:///old".into();
            browser.acknowledge_document(browser.document_generation);
            browser.pending_presentation = Some(request.clone());
        });
        assert!(
            receipt.try_recv().is_err(),
            "hidden content cannot report displayed"
        );

        view.update_in(cx, |browser, _window, cx| {
            browser.show_drive = true;
            browser.screen = Screen::from_cells(vec![vec![remuda_core::agent::StyledCell {
                text: "Design plan".into(),
                hyperlink: Some(request.uri.clone()),
                ..Default::default()
            }]])
            .unwrap();
            cx.notify();
        });
        let link = cx.debug_bounds("terminal-link-0-0").unwrap();
        cx.simulate_click(link.center(), gpui::Modifiers::none());
        view.update_in(cx, |browser, window, cx| {
            let (id, action) = drive_commands.try_recv().unwrap();
            assert!(matches!(action, DriveAction::Open(ref uri) if uri == &request.uri));
            send_drive_result
                .send((id, Ok(DriveResult::Document(document(None)))))
                .ok()
                .unwrap();
            browser.tick(window, cx);
            assert!(browser.active == Content::Document);
            assert_eq!(browser.document.as_ref().unwrap().uri, request.uri);
        });
        assert!(cx.debug_bounds("browser-document-body").is_some());
        assert_eq!(receipt.try_recv().ok(), Some(request.clone()));
        let terminal = cx.debug_bounds("browser-terminal-tab").unwrap();
        cx.simulate_click(terminal.center(), gpui::Modifiers::none());
        assert!(cx.debug_bounds("browser-document-body").is_none());
        cx.simulate_keystrokes("ctrl-shift-tab end space");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("space").unwrap(),
        });
        assert!(cx.debug_bounds("browser-document-body").is_some());
        assert!(
            receipt.try_recv().is_err(),
            "switching tabs cannot report a duplicate receipt"
        );

        cx.simulate_resize(gpui::size(px(1420.), px(720.)));
        view.update_in(cx, |browser, window, cx| {
            assert!(Browser::wide(window));
            assert_eq!(browser.document.as_ref().unwrap().uri, request.uri);
            cx.notify();
        });
        let document = cx.debug_bounds("browser-document-body").unwrap();
        assert!(document.size.width > px(1000.), "{document:?}");

        view.update_in(cx, |browser, window, cx| {
            let markdown = (0..90)
                .map(|line| format!("Paragraph {line}\n\n"))
                .collect::<String>();
            let media = browser_media::decode("pilot/long.md", markdown.as_bytes()).unwrap();
            browser.open_document(
                Document {
                    uri: "remuda:///pilot/long.md".into(),
                    media,
                    request: None,
                    #[cfg(any(target_os = "macos", windows))]
                    html: None,
                },
                true,
                window,
                cx,
            );
            cx.notify();
        });
        let reader = cx.debug_bounds("browser-document-scroll").unwrap();
        cx.simulate_event(gpui::ScrollEvent {
            position: reader.center(),
            delta: gpui::ScrollDelta::Lines(gpui::point(0., -6.)),
            modifiers: gpui::Modifiers::none(),
            touch_phase: gpui::TouchPhase::Moved,
            source: gpui::ScrollSource::Pointer,
        });
        assert!(cx.read(|app| view.read(app).document_scroll.offset().y < px(0.)));

        let folder = "remuda:///pilot/sub".to_owned();
        view.update_in(cx, |browser, window, cx| {
            browser.drive.as_mut().unwrap().listings.insert(
                None,
                DriveListing {
                    directory: Some("remuda:///pilot".into()),
                    entries: vec![remuda_native::net::browser::DriveEntry {
                        name: "sub".into(),
                        uri: folder.clone(),
                        is_directory: true,
                    }],
                    complete: true,
                    next: None,
                },
            );
            browser.select_drive(window, cx);
            cx.notify();
        });
        cx.simulate_keystrokes("down right");
        let (id, command) = drive_commands.try_recv().unwrap();
        assert!(matches!(command, DriveAction::List(Some(uri)) if uri == folder));
        send_drive_result
            .send((id, Err("Temporary failure".into())))
            .unwrap();
        view.update_in(cx, |browser, window, cx| browser.tick(window, cx));
        view.update(cx, |browser, cx| {
            browser.drive.as_mut().unwrap().pending = true;
            cx.notify();
        });
        let retry = cx.debug_bounds("drive-retry").unwrap();
        cx.simulate_click(retry.center(), gpui::Modifiers::none());
        assert!(drive_commands.try_recv().is_err());
        view.update_in(cx, |browser, window, cx| {
            browser.drive.as_mut().unwrap().pending = false;
            browser.drive_focus.focus(window, cx);
            cx.notify();
        });
        cx.simulate_keystrokes("tab enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        let (id, command) = drive_commands.try_recv().unwrap();
        assert!(drive_commands.try_recv().is_err());
        assert!(matches!(command, DriveAction::List(Some(uri)) if uri == folder));
        send_drive_result
            .send((
                id,
                Ok(DriveResult::Directory(
                    DriveListing {
                        directory: Some(folder.clone()),
                        entries: vec![remuda_native::net::browser::DriveEntry {
                            name: "plan.md".into(),
                            uri: format!("{folder}/plan.md"),
                            is_directory: false,
                        }],
                        complete: true,
                        next: None,
                    },
                    false,
                )),
            ))
            .ok()
            .unwrap();
        view.update_in(cx, |browser, window, cx| browser.tick(window, cx));
        cx.simulate_keystrokes("down");
        assert!(cx.read(|app| {
            let drive = view.read(app).drive.as_ref().unwrap();
            drive.rows().len() == 2
                && drive.selected.as_deref() == Some("remuda:///pilot/sub/plan.md")
        }));
        cx.simulate_keystrokes("left left");
        assert_eq!(
            cx.read(|app| view.read(app).drive.as_ref().unwrap().rows().len()),
            1
        );

        let image = image::DynamicImage::new_rgba8(32, 24);
        let mut png = std::io::Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).unwrap();
        view.update_in(cx, |browser, window, cx| {
            browser.open_document(
                Document {
                    uri: "remuda:///pilot/image.png".into(),
                    media: browser_media::decode("pilot/image.png", png.get_ref()).unwrap(),
                    request: None,
                    #[cfg(any(target_os = "macos", windows))]
                    html: None,
                },
                true,
                window,
                cx,
            );
            browser.document_focus.focus(window, cx);
            cx.notify();
        });
        let zoom = cx.debug_bounds("zoom-in").unwrap();
        cx.simulate_click(zoom.center(), gpui::Modifiers::none());
        assert_eq!(view.read_with(cx, |browser, _| browser.zoom), 1.25);
        view.update_in(cx, |browser, window, _| {
            assert!(browser.document_focus.is_focused(window))
        });
        cx.simulate_keystrokes("tab tab space");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("space").unwrap(),
        });
        assert_eq!(view.read_with(cx, |browser, _| browser.zoom), 1.);
        view.update_in(cx, |browser, window, cx| {
            assert!(browser.owns_document_focus(window, cx))
        });
        cx.simulate_keystrokes("tab enter");
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        assert!(cx.debug_bounds("browser-document-body").is_none());
        view.update_in(cx, |browser, window, _| {
            assert!(browser.focus.is_focused(window))
        });
    }
}
