use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, Context, Window, WindowBounds,
    WindowOptions,
};
use serde::{Deserialize, Serialize};
use std::{sync::mpsc, time::Duration};

mod provider;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Session {
    name: String,
    alive: bool,
    unavailable: bool,
}

// This worker makes only a List request. Its own deadline survives GUI exit,
// including when native IPC setup stalls before the client's read timeout.
fn worker() -> i32 {
    std::thread::spawn(|| {
        std::thread::sleep(provider::TIMEOUT);
        std::process::exit(124);
    });
    let path = remuda_native::daemon::socket_path("default");
    let rows = match remuda_native::client::request(&path, &remuda_core::protocol::Request::List) {
        Ok(remuda_core::protocol::Response::Sessions(rows)) => rows,
        _ => return 1,
    };
    let sessions: Vec<_> = rows
        .into_iter()
        .map(|s| Session {
            name: s.name,
            alive: s.alive,
            unavailable: s.unavailable,
        })
        .collect();
    i32::from(serde_json::to_writer(std::io::stdout(), &sessions).is_err())
}

fn label(value: &str) -> String {
    let mut text = String::from("\"");
    for ch in value.chars() {
        if ch == '\\' || ch == '"' {
            text.push('\\');
            text.push(ch);
        } else if ch.is_control() || remuda_core::protocol::is_secret_prompt_format_or_separator(ch)
        {
            text.push_str(&format!("\\u{{{:x}}}", ch as u32));
        } else {
            text.push(ch);
        }
    }
    text.push('"');
    text
}

struct LocalGui {
    sessions: Vec<Session>,
    error: bool,
    loading: bool,
    generation: u64,
    busy: bool,
    pending: bool,
    sender: mpsc::Sender<(u64, Result<Vec<Session>, String>)>,
    results: mpsc::Receiver<(u64, Result<Vec<Session>, String>)>,
}
impl LocalGui {
    fn new() -> Self {
        let (sender, results) = mpsc::channel();
        Self {
            sessions: vec![],
            error: false,
            loading: false,
            generation: 0,
            busy: false,
            pending: false,
            sender,
            results,
        }
    }
    fn refresh(&mut self) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("generation exhausted");
        self.loading = true;
        self.pending = true;
        self.launch();
    }
    fn launch(&mut self) {
        if self.busy || !self.pending {
            return;
        }
        self.busy = true;
        self.pending = false;
        let generation = self.generation;
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            let result = std::env::current_exe()
                .map_err(|e| e.to_string())
                .and_then(|path| {
                    provider::output(&path, &["--internal-list-worker"], provider::TIMEOUT)
                })
                .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()));
            let _ = sender.send((generation, result));
        });
    }
    fn drain(&mut self) {
        while let Ok((generation, result)) = self.results.try_recv() {
            self.busy = false;
            if generation == self.generation {
                self.loading = false;
                match result {
                    Ok(rows) => {
                        self.sessions = rows;
                        self.error = false;
                    }
                    Err(_) => self.error = true,
                }
            }
            self.launch();
        }
    }
}
impl Render for LocalGui {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.loading {
            "LOCAL SESSIONS · LOADING"
        } else if self.error {
            "LOCAL SESSIONS · STALE"
        } else {
            "LOCAL SESSIONS"
        };
        div()
            .size_full()
            .flex()
            .bg(rgb(0x101216))
            .text_color(rgb(0xe8eaf0))
            .font_family("Menlo")
            .child(
                div()
                    .w(px(300.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(rgb(0x191d24))
                    .p_4()
                    .child(div().text_lg().child("REMUDA"))
                    .child(div().pt_4().text_sm().child(title))
                    .children(self.sessions.clone().into_iter().map(|session| {
                        let status = if session.unavailable {
                            "unavailable"
                        } else if session.alive {
                            "running"
                        } else {
                            "exited"
                        };
                        div()
                            .py_2()
                            .child(div().child(label(&session.name)))
                            .child(div().text_sm().text_color(rgb(0x8a95a5)).child(status))
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .p_4()
                    .child(div().child("Read-only local session list"))
                    .when(self.error, |d| {
                        d.child(
                            div()
                                .text_color(rgb(0xf28787))
                                .child("Local daemon unavailable; listed sessions may be stale."),
                        )
                    })
                    .child(
                        div()
                            .id("refresh")
                            .debug_selector(|| "refresh".into())
                            .pt_4()
                            .cursor_pointer()
                            .child("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh();
                                cx.notify();
                            })),
                    ),
            )
    }
}
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--internal-list-worker") {
        std::process::exit(worker());
    }
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1000.), px(650.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|cx| {
                    let mut gui = LocalGui::new();
                    gui.refresh();
                    cx.spawn(async move |weak, cx| loop {
                        gpui::Timer::after(Duration::from_millis(150)).await;
                        if weak
                            .update(cx, |this: &mut LocalGui, cx| {
                                this.drain();
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                    })
                    .detach();
                    gui
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(name: &str) -> Session {
        Session {
            name: name.into(),
            alive: true,
            unavailable: false,
        }
    }
    #[test]
    fn stale_reply_is_ignored_and_failure_preserves_old_rows() {
        let mut gui = LocalGui::new();
        gui.sessions = vec![row("old")];
        gui.busy = true;
        gui.refresh();
        let old = gui.generation;
        gui.refresh();
        let latest = gui.generation;
        gui.pending = false;
        gui.sender.send((old, Ok(vec![row("wrong")]))).unwrap();
        gui.drain();
        assert_eq!(gui.sessions, vec![row("old")]);
        assert!(gui.loading);
        gui.busy = true;
        gui.sender.send((latest, Err("offline".into()))).unwrap();
        gui.drain();
        assert!(gui.error);
        assert!(!gui.loading);
        assert_eq!(gui.sessions, vec![row("old")]);
        gui.busy = true;
        gui.refresh();
        let current = gui.generation;
        gui.pending = false;
        gui.sender.send((current, Ok(vec![]))).unwrap();
        gui.drain();
        assert!(!gui.error);
        assert!(gui.sessions.is_empty());
    }
    #[test]
    fn labels_escape_controls_and_literal_escapes() {
        assert_ne!(label("\u{1b}"), label("\\u{1b}"));
        assert!(label("a\u{202e}b").contains("\\u{202e}"));
        assert_eq!(label("한글"), "\"한글\"");
    }
    #[gpui::test]
    fn refresh_click_starts_new_generation(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| {
            let mut gui = LocalGui::new();
            gui.busy = true;
            gui.sessions = vec![row("local")];
            gui
        });
        let button = cx.debug_bounds("refresh").expect("refresh rendered");
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        assert!(cx.read(|app| view.read(app).loading));
        assert_eq!(cx.read(|app| view.read(app).generation), 1);
        assert_eq!(
            cx.read(|app| view.read(app).sessions[0].name.clone()),
            "local"
        );
    }
}
