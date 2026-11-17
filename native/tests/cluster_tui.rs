//! End-to-end coverage for the cluster tree's read-only local IPC path.

#[path = "daemon_support/hosts.rs"]
mod hosts;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::cluster_tui::composer::{ComposerAction, LineComposer};
use remuda_native::cluster_tui::queue::{InputQueue, QueueEvent, QueueState, MAX_IO_RETRIES};
use remuda_native::cluster_tui::sender::InputSender;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_DAEMON: AtomicU64 = AtomicU64::new(0);

struct PrivateDaemon {
    child: Child,
    root: PathBuf,
    runtime: PathBuf,
    stopped: bool,
}

impl PrivateDaemon {
    fn start() -> Self {
        Self::start_with_init(None)
    }

    fn start_with_init(init: Option<&str>) -> Self {
        let root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let runtime = root.join(format!(
            "cluster-tree-{}-{}",
            std::process::id(),
            NEXT_DAEMON.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&runtime);
        std::fs::create_dir_all(&runtime).unwrap();
        let home = runtime.join("home");
        std::fs::create_dir_all(&home).unwrap();
        if let Some(init) = init {
            let config = runtime.join("config").join("remuda");
            std::fs::create_dir_all(&config).unwrap();
            std::fs::write(config.join("init.lua"), init).unwrap();
        }
        let child = Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "cluster-tree", "daemon"])
            .env("REMUDA_RUNTIME_DIR", &runtime)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", runtime.join("config"))
            .env("XDG_DATA_HOME", runtime.join("data"))
            .env("XDG_CACHE_HOME", runtime.join("cache"))
            .env("XDG_STATE_HOME", runtime.join("state"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self {
            child,
            root,
            runtime,
            stopped: false,
        }
    }

    fn path(&self) -> PathBuf {
        remuda_native::daemon::socket_path_in(&self.runtime, "cluster-tree")
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while remuda_native::client::request(&self.path(), &Request::List).is_err() {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("private daemon exited before binding: {status}");
            }
            assert!(Instant::now() < deadline, "private daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    fn stop(&mut self) {
        assert_eq!(
            unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGSTOP) },
            0
        );
        self.stopped = true;
    }

    #[cfg(unix)]
    fn resume(&mut self) {
        assert_eq!(
            unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGCONT) },
            0
        );
        self.stopped = false;
    }
}

impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        assert!(
            self.runtime.starts_with(&self.root),
            "daemon runtime must be scratch"
        );
        if self.stopped {
            #[cfg(unix)]
            unsafe {
                libc::kill(self.child.id() as libc::pid_t, libc::SIGCONT);
            }
            self.stopped = false;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        hosts::stop_hosts_under(&self.runtime);
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

#[cfg(unix)]
struct TuiPty {
    master: Box<dyn portable_pty::MasterPty + Send>,
    rows: u16,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    pid: u32,
    writer: Box<dyn std::io::Write + Send>,
    output: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl TuiPty {
    fn start(runtime: &std::path::Path) -> Self {
        Self::start_document_target(runtime, None)
    }

    fn start_document_target(runtime: &std::path::Path, target: Option<&str>) -> Self {
        Self::start_document_target_rows(runtime, target, 24)
    }

    fn start_document_target_rows(
        runtime: &std::path::Path,
        target: Option<&str>,
        rows: u16,
    ) -> Self {
        use std::io::Read;
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_remuda"));
        command.args(["-s", "cluster-tree", "cluster", "remote"]);
        command.env("REMUDA_RUNTIME_DIR", runtime);
        command.env("HOME", runtime.join("home"));
        command.env("XDG_CONFIG_HOME", runtime.join("config"));
        command.env("XDG_DATA_HOME", runtime.join("data"));
        command.env("XDG_CACHE_HOME", runtime.join("cache"));
        command.env("XDG_STATE_HOME", runtime.join("state"));
        if let Some(target) = target {
            command.env("REMUDA_DOCUMENT_TARGET", target);
        }
        let child = pty.slave.spawn_command(command).expect("spawn cluster TUI");
        let pid = child.process_id().expect("cluster TUI child PID");
        drop(pty.slave);
        let writer = pty.master.take_writer().unwrap();
        let mut reader = pty.master.try_clone_reader().unwrap();
        let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let reader_output = output.clone();
        let reader = std::thread::spawn(move || {
            let mut buffer = [0; 2048];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                reader_output
                    .lock()
                    .unwrap()
                    .extend_from_slice(&buffer[..count]);
            }
        });
        Self {
            master: pty.master,
            rows,
            child,
            pid,
            writer,
            output,
            reader: Some(reader),
        }
    }

    fn screen_text(&self) -> String {
        let output = self.output.lock().unwrap();
        let mut parser = vt100::Parser::new(self.rows, 100, 0);
        parser.process(&output);
        parser.screen().contents()
    }

    fn output_len(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    fn resize_rows(&mut self, rows: u16) {
        self.master
            .resize(portable_pty::PtySize {
                rows,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        self.rows = rows;
    }

    fn screens_from(&self, offset: usize) -> (String, String) {
        let output = self.output.lock().unwrap();
        let offset = offset.min(output.len());
        let mut parser = vt100::Parser::new(24, 100, 0);
        parser.process(&output[..offset]);
        let before = parser.screen().contents();
        parser.process(&output[offset..]);
        let after = parser.screen().contents();
        (before, after)
    }

    fn wait_for(&self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let output = self.screen_text();
            if output.contains(needle) {
                return output;
            }
            assert!(
                Instant::now() < deadline,
                "TUI did not render {needle:?}: {output}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(unix)]
impl Drop for TuiPty {
    fn drop(&mut self) {
        assert_eq!(self.child.process_id(), Some(self.pid));
        unsafe {
            libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
        }
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(unix)]
#[test]
fn stopped_daemon_input_becomes_uncertain_and_later_input_recovers() {
    use std::io::Write;

    let mut daemon = PrivateDaemon::start();
    daemon.wait_ready();
    let response = remuda_native::client::request(
        &daemon.path(),
        &Request::New {
            name: Some("tui-session".into()),
            command: vec![
                "sh".into(),
                "-c".into(),
                "stty -echo; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done".into(),
            ],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .unwrap();
    assert!(matches!(response, Response::Value(name) if name == "tui-session"));
    let instance_id = match remuda_native::client::request(&daemon.path(), &Request::List).unwrap()
    {
        Response::Sessions(sessions) => sessions
            .into_iter()
            .find(|session| session.name == "tui-session")
            .and_then(|session| session.instance_id)
            .expect("the session instance id is listed"),
        other => panic!("unexpected List response: {other:?}"),
    };

    let mut tui = TuiPty::start(&daemon.runtime);
    tui.wait_for("tui-session", Duration::from_secs(5));
    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("Remuda > ", Duration::from_secs(3));
    tui.writer.write_all(b"first line").unwrap();
    tui.wait_for("first line", Duration::from_secs(5));

    daemon.stop();
    let redraw_start = tui.output_len();
    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("sending · first line", Duration::from_secs(3));
    let started = Instant::now();
    // InputLine gets an eight-second response budget and three bounded retries.
    let uncertain_bound = Duration::from_secs(36);
    let deadline = started + uncertain_bound;
    let mut progress_screens = std::collections::HashSet::new();
    let screen_after = loop {
        let screen = tui.screen_text();
        if screen.contains("delivery uncertain") {
            break screen;
        }
        if screen.contains("sending ·") {
            progress_screens.insert(screen.clone());
        }
        assert!(
            Instant::now() < deadline,
            "uncertain state was not rendered within 36s while daemon was SIGSTOPped; screen:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    let (screen_before, _) = tui.screens_from(redraw_start);
    assert!(
        started.elapsed() <= uncertain_bound && !screen_before.contains("delivery uncertain"),
        "uncertain state was already visible before the retry: screen at offset:\n{screen_before}\ncurrent screen:\n{screen_after}"
    );
    assert!(
        !progress_screens.is_empty(),
        "TUI did not render the pending line before its response became uncertain; saw {} progress snapshots:\n{}",
        progress_screens.len(),
        progress_screens.into_iter().collect::<Vec<_>>().join("\n---\n")
    );

    daemon.resume();
    tui.writer.write_all(b"second line\r").unwrap();
    tui.wait_for("got:second line", Duration::from_secs(15));
    let close = remuda_native::client::request(
        &daemon.path(),
        &Request::Close {
            name: "tui-session".into(),
            instance_id: Some(instance_id),
            confirm: Some(true),
        },
    )
    .unwrap();
    assert!(matches!(close, Response::Ok));
}

#[cfg(unix)]
#[test]
fn wheel_over_the_content_scrolls_the_session_instead_of_line_history() {
    use std::io::Write;
    let mut daemon = PrivateDaemon::start();
    daemon.wait_ready();
    let response = remuda_native::client::request(
        &daemon.path(),
        &Request::New {
            name: Some("wheel-session".into()),
            command: vec!["sh".into(), "-c".into(), "seq 1 200; sleep 60".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .unwrap();
    assert!(matches!(response, Response::Value(name) if name == "wheel-session"));

    let mut tui = TuiPty::start(&daemon.runtime);
    tui.wait_for("wheel-session", Duration::from_secs(5));
    // Without the capture request the alternate screen turns the wheel into
    // Up/Down, which is the reported defect.
    let captured = {
        let output = tui.output.lock().unwrap();
        output
            .windows(b"\x1b[?1000h".len())
            .any(|window| window == b"\x1b[?1000h")
    };
    assert!(captured, "the cluster TUI did not enable mouse capture");

    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("Remuda > ", Duration::from_secs(3));
    // `seq 1 200` fills the screen and leaves retained history behind it.
    tui.wait_for("19", Duration::from_secs(5));
    // Submit a line so Up has history to recall; the shell ignores input, so
    // the text can only reappear through the composer.
    tui.writer.write_all(b"draft line\r").unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let composer = tui
            .screen_text()
            .lines()
            .find(|line| line.contains("Remuda > "))
            .unwrap_or("")
            .to_owned();
        if !composer.contains("draft line") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Wheel up with the pointer over the content pane (right of the divider).
    tui.writer.write_all(b"\x1b[<64;60;10M").unwrap();
    let screen = tui.wait_for("scrollback 3 rows", Duration::from_secs(5));
    let composer = screen
        .lines()
        .find(|line| line.contains("Remuda > "))
        .unwrap_or("")
        .to_owned();
    assert!(
        !composer.contains("draft line"),
        "the wheel recalled a line instead of scrolling: {composer:?}\n{screen}"
    );

    // Up is still the composer's line history, and any key returns to live.
    tui.writer.write_all(b"\x1b[A").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let screen = tui.screen_text();
        let recalled = screen
            .lines()
            .find(|line| line.contains("Remuda > "))
            .is_some_and(|line| line.contains("draft line"));
        if recalled {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Up did not recall the submitted line:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    tui.writer.write_all(b"\x1b[C").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let screen = tui.screen_text();
        if !screen.contains("scrollback") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the scrollback footer did not clear after a key:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn local_tree_lists_and_captures_a_session_from_a_private_daemon() {
    let mut daemon = PrivateDaemon::start();
    daemon.wait_ready();
    let response = remuda_native::client::request(
        &daemon.path(),
        &Request::New {
            name: Some("tree-session".into()),
            command: vec!["sh".into(), "-c".into(), "sleep 30".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .unwrap();
    assert!(matches!(response, Response::Value(name) if name == "tree-session"));

    let frame = remuda_native::cluster_tui::read_frame(
        &daemon.path(),
        "studio",
        Some("studio/tree-session"),
        90,
        24,
    )
    .unwrap();
    assert!(frame.contains("studio / tree-session · live · snapshot 0s ago"));
    assert!(frame.contains("tree-session"));
}

#[test]
fn local_tree_frame_uses_daemon_registered_decorations_and_depth() {
    let mut daemon = PrivateDaemon::start_with_init(Some(
        r#"
        remuda.session_order = function(rows)
            local order = {}
            for _, row in ipairs(rows) do
                if row.name == "tree-parent" then
                    order[#order + 1] = { name = row.name, depth = 0 }
                end
            end
            for _, row in ipairs(rows) do
                if row.name ~= "tree-parent" then
                    order[#order + 1] = { name = row.name, depth = 1 }
                end
            end
            return order
        end
        remuda.session_list_decorate = function()
            return { agent_kind = "codex", description = "gpt-6.1-terra", layout = "compact" }
        end
        "#,
    ));
    daemon.wait_ready();
    for name in ["tree-parent", "tree-child"] {
        let response = remuda_native::client::request(
            &daemon.path(),
            &Request::New {
                name: Some(name.into()),
                command: vec!["sh".into(), "-c".into(), "sleep 30".into()],
                size: Size::new(80, 24),
                cwd: None,
                env: None,
            },
        )
        .unwrap();
        assert!(matches!(response, Response::Value(got) if got == name));
    }

    let frame = remuda_native::cluster_tui::read_frame(
        &daemon.path(),
        "studio",
        Some("studio/tree-parent"),
        100,
        24,
    )
    .unwrap();
    let plain = strip_sgr(&frame);
    assert_eq!(plain.matches("codex · gpt-6.1-terra").count(), 2, "{plain}");
    let child = plain
        .lines()
        .find(|line| line.contains("tree-child"))
        .unwrap();
    assert!(child.starts_with("       ●  tree-child"), "{child:?}");
    assert!(
        plain.find("tree-parent").unwrap() < plain.find("tree-child").unwrap(),
        "{plain}"
    );
}

#[cfg(unix)]
fn document_test_daemon(fixtures: &[(&str, &str)]) -> PrivateDaemon {
    use base64::Engine;
    let mut daemon = PrivateDaemon::start();
    daemon.wait_ready();
    let init = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "cluster-tree", "cluster", "init", "--no-listen"])
        .env("REMUDA_RUNTIME_DIR", &daemon.runtime)
        .env("HOME", daemon.runtime.join("home"))
        .env("XDG_CONFIG_HOME", daemon.runtime.join("config"))
        .env("XDG_DATA_HOME", daemon.runtime.join("data"))
        .env("XDG_CACHE_HOME", daemon.runtime.join("cache"))
        .env("XDG_STATE_HOME", daemon.runtime.join("state"))
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");
    for (path, text) in fixtures {
        let put = remuda_native::client::request(
            &daemon.path(),
            &Request::Vfs {
                args: vec!["put".into(), "-".into(), format!("remuda:///{path}")],
                input_base64: Some(base64::engine::general_purpose::STANDARD.encode(text)),
            },
        )
        .unwrap();
        assert!(
            matches!(put, Response::CommandResult { exit_code: 0, .. }),
            "{put:?}"
        );
    }
    let new = remuda_native::client::request(
        &daemon.path(),
        &Request::New {
            name: Some("doc-session".into()),
            command: vec![
                "sh".into(),
                "-c".into(),
                r#"stty -echo; while IFS= read -r line; do printf 'got:%s\n' "$line"; done"#.into(),
            ],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .unwrap();
    assert!(matches!(new, Response::Value(_)), "{new:?}");
    daemon
}

#[cfg(unix)]
#[test]
fn documents_browse_plain_text_and_return_without_sending_browser_input() {
    use std::io::Write;
    let mut daemon = document_test_daemon(&[("docs/one.txt", "BROWSER_FIXTURE\nsecond line\n")]);
    let mut tui = TuiPty::start(&daemon.runtime);
    tui.wait_for("doc-session", Duration::from_secs(5));
    tui.writer.write_all(b"\rsaved-draft").unwrap();
    tui.wait_for("saved-draft", Duration::from_secs(3));
    tui.writer.write_all(b"\x1bd").unwrap(); // Alt-d preserves the composer.
    let root = tui.wait_for("docs/", Duration::from_secs(5));
    assert!(
        root.contains("Documents") && !root.contains("one.txt"),
        "{root}"
    );
    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("one.txt", Duration::from_secs(5));
    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("BROWSER_FIXTURE", Duration::from_secs(5));
    assert_document_address(&mut tui);
    tui.writer
        .write_all(b"\x1b[200~DO_NOT_SEND\r\x1b[201~xyk\r")
        .unwrap();
    tui.writer.write_all(b"\x1b").unwrap();
    tui.wait_for("> docs/one.txt", Duration::from_secs(3));
    // A pending daemon read must not put the document back after Back.
    daemon.stop();
    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("Loading", Duration::from_secs(3));
    tui.writer.write_all(b"\x1b").unwrap();
    tui.wait_for("> docs/one.txt", Duration::from_secs(3));
    daemon.resume();
    std::thread::sleep(Duration::from_millis(700));
    let after_reply = tui.screen_text();
    assert!(
        after_reply.contains("> docs/one.txt") && !after_reply.contains("BROWSER_FIXTURE"),
        "{after_reply}"
    );
    // A completed deadline releases single-flight capacity for a real retry.
    daemon.stop();
    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("deadline exceeded", Duration::from_secs(6));
    daemon.resume();
    tui.writer.write_all(b"r").unwrap();
    tui.wait_for("BROWSER_FIXTURE", Duration::from_secs(5));
    tui.writer.write_all(b"q").unwrap();
    let returned = tui.wait_for("saved-draft", Duration::from_secs(3));
    assert!(!returned.contains("Documents"), "{returned}");
    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("got:saved-draft", Duration::from_secs(5));
    let capture = remuda_native::client::request(
        &daemon.path(),
        &Request::Capture {
            name: "doc-session".into(),
        },
    )
    .unwrap();
    assert!(
        matches!(capture, Response::Screen(ref s) if !s.contains("DO_NOT_SEND")),
        "{capture:?}"
    );
}

#[cfg(unix)]
#[test]
fn documents_read_markdown_as_prose_and_follow_its_local_link() {
    use std::io::Write;
    let daemon = document_test_daemon(&[
        (
            "docs/guide.md",
            "# Guide\n\nRead **this** [next](next.md).\n\n- one\n",
        ),
        ("docs/next.md", "# Next\n\nSecond page prose.\n"),
    ]);
    let mut tui = TuiPty::start(&daemon.runtime);
    tui.wait_for("doc-session", Duration::from_secs(5));
    tui.writer.write_all(b"d").unwrap();
    tui.wait_for("docs/", Duration::from_secs(5));
    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("guide.md", Duration::from_secs(5));
    tui.writer.write_all(b"\r").unwrap();
    let shown = tui.wait_for("Read this next [1].", Duration::from_secs(5));
    assert!(shown.contains("Guide"), "{shown}");
    assert!(
        !shown.contains("**this**") && !shown.contains("# Guide"),
        "{shown}"
    );
    tui.writer.write_all(b"\t").unwrap();
    tui.wait_for("remuda:///docs/next.md", Duration::from_secs(3));
    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("Second page prose.", Duration::from_secs(5));
    tui.writer.write_all(b"\x1b").unwrap();
    tui.wait_for("Read this next [1].", Duration::from_secs(3));
    tui.writer.write_all(b"q").unwrap();
    tui.wait_for("doc-session", Duration::from_secs(3));
}

fn assert_document_address(tui: &mut TuiPty) {
    use std::io::Write;
    tui.writer.write_all(b"a").unwrap();
    tui.wait_for("VFS address (escaped)", Duration::from_secs(3));
    tui.wait_for("remuda:///docs/one.txt", Duration::from_secs(3));
    tui.writer.write_all(b"\x1b").unwrap();
    tui.wait_for("BROWSER_FIXTURE", Duration::from_secs(3));
}

#[cfg(unix)]
fn document_mailbox(
    daemon: &PrivateDaemon,
    file: &str,
    body: Option<&serde_json::Value>,
) -> serde_json::Value {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    let listing = remuda_native::client::request(
        &daemon.path(),
        &Request::Vfs {
            args: vec!["ls".into(), "remuda:///ade-open/".into(), "--json".into()],
            input_base64: None,
        },
    )
    .unwrap();
    let Response::CommandResult { stdout_base64, .. } = listing else {
        panic!("{listing:?}")
    };
    let listing: serde_json::Value =
        serde_json::from_slice(&BASE64.decode(stdout_base64).unwrap()).unwrap();
    let Some(prefix) = listing[0]["path"].as_str() else {
        return serde_json::Value::Null;
    };
    let uri = format!("remuda:///{prefix}owner/{file}.json");
    let args = if body.is_some() {
        vec!["put".into(), "-".into(), uri]
    } else {
        vec!["read".into(), uri, "--max-bytes".into(), "4096".into()]
    };
    let reply = remuda_native::client::request(
        &daemon.path(),
        &Request::Vfs {
            args,
            input_base64: body.map(|v| BASE64.encode(v.to_string())),
        },
    )
    .unwrap();
    let Response::CommandResult {
        exit_code: 0,
        stdout_base64,
        ..
    } = reply
    else {
        panic!("{reply:?}")
    };
    if body.is_some() {
        return serde_json::Value::Null;
    }
    let envelope: serde_json::Value =
        serde_json::from_slice(&BASE64.decode(stdout_base64).unwrap()).unwrap();
    envelope["content"]
        .as_str()
        .and_then(|s| BASE64.decode(s).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

#[cfg(unix)]
fn wait_document_mailbox(
    daemon: &PrivateDaemon,
    file: &str,
    field: &str,
    expected: &serde_json::Value,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let value = document_mailbox(daemon, file, None);
        if value[field] == *expected {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "mailbox {file}: {value}, expected {field}={expected}"
        );
        std::thread::sleep(Duration::from_millis(40));
    }
}

#[cfg(unix)]
#[test]
fn direct_open_waits_for_body_after_three_to_four_row_resize() {
    use serde_json::json;
    let daemon =
        document_test_daemon(&[("docs/small.html", "<title>ACTUAL_BODY</title><p>Design</p>")]);
    let mut tui = TuiPty::start_document_target_rows(&daemon.runtime, Some("owner"), 3);
    let ready = wait_document_mailbox(&daemon, "ready", "mode", &json!("direct"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let request = json!({"instance":ready["instance"],"seq":1,"uri":"remuda:///docs/small.html","expires":now+60});
    document_mailbox(&daemon, "request", Some(&request));
    wait_document_mailbox(&daemon, "result", "seq", &json!(1));
    tui.wait_for("Documents", Duration::from_secs(5));
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let result = document_mailbox(&daemon, "result", None);
        assert_eq!(
            result["status"], "received",
            "false presentation acknowledgement: {result}"
        );
        assert!(!tui.screen_text().contains("ACTUAL_BODY"));
        std::thread::sleep(Duration::from_millis(40));
    }
    tui.resize_rows(4);
    tui.wait_for("ACTUAL_BODY", Duration::from_secs(5));
    let rendered = wait_document_mailbox(&daemon, "result", "status", &json!("rendered"));
    assert_eq!(rendered["seq"], 1);
    assert_eq!(rendered["instance"], ready["instance"]);
    assert_eq!(rendered["uri"], request["uri"]);
}

#[cfg(unix)]
#[test]
fn direct_document_request_renders_exact_viewer_and_preserves_draft() {
    use serde_json::json;
    use std::io::Write;
    let daemon = document_test_daemon(&[(
        "docs/design.html",
        "<title>DIRECT_DESIGN</title><p>Published design document</p>",
    )]);
    let mut tui = TuiPty::start_document_target(&daemon.runtime, Some("owner"));
    let other = TuiPty::start(&daemon.runtime);
    tui.wait_for("doc-session", Duration::from_secs(5));
    other.wait_for("doc-session", Duration::from_secs(5));
    tui.writer.write_all(b"\rdirect-draft").unwrap();
    tui.wait_for("direct-draft", Duration::from_secs(3));
    let ready = wait_document_mailbox(&daemon, "ready", "mode", &json!("direct"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut request = json!({"instance":"wrong-viewer","seq":1,"uri":"remuda:///docs/design.html","expires":now+60});
    document_mailbox(&daemon, "request", Some(&request));
    std::thread::sleep(Duration::from_millis(800));
    assert!(tui.screen_text().contains("direct-draft"));
    request["instance"] = ready["instance"].clone();
    document_mailbox(&daemon, "request", Some(&request));
    let shown = tui.wait_for("DIRECT_DESIGN", Duration::from_secs(8));
    assert!(shown.contains("Published design document"), "{shown}");
    let rendered = wait_document_mailbox(&daemon, "result", "status", &json!("rendered"));
    assert_eq!(rendered["instance"], ready["instance"]);
    assert_eq!(rendered["seq"], 1);
    assert_eq!(rendered["uri"], request["uri"]);
    assert_eq!(rendered["identity"][1].as_str().unwrap().len(), 64);
    assert!(!other.screen_text().contains("DIRECT_DESIGN"));
    tui.writer.write_all(b"a").unwrap();
    tui.wait_for("VFS address (escaped)", Duration::from_secs(3));
    request["seq"] = json!(2);
    document_mailbox(&daemon, "request", Some(&request));
    tui.wait_for("DIRECT_DESIGN", Duration::from_secs(8));
    wait_document_mailbox(&daemon, "result", "seq", &json!(2));
    wait_document_mailbox(&daemon, "result", "status", &json!("rendered"));
    tui.writer.write_all(b"q").unwrap();
    tui.wait_for("direct-draft", Duration::from_secs(3));
    document_mailbox(&daemon, "request", Some(&request));
    std::thread::sleep(Duration::from_millis(800));
    assert!(!tui.screen_text().contains("DIRECT_DESIGN"));
    request["seq"] = json!(3);
    request["expires"] = json!(now - 1);
    document_mailbox(&daemon, "request", Some(&request));
    wait_document_mailbox(&daemon, "result", "status", &json!("expired"));
    assert!(!tui.screen_text().contains("DIRECT_DESIGN"));
    request["seq"] = json!(4);
    request["expires"] = json!(now + 60);
    request["uri"] = json!("remuda:///docs/missing.html");
    document_mailbox(&daemon, "request", Some(&request));
    wait_document_mailbox(&daemon, "result", "status", &json!("load_failed"));
    tui.writer.write_all(b"q\r").unwrap();
    tui.wait_for("got:direct-draft", Duration::from_secs(5));
}

#[cfg(unix)]
#[test]
fn html_relative_link_round_trip_preserves_document_and_session_state() {
    use std::io::Write;
    let daemon = document_test_daemon(&[
        ("docs/index.html", "<title>HTML_GUIDE</title><p>First &amp; paragraph</p><script>NEVER_EXECUTED</script><a href='sub/next.html'>Next document</a><a href='#missing'>Unsupported fragment</a>"),
        ("docs/sub/next.html", "<title>NEXT_PAGE</title><p>Second document</p>"),
    ]);
    let mut tui = TuiPty::start(&daemon.runtime);
    tui.wait_for("doc-session", Duration::from_secs(5));
    tui.writer.write_all(b"\rhtml-draft").unwrap();
    tui.wait_for("html-draft", Duration::from_secs(3));
    tui.writer.write_all(b"\x1bd").unwrap();
    tui.wait_for("> docs/", Duration::from_secs(5));
    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("docs/index.html", Duration::from_secs(5));
    tui.writer.write_all(b"\x1b[B").unwrap();
    tui.wait_for("> docs/index.html", Duration::from_secs(5));
    tui.writer.write_all(b"\r").unwrap();
    let html = tui.wait_for("HTML_GUIDE", Duration::from_secs(5));
    assert!(
        html.contains("First & paragraph") && !html.contains("NEVER_EXECUTED"),
        "{html}"
    );
    tui.writer.write_all(b"\t").unwrap();
    tui.wait_for("[1] Next document", Duration::from_secs(3));
    tui.writer.write_all(b"a").unwrap();
    tui.wait_for("VFS address (escaped)", Duration::from_secs(3));
    tui.wait_for("remuda:///docs/sub/next.html", Duration::from_secs(3));
    tui.writer.write_all(b"\x1b").unwrap();
    tui.wait_for("HTML_GUIDE", Duration::from_secs(3));
    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("NEXT_PAGE", Duration::from_secs(5));
    tui.writer.write_all(b"\x1b").unwrap();
    let restored = tui.wait_for("HTML_GUIDE", Duration::from_secs(3));
    assert!(restored.contains("[1] Next document"), "{restored}");
    tui.writer.write_all(b"\t\r").unwrap();
    let blocked = tui.wait_for("Fragment links are not supported", Duration::from_secs(3));
    assert!(blocked.contains("HTML_GUIDE"), "{blocked}");
    tui.writer
        .write_all(b"\x1b[200~HTML_DO_NOT_SEND\x1b[201~")
        .unwrap();
    tui.writer.write_all(b"q\r").unwrap();
    let returned = tui.wait_for("got:html-draft", Duration::from_secs(5));
    assert!(!returned.contains("HTML_DO_NOT_SEND"), "{returned}");
}

#[cfg(unix)]
#[test]
fn local_tree_keeps_decorations_during_daemon_timeout_and_recovers() {
    let mut daemon = PrivateDaemon::start_with_init(Some(
        r#"remuda.session_list_decorate = function() return { agent_kind = 'codex', description = 'timeout-proof', layout = 'compact' } end"#,
    ));
    daemon.wait_ready();
    let response = remuda_native::client::request(
        &daemon.path(),
        &Request::New {
            name: Some("timeout-session".into()),
            command: vec!["sh".into(), "-c".into(), "sleep 30".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .unwrap();
    assert!(
        matches!(&response, Response::Value(name) if name == "timeout-session"),
        "{response:?}"
    );
    let tui = TuiPty::start(&daemon.runtime);
    tui.wait_for("timeout-proof", Duration::from_secs(5));
    daemon.stop();
    tui.wait_for("stale", Duration::from_secs(5));
    let screen = tui.screen_text();
    assert!(screen.contains("timeout-proof"), "{screen}");
    daemon.resume();
    let update = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "cluster-tree", "-e", "remuda.session_list_decorate = function() return { agent_kind = 'codex', description = 'updated-proof', layout = 'compact' } end"])
        .env("REMUDA_RUNTIME_DIR", &daemon.runtime)
        .env("HOME", daemon.runtime.join("home"))
        .env("XDG_CONFIG_HOME", daemon.runtime.join("config"))
        .env("XDG_DATA_HOME", daemon.runtime.join("data"))
        .env("XDG_CACHE_HOME", daemon.runtime.join("cache"))
        .env("XDG_STATE_HOME", daemon.runtime.join("state"))
        .output().unwrap();
    assert!(update.status.success(), "{update:?}");
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let screen = tui.screen_text();
        if screen.contains("updated-proof") && !screen.contains("stale") {
            break;
        }
        assert!(Instant::now() < deadline, "{screen}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
#[test]
fn typing_stays_responsive_while_the_local_view_decorator_is_slow() {
    let mut daemon = PrivateDaemon::start_with_init(Some(
        r#"
        remuda.session_list_decorate = function()
            os.execute("sleep 1")
            return { agent_kind = "codex", description = "slow" }
        end
        "#,
    ));
    daemon.wait_ready();
    let response = remuda_native::client::request(
        &daemon.path(),
        &Request::New {
            name: Some("slow-session".into()),
            command: vec!["sh".into(), "-c".into(), "sleep 30".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .unwrap();
    assert!(matches!(response, Response::Value(name) if name == "slow-session"));
    let mut tui = TuiPty::start(&daemon.runtime);
    tui.wait_for("slow-session", Duration::from_secs(5));
    // The 250 ms view timeout is now in flight; a synchronous renderer call
    // would hold this key for that long.
    std::thread::sleep(Duration::from_millis(60));
    let started = Instant::now();
    tui.writer.write_all(b"?").unwrap();
    tui.wait_for("remuda · cluster help", Duration::from_secs(2));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(200),
        "typing latency {elapsed:?}"
    );
}

fn strip_sgr(text: &str) -> String {
    let mut plain = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            for escape in chars.by_ref() {
                if escape == 'm' {
                    break;
                }
            }
        } else {
            plain.push(ch);
        }
    }
    plain
}

#[test]
fn local_daemon_composer_input_appears_once_in_capture() {
    let mut daemon = PrivateDaemon::start();
    daemon.wait_ready();
    let response = remuda_native::client::request(
        &daemon.path(),
        &Request::New {
            name: Some("composer-session".into()),
            command: vec![
                "sh".into(),
                "-c".into(),
                r#"stty -echo; IFS= read -r line; printf 'line:%s\n' "$line"; sleep 30"#.into(),
            ],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .unwrap();
    assert!(matches!(response, Response::Value(name) if name == "composer-session"));
    let instance_id = match remuda_native::client::request(&daemon.path(), &Request::List).unwrap()
    {
        Response::Sessions(sessions) => sessions
            .into_iter()
            .find(|session| session.name == "composer-session")
            .and_then(|session| session.instance_id)
            .expect("List must expose the running session instance id"),
        other => panic!("unexpected List response: {other:?}"),
    };

    let keymap = remuda_native::keybindings::fetch(&daemon.path(), Duration::from_secs(3)).unwrap();
    let mut composer = LineComposer::default();
    for character in "once only".chars() {
        composer.handle_key(
            KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            &keymap,
        );
    }
    let bytes =
        match composer.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &keymap) {
            ComposerAction::Submit(bytes) => bytes,
            other => panic!("expected composer submission, got {other:?}"),
        };
    let now = Instant::now();
    let mut sender = InputSender::with_client_id([0x72; 16]);
    let mut queue = InputQueue::default();
    sender
        .enqueue(&mut queue, "composer-session", &instance_id, bytes, now)
        .unwrap();
    let result = sender.attempt_due(&mut queue, now, |request| {
        remuda_native::client::request(&daemon.path(), request)
    });
    assert!(matches!(result, Some(QueueEvent::Sent { seq: 1, .. })));
    assert_eq!(queue.items().next().unwrap().state, QueueState::Sent);

    let deadline = Instant::now() + Duration::from_secs(5);
    let screen = loop {
        let screen = match remuda_native::client::request(
            &daemon.path(),
            &Request::Capture {
                name: "composer-session".into(),
            },
        )
        .unwrap()
        {
            Response::Screen(screen) => screen,
            other => panic!("unexpected Capture response: {other:?}"),
        };
        if screen.contains("line:once only") {
            break screen;
        }
        assert!(
            Instant::now() < deadline,
            "typed line never appeared in capture"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(screen.matches("line:once only").count(), 1);
}

#[test]
fn lost_sequence_rotates_client_before_the_next_line() {
    let mut daemon = PrivateDaemon::start();
    daemon.wait_ready();
    let response = remuda_native::client::request(
        &daemon.path(),
        &Request::New {
            name: Some("recovery-session".into()),
            command: vec![
                "sh".into(),
                "-c".into(),
                r#"stty -echo; IFS= read -r line; printf 'line:%s\n' "$line"; sleep 30"#.into(),
            ],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .unwrap();
    assert!(matches!(response, Response::Value(name) if name == "recovery-session"));
    let instance_id = match remuda_native::client::request(&daemon.path(), &Request::List).unwrap()
    {
        Response::Sessions(sessions) => sessions
            .into_iter()
            .find(|session| session.name == "recovery-session")
            .and_then(|session| session.instance_id)
            .expect("List must expose the running session instance id"),
        other => panic!("unexpected List response: {other:?}"),
    };

    let now = Instant::now();
    let mut sender = InputSender::with_client_id([0x31; 16]);
    let mut queue = InputQueue::default();
    sender
        .enqueue(
            &mut queue,
            "recovery-session",
            &instance_id,
            b"lost\r".to_vec(),
            now,
        )
        .unwrap();
    let first_client = queue.items().next().unwrap().client_id.clone();
    sender
        .enqueue(
            &mut queue,
            "recovery-session",
            &instance_id,
            b"after loss\r".to_vec(),
            now,
        )
        .unwrap();
    for attempt in 0..=MAX_IO_RETRIES {
        let at = now + Duration::from_secs(u64::from(attempt) + 1);
        let result = sender.attempt_due(&mut queue, at, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "simulated lost sequence",
            ))
        });
        if attempt == MAX_IO_RETRIES {
            assert!(matches!(result, Some(QueueEvent::Uncertain { seq: 1, .. })));
        } else {
            assert!(matches!(
                result,
                Some(QueueEvent::RetryScheduled { seq: 1, .. })
            ));
        }
    }

    let next = queue.items().next_back().unwrap();
    assert_eq!(next.seq, 1);
    assert_ne!(next.client_id, first_client);
    let result = sender.attempt_due(&mut queue, now + Duration::from_secs(11), |request| {
        remuda_native::client::request(&daemon.path(), request)
    });
    assert!(matches!(result, Some(QueueEvent::Sent { seq: 1, .. })));

    let deadline = Instant::now() + Duration::from_secs(5);
    let screen = loop {
        let screen = match remuda_native::client::request(
            &daemon.path(),
            &Request::Capture {
                name: "recovery-session".into(),
            },
        )
        .unwrap()
        {
            Response::Screen(screen) => screen,
            other => panic!("unexpected Capture response: {other:?}"),
        };
        if screen.contains("line:after loss") {
            break screen;
        }
        assert!(Instant::now() < deadline, "recovered input never appeared");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(screen.matches("line:after loss").count(), 1);
    assert!(!screen.contains("line:lost"));
}
