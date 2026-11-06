//! `remuda attach` shows a session's screen and follows its output, whichever
//! backend runs it (#713). Driven over the raw attach protocol, so no TTY is
//! needed. Same scenario on both; see `daemon_support/parity.rs`.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Mode, Parity, MODES, PATIENCE};

/// One attached viewer: everything the daemon streams lands in `seen`.
struct Viewer {
    mode: Mode,
    stream: remuda_native::ipc::Stream,
    seen: Arc<Mutex<Vec<u8>>>,
    ended: Arc<Mutex<bool>>,
    generation: u64,
}

impl Viewer {
    fn attach(daemon: &Parity, name: &str) -> Self {
        let mut stream = remuda_native::ipc::connect(&daemon.socket()).expect("connect");
        let mut line =
            serde_json::to_string(&Request::AttachTracked { name: name.into() }).unwrap();
        line.push('\n');
        stream.write_all(line.as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut reader =
            BufReader::new(remuda_native::ipc::TryClone::try_clone(&stream).expect("clone"));
        let mut ack = String::new();
        reader.read_line(&mut ack).unwrap();
        let generation = match serde_json::from_str::<Response>(&ack) {
            Ok(Response::AttachStarted { generation }) => generation,
            other => panic!("{:?}: attach refused: {other:?} ({ack})", daemon.mode),
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let ended = Arc::new(Mutex::new(false));
        std::thread::spawn({
            let seen = Arc::clone(&seen);
            let ended = Arc::clone(&ended);
            move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => seen.lock().unwrap().extend_from_slice(&buf[..n]),
                    }
                }
                *ended.lock().unwrap() = true;
            }
        });
        Self {
            mode: daemon.mode,
            stream,
            seen,
            ended,
            generation,
        }
    }

    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let seen = String::from_utf8_lossy(&self.seen.lock().unwrap()).into_owned();
            if seen.contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{:?}: {needle:?} never reached the viewer:\n{seen:?}",
                self.mode
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_ended(&self) {
        let deadline = Instant::now() + PATIENCE;
        while !*self.ended.lock().unwrap() {
            assert!(
                Instant::now() < deadline,
                "{:?}: the attach stream never ended",
                self.mode
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Viewer {
    /// Detach: the reader thread holds a clone, so close both directions.
    fn drop(&mut self) {
        remuda_native::ipc::wake(&self.stream);
    }
}

impl Viewer {
    fn type_keys(&mut self, keys: &[u8]) {
        self.stream.write_all(keys).unwrap();
        self.stream.flush().unwrap();
    }
}

fn attached(daemon: &Parity, name: &str) -> bool {
    match daemon.request(&Request::List) {
        Response::Sessions(sessions) => sessions
            .iter()
            .find(|session| session.name == name)
            .is_some_and(|session| session.attached),
        other => panic!("{:?}: ls: {other:?}", daemon.mode),
    }
}

fn send_line(daemon: &Parity, name: &str, text: &str) {
    assert_eq!(
        daemon.request(&Request::SendLine {
            name: name.into(),
            text: text.into(),
        }),
        Response::Ok,
        "{:?}",
        daemon.mode
    );
}

#[test]
fn attach_paints_the_screen_and_follows_output_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        daemon.shell("sh");
        send_line(&daemon, "sh", "echo BEFORE:$((1+1))");
        daemon.wait_for("sh", "BEFORE:2");

        let viewer = Viewer::attach(&daemon, "sh");
        viewer.wait_for("BEFORE:2");
        send_line(&daemon, "sh", "echo AFTER:$((2+3))");
        viewer.wait_for("AFTER:5");
        assert_eq!(
            daemon.request(&Request::AttachStatus {
                name: "sh".into(),
                generation: viewer.generation,
            }),
            Response::AttachStatus { taken_over: false },
            "{mode:?}"
        );

        // Detaching leaves the session running, usable and unattached.
        drop(viewer);
        let deadline = Instant::now() + PATIENCE;
        while attached(&daemon, "sh") {
            assert!(Instant::now() < deadline, "{mode:?}: still attached");
            std::thread::sleep(Duration::from_millis(20));
        }
        send_line(&daemon, "sh", "echo STILL:$((3+4))");
        daemon.wait_for("sh", "STILL:7");

        // The child exiting ends the attach stream.
        let viewer = Viewer::attach(&daemon, "sh");
        viewer.wait_for("STILL:7");
        send_line(&daemon, "sh", "exit");
        viewer.wait_ended();
    }
}

#[test]
fn attaching_to_a_missing_session_is_refused_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        let mut stream = remuda_native::ipc::connect(&daemon.socket()).expect("connect");
        let mut line = serde_json::to_string(&Request::AttachTracked {
            name: "nobody".into(),
        })
        .unwrap();
        line.push('\n');
        stream.write_all(line.as_bytes()).unwrap();
        let mut ack = String::new();
        BufReader::new(stream).read_line(&mut ack).unwrap();
        assert!(ack.contains("no such session"), "{mode:?}: {ack}");
    }
}

#[test]
fn typing_into_an_attach_reaches_the_child_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        daemon.shell("sh");
        let mut viewer = Viewer::attach(&daemon, "sh");
        assert!(attached(&daemon, "sh"), "{mode:?}: not marked attached");
        viewer.type_keys(b"echo TYPED:$((5+6))\r");
        viewer.wait_for("TYPED:11");
        daemon.wait_for("sh", "TYPED:11");
        // Keystrokes one at a time keep their order.
        for key in b"echo ORDER:$((7*8))\r" {
            viewer.type_keys(&[*key]);
        }
        viewer.wait_for("ORDER:56");

        // Typed input leaves nothing behind that would hold up a close.
        drop(viewer);
        let deadline = Instant::now() + PATIENCE;
        while attached(&daemon, "sh") {
            assert!(Instant::now() < deadline, "{mode:?}: still attached");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            daemon.request(&Request::Close {
                name: "sh".into(),
                instance_id: None,
                confirm: None,
            }),
            Response::Ok,
            "{mode:?}"
        );
        let deadline = Instant::now() + PATIENCE;
        while matches!(daemon.request(&Request::List), Response::Sessions(s) if !s.is_empty()) {
            assert!(
                Instant::now() < deadline,
                "{mode:?}: close left the session listed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn a_second_attach_takes_over_the_first_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        daemon.shell("sh");
        let mut first = Viewer::attach(&daemon, "sh");
        let mut second = Viewer::attach(&daemon, "sh");
        assert!(second.generation > first.generation, "{mode:?}");
        first.wait_for("attached elsewhere");
        first.wait_ended();
        assert_eq!(
            daemon.request(&Request::AttachStatus {
                name: "sh".into(),
                generation: first.generation,
            }),
            Response::AttachStatus { taken_over: true },
            "{mode:?}"
        );
        // Only the new holder types.
        second.type_keys(b"echo SECOND:$((8+9))\r");
        second.wait_for("SECOND:17");
        // Its stream has ended, so the write may simply fail.
        let _ = first.stream.write_all(b"echo FIRST:$((9+9))\r");
        std::thread::sleep(Duration::from_millis(300));
        assert!(!daemon.screen("sh").contains("FIRST:18"), "{mode:?}");
    }
}

#[test]
fn mouse_state_follows_the_child_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        daemon.shell("sh");
        let mouse = || match daemon.request(&Request::MouseState { name: "sh".into() }) {
            Response::MouseState(state) => state,
            other => panic!("{mode:?}: mouse state: {other:?}"),
        };
        assert_eq!(
            mouse().mode,
            remuda_core::agent::MouseMode::None,
            "{mode:?}"
        );
        send_line(&daemon, "sh", r"printf '\033[?1000h\033[?2004hON\n'");
        daemon.wait_for("sh", "ON");
        let deadline = Instant::now() + PATIENCE;
        loop {
            let state = mouse();
            if state.mode != remuda_core::agent::MouseMode::None && state.bracketed_paste {
                break;
            }
            assert!(Instant::now() < deadline, "{mode:?}: {state:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
