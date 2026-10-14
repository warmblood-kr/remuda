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

/// A listener imported from the staged transaction, plus private residue.
struct Imported {
    listener: Listener,
    owner: Arc<SocketOwnership>,
    residue: Vec<String>,
}

/// Primary failure plus every entry left behind, kept through the importer.
#[derive(Debug)]
struct SeamError {
    error: std::io::Error,
    residue: Vec<String>,
}
impl From<std::io::Error> for SeamError {
    fn from(error: std::io::Error) -> Self {
        Self {
            error,
            residue: Vec::new(),
        }
    }
}
type StartupResult = Result<Imported, SeamError>;

/// Explicit, fallible removal of the published socket after an import error;
/// the legacy `SocketOwnership` destructor stays only as a fallback.
fn abort_published(
    path: &Path,
    opened: (u64, u64),
    inject_failure: bool,
    residue: &mut Vec<String>,
) {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(m) if (m.dev(), m.ino()) == opened => {
            let removed = if inject_failure {
                Err(std::io::Error::other("injected published unlink failure"))
            } else {
                std::fs::remove_file(path)
            };
            if let Err(error) = removed {
                residue.push(format!("published socket {}: {error}", path.display()));
            }
        }
        Ok(_) => residue.push(format!(
            "published socket {}: changed, preserved",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => residue.push(format!("published socket {}: {error}", path.display())),
    }
}

/// Test-only seam: the staged transaction, imported the way slice 3 will wire
/// it into startup. Production `serve_inner` still uses the legacy path.
fn staged_startup_listener(path: &Path, controls: &Controls) -> StartupResult {
    let opened = crate::staged_socket::startup::open(path, controls).map_err(|e| SeamError {
        error: e.error,
        residue: e.residue,
    })?;
    let mut residue = opened.residue;
    // Install published authority first so a panic still cleans up.
    let owner = SocketOwnership {
        path: path.to_path_buf(),
        identity: Mutex::new((opened.dev, opened.ino)),
    };
    let listener = Listener::UdSocket(opened.listener.into());
    let imported = if matches!(
        controls.failure,
        Some(Failure::Import | Failure::ImportCleanup)
    ) {
        Err(std::io::Error::other("injected import failure"))
    } else {
        listener.set_nonblocking(ListenerNonblockingMode::Accept)
    };
    if let Err(error) = imported {
        drop(listener);
        let inject = controls.failure == Some(Failure::ImportCleanup);
        abort_published(path, (opened.dev, opened.ino), inject, &mut residue);
        return Err(SeamError { error, residue });
    }
    Ok(Imported {
        listener,
        owner: Arc::new(owner),
        residue,
    })
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
                .map_err(SeamError::from)
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
    let Imported {
        listener, owner, ..
    } = harness.finish().unwrap();
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
    assert!(resources.owner.owns_path());
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

#[test]
fn staged_startup_finalizes_owner_mode_without_touching_umask() {
    for mode in [0o000, 0o500, 0o600] {
        let scratch = Scratch::new();
        let controls = Controls {
            directory_mode: Some(mode),
            ..Controls::default()
        };
        let opened = crate::staged_socket::startup::open(&scratch.public(), &controls)
            .map_err(|e| e.error)
            .unwrap_or_else(|e| panic!("mode {mode:o}: {e}"));
        assert!(opened.residue.is_empty(), "{:?}", opened.residue);
        assert_eq!(scratch.entries(), vec![scratch.public()]);
    }
}

#[test]
fn staged_startup_rejects_bad_leaf_before_any_mutation() {
    let scratch = Scratch::new();
    let nul = scratch.0.join("a\0b");
    let long = scratch.0.join("x".repeat(300));
    for public in [nul, long] {
        assert!(staged_startup_listener(&public, &Controls::default()).is_err());
        assert!(scratch.entries().is_empty());
    }
}

/// Staging path is `<dir>/.r660-<8hex>-<16hex>/s` = dir + 1 + 31 + 2 bytes.
fn exact_budget_dir(scratch: &Scratch, glyph: &str, slack: usize) -> PathBuf {
    use crate::staged_socket::SOCKET_PATH_LIMIT;
    let fixed =
        scratch.0.as_os_str().len() + "/".len() + "/.r660-00000000-0000000000000000/s".len();
    let room = SOCKET_PATH_LIMIT + slack - fixed;
    let dir = scratch.0.join(format!(
        "{}{}",
        glyph.repeat(room / glyph.len()),
        "d".repeat(room % glyph.len())
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    dir
}

#[test]
fn staged_startup_binds_at_the_exact_byte_budget() {
    for glyph in ["d", "é"] {
        let scratch = Scratch::new();
        let dir = exact_budget_dir(&scratch, glyph, 0);
        let public = dir.join("p");
        let opened = crate::staged_socket::startup::open(&public, &Controls::default())
            .map_err(|e| e.error)
            .unwrap();
        assert!(ipc::connect(&public).is_ok(), "{glyph}");
        drop(opened);
        let scratch_over = Scratch::new();
        let over = exact_budget_dir(&scratch_over, glyph, 1);
        let refused = crate::staged_socket::startup::open(&over.join("p"), &Controls::default());
        assert!(refused.is_err(), "{glyph}");
        assert!(std::fs::read_dir(&over).unwrap().next().is_none());
    }
}

#[test]
fn staged_startup_seam_keeps_failure_residue() {
    let scratch = Scratch::new();
    let controls = Controls {
        failure: Some(Failure::AfterPublishUnlink),
        ..Controls::default()
    };
    let error = staged_startup_listener(&scratch.public(), &controls)
        .err()
        .unwrap();
    assert!(error.residue.iter().any(|r| r.contains("published")));
}

#[test]
fn staged_startup_seam_keeps_post_commit_residue_with_listener() {
    let scratch = Scratch::new();
    let controls = Controls {
        failure: Some(Failure::PostCommitRmdir),
        ..Controls::default()
    };
    let imported = staged_startup_listener(&scratch.public(), &controls)
        .ok()
        .unwrap();
    assert!(imported
        .residue
        .iter()
        .any(|r| r.contains("staging directory")));
    assert!(imported.owner.owns_path());
}

#[test]
fn staged_startup_import_cleanup_failure_is_reported_not_swallowed() {
    let scratch = Scratch::new();
    let controls = Controls {
        failure: Some(Failure::ImportCleanup),
        ..Controls::default()
    };
    let error = staged_startup_listener(&scratch.public(), &controls)
        .err()
        .unwrap();
    assert!(error.error.to_string().contains("import"));
    assert!(error
        .residue
        .iter()
        .any(|r| r.contains(scratch.public().to_str().unwrap()) && r.contains("unlink")));
}
