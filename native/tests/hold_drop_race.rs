//! [TESTS ONLY, no fix] Measures the Windows same-thread `Hold::drop` race
//! from steps/029: a pause sweep to bound the threshold, and a reader-side
//! injection hook that forces the race regardless of caller timing — the
//! test a real structural fix must pass and a timing-narrowing band-aid
//! cannot. See steps/029 for the full mechanism reasoning.
//!
//! Every test that could hang runs its `hold`/`drop` sequence on a spawned
//! WORKER thread and waits on a channel with a bounded `recv_timeout`
//! instead of blocking directly — a stuck worker fails that one test
//! normally (an ordinary assertion failure) and leaves every sibling test,
//! and every later `cargo test` target, free to report its own result. An
//! earlier run in this investigation lost its entire measurement by using
//! `std::process::exit` as the normal path for an expected hang; a leaked,
//! permanently-parked worker thread does not stop the process from
//! finishing normally once every `#[test]` fn has returned.

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(10);

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remuda-race-{}-{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn scratch(tag: &str) -> PathBuf {
    daemon::socket_path_in(&scratch_dir(tag), "s")
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// An in-process daemon, on its own thread. Returns once it actually answers.
fn daemon_at(path: &Path) -> impl Drop {
    let serving = path.to_path_buf();
    std::thread::spawn(move || {
        let _ = daemon::serve(&serving);
    });
    let deadline = Instant::now() + PATIENCE;
    while remuda_native::ipc::connect(path).is_err() {
        assert!(Instant::now() < deadline, "daemon never bound {path:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    Cleanup(path.to_path_buf())
}

/// A daemon as its own OS process — the real shape (`bin/remuda.rs`'s
/// `start_daemon`), copied from `tests/daemon.rs`'s own `Daemon`.
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

/// Last-resort backstop only — never the expected path for a predicted red.
/// Arms once; if the whole binary is somehow still alive 5 minutes from now
/// (far longer than the sum of every test's own bounded timeout), something
/// is wedged well beyond what this file is measuring.
static BACKSTOP: Once = Once::new();
fn arm_backstop() {
    BACKSTOP.call_once(|| {
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(300));
            let _ = std::io::stderr()
                .write_all(b"BACKSTOP: hold_drop_race still running after 5 minutes\n");
            std::process::exit(101);
        });
    });
}

/// `hold -> pause -> drop`, both on ONE spawned worker thread (same thread
/// creates and drops — the shape under test), while the caller waits with a
/// bounded timeout instead of blocking directly.
fn hold_pause_drop(
    path: PathBuf,
    name: &'static str,
    pause: Duration,
    drain_delay: Duration,
) -> Result<(), String> {
    arm_backstop();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let hold = if drain_delay.is_zero() {
            client::hold(&path, name)
        } else {
            client::hold_with_drain_delay(&path, name, drain_delay)
        };
        let hold = match hold {
            Ok(h) => h,
            Err(e) => {
                let _ = tx.send(Err(format!("hold failed: {e}")));
                return;
            }
        };
        if !pause.is_zero() {
            std::thread::sleep(pause);
        }
        drop(hold);
        let _ = tx.send(Ok(()));
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(r) => r,
        Err(_) => Err("same-thread hold/drop did not complete within 10s".to_string()),
    }
}

/// `hold` on the test thread, `drop` wrapped in its own spawned thread — the
/// cross-thread shape that has passed every time so far.
fn cross_thread_hold_drop(
    path: PathBuf,
    name: &'static str,
    drain_delay: Duration,
) -> Result<(), String> {
    arm_backstop();
    let hold = if drain_delay.is_zero() {
        client::hold(&path, name)
    } else {
        client::hold_with_drain_delay(&path, name, drain_delay)
    }
    .map_err(|e| format!("hold failed: {e}"))?;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(hold);
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| "cross-thread drop did not complete within 5s".to_string())
}

/// `cycles` immediate hold/drop round trips, alternating between two
/// already-started sessions, all on one spawned worker thread. Alternating
/// (rather than reusing one name) gives the daemon a full cycle to notice
/// each drop's disconnect before that same name is attached again — a
/// daemon-side bookkeeping gap, unrelated to the client-side race under
/// test, that a same-name loop hits immediately. `progress` lets a timeout
/// say which cycle it stuck on.
fn hold_drop_loop(
    path: PathBuf,
    a: &'static str,
    b: &'static str,
    cycles: usize,
) -> Result<(), String> {
    arm_backstop();
    let progress = Arc::new(AtomicUsize::new(0));
    let progress_reader = progress.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for i in 0..cycles {
            let name = if i % 2 == 0 { a } else { b };
            // Alternating sessions gives the daemon a cycle to notice each
            // drop's disconnect, but under heavy parallel load (many other
            // tests in this binary contending for CPU) that is not always
            // enough — a bounded retry on the daemon's own "still attached"
            // bookkeeping window, not on the client-side race under test.
            let retry_deadline = Instant::now() + Duration::from_secs(5);
            let hold = loop {
                match client::hold(&path, name) {
                    Ok(h) => break h,
                    Err(e) if Instant::now() < retry_deadline => {
                        std::thread::sleep(Duration::from_millis(5));
                        let _ = e;
                        continue;
                    }
                    Err(e) => {
                        let _ = tx.send(Err(format!("hold failed at cycle {i} ({name}): {e}")));
                        return;
                    }
                }
            };
            drop(hold);
            progress.store(i + 1, Ordering::SeqCst);
        }
        let _ = tx.send(Ok(()));
    });
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(r) => r,
        Err(_) => Err(format!(
            "stuck at cycle {} of {cycles}",
            progress_reader.load(Ordering::SeqCst)
        )),
    }
}

macro_rules! pause_test_in_process {
    ($fn_name:ident, $ms:literal) => {
        #[test]
        fn $fn_name() {
            let path = scratch(concat!("pause-in-", stringify!($ms)));
            let _daemon = daemon_at(&path);
            new_session(&path, "a");
            let result = hold_pause_drop(path, "a", Duration::from_millis($ms), Duration::ZERO);
            assert!(
                result.is_ok(),
                "pause={}ms same-thread hold/drop (in-process): {}",
                $ms,
                result.unwrap_err()
            );
        }
    };
}

macro_rules! pause_test_out_of_process {
    ($fn_name:ident, $ms:literal) => {
        #[test]
        fn $fn_name() {
            let dir = scratch_dir(concat!("pause-oop-", stringify!($ms)));
            let path = daemon::socket_path_in(&dir, "s");
            let _daemon = Daemon::spawn(&dir);
            new_session(&path, "a");
            let result = hold_pause_drop(path, "a", Duration::from_millis($ms), Duration::ZERO);
            assert!(
                result.is_ok(),
                "pause={}ms same-thread hold/drop (out-of-process): {}",
                $ms,
                result.unwrap_err()
            );
        }
    };
}

pause_test_in_process!(pause_0ms_in_process, 0);
pause_test_in_process!(pause_10ms_in_process, 10);
pause_test_in_process!(pause_50ms_in_process, 50);
pause_test_in_process!(pause_100ms_in_process, 100);
pause_test_in_process!(pause_250ms_in_process, 250);
pause_test_in_process!(pause_500ms_in_process, 500);
pause_test_in_process!(pause_1000ms_in_process, 1000);

pause_test_out_of_process!(pause_0ms_out_of_process, 0);
pause_test_out_of_process!(pause_10ms_out_of_process, 10);
pause_test_out_of_process!(pause_50ms_out_of_process, 50);
pause_test_out_of_process!(pause_100ms_out_of_process, 100);
pause_test_out_of_process!(pause_250ms_out_of_process, 250);
pause_test_out_of_process!(pause_500ms_out_of_process, 500);
pause_test_out_of_process!(pause_1000ms_out_of_process, 1000);

#[test]
fn immediate_hold_drop_loop_in_process() {
    let path = scratch("loop-in-process");
    let _daemon = daemon_at(&path);
    new_session(&path, "a");
    new_session(&path, "b");
    let result = hold_drop_loop(path, "a", "b", 50);
    assert!(result.is_ok(), "{}", result.unwrap_err());
}

#[test]
fn immediate_hold_drop_loop_out_of_process() {
    let dir = scratch_dir("loop-oop");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = Daemon::spawn(&dir);
    new_session(&path, "a");
    new_session(&path, "b");
    let result = hold_drop_loop(path, "a", "b", 50);
    assert!(result.is_ok(), "{}", result.unwrap_err());
}

/// The existing control shape, kept as a regression baseline: 5-for-5 on
/// real Windows CI across every earlier run in this investigation.
#[test]
fn cross_thread_hold_drop_plain() {
    let path = scratch("cross-thread-plain");
    let _daemon = daemon_at(&path);
    new_session(&path, "a");
    let result = cross_thread_hold_drop(path, "a", Duration::ZERO);
    assert!(result.is_ok(), "{}", result.unwrap_err());
}

/// [EXPECTED RED before the fix] Control on the injection hook itself: a 2s
/// reader delay should defeat even cross-thread's own incidental scheduling
/// latency. If this ever passes without a fix, the hook has no teeth and a
/// green run under the real fix would prove nothing. See steps/029.
#[test]
fn cross_thread_hold_drop_with_injection() {
    let path = scratch("cross-thread-injected");
    let _daemon = daemon_at(&path);
    new_session(&path, "a");
    let result = cross_thread_hold_drop(path, "a", Duration::from_secs(2));
    assert!(result.is_ok(), "{}", result.unwrap_err());
}

/// [EXPECTED RED before the fix] The real discriminator: the reader is
/// deliberately delayed regardless of the caller's own timing, so only a
/// structural fix — not a caller-side pause of any length — can pass this.
/// See steps/029.
#[test]
fn same_thread_hold_drop_with_injection() {
    let path = scratch("same-thread-injected");
    let _daemon = daemon_at(&path);
    new_session(&path, "a");
    let result = hold_pause_drop(path, "a", Duration::ZERO, Duration::from_secs(2));
    assert!(result.is_ok(), "{}", result.unwrap_err());
}
