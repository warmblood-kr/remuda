//! Private synthetic child/PTY and authenticated local requests only.
use super::*;
use crate::session_host::api::Request;
use crate::session_host::control::ConnectionId;
use crate::session_host::coordinator::Admission;
use crate::session_host::entry::Client;
use crate::session_host::{owned::Owned, server::Server, unix::Endpoint, viewport::Viewport};
use base64::Engine;
use portable_pty::CommandBuilder;
use remuda_core::Size;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
static NEXT: AtomicU64 = AtomicU64::new(0);

fn eventually<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(
            Instant::now() < until,
            "private typed-view fixture deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
struct Fixture {
    root: PathBuf,
    host: Arc<Owned>,
    server: Arc<Server>,
    thread: Option<std::thread::JoinHandle<()>>,
    sequence: AtomicU64,
}
impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from("/tmp").join(format!(
            "rmv-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let endpoint = Endpoint::bind(&root.join("host")).unwrap();
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "session_host::latest::tests::typed_view_child_fixture",
            "--ignored",
            "--nocapture",
        ]);
        command.env_clear();
        command.env("REMUDA_TYPED_VIEW_TEST_ROOT", &root);
        command.cwd(&root);
        let host = Arc::new(
            Owned::spawn([7; 16], "typed-fixture".into(), command, Size::new(80, 24)).unwrap(),
        );
        let server = Arc::new(Server::new(endpoint, host.clone()));
        let running = server.clone();
        let thread = Some(std::thread::spawn(move || running.run().unwrap()));
        let fixture = Self {
            root,
            host,
            server,
            thread,
            sequence: AtomicU64::new(0),
        };
        eventually(|| fixture.root.join("ready").exists().then_some(()));
        fixture
    }
    fn viewer(&self) -> Client {
        Client::open_viewport(&self.root.join("host"), self.host.identity()).unwrap()
    }
    fn send(&self, text: &str) {
        let fence = self.host.acquire(ConnectionId(900)).unwrap();
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let act = crate::session_host::wire::InputAct::Bytes(format!("{text}\n").into_bytes());
        let Admission::Started(token) = self.host.admit(fence, [5; 16], sequence, &act).unwrap()
        else {
            panic!("new fixture act");
        };
        self.host.execute(token).unwrap();
        self.host.release(fence).unwrap();
    }
    fn generation(&self) -> u64 {
        self.host.view.status().unwrap().0
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            if let Ok(fence) = self.host.acquire(ConnectionId(901)) {
                let result = self.host.close(fence);
                let _ = self.host.release(fence);
                if result.is_ok() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.server.stop_fixture();
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
struct Captured {
    id: u64,
    generation: u64,
    pages: usize,
    bytes: usize,
}
fn begin(client: &mut Client) -> Captured {
    eventually(|| {
        match client
            .request(&Request::Viewport { scrollback: 0 })
            .unwrap()
        {
            Response::Viewport {
                snapshot,
                generation,
                pages,
                bytes,
                ..
            } => Some(Captured {
                id: snapshot,
                generation,
                pages,
                bytes,
            }),
            Response::Error { code } if code == "snapshot_budget_busy" => None,
            other => panic!("typed snapshot refusal: {other:?}"),
        }
    })
}
fn read(client: &mut Client, captured: &Captured) -> Viewport {
    let mut bytes = Vec::new();
    for index in 0..captured.pages {
        let Response::Page {
            snapshot,
            index: received,
            base64,
        } = client
            .request(&Request::Page {
                snapshot: captured.id,
                index,
            })
            .unwrap()
        else {
            panic!("typed page refusal");
        };
        assert_eq!((snapshot, received), (captured.id, index));
        bytes.extend(
            base64::engine::general_purpose::STANDARD
                .decode(base64)
                .unwrap(),
        );
    }
    assert_eq!(bytes.len(), captured.bytes);
    let viewport: Viewport = serde_json::from_slice(&bytes).unwrap();
    viewport.validate().unwrap();
    assert_eq!(viewport.generation, captured.generation);
    viewport
}
fn ack(client: &mut Client, captured: &Captured) -> Response {
    client
        .request(&Request::AcknowledgeViewport {
            snapshot: captured.id,
            generation: captured.generation,
        })
        .unwrap()
}

#[test]
fn historical_frame_remains_coherent_and_ack_reports_newer_output_resync() {
    let fixture = Fixture::new();
    let mut client = fixture.viewer();
    let captured = begin(&mut client);
    fixture.send("emit");
    eventually(|| (fixture.generation() > captured.generation).then_some(()));
    let view = read(&mut client, &captured);
    assert_eq!(view.generation, captured.generation);
    let Response::ViewState {
        generation,
        resync_required,
        state,
        ..
    } = ack(&mut client, &captured)
    else {
        panic!("typed ACK refusal");
    };
    assert!(generation > captured.generation && resync_required);
    assert_eq!(state, ViewState::ResyncRequired);
    let next = begin(&mut client);
    let _ = read(&mut client, &next);
    assert!(next.generation > captured.generation);
    assert!(matches!(
        ack(&mut client, &next),
        Response::ViewState {
            resync_required: false,
            ..
        }
    ));
}

#[test]
fn incomplete_wrong_kind_generation_and_duplicate_ack_never_make_a_baseline() {
    let fixture = Fixture::new();
    let mut client = fixture.viewer();
    let captured = begin(&mut client);
    assert!(matches!(
        ack(&mut client, &captured),
        Response::Error { .. }
    ));
    let _ = read(&mut client, &captured);
    assert!(matches!(
        client
            .request(&Request::Acknowledge {
                snapshot: captured.id,
                generation: captured.generation
            })
            .unwrap(),
        Response::Error { .. }
    ));
    assert!(matches!(
        client
            .request(&Request::AcknowledgeViewport {
                snapshot: captured.id,
                generation: captured.generation + 1
            })
            .unwrap(),
        Response::Error { .. }
    ));
    assert!(matches!(
        client
            .request(&Request::WatchAfter {
                generation: captured.generation,
                timeout_ms: 0
            })
            .unwrap(),
        Response::Error { .. }
    ));
    assert!(matches!(
        ack(&mut client, &captured),
        Response::ViewState { .. }
    ));
    assert!(matches!(
        ack(&mut client, &captured),
        Response::Error { .. }
    ));
    assert!(matches!(
        client
            .request(&Request::WatchAfter {
                generation: u64::MAX,
                timeout_ms: 0
            })
            .unwrap(),
        Response::Error { .. }
    ));
}

#[test]
fn output_between_generation_sample_and_wait_is_not_lost() {
    let fixture = Fixture::new();
    let mut latest = Latest::new(&fixture.host.view).unwrap();
    let generation = fixture.generation();
    latest.acknowledge(generation, &fixture.host.view).unwrap();
    let (entered, ready) = std::sync::mpsc::sync_channel(1);
    let (release, resume) = std::sync::mpsc::sync_channel(1);
    latest.before_sleep = Some(Box::new(move || {
        entered.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(2)).unwrap();
    }));
    std::thread::scope(|scope| {
        let view = &fixture.host.view;
        let worker = scope.spawn(move || latest.wait(generation, 750, view).unwrap());
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        fixture.send("emit");
        eventually(|| (fixture.generation() > generation).then_some(()));
        release.send(()).unwrap();
        assert!(matches!(
            worker.join().unwrap(),
            Response::ViewState {
                resync_required: true,
                ..
            }
        ));
    });
}

#[test]
fn stopped_viewer_does_not_block_drain_queries_healthy_view_or_controller() {
    let fixture = Fixture::new();
    let mut stopped = fixture.viewer();
    let held = begin(&mut stopped);
    for _ in 0..16 {
        stopped
            .send_fixture(&Request::Page {
                snapshot: held.id,
                index: 0,
            })
            .unwrap();
    }
    let before = fixture.generation();
    fixture.send("continuous");
    eventually(|| (fixture.generation() > before + 10).then_some(()));
    eventually(|| fixture.root.join("queries").exists().then_some(()));
    let mut healthy = fixture.viewer();
    let frame = begin(&mut healthy);
    let view = read(&mut healthy, &frame);
    assert!(view.generation > before);
    assert!(matches!(
        ack(&mut healthy, &frame),
        Response::ViewState { .. }
    ));
    let fence = fixture.host.acquire(ConnectionId(902)).unwrap();
    let resize_started = Instant::now();
    fixture.host.resize(fence, 25, 90).unwrap();
    println!(
        "synthetic control resize during output: {} us",
        resize_started.elapsed().as_micros()
    );
    fixture.host.release(fence).unwrap();
    let info = fixture.host.inspect().unwrap();
    assert_eq!((info.rows, info.cols), (25, 90));
    assert!(!info.backend_fault);
    assert_eq!((info.unsupported_osc, info.query_overflow), (0, 0));
    assert!(fixture.generation() > frame.generation);
}

#[test]
fn eof_is_explicit_without_requiring_another_output_generation() {
    let fixture = Fixture::new();
    let mut client = fixture.viewer();
    let captured = begin(&mut client);
    let _ = read(&mut client, &captured);
    let _ = ack(&mut client, &captured);
    fixture.send("exit");
    eventually(|| fixture.host.inspect().unwrap().output_eof.then_some(()));
    assert!(matches!(
        client
            .request(&Request::WatchAfter {
                generation: captured.generation,
                timeout_ms: 0
            })
            .unwrap(),
        Response::ViewState {
            state: ViewState::Ended,
            ..
        }
    ));
}

#[test]
fn expired_frame_cannot_ack_or_watch_and_a_fresh_frame_restores_the_baseline() {
    let fixture = Fixture::new();
    let mut client = fixture.viewer();
    let captured = begin(&mut client);
    let until = Instant::now() + Duration::from_millis(2200);
    while Instant::now() < until {
        assert!(matches!(
            client.request(&Request::Inspect).unwrap(),
            Response::Info { .. }
        ));
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(matches!(
        client
            .request(&Request::Page {
                snapshot: captured.id,
                index: 0
            })
            .unwrap(),
        Response::Error { .. }
    ));
    assert!(matches!(
        ack(&mut client, &captured),
        Response::Error { .. }
    ));
    assert!(matches!(
        client
            .request(&Request::WatchAfter {
                generation: captured.generation,
                timeout_ms: 0
            })
            .unwrap(),
        Response::Error { .. }
    ));
    let fresh = begin(&mut client);
    let _ = read(&mut client, &fresh);
    assert!(matches!(
        ack(&mut client, &fresh),
        Response::ViewState {
            resync_required: false,
            ..
        }
    ));
}

#[test]
fn complete_frames_render_during_continuous_output_without_chasing_a_quiet_generation() {
    let fixture = Fixture::new();
    fixture.send("continuous");
    eventually(|| fixture.root.join("queries").exists().then_some(()));
    let mut client = fixture.viewer();
    let mut previous = 0;
    for _ in 0..3 {
        let started = Instant::now();
        let captured = begin(&mut client);
        let view = read(&mut client, &captured);
        let copy_started = Instant::now();
        let copied = fixture.host.view.viewport(0).unwrap();
        println!("synthetic frame G={} bytes={} capture/pages/decode={} us parser-lock-wait-plus-copy={} us copied-G={}",
            captured.generation, captured.bytes, started.elapsed().as_micros(), copy_started.elapsed().as_micros(), copied.generation);
        assert!(view.generation > previous);
        assert!(matches!(
            ack(&mut client, &captured),
            Response::ViewState {
                resync_required: true,
                ..
            }
        ));
        previous = view.generation;
    }
    assert!(fixture.generation() > previous);
    assert!(!fixture.host.inspect().unwrap().backend_fault);
}

#[test]
fn watch_deadlines_rate_and_reconnect_require_explicit_bounded_state() {
    let fixture = Fixture::new();
    let mut client = fixture.viewer();
    let captured = begin(&mut client);
    let _ = read(&mut client, &captured);
    let _ = ack(&mut client, &captured);
    let generation = captured.generation;
    assert!(matches!(
        client
            .request(&Request::WatchAfter {
                generation,
                timeout_ms: 751
            })
            .unwrap(),
        Response::Error { .. }
    ));
    assert!(matches!(
        client
            .request(&Request::WatchAfter {
                generation,
                timeout_ms: 0
            })
            .unwrap(),
        Response::ViewState { .. }
    ));
    let mut limited = Latest::new(&fixture.host.view).unwrap();
    let current = fixture.generation();
    limited.acknowledge(current, &fixture.host.view).unwrap();
    limited.next_request = Instant::now() + Duration::from_secs(1);
    assert_eq!(
        limited.wait(current, 0, &fixture.host.view).unwrap_err(),
        "watch_request_cooldown"
    );
    drop(client);
    let mut reconnected = fixture.viewer();
    assert!(matches!(
        reconnected
            .request(&Request::WatchAfter {
                generation,
                timeout_ms: 0
            })
            .unwrap(),
        Response::Error { .. }
    ));
    let fresh = begin(&mut reconnected);
    let _ = read(&mut reconnected, &fresh);
    assert!(matches!(
        ack(&mut reconnected, &fresh),
        Response::ViewState { .. }
    ));
}

#[test]
#[ignore = "private synthetic child launched only by its fixture"]
fn typed_view_child_fixture() {
    let root = PathBuf::from(
        std::env::var_os("REMUDA_TYPED_VIEW_TEST_ROOT").expect("private child fixture only"),
    );
    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    assert_eq!(unsafe { libc::tcgetattr(0, termios.as_mut_ptr()) }, 0);
    let mut termios = unsafe { termios.assume_init() };
    unsafe {
        libc::cfmakeraw(&mut termios);
    }
    assert_eq!(unsafe { libc::tcsetattr(0, libc::TCSANOW, &termios) }, 0);
    let mut output = std::io::stdout().lock();
    output.write_all(b"\x1b[2J\x1b[Htyped-ready\r\n").unwrap();
    output.flush().unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join("ready"))
        .unwrap();
    let mut input = std::io::stdin().lock();
    let mut command = Vec::new();
    let mut byte = [0];
    while input.read_exact(&mut byte).is_ok() {
        if matches!(byte[0], b'\r' | b'\n') {
            if command == b"exit" {
                return;
            }
            if command == b"continuous" {
                continuous(&mut input, &mut output, &root);
            } else {
                output.write_all(b"\x1b[32mnew output\x1b[0m\r\n").unwrap();
                output.flush().unwrap();
            }
            command.clear();
        } else {
            assert!(command.len() < 64);
            command.push(byte[0]);
        }
    }
}
fn continuous(input: &mut impl Read, output: &mut impl Write, root: &std::path::Path) {
    let until = Instant::now() + Duration::from_secs(5);
    let mut count = 0u64;
    while Instant::now() < until {
        writeln!(
            output,
            "\x1b[33mrow:{count:08}:wide:界:abcdefghijklmnopqrstuvwxyz\x1b[0m\r"
        )
        .unwrap();
        count += 1;
        if count.is_multiple_of(1024) {
            output.write_all(b"\x1b[6n").unwrap();
            output.flush().unwrap();
            let mut byte = [0];
            for _ in 0..64 {
                input.read_exact(&mut byte).unwrap();
                if byte[0] == b'R' {
                    break;
                }
            }
            assert_eq!(byte[0], b'R');
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(root.join("queries"))
                .unwrap()
                .write_all(count.to_string().as_bytes())
                .unwrap();
        }
    }
    output.flush().unwrap();
}
