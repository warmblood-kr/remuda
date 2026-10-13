//! Bounded startup publication regressions; no hooks exist in ordinary builds.

use super::*;
use crate::staged_socket::startup::{Boundary, Controls, Failure, Pause};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = PathBuf::from("/tmp").join(format!(
            "r660-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
    fn public(&self) -> PathBuf {
        self.0.join("public")
    }
    fn entries(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap() != "public.lock")
            .collect()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type StartupResult = std::io::Result<(Listener, Arc<SocketOwnership>)>;

/// Test-only seam: the staged transaction, imported the way slice 3 will wire
/// it into startup. Production `serve_inner` still uses the legacy path.
fn staged_startup_listener(path: &Path, controls: &Controls) -> StartupResult {
    let opened = crate::staged_socket::startup::open(path, controls)?;
    let listener = Listener::UdSocket(opened.listener.into());
    listener.set_nonblocking(ListenerNonblockingMode::Accept)?;
    let owner = SocketOwnership {
        path: path.to_path_buf(),
        identity: Mutex::new((opened.dev, opened.ino)),
    };
    Ok((listener, Arc::new(owner)))
}
struct Harness {
    deadline: Instant,
    reached: Receiver<PathBuf>,
    release: Option<Sender<()>>,
    completion: Receiver<StartupResult>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Harness {
    fn start(path: PathBuf, boundary: Boundary, failure: Option<Failure>) -> Self {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (reached_tx, reached) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (completed, completion) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let controls = Controls {
                pause: Some(Pause {
                    boundary,
                    reached: reached_tx,
                    release: release_rx,
                    deadline,
                }),
                failure,
                ..Controls::default()
            };
            let result = SocketLock::acquire(&path, None)
                .and_then(|_lock| staged_startup_listener(&path, &controls));
            let _ = completed.send(result);
        });
        Self {
            deadline,
            reached,
            release: Some(release),
            completion,
            worker: Some(worker),
        }
    }
    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
    fn reached(&self) -> PathBuf {
        self.reached.recv_timeout(self.remaining()).unwrap()
    }
    fn finish(&mut self) -> StartupResult {
        self.release.take().unwrap().send(()).unwrap();
        let result = self.completion.recv_timeout(self.remaining()).unwrap();
        self.worker.take().unwrap().join().unwrap();
        result
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if self.worker.is_some() && self.completion.recv_timeout(self.remaining()).is_ok() {
            let _ = self.worker.take().unwrap().join();
        }
    }
}

fn socket_fd(listener: &Listener) -> i32 {
    let Listener::UdSocket(socket) = listener;
    socket.as_fd().as_raw_fd()
}

fn assert_private_window(boundary: Boundary) {
    let scratch = Scratch::new();
    let public = scratch.public();
    let mut harness = Harness::start(public.clone(), boundary, None);
    let staged = harness.reached();
    assert!(
        ipc::connect(&public).is_err(),
        "public endpoint accepts before publication"
    );
    assert_ne!(staged, public);
    let (listener, owner) = harness.finish().unwrap();
    assert!(owner.owns_path());
    assert_eq!(
        std::fs::symlink_metadata(&public).unwrap().mode() & 0o777,
        0o600
    );
    assert_eq!(scratch.entries(), vec![public.clone()]);
    let deadline = harness.deadline;
    let server = std::thread::spawn(move || {
        let Listener::UdSocket(ref socket) = listener;
        let mut poll = libc::pollfd {
            fd: socket.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            unsafe {
                libc::poll(
                    &mut poll,
                    1,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            } > 0
        );
        // Accept is nonblocking; BSD-derived kernels let the accepted stream
        // inherit that, so the daemon's accept loop normalizes it to blocking
        // for request streams (daemon.rs `stream.set_nonblocking(false)`).
        assert_ne!(
            unsafe { libc::fcntl(socket_fd(&listener), libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        let stream = listener.accept().unwrap();
        stream.set_nonblocking(false).unwrap();
        let Stream::UdSocket(ref socket) = stream;
        let raw = socket.as_fd().as_raw_fd();
        assert_ne!(
            unsafe { libc::fcntl(raw, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        assert_eq!(
            unsafe { libc::fcntl(raw, libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout = libc::timeval {
            tv_sec: remaining.as_secs() as _,
            tv_usec: remaining.subsec_micros() as _,
        };
        for option in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        raw,
                        libc::SOL_SOCKET,
                        option,
                        (&timeout as *const libc::timeval).cast(),
                        std::mem::size_of_val(&timeout) as _,
                    )
                },
                0
            );
        }
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        assert!(matches!(
            read_request(&stream, &mut reader).unwrap(),
            Some(Request::Version)
        ));
        reply(&stream, &Response::Value(crate::dist::BUILD_VERSION.into())).unwrap();
    });
    let response =
        crate::client::request_with_timeout(&public, &Request::Version, harness.remaining())
            .unwrap();
    assert!(matches!(response, Response::Value(_)));
    server.join().unwrap();
    drop(owner);
    assert!(!public.exists());
}

#[test]
fn staged_startup_bound_window_is_private_and_renamed_route_replies() {
    assert_private_window(Boundary::Bound);
}
#[test]
fn staged_startup_listening_window_is_private_and_renamed_route_replies() {
    assert_private_window(Boundary::Listening);
}

#[test]
fn staged_startup_persistent_capture_failure_preserves_unknown_residue() {
    let scratch = Scratch::new();
    let mut harness = Harness::start(
        scratch.public(),
        Boundary::Bound,
        Some(Failure::CapturePersistent),
    );
    let staged = harness.reached();
    assert!(harness.finish().is_err());
    assert!(!scratch.public().exists());
    assert!(std::fs::symlink_metadata(staged)
        .unwrap()
        .file_type()
        .is_socket());
}

#[test]
fn staged_startup_capture_recovers_once() {
    let scratch = Scratch::new();
    let mut harness = Harness::start(
        scratch.public(),
        Boundary::Bound,
        Some(Failure::CaptureOnce),
    );
    harness.reached();
    let resources = harness.finish().unwrap();
    assert!(resources.1.owns_path());
    drop(resources);
    assert!(scratch.entries().is_empty());
}

#[test]
fn staged_startup_rename_error_cleans_only_staging() {
    let scratch = Scratch::new();
    let mut harness = Harness::start(scratch.public(), Boundary::Listening, Some(Failure::Rename));
    harness.reached();
    assert!(harness.finish().is_err());
    assert!(scratch.entries().is_empty());
}

#[test]
fn staged_startup_later_failure_uses_terminal_published_cleanup() {
    let scratch = Scratch::new();
    let mut harness = Harness::start(
        scratch.public(),
        Boundary::Listening,
        Some(Failure::AfterPublish),
    );
    harness.reached();
    assert!(harness.finish().is_err());
    assert!(scratch.entries().is_empty());
}

fn replacement_preserved(kind: &str, staged_source: bool) {
    let scratch = Scratch::new();
    let mut harness = Harness::start(scratch.public(), Boundary::Listening, None);
    let leaf = harness.reached();
    let replacement = if staged_source {
        leaf
    } else {
        scratch.public()
    };
    let original = replacement.with_file_name("original");
    if staged_source {
        std::fs::rename(&replacement, &original).unwrap();
    }
    let socket = match kind {
        "socket" => Some(std::os::unix::net::UnixListener::bind(&replacement).unwrap()),
        "file" => {
            std::fs::write(&replacement, b"sentinel").unwrap();
            None
        }
        "symlink" => {
            std::os::unix::fs::symlink("missing-target", &replacement).unwrap();
            None
        }
        _ => unreachable!(),
    };
    let before = std::fs::symlink_metadata(&replacement).unwrap();
    assert!(harness.finish().is_err());
    let after = std::fs::symlink_metadata(&replacement).unwrap();
    assert_eq!(
        (before.dev(), before.ino(), before.mode()),
        (after.dev(), after.ino(), after.mode())
    );
    match kind {
        "file" => assert_eq!(std::fs::read(&replacement).unwrap(), b"sentinel"),
        "symlink" => assert_eq!(
            std::fs::read_link(&replacement).unwrap(),
            Path::new("missing-target")
        ),
        _ => {}
    }
    if staged_source {
        assert!(!scratch.public().exists());
    }
    drop(socket);
}

#[test]
fn staged_startup_preserves_replacement_socket() {
    replacement_preserved("socket", true);
}
#[test]
fn staged_startup_preserves_replacement_file() {
    replacement_preserved("file", true);
}
#[test]
fn staged_startup_preserves_replacement_symlink() {
    replacement_preserved("symlink", true);
}
#[test]
fn staged_startup_preserves_occupied_socket() {
    replacement_preserved("socket", false);
}
#[test]
fn staged_startup_preserves_occupied_file() {
    replacement_preserved("file", false);
}
#[test]
fn staged_startup_preserves_occupied_symlink() {
    replacement_preserved("symlink", false);
}

#[test]
fn staged_startup_over_budget_path_mutates_nothing() {
    let scratch = Scratch::new();
    // Public path fits; the staging path (extra component + leaf) does not.
    let room = 107usize.saturating_sub(scratch.0.as_os_str().len() + 1);
    let public = scratch.0.join("p".repeat(room));
    let controls = Controls::default();
    assert!(staged_startup_listener(&public, &controls).is_err());
    assert!(scratch.entries().is_empty());
}
