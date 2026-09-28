//! End-to-end coverage for the cluster tree's read-only local IPC path.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::cluster_tui::composer::{ComposerAction, LineComposer};
use remuda_native::cluster_tui::queue::{InputQueue, QueueEvent, QueueState};
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
}

impl PrivateDaemon {
    fn start() -> Self {
        let root = PathBuf::from("/tmp");
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
}

impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        assert!(
            self.runtime.starts_with(&self.root),
            "daemon runtime must be scratch"
        );
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.runtime);
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
