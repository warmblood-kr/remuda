//! [DIAGNOSTIC, not a fix] Out-of-process counterpart to steps/030's
//! same-thread `Hold::drop` hang. Every diagnostic test to date runs the
//! daemon **in-process** (a spawned thread inside the test binary). Real
//! usage always runs it as its own OS process (`bin/remuda.rs`'s
//! `start_daemon`) — a structural difference nobody had tested yet. See
//! steps/030 for the full investigation and the prediction this run
//! pre-registers against.

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(10);

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remuda-oop-{}-{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// A daemon as its own OS PROCESS — the real shape (`bin/remuda.rs`'s
/// `start_daemon`), copied from `tests/daemon.rs`'s own `Daemon`. Every
/// prior diagnostic test used an in-process thread instead.
struct Daemon(std::process::Child);

impl Daemon {
    fn spawn(dir: &Path) -> Self {
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s", "daemon"])
            .env("REMUDA_RUNTIME_DIR", dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn daemon");
        let path = daemon::socket_path_in(dir, "s");
        let deadline = Instant::now() + PATIENCE;
        while remuda_native::ipc::connect(&path).is_err() {
            assert!(Instant::now() < deadline, "daemon never bound {path:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        Self(child)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn new_session(path: &Path, name: &str) {
    let response = client::request(
        path,
        &Request::New {
            name: Some(name.to_string()),
            command: vec!["sh".into()],
            size: Size::new(80, 24),
        },
    )
    .expect("new");
    assert_eq!(
        response,
        Response::Value(name.to_string()),
        "New answers with the name it gave the session"
    );
}

// ---- shared stage registry — same shape as native/src/tui.rs's test module,
// duplicated here since each `tests/*.rs` file compiles as its own binary. ----

struct StageRegistry(Mutex<Vec<(&'static str, Arc<AtomicUsize>)>>);
static STAGE_REGISTRY: OnceLock<StageRegistry> = OnceLock::new();

fn stage_registry() -> &'static StageRegistry {
    STAGE_REGISTRY.get_or_init(|| StageRegistry(Mutex::new(Vec::new())))
}

fn register_stage(name: &'static str) -> Arc<AtomicUsize> {
    let stage = Arc::new(AtomicUsize::new(0));
    stage_registry()
        .0
        .lock()
        .unwrap()
        .push((name, stage.clone()));
    stage
}

fn deregister_stage(name: &'static str) {
    stage_registry()
        .0
        .lock()
        .unwrap()
        .retain(|(n, _)| *n != name);
}

fn dump_all_stages() -> String {
    let entries = stage_registry().0.lock().unwrap();
    let mut out = String::new();
    for (name, stage) in entries.iter() {
        out.push_str(&format!(
            "STUCK: {name} AFTER STAGE {}\n",
            stage.load(Ordering::SeqCst)
        ));
    }
    out
}

struct Finished(Arc<AtomicBool>, &'static str);
impl Drop for Finished {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
        deregister_stage(self.1);
    }
}

fn spawn_watchdog(finished: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        for _ in 0..300 {
            std::thread::sleep(Duration::from_millis(200));
            if finished.load(Ordering::SeqCst) {
                return;
            }
        }
        let dump = dump_all_stages();
        let _ = std::io::stderr().write_all(dump.as_bytes());
        std::process::exit(101);
    });
}

/// Out-of-process counterpart of `hold_a_drop_a_hold_b_drop_b_same_thread`
/// (native/src/tui.rs): bare `hold A -> drop A -> hold B -> drop B`, all on
/// the test thread, but against a REAL spawned daemon process.
#[test]
fn out_of_process_hold_a_drop_a_hold_b_drop_b_same_thread() {
    let stage = register_stage("out_of_process_same_thread");
    let finished = Arc::new(AtomicBool::new(false));
    let _sentinel = Finished(finished.clone(), "out_of_process_same_thread");
    spawn_watchdog(finished);

    let dir = scratch_dir("same-thread");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = Daemon::spawn(&dir);
    new_session(&path, "a");
    new_session(&path, "b");

    let hold_a = client::hold(&path, "a").unwrap();
    stage.store(1, Ordering::SeqCst);

    drop(hold_a);
    stage.store(2, Ordering::SeqCst);

    let hold_b = client::hold(&path, "b").unwrap();
    stage.store(3, Ordering::SeqCst);

    drop(hold_b);
    stage.store(4, Ordering::SeqCst);
}

/// Out-of-process paired comparison: identical sequence, but hold/drop
/// wrapped in a spawned thread + `recv_timeout` watchdog, mirroring the
/// in-process `dropping_one_sessions_hold_then_holding_another_does_not_hang`
/// (native/src/tui.rs) — against a real spawned daemon process.
#[test]
fn out_of_process_hold_a_drop_a_hold_b_drop_b_cross_thread() {
    let dir = scratch_dir("cross-thread");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = Daemon::spawn(&dir);
    new_session(&path, "a");
    new_session(&path, "b");

    let hold_a = client::hold(&path, "a").unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(hold_a);
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(Duration::from_secs(5)).is_ok(),
        "STAGE 1: Hold::drop for session A did not return within 5s"
    );

    let path2 = path.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let hold_b = client::hold(&path2, "b");
        let _ = tx.send(hold_b.is_ok());
    });
    assert!(
        rx.recv_timeout(Duration::from_secs(5))
            .expect("STAGE 2: client::hold for session B did not return within 5s"),
        "STAGE 2: hold B must succeed once A's is released"
    );
}

/// Same shape as the same-thread test above, but with a 1s pause between
/// hold and drop — a settled screen, the way a human pauses before
/// detaching, rather than dropping immediately after attaching.
#[test]
fn out_of_process_settled_drop_does_not_hang_immediately() {
    let stage = register_stage("out_of_process_settled");
    let finished = Arc::new(AtomicBool::new(false));
    let _sentinel = Finished(finished.clone(), "out_of_process_settled");
    spawn_watchdog(finished);

    let dir = scratch_dir("settled");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = Daemon::spawn(&dir);
    new_session(&path, "a");
    new_session(&path, "b");

    let hold_a = client::hold(&path, "a").unwrap();
    stage.store(1, Ordering::SeqCst);

    std::thread::sleep(Duration::from_secs(1));

    drop(hold_a);
    stage.store(2, Ordering::SeqCst);

    let hold_b = client::hold(&path, "b").unwrap();
    stage.store(3, Ordering::SeqCst);

    drop(hold_b);
    stage.store(4, Ordering::SeqCst);
}
