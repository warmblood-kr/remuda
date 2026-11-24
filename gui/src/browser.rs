use gpui::{
    div, prelude::*, px, rgb, AnyElement, App, Bounds, Context, FocusHandle, KeyDownEvent, Window,
    WindowBounds, WindowOptions,
};
use remuda_core::agent::{AgentProcess, Size};
use remuda_native::net::browser::{
    ssh_arguments, Client, DocumentRequest, DriveListing, Operation, Outcome, Reply,
};
use remuda_native::{CommandBuilder, PtyAgent};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{
    cell::Cell,
    rc::Rc,
    sync::{mpsc, Arc},
};

use crate::browser_media;
use crate::terminal_view::{self, Screen, Viewport};

const SPLIT_WIDTH: f32 = 1120.;
const SIDEBAR_WIDTH: f32 = 208.;

struct Document {
    uri: String,
    media: browser_media::Document,
    request: Option<DocumentRequest>,
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
    ) -> Result<DriveResult, String> {
        let append = after.is_some();
        match self {
            Self::Remote(client) => list(
                client,
                match after {
                    Some(after) => Operation::ListMore { directory, after },
                    None => Operation::List { directory },
                },
                append,
            ),
            Self::Local => remuda_native::net::browser::list_local_drive(
                directory.as_deref(),
                after.as_deref(),
            )
            .map(|listing| DriveResult::Directory(listing, append))
            .map_err(|error| error.to_string()),
        }
    }
}

enum DriveAction {
    List(Option<String>),
    More(Option<String>, String),
    Open(String),
}

enum DriveResult {
    Directory(DriveListing, bool),
    Document(Document),
}

struct Drive {
    actions: mpsc::SyncSender<(u64, DriveAction)>,
    results: mpsc::Receiver<(u64, Result<DriveResult, String>)>,
    location: Vec<Option<String>>,
    listing: Option<DriveListing>,
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
                    DriveAction::List(directory) => source.list(directory, None),
                    DriveAction::More(directory, after) => source.list(directory, Some(after)),
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
            location: vec![None],
            listing: None,
            call: 0,
            pending: false,
            error: None,
        }
    }

    fn send(&mut self, action: DriveAction) {
        self.call = self.call.wrapping_add(1);
        self.pending = self.actions.try_send((self.call, action)).is_ok();
        self.error = (!self.pending).then(|| "Drive is busy. Try again.".into());
    }

    fn list(&mut self) {
        self.listing = None;
        let directory = self.location.last().cloned().flatten();
        self.send(DriveAction::List(directory));
    }
}

fn list(client: &Client, operation: Operation, append: bool) -> Result<DriveResult, String> {
    match client.send(&operation) {
        Ok(Reply::Directory(listing)) => Ok(DriveResult::Directory(listing, append)),
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
    panel_bounds: Rc<Cell<Option<Bounds<gpui::Pixels>>>>,
    viewport: Viewport,
    focus: FocusHandle,
    document_focus: FocusHandle,
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
            #[cfg(target_os = "macos")]
            crate::macos_lifecycle::install_window(
                window,
                app,
                crate::macos_lifecycle::WindowKind::Browser,
            );
            let focus = app.focus_handle();
            let document_focus = app.focus_handle();
            focus.focus(window);
            let handle = window.window_handle();
            app.new(|cx: &mut Context<Browser>| {
                cx.observe_window_activation(window, |_, _, cx| cx.notify())
                    .detach();
                cx.spawn(async move |view, cx| loop {
                    gpui::Timer::after(Duration::from_millis(100)).await;
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
                    false,
                    client,
                    terminal,
                    screen,
                    focus,
                    document_focus,
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
        embedded: bool,
        client: Option<Client>,
        terminal: Option<PtyAgent>,
        screen: Screen,
        focus: FocusHandle,
        document_focus: FocusHandle,
    ) -> Self {
        let (send_document, documents) = mpsc::sync_channel(1);
        let (displayed, results) = mpsc::channel();
        let remote = client.map(Arc::new);
        if let Some(poller) = remote.clone() {
            std::thread::spawn(move || serve(poller, send_document, results));
        }
        let source = remote
            .map(DriveSource::Remote)
            .or_else(|| embedded.then_some(DriveSource::Local))
            .map(Arc::new);
        let drive = source.clone().map(Drive::new);
        let assets = source.map(Assets::new);
        Self {
            destination,
            embedded,
            documents,
            displayed,
            document: None,
            pending_presentation: None,
            painted: Default::default(),
            active: Content::Terminal,
            show_drive: false,
            drive,
            assets,
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
            panel_bounds: Default::default(),
            viewport: Default::default(),
            focus,
            document_focus,
        }
    }

    pub(crate) fn local(client: Option<Client>, focus: FocusHandle, cx: &mut App) -> Self {
        Self::new(
            String::new(),
            true,
            client,
            None,
            Screen::default(),
            focus,
            cx.focus_handle(),
        )
    }

    #[cfg(test)]
    pub(crate) fn seed_test_document(&mut self, window: &mut Window) {
        let uri = "remuda:///pilot/plan.md".to_owned();
        let media = browser_media::decode("pilot/plan.md", b"# Design plan").unwrap();
        self.open_document(
            Document {
                uri,
                media,
                request: None,
            },
            true,
            window,
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
    pub(crate) fn owns_document_focus(&self, window: &Window) -> bool {
        self.document_focus.is_focused(window)
    }
    pub(crate) fn select_terminal(&mut self) {
        self.active = Content::Terminal;
    }
    pub(crate) fn select_document(&mut self, window: &mut Window) {
        self.active = Content::Document;
        self.document_focus.focus(window);
    }
    pub(crate) fn select_drive(&mut self) {
        self.show_drive = true;
        if let Some(drive) = &mut self.drive {
            if drive.listing.is_none() && !drive.pending {
                drive.list();
            }
        }
    }

    pub(crate) fn wide(window: &Window) -> bool {
        f32::from(window.viewport_size().width) >= SPLIT_WIDTH
    }

    pub(crate) fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.painted.get() && self.pending_presentation.take().is_some() {
            cx.notify();
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
            self.open_document(document, !typing, window);
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
                        if append {
                            if let Some(previous) = drive.listing.take() {
                                let mut rows = previous.entries;
                                rows.append(&mut listing.entries);
                                listing.entries = rows;
                            }
                        }
                        drive.listing = Some(listing);
                    }
                    Ok(DriveResult::Document(document)) => {
                        self.open_document(document, true, window);
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

    fn open_document(&mut self, document: Document, focus: bool, window: &mut Window) {
        if self
            .document
            .as_ref()
            .is_none_or(|current| current.uri != document.uri)
        {
            self.zoom = 1.;
            self.document_scroll.set_offset(gpui::point(px(0.), px(0.)));
        }
        if let Some(request) = document.request.clone() {
            self.pending_presentation = Some(request);
            self.painted = Default::default();
        }
        self.document_generation = self.document_generation.wrapping_add(1);
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
                self.document_focus.focus(window);
            }
        }
    }

    fn close_document(&mut self, window: &mut Window) {
        if let Some(document) = self.document.take() {
            document.media.release_images(window);
        }
        self.document_generation = self.document_generation.wrapping_add(1);
        self.pending_presentation = None;
        self.active = Content::Terminal;
        self.focus.focus(window);
    }

    fn refresh_terminal(&mut self, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminal.as_mut() {
            if let Some(size) = self.viewport.get() {
                if terminal.size() != size && terminal.is_alive() {
                    let _ = terminal.resize(size);
                }
            }
            if let Ok(screen) = terminal
                .screen_cells_at(self.terminal_scroll.offset)
                .map_err(|e| e.to_string())
                .and_then(Screen::from_cells)
            {
                if screen != self.screen {
                    self.screen = screen;
                    cx.notify();
                }
            }
        }
    }

    fn scroll_terminal(&mut self, event: &gpui::ScrollWheelEvent, cx: &mut Context<Self>) {
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
            if directory {
                self.drive_scroll.set_offset(gpui::point(px(0.), px(0.)));
                drive.location.push(Some(uri));
                drive.list();
            } else {
                drive.send(DriveAction::Open(uri));
            }
        }
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let switcher = div()
            .flex()
            .gap_1()
            .p_2()
            .child(
                button("sessions-mode", "Sessions", !self.show_drive).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.show_drive = false;
                        cx.notify();
                    },
                )),
            )
            .child(
                button("drive-mode", "Drive", self.show_drive).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.show_drive = true;
                        if let Some(drive) = &mut this.drive {
                            if drive.listing.is_none() && !drive.pending {
                                drive.list();
                            }
                        }
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
                            this.focus.focus(window);
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
        self.drive
            .as_ref()
            .map(|drive| {
                let path = drive
                    .listing
                    .as_ref()
                    .and_then(|list| list.directory.clone());
                div()
                    .id("drive-entries")
                    .track_scroll(&self.drive_scroll)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_2()
                    .when(drive.location.len() > 1, |d| {
                        d.child(
                            button("drive-up", "↑ Parent folder", false).on_click(cx.listener(
                                |this, _, _, cx| {
                                    if let Some(drive) = &mut this.drive {
                                        drive.location.pop();
                                        drive.list();
                                    }
                                    cx.notify();
                                },
                            )),
                        )
                    })
                    .when_some(path, |d, path| {
                        d.child(div().text_xs().truncate().child(path))
                    })
                    .when(drive.pending, |d| d.child(div().p_2().child("Loading…")))
                    .when_some(drive.error.clone(), |d, error| {
                        d.child(div().p_2().child(error).child(
                            button("drive-retry", "Reload folder", false).on_click(cx.listener(
                                |this, _, _, cx| {
                                    if let Some(drive) = &mut this.drive {
                                        drive.list();
                                    }
                                    cx.notify();
                                },
                            )),
                        ))
                    })
                    .when_some(
                        drive.listing.as_ref().filter(|_| !drive.pending),
                        |d, listing| {
                            d.children(listing.entries.iter().map(|entry| {
                                let uri = entry.uri.clone();
                                let is_directory = entry.is_directory;
                                let label = if is_directory {
                                    format!("▸ {}", entry.name)
                                } else {
                                    entry.name.clone()
                                };
                                button(
                                    gpui::SharedString::from(format!("drive:{uri}")),
                                    label,
                                    false,
                                )
                                .truncate()
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.visit(uri.clone(), is_directory);
                                        cx.notify();
                                    },
                                ))
                            }))
                            .when(listing.entries.is_empty(), |d| {
                                d.child(div().p_2().child("No documents"))
                            })
                            .when(!listing.complete, |d| {
                                d.child(button("drive-more", "Load more", false).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        if let Some(drive) = &mut this.drive {
                                            let next = drive
                                                .listing
                                                .as_ref()
                                                .and_then(|listing| listing.next.clone());
                                            if let Some(after) = next {
                                                let directory =
                                                    drive.location.last().cloned().flatten();
                                                drive.send(DriveAction::More(directory, after));
                                            }
                                        }
                                        cx.notify();
                                    }),
                                ))
                            })
                        },
                    )
                    .into_any_element()
            })
            .unwrap_or_else(|| {
                div()
                    .p_3()
                    .child("Drive connection is not configured.")
                    .into_any_element()
            })
    }

    fn terminal_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let panel_bounds = self.panel_bounds.clone();
        div()
            .id("browser-terminal")
            .debug_selector(|| "browser-terminal".into())
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_click(cx.listener(|this, _, window, cx| {
                this.active = Content::Terminal;
                this.focus.focus(window);
                cx.notify();
            }))
            .on_key_down(cx.listener(Self::key_down))
            .on_scroll_wheel(cx.listener(|this, event, _, cx| this.scroll_terminal(event, cx)))
            .child(
                gpui::canvas(
                    move |bounds, _, _| panel_bounds.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(terminal_view::measure(self.viewport.clone()))
            .child(self.screen.view())
            .into_any_element()
    }

    pub(crate) fn document_panel(&mut self, pane_width: f32, cx: &mut Context<Self>) -> AnyElement {
        let Some(document) = self.document.as_ref() else {
            return div()
                .flex_1()
                .p_6()
                .child("Open a document from Drive")
                .into_any_element();
        };
        let presentation = self
            .pending_presentation
            .clone()
            .filter(|request| request.uri == document.uri && !self.painted.get())
            .map(|request| {
                let painted = self.painted.clone();
                let displayed = self.displayed.clone();
                gpui::canvas(
                    |_, _, _| (),
                    move |bounds, _, _, _| {
                        if bounds.size.width > px(0.)
                            && bounds.size.height > px(0.)
                            && !painted.replace(true)
                        {
                            let _ = displayed.send(request);
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
                        d.child(button("zoom-in", "+", false).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.zoom = (this.zoom * 1.25).min(8.);
                                cx.notify();
                            },
                        )))
                        .child(
                            button("zoom-fit", "Fit", false).on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.zoom = 1.;
                                    cx.notify();
                                },
                            )),
                        )
                    })
                    .child(
                        button("close-document", "Close", false).on_click(cx.listener(
                            |this, _, window, cx| {
                                this.close_document(window);
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(
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
    Ok(Document {
        uri,
        media,
        request,
    })
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
        let wide = Self::wide(window) && self.document.is_some();
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
            .when(!wide, |d| {
                d.child(
                    div()
                        .flex()
                        .gap_2()
                        .p_2()
                        .bg(rgb(0xe5e8e8))
                        .child(
                            button("terminal-tab", "Terminal", self.active == Content::Terminal)
                                .debug_selector(|| "browser-terminal-tab".into())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.active = Content::Terminal;
                                    this.focus.focus(window);
                                    cx.notify();
                                })),
                        )
                        .child(
                            button(
                                "document-tab",
                                if has_unseen {
                                    "Document •"
                                } else {
                                    "Document"
                                },
                                self.active == Content::Document,
                            )
                            .debug_selector(|| "browser-document-tab".into())
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.active = Content::Document;
                                    this.document_focus.focus(window);
                                    cx.notify();
                                },
                            )),
                        ),
                )
            });
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
            .child(self.sidebar(cx))
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            focus.focus(window);
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
                    location: vec![None],
                    listing: None,
                    call: 0,
                    pending: false,
                    error: None,
                }),
                assets: None,
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
                panel_bounds: Default::default(),
                viewport: Default::default(),
                focus,
                document_focus: cx.focus_handle(),
            }
        });
        cx.simulate_resize(gpui::size(px(800.), px(600.)));
        assert!(cx.debug_bounds("browser-terminal-tab").is_some());
        assert!(cx.debug_bounds("browser-document-tab").is_some());
        assert!(cx.debug_bounds("browser-document-body").is_none());

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
        });
        assert!(
            receipt.try_recv().is_err(),
            "hidden content cannot report displayed"
        );

        view.update_in(cx, |browser, window, cx| {
            browser.show_drive = true;
            browser.visit(request.uri.clone(), false);
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

        cx.simulate_resize(gpui::size(px(1420.), px(720.)));
        view.update_in(cx, |browser, window, cx| {
            assert!(Browser::wide(window));
            assert_eq!(browser.document.as_ref().unwrap().uri, request.uri);
            cx.notify();
        });
        let terminal = cx.debug_bounds("browser-terminal").unwrap();
        let doc = cx.debug_bounds("browser-document-body").unwrap();
        assert!(
            terminal.right() <= doc.left(),
            "{terminal:?} overlaps {doc:?}"
        );
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
                },
                true,
                window,
            );
            cx.notify();
        });
        let reader = cx.debug_bounds("browser-document-scroll").unwrap();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: reader.center(),
            delta: gpui::ScrollDelta::Lines(gpui::point(0., -6.)),
            modifiers: gpui::Modifiers::none(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        assert!(cx.read(|app| view.read(app).document_scroll.offset().y < px(0.)));
    }
}
