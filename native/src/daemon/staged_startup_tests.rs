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
    let opened = crate::staged_socket::startup::open(path, controls).map_err(|e| e.error)?;
    // Install published authority first: a later import error cleans it up.
    let owner = SocketOwnership {
        path: path.to_path_buf(),
        identity: Mutex::new((opened.dev, opened.ino)),
    };
    let listener = Listener::UdSocket(opened.listener.into());
    if controls.failure == Some(Failure::Import) {
        return Err(std::io::Error::other("injected import failure"));
    }
    listener.set_nonblocking(ListenerNonblockingMode::Accept)?;
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
    use crate::staged_socket::SOCKET_PATH_LIMIT;
    let scratch = Scratch::new();
    // Public path sits exactly at the limit; the staging path (extra stage
    // component) cannot fit, so preflight must refuse before any mutation.
    let dir_len = SOCKET_PATH_LIMIT - scratch.0.as_os_str().len() - "/".len() - "/p".len();
    let dir = scratch.0.join("d".repeat(dir_len));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    let public = dir.join("p");
    assert_eq!(public.as_os_str().len(), SOCKET_PATH_LIMIT);
    assert!(staged_startup_listener(&public, &Controls::default()).is_err());
    assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
}

fn open_with(
    scratch: &Scratch,
    failure: Failure,
) -> Result<crate::staged_socket::startup::Opened, crate::staged_socket::startup::OpenError> {
    let controls = Controls {
        failure: Some(failure),
        ..Controls::default()
    };
    crate::staged_socket::startup::open(&scratch.public(), &controls)
}

#[test]
fn staged_startup_failure_before_leaf_removes_only_the_empty_directory() {
    for failure in [Failure::StageOpen, Failure::Bind] {
        let scratch = Scratch::new();
        let error = open_with(&scratch, failure).err().unwrap();
        assert!(error.residue.is_empty(), "{failure:?}: {:?}", error.residue);
        assert!(scratch.entries().is_empty(), "{failure:?}");
    }
}

#[test]
fn staged_startup_clone_failure_removes_receipted_leaf_and_directory() {
    let scratch = Scratch::new();
    let error = open_with(&scratch, Failure::Clone).err().unwrap();
    assert!(error.residue.is_empty(), "{:?}", error.residue);
    assert!(scratch.entries().is_empty());
}

#[test]
fn staged_startup_persistent_capture_failure_reports_residue() {
    let scratch = Scratch::new();
    let error = open_with(&scratch, Failure::CapturePersistent)
        .err()
        .unwrap();
    assert!(error
        .residue
        .iter()
        .any(|r| r.contains("unreceipted socket")));
    assert!(error.residue.iter().any(|r| r.contains("not empty")));
    assert_eq!(scratch.entries().len(), 1);
}

#[test]
fn staged_startup_unlink_failure_after_publish_is_reported() {
    let scratch = Scratch::new();
    let error = open_with(&scratch, Failure::AfterPublishUnlink)
        .err()
        .unwrap();
    assert!(error
        .residue
        .iter()
        .any(|r| r.contains(scratch.public().to_str().unwrap())));
    assert!(std::fs::symlink_metadata(scratch.public())
        .unwrap()
        .file_type()
        .is_socket());
    assert_eq!(scratch.entries(), vec![scratch.public()]);
}

#[test]
fn staged_startup_post_commit_rmdir_failure_keeps_generation_active() {
    let scratch = Scratch::new();
    let opened = open_with(&scratch, Failure::PostCommitRmdir).ok().unwrap();
    assert!(opened
        .residue
        .iter()
        .any(|r| r.contains("staging directory")));
    assert!(ipc::connect(&scratch.public()).is_ok());
    assert_eq!(scratch.entries().len(), 2);
}

#[test]
fn staged_startup_import_failure_cleans_published_socket() {
    let scratch = Scratch::new();
    let controls = Controls {
        failure: Some(Failure::Import),
        ..Controls::default()
    };
    assert!(staged_startup_listener(&scratch.public(), &controls).is_err());
    assert!(scratch.entries().is_empty());
}

#[test]
fn staged_startup_unsupported_and_cross_device_preserve_occupied_destination() {
    for failure in [Failure::RenameUnsupported, Failure::RenameCrossDevice] {
        let scratch = Scratch::new();
        std::fs::write(scratch.public(), b"sentinel").unwrap();
        let error = open_with(&scratch, failure).err().unwrap();
        assert!(error.residue.is_empty(), "{failure:?}: {:?}", error.residue);
        assert_eq!(std::fs::read(scratch.public()).unwrap(), b"sentinel");
        assert_eq!(scratch.entries(), vec![scratch.public()], "{failure:?}");
    }
}
