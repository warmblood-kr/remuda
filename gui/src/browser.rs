use gpui::{
    div, prelude::*, px, rgb, App, Bounds, Context, FocusHandle, KeyDownEvent, Window,
    WindowBounds, WindowOptions,
};
use remuda_core::agent::{AgentProcess, Size};
use remuda_native::net::browser::{
    ssh_arguments, Client, DocumentRequest, Operation, Outcome, Reply,
};
use remuda_native::{CommandBuilder, PtyAgent};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Document {
    request: DocumentRequest,
    title: String,
    body: String,
}

struct Browser {
    documents: mpsc::Receiver<Document>,
    displayed: mpsc::Sender<DocumentRequest>,
    document: Option<Document>,
    pending_presentation: bool,
    document_active: bool,
    terminal: Option<PtyAgent>,
    terminal_text: String,
    focus: FocusHandle,
}

pub fn open(
    app: &mut App,
    bounds: Bounds<gpui::Pixels>,
    destination: String,
    client: Option<Client>,
) {
    let (send_document, documents) = mpsc::sync_channel(1);
    let (displayed, results) = mpsc::channel();
    if let Some(client) = client {
        std::thread::spawn(move || serve(&client, send_document, results));
    }
    let mut command = CommandBuilder::new("ssh");
    command.args(ssh_arguments(&destination));
    command.env("TERM", "xterm-256color");
    command.env_remove("REMUDA_BROWSER_CLIENT");
    let (terminal, terminal_text) = match PtyAgent::spawn(command, Size::new(100, 30)) {
        Ok(terminal) => (Some(terminal), String::new()),
        Err(error) => (None, format!("Could not connect to {destination}: {error}")),
    };
    app.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |window, app| {
            let focus = app.focus_handle();
            focus.focus(window);
            let window = window.window_handle();
            app.new(|cx: &mut Context<Browser>| {
                cx.spawn(async move |view, cx| loop {
                    gpui::Timer::after(Duration::from_millis(100)).await;
                    if window
                        .update(cx, |_, window, cx| {
                            view.update(cx, |browser, cx| {
                                if let Ok(document) = browser.documents.try_recv() {
                                    window.activate_window();
                                    browser.document = Some(document);
                                    browser.pending_presentation = true;
                                    browser.document_active = true;
                                    cx.notify();
                                }
                                if !browser.document_active {
                                    browser.refresh_terminal(cx);
                                }
                            })
                        })
                        .is_err()
                    {
                        break;
                    }
                })
                .detach();
                Browser {
                    documents,
                    displayed,
                    document: None,
                    pending_presentation: false,
                    document_active: false,
                    terminal,
                    terminal_text,
                    focus,
                }
            })
        },
    )
    .expect("open native Browser window");
    app.activate(true);
}

impl Browser {
    fn refresh_terminal(&mut self, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminal.as_mut() {
            if let Ok(screen) = terminal.screen_text() {
                if screen != self.terminal_text {
                    self.terminal_text = screen;
                    cx.notify();
                }
            }
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.document_active {
            return;
        }
        let key = &event.keystroke;
        let control;
        let input: &[u8] = match key.key.as_str() {
            "enter" => b"\r",
            "escape" => b"\x1b",
            "tab" => b"\t",
            "backspace" => b"\x7f",
            "up" => b"\x1b[A",
            "down" => b"\x1b[B",
            "right" => b"\x1b[C",
            "left" => b"\x1b[D",
            key_name
                if key.modifiers.control
                    && key_name.len() == 1
                    && key_name.as_bytes()[0].is_ascii_lowercase() =>
            {
                control = [key_name.as_bytes()[0] & 0x1f];
                &control
            }
            _ => key.key_char.as_deref().unwrap_or_default().as_bytes(),
        };
        if !input.is_empty() {
            if let Some(terminal) = self.terminal.as_mut() {
                if let Err(error) = terminal.write(input) {
                    self.terminal_text = error.to_string();
                    cx.notify();
                }
            }
        }
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
    client: &Client,
    documents: mpsc::SyncSender<Document>,
    results: mpsc::Receiver<DocumentRequest>,
) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let instance = format!("{}-{nonce}", std::process::id());
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
        match client.send(&poll) {
            Ok(Reply::Event(Some(request))) => {
                if current.as_ref() != Some(&request) {
                    current = Some(request.clone());
                    outcome = None;
                    if let Some(document) = read(client, request.clone()) {
                        match documents.try_send(document) {
                            Ok(()) => {}
                            Err(mpsc::TrySendError::Disconnected(_)) => return,
                            Err(mpsc::TrySendError::Full(_)) => outcome = Some(Outcome::ReadFailed),
                        }
                    } else {
                        outcome = Some(Outcome::ReadFailed);
                    }
                }
                if let Some(outcome) = outcome {
                    let _ = client.send(&Operation::Finish {
                        instance: instance.clone(),
                        request,
                        outcome,
                    });
                }
            }
            Ok(Reply::Event(None)) => {
                current = None;
                outcome = None;
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn read(client: &Client, request: DocumentRequest) -> Option<Document> {
    let Reply::Document(source) = client
        .send(&Operation::Read {
            uri: request.uri.clone(),
        })
        .ok()?
    else {
        return None;
    };
    let uri = remuda_core::vfs::VfsUri::parse(&request.uri).ok()?;
    let projected = remuda_native::document_markdown::parse(&uri.path, &source).ok()?;
    let title = if projected.title.is_empty() {
        uri.path.rsplit('/').next()?.to_owned()
    } else {
        projected.title
    };
    Some(Document {
        title,
        body: projected.lines.join("\n"),
        request,
    })
}

impl Render for Browser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = div()
            .flex()
            .gap_2()
            .p_2()
            .bg(rgb(0xe5e8e8))
            .child(
                div()
                    .id("terminal-tab")
                    .p_2()
                    .cursor_pointer()
                    .child("Terminal")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.document_active = false;
                        this.focus.focus(window);
                        cx.notify();
                    })),
            )
            .when_some(self.document.as_ref(), |tabs, document| {
                tabs.child(
                    div()
                        .id("document-tab")
                        .p_2()
                        .cursor_pointer()
                        .child(document.title.clone())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.document_active = true;
                            cx.notify();
                        })),
                )
            });
        let mut view = div()
            .flex()
            .flex_col()
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .bg(rgb(0xf7f7f5))
            .text_color(rgb(0x1c2327))
            .child(tabs);
        if let Some(document) = self.document.as_ref().filter(|_| self.document_active) {
            if self.pending_presentation {
                self.pending_presentation = false;
                let request = document.request.clone();
                let displayed = self.displayed.clone();
                window.on_next_frame(move |_, _| {
                    let _ = displayed.send(request);
                });
            }
            view = view.child(
                div()
                    .id("browser-document")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .p_6()
                    .max_w(px(820.))
                    .mx_auto()
                    .child(div().text_xl().pb_4().child(document.title.clone()))
                    .child(document.body.clone()),
            );
        } else {
            view = view.child(
                div()
                    .flex_1()
                    .p_3()
                    .bg(rgb(0x101216))
                    .text_color(rgb(0xe8eaf0))
                    .font_family(if cfg!(windows) { "Consolas" } else { "Menlo" })
                    .children(
                        self.terminal_text
                            .lines()
                            .map(|line| div().whitespace_nowrap().child(line.to_owned())),
                    ),
            );
        }
        view
    }
}
