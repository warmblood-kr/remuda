use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, Context, FocusHandle, KeyDownEvent,
    Window, WindowBounds, WindowOptions,
};
use remuda_native::net::browser::valid_ssh_destination;
use std::{path::Path, process::Command};

pub(super) fn open() {
    Application::new().run(move |app: &mut App| {
        let bounds = Bounds::centered(None, size(px(440.), px(175.)), app);
        let opened = app.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                is_resizable: false,
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title("Open Remuda Browser");
                let focus = cx.focus_handle();
                focus.focus(window);
                cx.new(|_| Picker {
                    host: String::new(),
                    replace: false,
                    error: None,
                    focus,
                })
            },
        );
        if let Err(error) = opened {
            eprintln!("remuda tray: cannot open the Browser host prompt: {error}");
            app.quit();
            return;
        }
        app.on_window_closed(|app| app.quit()).detach();
        app.activate(true);
    });
}

struct Picker {
    host: String,
    replace: bool,
    error: Option<String>,
    focus: FocusHandle,
}

fn browser_command(gui: &Path, host: &str) -> Option<Command> {
    if !valid_ssh_destination(host) {
        return None;
    }
    let mut command = Command::new(gui);
    command.args(["--browser", host]);
    Some(command)
}

impl Picker {
    fn submit(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let gui = match std::env::current_exe() {
            Ok(gui) => gui,
            Err(error) => {
                self.error = Some(format!("Cannot locate Browser: {error}"));
                cx.notify();
                return;
            }
        };
        match browser_command(&gui, &self.host) {
            None => self.error = Some("Enter a hostname, IP address, or SSH alias.".into()),
            Some(mut command) => match command.spawn() {
                Ok(_) => cx.quit(),
                Err(error) => self.error = Some(format!("Cannot start Browser: {error}")),
            },
        }
        cx.notify();
    }

    fn append(&mut self, text: &str) {
        if self.replace {
            self.host.clear();
            self.replace = false;
        }
        let remaining = 255usize.saturating_sub(self.host.len());
        self.host.extend(
            text.chars()
                .filter(|ch| ch.is_ascii_graphic())
                .take(remaining),
        );
    }

    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        let shortcut = if cfg!(target_os = "macos") {
            key.modifiers.platform
        } else {
            key.modifiers.control
        };
        self.error = None;
        match key.key.as_str() {
            "escape" => cx.quit(),
            "enter" => self.submit(window, cx),
            "a" if shortcut => self.replace = true,
            "v" if shortcut => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.append(text.trim());
                }
            }
            "backspace" if self.replace => {
                self.host.clear();
                self.replace = false;
            }
            "backspace" => {
                self.host.pop();
            }
            _ if !shortcut => {
                if let Some(text) = &key.key_char {
                    self.append(text);
                }
            }
            _ => {}
        }
        cx.stop_propagation();
        cx.notify();
    }
}

#[cfg(test)]
#[test]
fn browser_picker_uses_the_installed_gui_and_one_validated_host() {
    let gui = Path::new("/installed/remuda-gui");
    let command = browser_command(gui, "alice@studio").unwrap();
    assert_eq!(command.get_program(), gui.as_os_str());
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["--browser", "alice@studio"]
    );
    for host in ["", "-F", "studio other", "studio;other"] {
        assert!(browser_command(gui, host).is_none());
    }
}

impl Render for Picker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("browser-host-picker")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            .size_full()
            .p_4()
            .bg(rgb(0x191d24))
            .text_color(rgb(0xe8eaf0))
            .when(cfg!(target_os = "macos"), |root| root.font_family("Menlo"))
            .child("SSH host or alias")
            .child(
                div()
                    .my_2()
                    .px_2()
                    .py_1()
                    .border_1()
                    .border_color(rgb(0x799ebe))
                    .bg(rgb(if self.replace { 0x233e57 } else { 0x101216 }))
                    .truncate()
                    .child(if self.host.is_empty() {
                        "user@host".into()
                    } else {
                        self.host.clone()
                    }),
            )
            .child(
                div()
                    .id("connect-browser")
                    .px_2()
                    .py_1()
                    .bg(rgb(0x233e57))
                    .cursor_pointer()
                    .child("Connect")
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            )
            .when_some(self.error.clone(), |d, error| {
                d.child(
                    div()
                        .pt_1()
                        .text_sm()
                        .text_color(rgb(0xffad9f))
                        .child(error),
                )
            })
    }
}
