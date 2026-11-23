use gpui::{div, prelude::*, px, rgb, App, Bounds, Context, Window, WindowBounds, WindowOptions};
use remuda_native::net::browser::{Client, DocumentRequest, Operation, Outcome, Reply};
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
}

pub fn open(app: &mut App, bounds: Bounds<gpui::Pixels>, client: Client) {
    let (send_document, documents) = mpsc::sync_channel(1);
    let (displayed, results) = mpsc::channel();
    std::thread::spawn(move || serve(&client, send_document, results));
    app.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |window, app| {
            let window = window.window_handle();
            app.new(|cx: &mut Context<Browser>| {
                cx.spawn(async move |view, cx| loop {
                    gpui::Timer::after(Duration::from_millis(100)).await;
                    if window
                        .update(cx, |_, window, cx| {
                            view.update(cx, |browser, cx| {
                                if let Ok(document) = browser.documents.try_recv() {
                                    window.set_window_title(&document.title);
                                    window.activate_window();
                                    window.refresh();
                                    browser.document = Some(document);
                                    browser.pending_presentation = true;
                                    cx.notify();
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
                }
            })
        },
    )
    .expect("open native Browser window");
    app.activate(true);
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
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let mut view = div()
            .size_full()
            .bg(rgb(0xf7f7f5))
            .text_color(rgb(0x1c2327));
        if let Some(document) = &self.document {
            if self.pending_presentation {
                self.pending_presentation = false;
                let request = document.request.clone();
                let displayed = self.displayed.clone();
                window.on_next_frame(move |_, _| {
                    let _ = displayed.send(request);
                });
            }
            view = view
                .child(div().bg(rgb(0xe5e8e8)).p_3().child(document.title.clone()))
                .child(
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
            view = view.p_6().child("Waiting for a document from Butler…");
        }
        view
    }
}
