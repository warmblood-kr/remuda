//! End-to-end coverage for the cluster tree's read-only local IPC path.

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
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

#[cfg(unix)]
struct TuiPty {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    pid: u32,
    writer: Box<dyn std::io::Write + Send>,
    output: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl TuiPty {
    fn start(runtime: &std::path::Path) -> Self {
        use std::io::Read;
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
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
            child,
            pid,
            writer,
            output,
            reader: Some(reader),
        }
    }

    fn screen_text(&self) -> String {
        let output = self.output.lock().unwrap();
        let mut parser = vt100::Parser::new(24, 100, 0);
        parser.process(&output);
        parser.screen().contents()
    }

    fn output_len(&self) -> usize {
        self.output.lock().unwrap().len()
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
fn stopped_daemon_keeps_cluster_tui_ticking_and_recovers_input() {
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
    tui.wait_for("$ tui-session> ▏", Duration::from_secs(3));
    tui.writer.write_all(b"first line").unwrap();
    tui.wait_for("first line▏", Duration::from_secs(5));

    daemon.stop();
    let redraw_start = tui.output_len();
    tui.writer.write_all(b"\r").unwrap();
    tui.wait_for("sending input to tui-session", Duration::from_secs(3));
    let started = Instant::now();
    let uncertain_bound = Duration::from_secs(8);
    let deadline = started + uncertain_bound;
    let mut progress_screens = std::collections::HashSet::new();
    let screen_after = loop {
        let screen = tui.screen_text();
        if screen.contains("delivery uncertain") {
            break screen;
        }
        if screen.contains("sending input") {
            progress_screens.insert(screen.clone());
        }
        assert!(
            Instant::now() < deadline,
            "uncertain state was not rendered within 8s while daemon was SIGSTOPped; screen:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    let (screen_before, _) = tui.screens_from(redraw_start);
    assert!(
        started.elapsed() <= uncertain_bound && !screen_before.contains("delivery uncertain"),
        "uncertain state was already visible before the retry: screen at offset:\n{screen_before}\ncurrent screen:\n{screen_after}"
    );
    assert!(
        progress_screens.len() >= 3,
        "TUI did not keep rendering changing progress while daemon was SIGSTOPped; saw {} progress snapshots:\n{}",
        progress_screens.len(),
        progress_screens.into_iter().collect::<Vec<_>>().join("\n---\n")
    );

    daemon.resume();
    tui.writer.write_all(b"second line\r").unwrap();
    tui.wait_for("got:second line", Duration::from_secs(10));
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

    let mut composer = LineComposer::default();
    for character in "once only".chars() {
        composer.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    }
    let bytes = match composer.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
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
