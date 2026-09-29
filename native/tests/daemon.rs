//! The daemon end to end: over a real socket, and through a real terminal.
//!
//! 정수님, 2026-09-10: *"a tmux like pty manager, which support daemon mode.
//! with session list so that user can select a session to attach."* These are
//! the three claims in that sentence — sessions outlive their client, they can
//! be listed, and one can be attached — each exercised rather than asserted.
//!
//! The attach test runs the shipped `remuda` binary **inside a pty of our own**,
//! which is the only way to exercise raw mode and the detach key at all: those
//! paths are unreachable without a controlling terminal. remuda is used to test
//! remuda, and that is not circular — the pty under the test is this crate's,
//! the terminal under test is the binary's.

use remuda_core::protocol::{Request, Response};
use remuda_core::{Session, Size};
use remuda_native::{client, daemon, ipc, CommandBuilder, PtyAgent, SystemClock};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(10);

#[path = "daemon_support/spawn.rs"]
mod spawn;
use spawn::Daemon;
#[cfg(windows)]
#[path = "daemon_support/conpty.rs"]
mod conpty;

#[cfg(windows)]
struct AttachedInputWriter<'a, 'session>(&'a remuda_core::session::Attached<'session>);

#[cfg(windows)]
impl std::io::Write for AttachedInputWriter<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .write_raw(bytes)
            .map(|()| bytes.len())
            .map_err(|_| std::io::Error::other("ConPTY input write failed"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
fn answer_pending_conpty_queries(
    receiver: &Receiver<Vec<u8>>,
    writer: &mut impl std::io::Write,
    pending: &mut Vec<u8>,
    captured: &mut Vec<u8>,
) {
    for chunk in receiver.try_iter() {
        conpty::answer_conpty_cursor_queries(writer, pending, &chunk);
        captured.extend_from_slice(&chunk);
    }
}

/// A runtime directory of our own. Short enough for `sun_path` (~108 bytes) —
/// a long path fails at bind with a message no caller would guess from a
/// timeout, which is what the binary's startup-error handling exists for.
fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remuda-t{}-{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[cfg(unix)]
fn wait_for_send_writer_busy(socket: &Path, send_finished: &Receiver<()>) {
    let busy_deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match client::request(
            socket,
            &Request::Send {
                name: "target".into(),
                bytes: Vec::new(),
            },
        )
        .expect("probe whether the large Send owns the writer")
        {
            Response::Busy => return,
            Response::Ok => {
                assert!(
                    send_finished.try_recv().is_err(),
                    "large Send ended before the writer became busy"
                );
            }
            other => panic!("unexpected empty Send probe response: {other:?}"),
        }
        assert!(
            Instant::now() < busy_deadline,
            "large Send never occupied the session writer"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn spawn_stalled_send(
    socket: PathBuf,
    bytes: Vec<u8>,
    send_finished: std::sync::mpsc::Sender<()>,
) -> std::thread::JoinHandle<std::io::Result<Response>> {
    std::thread::spawn(move || {
        let result = loop {
            match client::request(
                &socket,
                &Request::Send {
                    name: "target".into(),
                    bytes: bytes.clone(),
                },
            ) {
                Ok(Response::Busy) => std::thread::sleep(Duration::from_millis(10)),
                result => break result,
            }
        };
        let _ = send_finished.send(());
        result
    })
}

fn unique_scratch_dir(tag: &str) -> PathBuf {
    static NEXT_RUNTIME: AtomicU64 = AtomicU64::new(1);
    let run = NEXT_RUNTIME.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("remuda-u{}-{run}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create unique test runtime directory");
    dir
}

/// The address, derived the way the shipped binary derives it. Hand-building
/// one that merely resembles it is what made the attach test fail first time.
fn scratch(tag: &str) -> PathBuf {
    daemon::socket_path_in(&scratch_dir(tag), "s")
}

/// Start a daemon and return once it actually answers, not once it was spawned.
fn daemon_at(path: &Path) -> impl Drop {
    let serving = path.to_path_buf();
    std::thread::spawn(move || {
        if let Err(error) = daemon::serve(&serving) {
            eprintln!("test daemon failed at {serving:?}: {error}");
        }
    });

    let deadline = Instant::now() + PATIENCE;
    while remuda_native::ipc::connect(path).is_err() {
        assert!(Instant::now() < deadline, "daemon never bound {path:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    Cleanup(path.to_path_buf())
}

#[cfg(unix)]
#[test]
fn autostart_reports_the_pid_and_recovery_for_a_held_socket_lock() {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = scratch_dir("lock-holder");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create isolated runtime dir");
    let socket = daemon::socket_path_in(&dir, "s");
    let mut lock_name = socket.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    std::fs::create_dir_all(lock_path.parent().unwrap()).expect("create socket directory");
    let mut lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .expect("create socket lock");
    let holder_pid = std::process::id();
    writeln!(lock_file, "{holder_pid}").expect("write holder pid");
    assert_eq!(
        unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "hold private socket lock"
    );

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s", "-e", "return 1"])
        .env("REMUDA_RUNTIME_DIR", &dir)
        .env("HOME", dir.join("home"))
        .env_remove("XDG_CONFIG_HOME")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run a client that autostarts the daemon");
    let stderr = child.stderr.take().expect("captured client stderr");
    let (first_send, first_receive) = std::sync::mpsc::channel();
    let (rest_send, rest_receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stderr);
        let mut line = String::new();
        let _ = std::io::BufRead::read_line(&mut reader, &mut line);
        let _ = first_send.send(line);
        let mut rest = String::new();
        let _ = std::io::Read::read_to_string(&mut reader, &mut rest);
        let _ = rest_send.send(rest);
    });
    let early_notice = first_receive.recv_timeout(Duration::from_millis(2500));
    let status = child.wait().expect("wait for failed autostart");
    let notice_was_early = early_notice.is_ok();
    let mut stderr = early_notice.unwrap_or_default();
    stderr.push_str(&rest_receive.recv().unwrap_or_default());
    let success = status.success();
    let names_holder = stderr.contains(&format!("pid {holder_pid}"));
    let explains_recovery = stderr.contains("kill -CONT")
        && stderr.contains(&format!("kill {holder_pid}"))
        && stderr.contains("retry");
    let announced_wait =
        stderr.contains("waiting for the socket lock held by another remuda daemon");

    assert_eq!(
        unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN) },
        0
    );
    drop(lock_file);
    std::fs::remove_dir_all(&dir).expect("remove private runtime dir");
    assert!(!success, "autostart must fail while lock is held: {stderr}");
    assert!(names_holder, "{stderr}");
    assert!(explains_recovery, "{stderr}");
    assert!(announced_wait, "{stderr}");
    assert!(
        notice_was_early,
        "the one-second socket-lock notice should reach the client before the three-second lock timeout"
    );
}

#[cfg(unix)]
#[test]
fn request_to_stopped_daemon_times_out_with_recovery_instructions() {
    let dir = scratch_dir("stopped-request");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create isolated runtime dir");
    let daemon = Daemon::spawn(&dir);
    let pid = daemon.0.id();
    let socket = daemon::socket_path_in(&dir, "s");
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGSTOP) }, 0);

    let (send, receive) = std::sync::mpsc::channel();
    let request_socket = socket.clone();
    std::thread::spawn(move || {
        let result = client::request(&request_socket, &Request::List);
        let _ = send.send(result);
    });
    let result = receive.recv_timeout(Duration::from_secs(12));

    let _ = unsafe { libc::kill(pid as i32, libc::SIGCONT) };
    let result = result.expect("request to stopped daemon must have a bounded timeout");
    let message = result
        .expect_err("stopped daemon did not answer")
        .to_string();
    assert!(message.contains(&socket.display().to_string()), "{message}");
    assert!(message.contains(&format!("pid {pid}")), "{message}");
    assert!(message.contains("kill -CONT"), "{message}");
    assert!(message.contains(&format!("kill {pid}")), "{message}");
    assert!(message.contains("verify"), "{message}");
    drop(daemon);
    std::fs::remove_dir_all(&dir).expect("remove private runtime dir");
}

#[cfg(unix)]
#[test]
fn daemon_lock_file_is_private() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch_dir("daemon-lock-mode");
    let mut daemon = Daemon::spawn(&dir);
    let socket = daemon::socket_path_in(&dir, "s");
    let lock = socket.with_extension("sock.lock");
    let metadata = std::fs::metadata(&lock).expect("daemon lock file exists");
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(
        std::fs::read_to_string(&lock)
            .expect("daemon lock holder is recorded")
            .trim(),
        daemon.0.id().to_string()
    );
    assert_eq!(
        client::request(
            &socket,
            &Request::Shutdown {
                requester_daemon_id: None,
                requester_session_id: None,
                requester_session_name: None,
                override_hosted: false,
            },
        )
        .expect("request shutdown"),
        Response::Ok
    );
    assert!(
        daemon.left_on_its_own(),
        "daemon exits after shutdown request"
    );
    assert!(!socket.exists(), "owned socket removed after shutdown");
    let metadata = std::fs::metadata(&lock).expect("lock inode remains for waiters");
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
}

#[cfg(unix)]
#[test]
fn daemon_socket_and_directory_are_private_on_first_start() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch_dir("private-socket-fresh");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create isolated runtime directory");
    let socket = daemon::socket_path_in(&dir, "s");
    let daemon = Daemon::spawn(&dir);
    let directory_mode = std::fs::metadata(socket.parent().unwrap())
        .expect("socket directory exists")
        .permissions()
        .mode()
        & 0o777;
    let socket_mode = std::fs::metadata(&socket)
        .expect("daemon socket exists")
        .permissions()
        .mode()
        & 0o777;
    drop(daemon);
    std::fs::remove_dir_all(&dir).expect("remove isolated runtime directory");

    assert_eq!(directory_mode, 0o700, "socket directory must be private");
    assert_eq!(socket_mode, 0o600, "daemon socket must be private");
}

#[cfg(unix)]
#[test]
fn daemon_tightens_a_preexisting_socket_directory() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch_dir("private-socket-existing");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create isolated runtime directory");
    let socket = daemon::socket_path_in(&dir, "s");
    std::fs::create_dir_all(socket.parent().unwrap()).expect("create socket directory");
    std::fs::set_permissions(
        socket.parent().unwrap(),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("make a permissive preexisting socket directory");

    let daemon = Daemon::spawn(&dir);
    let directory_mode = std::fs::metadata(socket.parent().unwrap())
        .expect("socket directory exists")
        .permissions()
        .mode()
        & 0o777;
    let socket_mode = std::fs::metadata(&socket)
        .expect("daemon socket exists")
        .permissions()
        .mode()
        & 0o777;
    drop(daemon);
    std::fs::remove_dir_all(&dir).expect("remove isolated runtime directory");

    assert_eq!(
        directory_mode, 0o700,
        "existing directory must be tightened"
    );
    assert_eq!(socket_mode, 0o600, "daemon socket must be private");
}

#[cfg(unix)]
#[test]
fn a_custom_absolute_socket_path_does_not_change_its_parent_mode() {
    use std::os::unix::fs::PermissionsExt;

    let runtime = scratch_dir("custom-absolute-socket");
    let custom = runtime.join("chosen");
    std::fs::create_dir(&custom).expect("create user-selected socket directory");
    std::fs::set_permissions(&custom, std::fs::Permissions::from_mode(0o755)).unwrap();
    let custom_server = custom.join("X").to_string_lossy().into_owned();
    let output = remuda_timed(&runtime, &["-s", &custom_server, "-e", "return true"]);

    assert!(
        output.status.success(),
        "custom socket path failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::metadata(&custom).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let _ = remuda_timed(&runtime, &["-s", &custom_server, "stop"]);
    std::fs::remove_dir_all(runtime).unwrap();
}

#[cfg(unix)]
#[test]
fn a_nested_socket_name_keeps_the_runtime_directory_private() {
    use std::os::unix::fs::PermissionsExt;

    let runtime = scratch_dir("nested-socket-name");
    let output = remuda_timed(&runtime, &["-s", "sub/X", "-e", "return true"]);

    assert!(
        output.status.success(),
        "nested socket path failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::metadata(runtime.join("remuda"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let _ = remuda_timed(&runtime, &["-s", "sub/X", "stop"]);
    std::fs::remove_dir_all(runtime).unwrap();
}

#[cfg(unix)]
#[test]
fn a_symlinked_runtime_directory_is_rejected_before_a_startup_log_is_created() {
    let runtime = scratch_dir("symlink-runtime");
    let target = runtime.join("target");
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, runtime.join("remuda")).unwrap();

    let output = remuda_timed(&runtime, &["-s", "s", "-e", "return true"]);

    assert!(!output.status.success());
    assert!(
        !target.join("s.log").exists(),
        "must not create log through symlink"
    );
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    std::fs::remove_dir_all(runtime).unwrap();
}

#[cfg(unix)]
#[test]
fn symlinked_runtime_base_is_rejected_with_trailing_separators_and_dot() {
    let root = scratch_dir("symlink-runtime-base");
    let target = root.join("target");
    let link = root.join("link");
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    for runtime in [link.clone(), root.join("link/"), root.join("link/.")] {
        let socket = runtime.join("remuda").join("s.sock");
        let result = daemon::prepare_socket_path(&socket, Some(&runtime));
        assert!(result.is_err(), "accepted symlink runtime {runtime:?}");
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn symlinked_custom_socket_parents_are_rejected_after_path_normalization() {
    let root = scratch_dir("symlink-custom-parent");
    let target = root.join("target");
    let link = root.join("link");
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    for parent in [
        link.clone(),
        PathBuf::from(format!("{}/", link.display())),
        PathBuf::from(format!("{}/.", link.display())),
    ] {
        let socket = parent.join("s.sock");
        let result = daemon::prepare_socket_path(&socket, Some(&root.join("runtime")));
        assert!(result.is_err(), "accepted symlink socket parent {parent:?}");
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn daemon_signal_cleanup_preserves_a_replacement_socket_path() {
    use std::os::unix::fs::FileTypeExt;

    let dir = scratch_dir("daemon-socket-owner");
    let mut daemon = Daemon::spawn(&dir);
    let socket = daemon::socket_path_in(&dir, "s");
    assert!(std::fs::symlink_metadata(&socket)
        .expect("bound socket exists")
        .file_type()
        .is_socket());

    std::fs::remove_file(&socket).expect("remove the daemon's socket entry");
    std::fs::write(&socket, b"replacement owned by another process")
        .expect("install replacement at the old socket path");
    let signalled = unsafe { libc::kill(daemon.0.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(signalled, 0, "send SIGTERM to daemon");
    assert!(daemon.left_on_its_own(), "daemon handles SIGTERM cleanly");
    assert_eq!(
        std::fs::read(&socket).expect("replacement remains after cleanup"),
        b"replacement owned by another process"
    );
}

#[cfg(unix)]
#[test]
fn sigusr1_rebinds_a_deleted_socket() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch_dir("daemon-sigusr1-rebind");
    let mut daemon = Daemon::spawn(&dir);
    let socket = daemon::socket_path_in(&dir, "s");
    let result = (|| {
        for cycle in 1..=20 {
            signal_rebind_and_wait(&daemon, &socket)
                .map_err(|error| format!("rebind cycle {cycle}/20: {error}"))?;
            let socket_mode = std::fs::metadata(&socket)
                .map_err(|error| error.to_string())?
                .permissions()
                .mode()
                & 0o777;
            let directory_mode = std::fs::metadata(socket.parent().unwrap())
                .map_err(|error| error.to_string())?
                .permissions()
                .mode()
                & 0o777;
            if socket_mode != 0o600 || directory_mode != 0o700 {
                return Err(format!(
                    "rebind cycle {cycle}/20 modes were socket {socket_mode:04o}, directory {directory_mode:04o}"
                ));
            }
        }
        Ok::<(), String>(())
    })();

    let socket_removed = stop_and_clean(&mut daemon, &dir, &socket);
    assert!(result.is_ok(), "{}", result.unwrap_err());
    assert!(socket_removed, "shutdown removes the rebound socket");
}

#[cfg(unix)]
#[test]
fn sigusr1_rebind_refreshes_cleanup_identity() {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let dir = scratch_dir("daemon-sigusr1-owner");
    let mut daemon = Daemon::spawn(&dir);
    let socket = daemon::socket_path_in(&dir, "s");
    let original = std::fs::symlink_metadata(&socket).expect("original socket");
    let result = signal_rebind_and_wait(&daemon, &socket).and_then(|()| {
        let rebound = std::fs::symlink_metadata(&socket).map_err(|error| error.to_string())?;
        if !rebound.file_type().is_socket() {
            return Err("rebound path is not a socket".into());
        }
        if (original.dev(), original.ino()) == (rebound.dev(), rebound.ino()) {
            return Err("rebind kept the old socket inode".into());
        }
        std::fs::remove_file(&socket).map_err(|error| error.to_string())?;
        std::fs::write(&socket, b"replacement after rebind").map_err(|error| error.to_string())?;
        Ok(())
    });

    let _ = unsafe { libc::kill(daemon.0.id() as libc::pid_t, libc::SIGTERM) };
    let stopped = daemon.left_on_its_own();
    let preserved = std::fs::read(&socket).ok().as_deref() == Some(b"replacement after rebind");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(result.is_ok(), "{}", result.unwrap_err());
    assert!(stopped, "daemon handles SIGTERM after rebinding");
    assert!(preserved, "cleanup preserves a replacement path inode");
}

#[cfg(unix)]
#[test]
fn sigusr1_on_the_owned_socket_is_silent() {
    use std::io::Read as _;
    use std::os::fd::AsRawFd;

    let dir = scratch_dir("daemon-sigusr1-owned");
    let socket = daemon::socket_path_in(&dir, "s");
    let mut command = spawn::base_command(&dir);
    command.stderr(std::process::Stdio::piped());
    let mut daemon = spawn::spawn_and_wait(command, &dir);

    let signalled = unsafe { libc::kill(daemon.0.id() as libc::pid_t, libc::SIGUSR1) };
    assert_eq!(signalled, 0, "send SIGUSR1");
    assert!(matches!(
        client::request(&socket, &Request::Version),
        Ok(Response::Value(_))
    ));

    // If SIGUSR1 takes the erroneous ipc::listen path, it writes its failure
    // promptly. Wait on the captured pipe for that output without relying on
    // whether a subsequent request happened to race the signal thread.
    let mut stderr = daemon.0.stderr.take().expect("captured daemon stderr");
    let mut pending = libc::pollfd {
        fd: stderr.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let readable = unsafe { libc::poll(&mut pending, 1, 200) };

    let stopped = stop_and_clean(&mut daemon, &dir, &socket);
    let mut output = String::new();
    stderr
        .read_to_string(&mut output)
        .expect("read daemon stderr");
    assert_eq!(
        readable, 0,
        "SIGUSR1 wrote stderr before shutdown: {output}"
    );
    assert!(stopped, "daemon stops and removes its socket");
    assert!(
        !output.contains("could not rebind socket"),
        "a signal on the daemon's own live socket is a no-op: {output}"
    );
}

#[cfg(unix)]
#[test]
fn sigusr1_with_a_queued_old_listener_client_does_not_strand_shutdown() {
    use interprocess::local_socket::traits::Stream as _;

    let dir = scratch_dir("daemon-sigusr1-queued");
    let mut daemon = Daemon::spawn(&dir);
    let socket = daemon::socket_path_in(&dir, "s");
    let pid = daemon.0.id() as libc::pid_t;

    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0, "stop daemon");
    let mut stopped_status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut stopped_status, libc::WUNTRACED) },
        pid,
        "wait until daemon is stopped"
    );
    assert!(libc::WIFSTOPPED(stopped_status), "daemon reached SIGSTOP");

    let mut queued = ipc::connect(&socket).expect("queue a client on the old listener");
    let mut request = serde_json::to_vec(&Request::Version).expect("serialize version request");
    request.push(b'\n');
    queued.write_all(&request).expect("write queued request");
    queued
        .set_nonblocking(true)
        .expect("make queued client nonblocking");
    std::fs::remove_file(&socket).expect("remove old listener path");
    assert_eq!(
        unsafe { libc::kill(pid, libc::SIGUSR1) },
        0,
        "send rebind signal"
    );
    assert_eq!(
        unsafe { libc::kill(pid, libc::SIGCONT) },
        0,
        "resume daemon"
    );

    let rebind_deadline = Instant::now() + Duration::from_secs(2);
    while std::fs::symlink_metadata(&socket).is_err() {
        assert!(Instant::now() < rebind_deadline, "socket was not rebound");
        std::thread::yield_now();
    }

    let signalled_at = Instant::now();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0, "send SIGTERM");
    let exit_deadline = signalled_at + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = daemon.0.try_wait().expect("poll daemon exit") {
            break status;
        }
        assert!(
            Instant::now() < exit_deadline,
            "SIGTERM was stranded behind stale listener readiness"
        );
        std::thread::sleep(Duration::from_millis(5));
    };

    let mut response = Vec::new();
    let read_deadline = Instant::now() + Duration::from_secs(2);
    let client_closed_cleanly = loop {
        let mut bytes = [0u8; 256];
        match queued.read(&mut bytes) {
            Ok(0) => break true,
            Ok(count) => {
                response.extend_from_slice(&bytes[..count]);
                if response.contains(&b'\n') {
                    break serde_json::from_slice::<Response>(
                        response
                            .split(|byte| *byte == b'\n')
                            .next()
                            .unwrap_or_default(),
                    )
                    .is_ok();
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= read_deadline {
                    break false;
                }
                std::thread::yield_now();
            }
            Err(_) => break true,
        }
    };
    let _ = std::fs::remove_dir_all(&dir);
    assert!(status.success(), "SIGTERM exits successfully: {status}");
    assert!(
        signalled_at.elapsed() < Duration::from_secs(2),
        "SIGTERM exits within two seconds"
    );
    assert!(
        client_closed_cleanly,
        "queued old-listener client gets a response or a clean connection close"
    );
}

#[cfg(unix)]
#[test]
fn sigusr1_refuses_to_displace_a_live_replacement_socket() {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::net::UnixListener;

    let dir = scratch_dir("sigusr1-live");
    let mut daemon = Daemon::spawn(&dir);
    let socket = daemon::socket_path_in(&dir, "s");
    std::fs::remove_file(&socket).expect("remove original socket");
    let replacement = UnixListener::bind(&socket).expect("bind live replacement");
    replacement
        .set_nonblocking(true)
        .expect("make replacement accept nonblocking");
    let replacement_id = std::fs::symlink_metadata(&socket).expect("replacement metadata");
    let result = (|| -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(2);
        // The second accepted probe proves the first refused bind completed
        // before the test asks the daemon to stop.
        for _ in 0..2 {
            let signalled = unsafe { libc::kill(daemon.0.id() as libc::pid_t, libc::SIGUSR1) };
            if signalled != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            loop {
                match replacement.accept() {
                    Ok((_stream, _address)) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return Err("daemon did not probe the live replacement".into());
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
        }

        let after = std::fs::symlink_metadata(&socket).map_err(|error| error.to_string())?;
        if (after.dev(), after.ino()) != (replacement_id.dev(), replacement_id.ino()) {
            return Err("live replacement socket inode changed".into());
        }
        if daemon
            .0
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err("daemon exited while handling SIGUSR1".into());
        }
        Ok(())
    })();

    let _ = unsafe { libc::kill(daemon.0.id() as libc::pid_t, libc::SIGTERM) };
    let stopped = daemon.left_on_its_own();
    let after_shutdown = std::fs::symlink_metadata(&socket).expect("replacement remains");
    let remains = (after_shutdown.dev(), after_shutdown.ino())
        == (replacement_id.dev(), replacement_id.ino());
    drop(replacement);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(result.is_ok(), "{}", result.unwrap_err());
    assert!(stopped, "daemon handles SIGTERM after failed rebind");
    assert!(remains, "shutdown preserves the live replacement socket");
}

#[cfg(unix)]
fn signal_rebind_and_wait(daemon: &Daemon, socket: &Path) -> Result<(), String> {
    std::fs::remove_file(socket).map_err(|error| error.to_string())?;
    let signalled = unsafe { libc::kill(daemon.0.id() as libc::pid_t, libc::SIGUSR1) };
    if signalled != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    while ipc::connect(socket).is_err() {
        if Instant::now() >= deadline {
            return Err(format!("daemon did not rebind {socket:?} after SIGUSR1"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    match client::request(socket, &Request::List) {
        Ok(Response::Sessions(_)) => Ok(()),
        Ok(response) => Err(format!("unexpected response: {response:?}")),
        Err(error) => Err(format!("rebound socket did not serve requests: {error}")),
    }
}

#[cfg(unix)]
fn stop_and_clean(daemon: &mut Daemon, dir: &Path, socket: &Path) -> bool {
    let _ = unsafe { libc::kill(daemon.0.id() as libc::pid_t, libc::SIGTERM) };
    let stopped = daemon.left_on_its_own();
    let socket_removed = !socket.exists();
    let _ = std::fs::remove_dir_all(dir);
    stopped && socket_removed
}

#[cfg(unix)]
#[test]
fn concurrent_daemon_starts_serialize_stale_socket_replacement() {
    let dir = scratch_dir("daemon-start-lock");
    let socket = daemon::socket_path_in(&dir, "s");
    let stale = ipc::listen(&socket).expect("create stale socket entry");
    drop(stale);

    let first = spawn::base_command(&dir)
        .spawn()
        .expect("spawn first daemon");
    let second = spawn::base_command(&dir)
        .spawn()
        .expect("spawn second daemon");
    let mut first = Daemon(first);
    let mut second = Daemon(second);
    let deadline = Instant::now() + PATIENCE;
    loop {
        let serving = ipc::connect(&socket).is_ok();
        let first_done = first.0.try_wait().expect("poll first daemon");
        let second_done = second.0.try_wait().expect("poll second daemon");
        if serving && (first_done.is_some() ^ second_done.is_some()) {
            let Some(loser) = first_done.or(second_done) else {
                unreachable!("exactly one daemon exited")
            };
            assert!(!loser.success(), "only one daemon can own the endpoint");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon startup race did not settle"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        client::request(
            &socket,
            &Request::Shutdown {
                requester_daemon_id: None,
                requester_session_id: None,
                requester_session_name: None,
                override_hosted: false,
            },
        )
        .expect("stop winning daemon"),
        Response::Ok
    );
    let first_exited = first.left_on_its_own();
    let second_exited = second.left_on_its_own();
    assert!(
        first_exited ^ second_exited,
        "one daemon exits successfully"
    );
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn new_session(path: &Path, name: &str) {
    let response = client::request(
        path,
        &Request::New {
            name: Some(name.to_string()),
            command: vec!["sh".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("new");
    assert_eq!(
        response,
        Response::Value(name.to_string()),
        "New answers with the name it gave the session"
    );
}

#[test]
fn session_identity_survives_as_a_new_value_after_daemon_restart_and_output_versions_advance() {
    let runtime = scratch_dir("ver-restart");
    let socket = daemon::socket_path_in(&runtime, "s");
    let mut first_daemon = spawn::Daemon::spawn(&runtime);
    start_shell_session(
        &socket,
        "sleep 1; printf first; sleep 1; printf later; sleep 30",
    );
    let first_summary = listed_session(&socket);
    let first_version = wait_for_quiet_output_version(&socket);
    let front_request = serde_json::to_vec(&Request::CaptureStyled {
        name: "versioned".into(),
        scrollback: 0,
    })
    .expect("encode front request");
    let front_capture: Response = serde_json::from_slice(
        &remuda_native::remote_front::forward_frame(&socket, &front_request)
            .expect("front forwards capture"),
    )
    .expect("decode front response");
    assert!(
        matches!(front_capture, Response::StyledScreen { instance_id, output_version, .. }
        if instance_id == first_summary.instance_id && output_version == Some(first_version))
    );
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(capture_version(&socket), first_version);
    wait_for_output_version(&socket, first_version);

    client::request(
        &socket,
        &Request::Shutdown {
            requester_daemon_id: None,
            requester_session_id: None,
            requester_session_name: None,
            override_hosted: false,
        },
    )
    .expect("stop first daemon");
    assert!(
        first_daemon.left_on_its_own(),
        "first daemon should stop cleanly"
    );

    let mut second_daemon = spawn::Daemon::spawn(&runtime);
    start_shell_session(&socket, "printf second; sleep 30");
    let second_summary = listed_session(&socket);
    assert_ne!(first_summary.instance_id, second_summary.instance_id);
    let _ = client::request(
        &socket,
        &Request::Shutdown {
            requester_daemon_id: None,
            requester_session_id: None,
            requester_session_name: None,
            override_hosted: false,
        },
    );
    assert!(
        second_daemon.left_on_its_own(),
        "second daemon should stop cleanly"
    );
}

#[test]
fn input_batches_acknowledge_duplicates_and_check_instance_before_deduplication() {
    let runtime = scratch_dir("input-dedup");
    let socket = daemon::socket_path_in(&runtime, "s");
    let mut running = spawn::Daemon::spawn(&runtime);
    start_shell_session(&socket, "sleep 30");
    let session = listed_session(&socket);
    let request = |instance_id: String| Request::Input {
        name: "versioned".into(),
        instance_id,
        client_id: "01010101010101010101010101010101".into(),
        seq: 1,
        bytes: b"one batch\r".to_vec(),
    };
    let instance_id = session.instance_id.expect("instance identity");
    assert_eq!(
        client::request(&socket, &request(instance_id.clone())).expect("first input"),
        Response::Ack { duplicate: false }
    );
    assert_eq!(
        client::request(&socket, &request(instance_id)).expect("retry input"),
        Response::Ack { duplicate: true }
    );
    assert_eq!(
        client::request(&socket, &request("stale-instance".into())).expect("stale input"),
        Response::WrongInstance
    );
    let _ = client::request(
        &socket,
        &Request::Shutdown {
            requester_daemon_id: None,
            requester_session_id: None,
            requester_session_name: None,
            override_hosted: false,
        },
    );
    assert!(running.left_on_its_own(), "daemon should stop cleanly");
}

#[cfg(unix)]
#[test]
fn stalled_pty_write_times_out_without_blocking_reads_and_recovers() {
    let runtime = scratch_dir("pty-write-timeout");
    let socket = daemon::socket_path_in(&runtime, "s");
    let mut running = spawn::Daemon::spawn(&runtime);
    let reader_marker = start_reader_waiting_session(&socket, &runtime);

    let bytes = vec![b'\n'; 1024 * 1024];
    let send_socket = socket.clone();
    let (sent, result) = std::sync::mpsc::channel();
    let started = Instant::now();
    let sender = std::thread::spawn(move || {
        let result = client::request(
            &send_socket,
            &Request::Send {
                name: "versioned".into(),
                bytes,
            },
        );
        let _ = sent.send(result);
    });

    // Wait until the first writer has had time to fill the PTY, then prove an
    // independent request can make progress and a second input is not queued.
    let busy_deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match client::request(
            &socket,
            &Request::Send {
                name: "versioned".into(),
                bytes: b"later\n".to_vec(),
            },
        )
        .expect("second send")
        {
            Response::Busy => break,
            Response::Ok => {
                assert!(
                    Instant::now() < busy_deadline,
                    "the large write never occupied the session writer"
                );
                if let Ok(result) = result.try_recv() {
                    panic!("large write finished before a second write was refused: {result:?}");
                }
            }
            other => panic!("unexpected second send response: {other:?}"),
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let read_started = Instant::now();
    let _ = listed_session(&socket);
    let _ = capture_version(&socket);
    assert!(
        read_started.elapsed() < Duration::from_millis(500),
        "list and capture should stay responsive during the blocked write"
    );

    assert_eq!(
        result
            .recv_timeout(Duration::from_secs(4))
            .expect("bounded send response")
            .expect("send request"),
        Response::WriteTimeout
    );
    sender.join().expect("send client thread");
    assert!(started.elapsed() < Duration::from_secs(6));
    std::fs::write(&reader_marker, b"read now").expect("allow child to read input");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match client::request(
            &socket,
            &Request::Send {
                name: "versioned".into(),
                bytes: b"after recovery\n".to_vec(),
            },
        )
        .expect("retry after child begins reading")
        {
            Response::Ok => break,
            Response::Busy => {
                assert!(Instant::now() < deadline, "PTY writer did not recover");
                std::thread::sleep(Duration::from_millis(50));
            }
            other => panic!("unexpected recovery response: {other:?}"),
        }
    }

    let _ = client::request(
        &socket,
        &Request::Shutdown {
            requester_daemon_id: None,
            requester_session_id: None,
            requester_session_name: None,
            override_hosted: false,
        },
    );
    assert!(
        running.left_on_its_own(),
        "daemon exits after shutdown request"
    );
}

#[cfg(unix)]
#[test]
fn attached_keystrokes_survive_a_pty_write_timeout_without_detaching() {
    let runtime = scratch_dir("attach-write-timeout");
    let socket = daemon::socket_path_in(&runtime, "s");
    let _daemon = daemon_at(&socket);
    let marker = runtime.join("start-reader");
    let capture_path = runtime.join("typed-bytes");
    let mut env = std::collections::HashMap::new();
    env.insert("READER_MARKER".into(), marker.display().to_string());
    env.insert("CAPTURE_PATH".into(), capture_path.display().to_string());
    let response = client::request(
        &socket,
        &Request::New {
            name: Some("target".into()),
            command: vec![
                "sh".into(),
                "-c".into(),
                "stty raw -echo; while [ ! -e \"$READER_MARKER\" ]; do sleep 0.02; done; cat >\"$CAPTURE_PATH\"".into(),
            ],
            size: Size::new(80, 24),
            cwd: None,
            env: Some(env),
        },
    )
    .expect("start non-reading child");
    assert!(matches!(response, Response::Value(_)));

    let stream = raw_attach(&socket, "target");
    let mut typed = vec![b'\n'; 1024 * 1024];
    typed.extend_from_slice(b"last-human-keystrokes\n");
    let expected = typed.clone();
    let (stream_tx, stream_rx) = std::sync::mpsc::channel();
    let sender = std::thread::spawn(move || {
        let mut stream = stream;
        let result = stream.write_all(&typed);
        let _ = stream_tx.send((stream, result));
    });

    // Let the attach pump reach its write deadline while the child does not
    // read, then prove the connection remains attached while waiting.
    std::thread::sleep(Duration::from_millis(2300));
    assert_eq!(target_row(&socket, "r.attached"), "true");
    assert!(
        !capture_path.exists(),
        "the child has not begun draining its terminal input"
    );

    std::fs::write(&marker, b"read now").expect("allow child to read typed bytes");
    let (stream, result) = stream_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("attach client write completes after child starts reading");
    result.expect("send all human input to attach socket");
    sender.join().expect("attach client writer thread");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if std::fs::read(&capture_path).ok().as_deref() == Some(expected.as_slice()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "attached bytes did not drain exactly"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(target_row(&socket, "r.attached"), "true");
    drop(stream);
    let detached_deadline = Instant::now() + PATIENCE;
    while target_row(&socket, "r.attached") == "true" {
        assert!(
            Instant::now() < detached_deadline,
            "attach did not release on EOF"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn attach_during_stalled_send_preserves_human_input_before_and_after_timeout() {
    let runtime = unique_scratch_dir("probe-attach-stall");
    let socket = daemon::socket_path_in(&runtime, "s");
    let _daemon = daemon_at(&socket);
    let marker = runtime.join("start-reader");
    let capture_path = runtime.join("typed-bytes");
    let mut env = std::collections::HashMap::new();
    env.insert("READER_MARKER".into(), marker.display().to_string());
    env.insert("CAPTURE_PATH".into(), capture_path.display().to_string());
    assert!(matches!(
        client::request(
            &socket,
            &Request::New {
                name: Some("target".into()),
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    "stty raw -echo; while [ ! -e \"$READER_MARKER\" ]; do sleep 0.02; done; cat >\"$CAPTURE_PATH\"".into(),
                ],
                size: Size::new(80, 24),
                cwd: None,
                env: Some(env),
            },
        )
        .expect("start non-reading child"),
        Response::Value(_)
    ));

    let mut send_bytes = vec![b'\n'; 1024 * 1024];
    send_bytes.extend_from_slice(b"SEND-END\n");
    let expected_send = send_bytes.clone();
    let started = Instant::now();
    let (send_finished_tx, send_finished_rx) = std::sync::mpsc::channel();
    let sender = spawn_stalled_send(socket.clone(), send_bytes, send_finished_tx);

    // Empty sends make safe probes: they cannot affect the captured bytes.
    // Observe Busy before attaching, rather than assuming a fixed delay.
    wait_for_send_writer_busy(&socket, &send_finished_rx);
    let mut stream = raw_attach(&socket, "target");
    stream
        .write_all(b"HUMAN-ONE\n")
        .expect("type during blocked Send");
    let send_result = sender
        .join()
        .expect("send client thread")
        .expect("send request");
    eprintln!(
        "probe: send -> {send_result:?} after {:?}",
        started.elapsed()
    );
    assert_eq!(send_result, Response::WriteTimeout);

    // The attach pump must retain input across the timeout without replaying
    // or detaching; this second write arrives after the original call expired.
    stream
        .write_all(b"HUMAN-TWO\n")
        .expect("type after Send timeout");
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(
        target_row(&socket, "r.attached"),
        "true",
        "detached during stall"
    );
    assert!(
        !capture_path.exists(),
        "child must stay unread until the test releases it"
    );

    std::fs::write(&marker, b"go").expect("allow child to drain input");
    let mut expected = expected_send;
    expected.extend_from_slice(b"HUMAN-ONE\nHUMAN-TWO\n");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let got = std::fs::read(&capture_path).unwrap_or_default();
        if got == expected {
            break;
        }
        if Instant::now() > deadline {
            let tail = String::from_utf8_lossy(&got[got.len().saturating_sub(60)..]);
            let position = |needle: &[u8]| {
                got.windows(needle.len())
                    .position(|window| window == needle)
            };
            panic!(
                "mismatch: got {} want {}, SEND-END@{:?} ONE@{:?} TWO@{:?} tail {tail:?}",
                got.len(),
                expected.len(),
                position(b"SEND-END"),
                position(b"HUMAN-ONE"),
                position(b"HUMAN-TWO"),
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(target_row(&socket, "r.attached"), "true");
    drop(stream);
}

#[cfg(unix)]
fn start_reader_waiting_session(socket: &Path, runtime: &Path) -> PathBuf {
    // Keep the master unread until the test signals it, then drain input so
    // recovery is deterministic regardless of parallel test scheduling.
    let marker = runtime.join("start-reader");
    let mut env = std::collections::HashMap::new();
    env.insert("READER_MARKER".into(), marker.display().to_string());
    assert!(matches!(
        client::request(
            socket,
            &Request::New {
                name: Some("versioned".into()),
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    "stty -echo; while [ ! -e \"$READER_MARKER\" ]; do sleep 0.02; done; cat >/dev/null".into(),
                ],
                size: Size::new(80, 24),
                cwd: None,
                env: Some(env),
            },
        )
        .expect("start non-reading child"),
        Response::Value(_)
    ));
    marker
}

fn start_shell_session(socket: &Path, script: &str) {
    let response = client::request(
        socket,
        &Request::New {
            name: Some("versioned".into()),
            command: vec!["sh".into(), "-c".into(), script.into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("start session");
    assert!(matches!(response, Response::Value(_)));
}

fn listed_session(socket: &Path) -> remuda_core::SessionSummary {
    match client::request(socket, &Request::List).expect("list session") {
        Response::Sessions(sessions) => sessions
            .into_iter()
            .find(|session| session.name == "versioned")
            .expect("session is listed"),
        other => panic!("unexpected list response: {other:?}"),
    }
}

fn capture_version(socket: &Path) -> u64 {
    match client::request(
        socket,
        &Request::CaptureStyled {
            name: "versioned".into(),
            scrollback: 0,
        },
    )
    .expect("capture session")
    {
        Response::StyledScreen {
            output_version: Some(version),
            ..
        } => version,
        Response::StyledScreen {
            output_version: None,
            ..
        } => panic!("daemon did not provide an output version"),
        other => panic!("unexpected capture response: {other:?}"),
    }
}

#[test]
fn sync_returns_immediately_when_since_is_older_than_current_output() {
    let runtime = scratch_dir("sync-immediate");
    let socket = daemon::socket_path_in(&runtime, "s");
    let mut running = spawn::Daemon::spawn(&runtime);
    start_shell_session(&socket, "printf ready; sleep 30");
    let current = wait_for_quiet_output_version(&socket);
    assert!(current > 0, "fixture must produce a versioned frame");
    let started = Instant::now();
    let response = client::request(
        &socket,
        &Request::Sync {
            name: "versioned".into(),
            instance_id: None,
            since: current - 1,
            timeout_ms: 30_000,
        },
    )
    .expect("sync response");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "new output must return immediately"
    );
    assert!(matches!(response, Response::Sync { output_version, .. } if output_version == current));
    stop_daemon(&runtime, &socket, &mut running);
}

#[test]
fn sync_waits_for_output_and_returns_the_new_snapshot() {
    let runtime = scratch_dir("sync-output");
    let socket = daemon::socket_path_in(&runtime, "s");
    let trigger = runtime.join("release-sync-output");
    let trigger_arg = shell_test_path(&trigger);
    let script =
        format!("while [ ! -e {trigger_arg} ]; do sleep 0.02; done; printf after-sync; sleep 30");
    let mut running = spawn::Daemon::spawn(&runtime);
    start_shell_session(&socket, &script);
    let since = wait_for_quiet_output_version(&socket);
    let request_socket = socket.clone();
    let request = std::thread::spawn(move || {
        client::request(
            &request_socket,
            &Request::Sync {
                name: "versioned".into(),
                instance_id: None,
                since,
                timeout_ms: 5_000,
            },
        )
        .expect("sync response")
    });
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !request.is_finished(),
        "sync should wait while the version is unchanged"
    );
    let triggered_at = Instant::now();
    std::fs::write(&trigger, b"go").expect("release shell output");
    let response = request.join().expect("sync worker");
    assert!(
        matches!(response, Response::Sync { output_version, snapshot, .. }
        if output_version > since && snapshot.rows.iter().flatten().any(|run| run.text.contains("after-sync")))
    );
    assert!(
        triggered_at.elapsed() < Duration::from_secs(1),
        "output notification should wake Sync well before its 5s timeout"
    );
    stop_daemon(&runtime, &socket, &mut running);
}

#[test]
fn sync_waiter_wakes_when_session_is_resized() {
    let runtime = scratch_dir("sync-resize");
    let socket = daemon::socket_path_in(&runtime, "s");
    let mut running = spawn::Daemon::spawn(&runtime);
    start_shell_session(&socket, "sleep 30");
    let since = wait_for_quiet_output_version(&socket);
    let request_socket = socket.clone();
    let request = std::thread::spawn(move || {
        client::request(
            &request_socket,
            &Request::Sync {
                name: "versioned".into(),
                instance_id: None,
                since,
                timeout_ms: 5_000,
            },
        )
        .expect("sync response after resize")
    });
    std::thread::sleep(Duration::from_millis(150));
    assert!(matches!(
        client::request(
            &socket,
            &Request::Resize {
                name: "versioned".into(),
                size: Size::new(100, 30),
            },
        ),
        Ok(Response::Ok)
    ));
    let triggered_at = Instant::now();
    let response = request.join().expect("sync worker");
    assert!(matches!(response, Response::Sync { output_version, .. } if output_version > since));
    assert!(
        triggered_at.elapsed() < Duration::from_secs(1),
        "resize notification should wake Sync well before its 5s timeout"
    );
    stop_daemon(&runtime, &socket, &mut running);
}

#[test]
fn sync_times_out_with_the_current_unchanged_frame() {
    let runtime = scratch_dir("sync-timeout");
    let socket = daemon::socket_path_in(&runtime, "s");
    let mut running = spawn::Daemon::spawn(&runtime);
    start_shell_session(&socket, "sleep 30");
    let current = wait_for_quiet_output_version(&socket);
    let started = Instant::now();
    let response = client::request(
        &socket,
        &Request::Sync {
            name: "versioned".into(),
            instance_id: None,
            since: current,
            timeout_ms: 60,
        },
    )
    .expect("sync timeout response");
    assert!(started.elapsed() >= Duration::from_millis(40));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(matches!(response, Response::Sync { output_version, .. } if output_version == current));
    stop_daemon(&runtime, &socket, &mut running);
}

#[test]
fn sync_waiter_wakes_when_child_exits() {
    let runtime = scratch_dir("sync-child-exit");
    let socket = daemon::socket_path_in(&runtime, "s");
    let mut running = spawn::Daemon::spawn(&runtime);
    start_shell_session(&socket, "sleep 30");
    let since = wait_for_quiet_output_version(&socket);
    let request_socket = socket.clone();
    let request = std::thread::spawn(move || {
        client::request(
            &request_socket,
            &Request::Sync {
                name: "versioned".into(),
                instance_id: None,
                since,
                timeout_ms: 5_000,
            },
        )
        .expect("sync response after child exit")
    });
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        matches!(
            client::request(
                &socket,
                &Request::Close {
                    name: "versioned".into(),
                },
            ),
            Ok(Response::Ok)
        ),
        "closing the child must succeed while Sync is waiting"
    );
    let triggered_at = Instant::now();
    let response = request.join().expect("sync worker");
    assert!(
        triggered_at.elapsed() < Duration::from_secs(1),
        "child exit must wake the waiter before its Sync timeout"
    );
    assert!(matches!(response, Response::Error(message) if message.contains("exited")));
    stop_daemon(&runtime, &socket, &mut running);
}

#[test]
fn sync_rechecks_instance_after_close_and_relaunch_during_wait() {
    let runtime = scratch_dir("sync-relaunch");
    let socket = daemon::socket_path_in(&runtime, "s");
    let mut running = spawn::Daemon::spawn(&runtime);
    start_shell_session(&socket, "sleep 30");
    let old_instance = listed_session(&socket).instance_id.expect("instance id");
    let since = wait_for_quiet_output_version(&socket);
    let request_socket = socket.clone();
    let request = std::thread::spawn(move || {
        client::request(
            &request_socket,
            &Request::Sync {
                name: "versioned".into(),
                instance_id: Some(old_instance),
                since,
                timeout_ms: 5_000,
            },
        )
        .expect("sync response after relaunch")
    });
    std::thread::sleep(Duration::from_millis(150));
    assert!(matches!(
        client::request(
            &socket,
            &Request::Close {
                name: "versioned".into(),
            },
        ),
        Ok(Response::Ok)
    ));
    start_shell_session(&socket, "sleep 30");
    let triggered_at = Instant::now();
    let response = request.join().expect("sync worker");
    assert_eq!(response, Response::WrongInstance);
    assert!(
        triggered_at.elapsed() < Duration::from_secs(1),
        "post-wait instance check should return promptly"
    );
    stop_daemon(&runtime, &socket, &mut running);
}

#[test]
fn sync_rejects_a_wrong_instance_without_waiting() {
    let runtime = scratch_dir("sync-wrong-instance");
    let socket = daemon::socket_path_in(&runtime, "s");
    let mut running = spawn::Daemon::spawn(&runtime);
    start_shell_session(&socket, "sleep 30");
    let started = Instant::now();
    let response = client::request(
        &socket,
        &Request::Sync {
            name: "versioned".into(),
            instance_id: Some("old-session-instance".into()),
            since: 0,
            timeout_ms: 30_000,
        },
    )
    .expect("wrong-instance response");
    assert_eq!(response, Response::WrongInstance);
    assert!(started.elapsed() < Duration::from_secs(1));
    stop_daemon(&runtime, &socket, &mut running);
}

fn stop_daemon(runtime: &Path, socket: &Path, running: &mut spawn::Daemon) {
    assert_eq!(
        socket,
        daemon::socket_path_in(runtime, "s"),
        "only stop the private daemon created under this test's scratch runtime"
    );
    let _ = client::request(
        socket,
        &Request::Shutdown {
            requester_daemon_id: None,
            requester_session_id: None,
            requester_session_name: None,
            override_hosted: false,
        },
    );
    assert!(
        running.left_on_its_own(),
        "private daemon should stop cleanly"
    );
}

fn wait_for_output_version(socket: &Path, original: u64) {
    let deadline = Instant::now() + PATIENCE;
    while capture_version(socket) <= original {
        assert!(Instant::now() < deadline, "output version did not increase");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_quiet_output_version(socket: &Path) -> u64 {
    let deadline = Instant::now() + PATIENCE;
    let mut version = capture_version(socket);
    let mut quiet_since = Instant::now();
    loop {
        assert!(
            Instant::now() < deadline,
            "output version did not become quiet"
        );
        std::thread::sleep(Duration::from_millis(200));
        let next = capture_version(socket);
        if next != version {
            version = next;
            quiet_since = Instant::now();
        } else if quiet_since.elapsed() >= Duration::from_millis(400) {
            return version;
        }
    }
}

fn shell_test_path(path: &Path) -> String {
    #[cfg(windows)]
    let value = {
        let value = path.to_string_lossy().replace('\\', "/");
        if let Some((drive, rest)) = value.split_once(':') {
            format!(
                "/{}/{}",
                drive.to_ascii_lowercase(),
                rest.trim_start_matches('/')
            )
        } else {
            value
        }
    };
    #[cfg(not(windows))]
    let value = path.to_string_lossy().into_owned();
    format!("\"{}\"", value.replace('"', "\\\""))
}

#[test]
fn old_json_shapes_parse_with_defaults_for_additive_session_fields() {
    let summary = remuda_core::SessionSummary {
        id: "".into(),
        name: "old-client".into(),
        instance_id: Some("new-field".into()),
        output_version: Some(7),
        alive: true,
        idle: Duration::ZERO,
        output_idle: None,
        size: Size::new(80, 24),
        attached: false,
        human_idle: None,
        mouse_tracking: false,
    };
    let mut old_summary = serde_json::to_value(summary).expect("serialize summary");
    let fields = old_summary.as_object_mut().expect("summary object");
    fields.remove("id");
    fields.remove("instance_id");
    fields.remove("output_version");
    let decoded: remuda_core::SessionSummary =
        serde_json::from_value(old_summary).expect("parse old session summary");
    assert_eq!(decoded.instance_id, None);
    assert_eq!(decoded.output_version, None);

    let response = Response::StyledScreen {
        rows: vec![],
        instance_id: Some("new-field".into()),
        output_version: Some(7),
        wrapped: vec![],
        scrollback_len: 0,
        scrollback_total: 0,
        cursor: remuda_core::agent::Cursor {
            row: 0,
            col: 0,
            visible: true,
        },
    };
    let mut old_response = serde_json::to_value(response).expect("serialize response");
    let fields = old_response
        .as_object_mut()
        .and_then(|variant| variant.get_mut("StyledScreen"))
        .and_then(serde_json::Value::as_object_mut)
        .expect("styled response object");
    fields.remove("instance_id");
    fields.remove("output_version");
    let decoded: Response = serde_json::from_value(old_response).expect("parse old response");
    assert!(matches!(
        decoded,
        Response::StyledScreen {
            instance_id: None,
            output_version: None,
            ..
        }
    ));
}

#[test]
fn resizing_without_child_output_advances_the_output_version() {
    let runtime = scratch_dir("resize-ver");
    let socket = daemon::socket_path_in(&runtime, "s");
    let mut daemon = spawn::Daemon::spawn(&runtime);
    start_shell_session(&socket, "sleep 30");
    let before = capture_version(&socket);
    let response = client::request(
        &socket,
        &Request::Resize {
            name: "versioned".into(),
            size: Size::new(81, 24),
        },
    )
    .expect("resize session");
    assert_eq!(response, Response::Ok);
    assert!(capture_version(&socket) > before);
    client::request(
        &socket,
        &Request::Shutdown {
            requester_daemon_id: None,
            requester_session_id: None,
            requester_session_name: None,
            override_hosted: false,
        },
    )
    .expect("stop daemon");
    assert!(daemon.left_on_its_own(), "daemon should stop cleanly");
}

/// Connect using the original attach wire shape and leave the connection in
/// raw mode. Reading the acknowledgement one byte at a time keeps any initial
/// screen bytes in the socket for the caller.
fn raw_attach(path: &Path, name: &str) -> ipc::Stream {
    let mut stream = ipc::connect(path).expect("connect attach client");
    let mut request = serde_json::to_vec(&Request::Attach {
        name: name.to_string(),
    })
    .expect("serialize Attach");
    request.push(b'\n');
    stream.write_all(&request).expect("send Attach");
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    while stream.read(&mut byte).expect("read Attach response") == 1 {
        response.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    assert_eq!(
        serde_json::from_slice::<Response>(&response).expect("parse Attach response"),
        Response::Ok
    );
    stream
}

fn raw_attach_tracked(path: &Path, name: &str) -> (ipc::Stream, u64) {
    let mut stream = ipc::connect(path).expect("connect tracked attach client");
    let mut request = serde_json::to_vec(&Request::AttachTracked {
        name: name.to_string(),
    })
    .expect("serialize tracked Attach");
    request.push(b'\n');
    stream.write_all(&request).expect("send tracked Attach");
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    while stream
        .read(&mut byte)
        .expect("read tracked Attach response")
        == 1
    {
        response.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    let Response::AttachStarted { generation } =
        serde_json::from_slice(&response).expect("parse tracked Attach response")
    else {
        panic!("unexpected tracked attach response: {response:?}");
    };
    (stream, generation)
}

fn capture(path: &Path, name: &str) -> String {
    match client::request(
        path,
        &Request::Capture {
            name: name.to_string(),
        },
    ) {
        Ok(Response::Screen(text)) => text,
        other => panic!("capture failed: {other:?}"),
    }
}

fn capture_styled(path: &Path, name: &str, scrollback: usize) -> String {
    match client::request(
        path,
        &Request::CaptureStyled {
            name: name.to_string(),
            scrollback,
        },
    ) {
        Ok(Response::StyledScreen { rows, .. }) => rows
            .into_iter()
            .flat_map(|row| row.into_iter().map(|run| run.text))
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("styled capture failed: {other:?}"),
    }
}

fn wait_for(path: &Path, name: &str, needle: &str) -> String {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let screen = capture(path, name);
        if screen.contains(needle) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "{needle:?} never appeared in {name}. screen:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_session_screen(session: &Session, needle: &str) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let screen = session.screen_text().expect("viewer screen");
        if screen.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "attach did not repaint {needle:?}. viewer saw:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn collect_until_bytes(receiver: &Receiver<Vec<u8>>, needle: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + PATIENCE;
    let mut output = Vec::new();
    while !output.windows(needle.len()).any(|window| window == needle) {
        if Instant::now() >= deadline {
            panic!(
                "terminal output omitted {needle:?}; received {} bytes, tail: {}",
                output.len(),
                escaped_tail(&output)
            );
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(chunk) => output.extend_from_slice(&chunk),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                panic!(
                    "attach client exited before writing {needle:?}; received {} bytes, tail: {}",
                    output.len(),
                    escaped_tail(&output)
                )
            }
        }
    }
    output
}

fn escaped_tail(output: &[u8]) -> String {
    let start = output.len().saturating_sub(400);
    output[start..]
        .iter()
        .flat_map(|byte| std::ascii::escape_default(*byte))
        .map(char::from)
        .collect()
}

#[cfg(windows)]
fn assert_bytes_in_order(output: &[u8], needles: &[&[u8]]) {
    let mut cursor = 0;
    for needle in needles {
        let Some(offset) = output[cursor..]
            .windows(needle.len())
            .position(|window| window == *needle)
        else {
            panic!(
                "terminal output omitted ordered bytes {needle:?}; received {} bytes, tail: {}",
                output.len(),
                escaped_tail(output)
            );
        };
        cursor += offset + needle.len();
    }
}

fn assert_detach_restore(receiver: &Receiver<Vec<u8>>) {
    #[cfg(unix)]
    let _received_restore = collect_until_bytes(
        receiver,
        b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?2004l",
    );
    #[cfg(windows)]
    {
        let received = collect_until_bytes(receiver, b"remuda: detached");
        assert_bytes_in_order(
            &received,
            // ConPTY strips the mouse resets, reorders the shell's own ?2004l
            // and repaints (even truncates) lines, so only the typed round
            // trip and the detach prefix are stable; the release poll below
            // proves the detach itself.
            &[b"42-typed", b"remuda: detached"],
        );
    }
}

fn traced_input(path: &Path) -> Vec<u8> {
    std::fs::read_to_string(path)
        .expect("input trace file")
        .lines()
        .filter_map(|line| line.split_once(' '))
        .flat_map(|(_, bytes)| bytes.split_ascii_whitespace())
        .map(|byte| u8::from_str_radix(byte, 16).expect("hex byte"))
        .collect()
}

#[test]
fn a_second_listener_cannot_unlink_a_live_daemons_socket() {
    let path = scratch("live-listener");
    let _first = ipc::listen(&path).expect("first listener binds");

    let second = ipc::listen(&path).expect_err("a live listener must keep its address");
    // Windows refuses a second FILE_FLAG_FIRST_PIPE_INSTANCE pipe with
    // ERROR_ACCESS_DENIED, which std reports as PermissionDenied.
    let expected = if cfg!(windows) {
        std::io::ErrorKind::PermissionDenied
    } else {
        std::io::ErrorKind::AddrInUse
    };
    assert_eq!(second.kind(), expected);
    assert!(
        ipc::connect(&path).is_ok(),
        "the first daemon remains reachable"
    );
}

#[test]
fn sessions_are_listed_and_kept_apart() {
    let path = scratch("list");
    let _daemon = daemon_at(&path);

    // Negative control: the list is empty before anything is started, so a
    // "sessions appear" assertion cannot be satisfied by a list that is simply
    // always full.
    match client::request(&path, &Request::List).expect("list") {
        Response::Sessions(s) => assert!(s.is_empty(), "a fresh daemon holds nothing"),
        other => panic!("unexpected: {other:?}"),
    }

    new_session(&path, "alpha");
    new_session(&path, "bravo");

    let names = match client::request(&path, &Request::List).expect("list") {
        Response::Sessions(s) => s.into_iter().map(|s| s.name).collect::<Vec<_>>(),
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(names, ["alpha", "bravo"]);

    // Instructions must land in the session they name and nowhere else — the
    // property a shared pty would break.
    client::request(
        &path,
        &Request::SendLine {
            name: "alpha".into(),
            text: "echo $((6*7))-alpha".into(),
        },
    )
    .expect("send");

    wait_for(&path, "alpha", "42-alpha");
    let bravo = capture(&path, "bravo");
    assert!(
        !bravo.contains("42-alpha"),
        "bravo saw alpha's output:\n{bravo}"
    );
}

#[test]
fn an_unnamed_session_names_itself_after_the_program_and_dedupes() {
    // Bug A by construction: the person types the program, never a name, so no
    // leading positional can eat the word they meant to run.
    let path = scratch("generated");
    let _daemon = daemon_at(&path);

    let make = || {
        client::request(
            &path,
            &Request::New {
                name: None,
                command: vec!["sh".into()],
                size: Size::new(80, 24),
                cwd: None,
                env: None,
            },
        )
        .expect("new")
    };

    assert_eq!(make(), Response::Value("sh".into()));
    assert_eq!(make(), Response::Value("sh-2".into()));
    assert_eq!(make(), Response::Value("sh-3".into()));

    // And the caller can address what it just made, which is the whole reason
    // `New` had to start answering with a value.
    let seen: Vec<String> = match client::request(&path, &Request::List).expect("ls") {
        Response::Sessions(sessions) => sessions.into_iter().map(|s| s.name).collect(),
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(seen, vec!["sh", "sh-2", "sh-3"]);
}

#[test]
fn a_taken_name_is_refused_in_words() {
    let path = scratch("dup");
    let _daemon = daemon_at(&path);
    new_session(&path, "only");

    let again = client::request(
        &path,
        &Request::New {
            name: Some("only".into()),
            command: vec!["sh".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("second new");

    match again {
        Response::Error(reason) => assert!(
            reason.contains("name taken"),
            "the reason must say what went wrong, got: {reason}"
        ),
        other => panic!("a duplicate name must be refused, got {other:?}"),
    }
}

#[test]
fn a_session_launches_into_the_cwd_it_is_given() {
    // Without a `cwd`, a session inherits the daemon's own directory — this
    // proves the caller can override that, not merely that the daemon starts.
    let path = scratch("cwd");
    let _daemon = daemon_at(&path);

    let dir = scratch_dir("cwd-target");
    let response = client::request(
        &path,
        &Request::New {
            name: Some("in-tmp".into()),
            command: vec!["pwd".into()],
            size: Size::new(80, 24),
            cwd: Some(dir.to_string_lossy().into_owned()),
            env: None,
        },
    )
    .expect("new");
    assert_eq!(response, Response::Value("in-tmp".into()));

    // `pwd`'s own rendering of a path is platform-specific (Windows' shell
    // spells it `\\?\C:\...`, a POSIX shell `/c/...`) — the directory's own
    // name is the one substring both agree on, so that is what we look for.
    let needle = dir
        .file_name()
        .and_then(|n| n.to_str())
        .expect("scratch dir has a name")
        .to_string();
    wait_for(&path, "in-tmp", &needle);
}

#[test]
fn a_live_session_resizes_and_reports_its_new_size() {
    let path = scratch("resize");
    let _daemon = daemon_at(&path);
    new_session(&path, "resizable");

    let target = Size::new(120, 36);
    assert_eq!(
        client::request(
            &path,
            &Request::Resize {
                name: "resizable".into(),
                size: target
            },
        )
        .expect("resize"),
        Response::Ok
    );
    let sessions = match client::request(&path, &Request::List).expect("list") {
        Response::Sessions(sessions) => sessions,
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(sessions[0].size, target);
    client::request(
        &path,
        &Request::SendLine {
            name: "resizable".into(),
            text: "stty size".into(),
        },
    )
    .expect("ask terminal size");
    wait_for(&path, "resizable", "36 120");
}

#[test]
fn a_topic_directory_can_be_made_listed_and_removed_even_with_a_space_in_its_name() {
    // A space in the name is the whole point: `os.execute("mkdir -p ...")`
    // would mangle this, real `std::fs` calls do not.
    let path = scratch("dirverbs");
    let _daemon = daemon_at(&path);

    let base = scratch_dir("dirverbs-base");
    let target = base.join("topic with a space");
    let target_str = target.to_string_lossy().into_owned();

    let response = client::request(
        &path,
        &Request::Mkdir {
            path: target_str.clone(),
        },
    )
    .expect("mkdir");
    assert_eq!(response, Response::Ok);
    assert!(target.is_dir());

    std::fs::write(target.join("note.txt"), b"hi").expect("write");

    match client::request(
        &path,
        &Request::ListDir {
            path: target_str.clone(),
        },
    )
    .expect("list_dir")
    {
        Response::Entries(names) => assert_eq!(names, vec!["note.txt".to_string()]),
        other => panic!("unexpected: {other:?}"),
    }

    let response = client::request(&path, &Request::RemoveDirAll { path: target_str })
        .expect("remove_dir_all");
    assert_eq!(response, Response::Ok);
    assert!(!target.exists());
}

#[test]
fn a_wire_size_below_the_floor_is_clamped_not_honoured() {
    // A constructor that clamps is worth nothing if a peer can post JSON around
    // it. 11 columns is the width that silently ate keystrokes in the
    // implementation this replaces.
    let request: Request =
        serde_json::from_str(r#"{"New":{"name":"tiny","command":[],"size":{"cols":11,"rows":2}}}"#)
            .expect("parse");

    match request {
        Request::New { size, .. } => {
            assert_eq!(
                (size.cols(), size.rows()),
                (80, 24),
                "clamped on the way in"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// EXPR evaluated with `r` bound to session "target"'s Lua `ls()` row.
fn target_row(path: &Path, expr: &str) -> String {
    let code = format!(
        "for _, r in ipairs(remuda.ls()) do if r.name == 'target' then return tostring({expr}) end end"
    );
    match client::request(path, &Request::Eval { code, name: None }) {
        Ok(Response::Value(value)) => value,
        other => panic!("ls row: {other:?}"),
    }
}

fn wait_for_target_attach(
    path: &Path,
    child: &mut dyn portable_pty::Child,
    captured_output: &Mutex<Vec<u8>>,
) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let response = client::request(path, &Request::List)
            .unwrap_or_else(|error| Response::error(format!("List request failed: {error}")));
        let attached = matches!(
            &response,
            Response::Sessions(sessions)
                if sessions.iter().any(|session| session.name == "target" && session.attached)
        );
        if attached {
            return;
        }
        if let Some(status) = child.try_wait().expect("poll client A before takeover") {
            let output = captured_output
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            panic!(
                "client A exited before attaching ({status:?}); last List response: {response:?}; client A output: {:?}",
                String::from_utf8_lossy(&output)
            );
        }
        if Instant::now() >= deadline {
            let captured = captured_output
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let output = String::from_utf8_lossy(&captured).into_owned();
            drop(captured);
            let _ = child.kill();
            panic!(
                "client A never attached; last List response: {response:?}; client A output: {output:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_human_attaches_through_a_real_terminal_and_detaches_with_ctrl_backslash() {
    // The binary derives its socket as $REMUDA_RUNTIME_DIR/remuda/default.sock,
    // so the daemon must listen exactly there. Pointing the test somewhere else
    // is what made the first run fail — and it failed as "no repaint", which
    // names the wrong wall just like the swallowed startup error did.
    let dir = scratch_dir("attach");
    let path = daemon::socket_path_in(&dir, "default");
    let _daemon = daemon_at(&path);
    new_session(&path, "target");

    // Put something on screen BEFORE attaching, so the repaint has something to
    // prove. A viewer that only streams would show a blank terminal here.
    client::request(
        &path,
        &Request::SendLine {
            name: "target".into(),
            text: "echo $((11*11))-before".into(),
        },
    )
    .expect("send");
    wait_for(&path, "target", "121-before");

    // The real binary, on a real pty, so raw mode is actually entered.
    let trace_path = scratch_dir("attach-input-trace").join("input.hex");
    let _ = std::fs::remove_file(&trace_path);
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_remuda"));
    cmd.arg("attach");
    cmd.arg("target");
    cmd.env("REMUDA_RUNTIME_DIR", &dir);
    cmd.env("REMUDA_TRACE_INPUT", &trace_path);
    let viewer = Session::new(
        "viewer",
        Box::new(PtyAgent::spawn(cmd, Size::new(80, 24)).expect("spawn viewer")),
        Arc::new(SystemClock::new()),
    );
    let held = viewer.attach();
    let viewer_output = held.subscribe().expect("capture raw terminal output");

    // 1. Repaint: what was already there arrives without the program redrawing.
    wait_for_session_screen(&viewer, "121-before");

    // #136: a scripted SendLine is not a human keystroke.
    let human_idle = || match client::request(&path, &Request::List) {
        Ok(Response::Sessions(list)) => list
            .into_iter()
            .find(|s| s.name == "target")
            .and_then(|s| s.human_idle),
        other => panic!("list: {other:?}"),
    };
    assert_eq!(human_idle(), None);
    // In Lua a never-typed session is idle forever, so the field is always
    // present and its absence can only mean an older core.
    assert_eq!(target_row(&path, "r.human_idle == math.huge"), "true");

    // 2. Keystrokes reach the far session, and its output comes back.
    held.write_raw(b"echo $((6*7))-typed\r").expect("type");
    wait_for(&path, "target", "42-typed");
    // ...and the attached human's keystrokes are what `human_idle` counts,
    // in the listing and in Lua's `ls()` row.
    assert!(
        human_idle().is_some(),
        "typing through attach sets human_idle"
    );
    assert_eq!(target_row(&path, "r.human_idle < math.huge"), "true");

    // 3. Ctrl-\ detaches. The proof is on the far side: close is refused
    //    while attached and accepted afterwards, so this cannot pass by the
    //    client merely exiting for some other reason. Managed input is
    //    accepted either way (95fbe7e).
    let close = || {
        client::request(
            &path,
            &Request::Close {
                name: "target".into(),
            },
        )
    };
    assert!(
        matches!(close(), Ok(Response::Error(_))),
        "while a human holds it, close must be refused"
    );

    held.write_raw(&[client::DETACH]).expect("Ctrl-\\");
    drop(held);

    assert_detach_restore(&viewer_output);
    let traced = traced_input(&trace_path);
    assert!(
        traced
            .windows(b"echo $((6*7))-typed\r".len())
            .any(|window| { window == b"echo $((6*7))-typed\r" }),
        "input trace omitted the typed bytes: {traced:?}"
    );
    assert!(
        traced.contains(&client::DETACH),
        "input trace omitted Ctrl-\\: {traced:?}"
    );

    let resumed = client::request(
        &path,
        &Request::SendLine {
            name: "target".into(),
            text: "echo $((9*9))-after".into(),
        },
    );
    assert!(matches!(resumed, Ok(Response::Ok)), "{resumed:?}");
    wait_for(&path, "target", "81-after");

    // The server drops the hold asynchronously after Ctrl-\, so poll: close
    // must succeed once the release lands, and never before the deadline.
    let deadline = Instant::now() + PATIENCE;
    loop {
        let closed = close();
        if matches!(closed, Ok(Response::Ok)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "after detach the hold must be released, so close succeeds: {closed:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn any_key_after_attached_session_exit_restores_the_terminal() {
    let dir = scratch_dir("attach-exit-any-key");
    let path = daemon::socket_path_in(&dir, "default");
    let _daemon = daemon_at(&path);
    #[cfg(unix)]
    let target_command = vec!["sh".into(), "-c".into(), "sleep 3".into()];
    #[cfg(windows)]
    let target_command = vec![
        "powershell.exe".into(),
        "-NoLogo".into(),
        "-NoProfile".into(),
        "-Command".into(),
        "Start-Sleep -Seconds 3".into(),
    ];
    let created = client::request(
        &path,
        &Request::New {
            name: Some("target".into()),
            command: target_command,
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("start short-lived target");
    assert_eq!(created, Response::Value("target".into()));

    let exit_status = dir.join("attach-exit-status");
    #[cfg(unix)]
    let mut cmd = {
        let mut cmd = CommandBuilder::new("sh");
        cmd.args([
            "-c",
            "\"$REMUDA_BIN\" attach target; printf '%s' \"$?\" > \"$REMUDA_EXIT_STATUS\"",
        ]);
        cmd.env("REMUDA_BIN", env!("CARGO_BIN_EXE_remuda"));
        cmd.env("REMUDA_EXIT_STATUS", &exit_status);
        cmd
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = CommandBuilder::new("powershell.exe");
        cmd.args([
            "-NoLogo",
            "-NoProfile",
            "-Command",
            "& $env:REMUDA_BIN attach target; $code = $LASTEXITCODE; Set-Content -NoNewline -Path $env:REMUDA_EXIT_STATUS -Value $code",
        ]);
        cmd.env("REMUDA_BIN", env!("CARGO_BIN_EXE_remuda"));
        cmd.env("REMUDA_EXIT_STATUS", &exit_status);
        cmd
    };
    cmd.env("REMUDA_TRACE_ATTACH_EXIT", "1");
    cmd.env("REMUDA_RUNTIME_DIR", &dir);
    let viewer = Session::new(
        "viewer",
        Box::new(PtyAgent::spawn(cmd, Size::new(80, 24)).expect("spawn viewer")),
        Arc::new(SystemClock::new()),
    );
    let held = viewer.attach();
    let viewer_output = held.subscribe().expect("capture viewer output");
    #[cfg(windows)]
    let mut conpty_pending = Vec::new();
    #[cfg(windows)]
    let mut terminal_output = Vec::new();
    #[cfg(windows)]
    let mut terminal_writer = AttachedInputWriter(&held);
    let deadline = Instant::now() + PATIENCE;
    loop {
        #[cfg(windows)]
        answer_pending_conpty_queries(
            &viewer_output,
            &mut terminal_writer,
            &mut conpty_pending,
            &mut terminal_output,
        );
        let response = client::request(&path, &Request::List).expect("list after target exit");
        let target_present = match response {
            Response::Sessions(sessions) => sessions.iter().any(|s| s.name == "target"),
            other => panic!("list: {other:?}"),
        };
        if !target_present {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "short-lived target was not reaped"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Wait until the attach client has painted its post-exit prompt before
    // sending the release key. The daemon can reap the target before the
    // client's terminal output reaches this outer ConPTY.
    #[cfg(unix)]
    wait_for_session_screen(&viewer, "[remuda] target");
    #[cfg(windows)]
    {
        let deadline = Instant::now() + PATIENCE;
        loop {
            answer_pending_conpty_queries(
                &viewer_output,
                &mut terminal_writer,
                &mut conpty_pending,
                &mut terminal_output,
            );
            let screen = viewer.screen_text().expect("viewer screen");
            if screen.contains("press any key") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "attach client never painted its post-exit prompt; terminal bytes:\n{}\nviewer screen:\n{screen}",
                escaped_tail(&terminal_output),
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    held.write_raw(b"k").expect("release attach with any key");
    let deadline = Instant::now() + PATIENCE;
    while viewer.is_alive() {
        #[cfg(windows)]
        answer_pending_conpty_queries(
            &viewer_output,
            &mut terminal_writer,
            &mut conpty_pending,
            &mut terminal_output,
        );
        if Instant::now() >= deadline {
            #[cfg(unix)]
            let output = viewer_output.try_iter().flatten().collect::<Vec<_>>();
            #[cfg(windows)]
            let output = terminal_output.clone();
            let saw_dsr = output.windows(4).any(|window| window == b"\x1b[6n");
            panic!(
                "attach client did not exit after one key; saw ESC[6n DSR query: {saw_dsr}\nterminal bytes:\n{}\nviewer screen:\n{}",
                String::from_utf8_lossy(&output),
                viewer.screen_text().expect("viewer screen on timeout"),
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_to_string(&exit_status)
            .expect("wrapper records attach client exit status")
            .trim(),
        "0"
    );
    #[cfg(unix)]
    let restored = collect_until_bytes(
        &viewer_output,
        b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?2004l",
    );
    #[cfg(windows)]
    let restored = terminal_output;
    if std::env::var_os("REMUDA_TRACE_ATTACH_EXIT").is_some() {
        let saw_dsr = restored.windows(4).any(|window| window == b"\x1b[6n");
        eprintln!(
            "attach input trace terminal bytes (saw ESC[6n DSR query: {saw_dsr}):\n{}\nviewer screen:\n{}",
            String::from_utf8_lossy(&restored),
            viewer
                .screen_text()
                .expect("viewer screen after attach exit"),
        );
    }
}

#[cfg(windows)]
#[test]
fn an_exited_conpty_session_is_reaped_by_list() {
    let dir = scratch_dir("conpty-exit-reap");
    let path = daemon::socket_path_in(&dir, "default");
    let _daemon = daemon_at(&path);
    let response = client::request(
        &path,
        &Request::New {
            name: Some("short-lived".into()),
            command: vec![
                "powershell.exe".into(),
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-Command".into(),
                "Start-Sleep -Seconds 1".into(),
            ],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("start short-lived PowerShell target");
    assert_eq!(response, Response::Value("short-lived".into()));

    let deadline = Instant::now() + PATIENCE;
    loop {
        let response = client::request(&path, &Request::List).expect("list sessions");
        let present = match response {
            Response::Sessions(sessions) => sessions.iter().any(|s| s.name == "short-lived"),
            other => panic!("list: {other:?}"),
        };
        if !present {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "exited ConPTY session was not reaped"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn a_new_attach_takes_over_and_old_raw_clients_get_a_plain_notice() {
    let path = scratch("takeover");
    let _daemon = daemon_at(&path);
    new_session(&path, "target");

    // This connection models a pre-upgrade client: it knows only the existing
    // Attach request followed by a raw byte stream, and treats EOF as ordinary.
    let mut old = raw_attach(&path, "target");
    let mut current = raw_attach(&path, "target");
    let mut old_output = Vec::new();
    old.read_to_end(&mut old_output)
        .expect("displaced connection closes cleanly");
    assert!(
        old_output
            .windows(b"[remuda] attached elsewhere, detached".len())
            .any(|window| window == b"[remuda] attached elsewhere, detached"),
        "older clients receive a printable explanation before EOF: {old_output:?}"
    );

    current
        .write_all(b"echo takeover-input\r")
        .expect("current client input");
    wait_for(&path, "target", "takeover-input");
    assert_eq!(target_row(&path, "r.attached"), "true");

    // A third attach is accepted immediately, even while the second daemon
    // handler is still unwinding its raw connection.
    let third = raw_attach(&path, "target");
    drop(current);
    drop(third);
    let deadline = Instant::now() + PATIENCE;
    while target_row(&path, "r.attached") == "true" {
        assert!(Instant::now() < deadline, "all attaches should release");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_agent_printing_the_takeover_notice_does_not_end_a_tracked_attach() {
    let path = scratch("spoof-notice");
    let _daemon = daemon_at(&path);
    new_session(&path, "target");
    let (stream, generation) = raw_attach_tracked(&path, "target");

    client::request(
        &path,
        &Request::SendLine {
            name: "target".into(),
            text: "printf '\\r\\n[remuda] attached elsewhere, detached\\r\\n'".into(),
        },
    )
    .expect("print the exact courtesy notice from the session");
    wait_for(&path, "target", "attached elsewhere, detached");
    assert_eq!(
        client::request(
            &path,
            &Request::AttachStatus {
                name: "target".into(),
                generation,
            },
        )
        .expect("query generation status"),
        Response::AttachStatus { taken_over: false }
    );
    assert_eq!(target_row(&path, "r.attached"), "true");
    drop(stream);
}

#[test]
fn a_tracked_client_exits_on_takeover_without_waiting_for_another_key() {
    let dir = scratch_dir("exit-on-takeover");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path);
    new_session(&path, "target");

    let pty = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open client pty");
    let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_remuda"));
    command.args(["-s", "s", "attach", "target"]);
    command.env("REMUDA_RUNTIME_DIR", &dir);
    let mut child = pty.slave.spawn_command(command).expect("spawn client A");
    // The test only uses the master side after spawning the child. Keeping the
    // parent's slave handle open can prevent some PTY implementations from
    // reporting EOF after the child exits.
    drop(pty.slave);
    #[cfg(windows)]
    let mut input_writer = pty.master.take_writer().expect("take client PTY writer");
    let mut reader = pty.master.try_clone_reader().expect("clone pty reader");
    let captured_output = Arc::new(Mutex::new(Vec::new()));
    let thread_output = Arc::clone(&captured_output);
    #[cfg(unix)]
    let (output_tx, output_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        #[cfg(windows)]
        let mut pending = Vec::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    #[cfg(windows)]
                    conpty::answer_conpty_cursor_queries(
                        &mut input_writer,
                        &mut pending,
                        &buf[..n],
                    );
                    thread_output
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .extend_from_slice(&buf[..n]);
                    #[cfg(unix)]
                    if output_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    wait_for_target_attach(&path, child.as_mut(), &captured_output);
    let _current = raw_attach(&path, "target");

    let deadline = Instant::now() + PATIENCE;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll client A") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "client A waited for terminal input after takeover"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.exit_code(), 2);
    #[cfg(unix)]
    {
        // Do not wait for PTY EOF here. Some platforms keep a reader open
        // after the child exits; the notice and mode resets are the evidence
        // this test needs, and arrive before EOF.
        let output = collect_until_bytes(&output_rx, b"\x1b[?1000l");
        assert!(
            output
                .windows(b"attached elsewhere, detached".len())
                .any(|window| window == b"attached elsewhere, detached"),
            "takeover reason was not displayed: {output:?}"
        );
        assert!(
            output
                .windows(b"\x1b[?1000l".len())
                .any(|w| w == b"\x1b[?1000l")
                && output
                    .windows(b"\x1b[?2004l".len())
                    .any(|w| w == b"\x1b[?2004l"),
            "the input modes were not reset before exit: {output:?}"
        );
    }
    // ConPTY does not provide stable screen bytes for assertions. On Windows,
    // the process exit and takeover exit code above prove client A detached.
}

#[test]
fn session_listing_reports_the_child_mouse_tracking_mode() {
    let path = scratch("mouse-tracking-list");
    let _daemon = daemon_at(&path);
    new_session(&path, "target");

    let listed = || match client::request(&path, &Request::List) {
        Ok(Response::Sessions(sessions)) => sessions
            .into_iter()
            .find(|session| session.name == "target")
            .expect("target is listed"),
        other => panic!("list: {other:?}"),
    };
    assert!(!listed().mouse_tracking);

    client::request(
        &path,
        &Request::SendLine {
            name: "target".into(),
            text: "printf '\\033[?1000h'; printf 'ready-%s\\n' mode".into(),
        },
    )
    .expect("enable mouse tracking");
    wait_for(&path, "target", "ready-mode");
    assert!(listed().mouse_tracking);
}

#[cfg(unix)]
#[test]
fn direct_attach_mouse_scrolls_the_full_history_and_returns_to_live_output() {
    let dir = scratch_dir("attach-mouse-scroll");
    let path = daemon::socket_path_in(&dir, "default");
    let _daemon = daemon_at(&path);
    new_session(&path, "target");

    client::request(
        &path,
        &Request::SendLine {
            name: "target".into(),
            text: "i=0; while [ $i -lt 10000 ]; do printf 'history-%05d\\n' \"$i\"; i=$((i+1)); done; printf 'history-end\\n'".into(),
        },
    )
    .expect("write retained history");
    // `history-end` appears in the echoed shell command before execution; wait
    // for a generated row that is not present in the command text.
    wait_for(&path, "target", "history-09999");
    wait_for(&path, "target", "history-end");
    let oldest = capture_styled(&path, "target", 10_000);
    assert!(
        oldest.contains("history-00014"),
        "the 10,000-row capture should reach old output; capture tail: {}",
        &oldest[oldest.len().saturating_sub(500)..]
    );

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_remuda"));
    cmd.args(["attach", "target"]);
    cmd.env("REMUDA_RUNTIME_DIR", &dir);
    let viewer = Session::new(
        "viewer",
        Box::new(PtyAgent::spawn(cmd, Size::new(80, 24)).expect("spawn viewer")),
        Arc::new(SystemClock::new()),
    );
    let held = viewer.attach();
    let output = held.subscribe().expect("capture viewer output");
    wait_for_session_screen(&viewer, "history-end");

    held.write_raw(b"\x1b[<64;10;10M").expect("wheel up");
    let near_history = collect_until_bytes(&output, b"[scrollback: 3 rows");
    assert!(near_history
        .windows(b"history-end".len())
        .any(|w| w == b"history-end"));

    held.write_raw(b"\x1b[<65;10;10M")
        .expect("wheel down to live");
    collect_until_bytes(&output, b"history-end");

    held.write_raw(b"\x1b[5~")
        .expect("PageUp enters scrollback");
    collect_until_bytes(&output, b"[scrollback: 24 rows");
    held.write_raw(b"\x1b[6~")
        .expect("PageDown returns to live");
    collect_until_bytes(&output, b"history-end");

    client::request(
        &path,
        &Request::SendLine {
            name: "target".into(),
            text: "printf 'resume-%s\\n' $((17*23))".into(),
        },
    )
    .expect("write after scroll mode");
    wait_for(&path, "target", "resume-391");
    let resumed = collect_until_bytes(&output, b"resume-391");
    assert!(
        resumed
            .windows(b"resume-391".len())
            .filter(|w| *w == b"resume-391")
            .count()
            == 1,
        "live output should resume exactly once after the historical repaint"
    );
}

#[cfg(unix)]
#[test]
fn direct_attach_forwards_mouse_reports_in_the_live_child_encoding() {
    use remuda_core::agent::{MouseEncoding, MouseMode, MouseState};

    let dir = scratch_dir("attach-mouse-forward");
    let path = daemon::socket_path_in(&dir, "default");
    let _daemon = daemon_at(&path);
    let script = "stty raw -echo; printf '\\033[?1000h\\033[?1006h'; dd bs=1 count=12 2>/dev/null | od -An -tx1; printf '\\nmouse-forwarded\\n'";
    let created = client::request(
        &path,
        &Request::New {
            name: Some("target".into()),
            command: vec!["sh".into(), "-c".into(), script.into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("start mouse-reporting child");
    assert_eq!(created, Response::Value("target".into()));

    let deadline = Instant::now() + PATIENCE;
    loop {
        match client::request(
            &path,
            &Request::MouseState {
                name: "target".into(),
            },
        ) {
            Ok(Response::MouseState(MouseState {
                mode: MouseMode::PressRelease,
                encoding: MouseEncoding::Sgr,
            })) => break,
            other => {
                assert!(
                    Instant::now() < deadline,
                    "child mouse mode not observed: {other:?}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_remuda"));
    cmd.args(["attach", "target"]);
    cmd.env("REMUDA_RUNTIME_DIR", &dir);
    let viewer = Session::new(
        "viewer",
        Box::new(PtyAgent::spawn(cmd, Size::new(80, 24)).expect("spawn viewer")),
        Arc::new(SystemClock::new()),
    );
    let held = viewer.attach();
    let output = held.subscribe().expect("capture viewer output");
    held.write_raw(b"\x1b[<64;10;10M")
        .expect("deliver host wheel report");
    wait_for(&path, "target", "mouse-forwarded");
    let screen = capture(&path, "target").replace([' ', '\n'], "");
    assert!(
        screen.contains("1b5b3c36343b31303b31304d"),
        "child did not receive the SGR mouse report: {screen}"
    );

    held.write_raw(&[client::DETACH]).expect("detach");
    drop(held);
    let _ = collect_until_bytes(
        &output,
        b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?2004l",
    );
}

#[cfg(unix)]
#[test]
fn mouse_off_knob_and_toggle_pass_reports_to_the_child_and_restore_host_modes() {
    let dir = scratch_dir("attach-mouse-toggle");
    let path = daemon::socket_path_in(&dir, "default");
    let _daemon = daemon_at(&path);
    let script = "stty raw -echo; printf 'child-ready\\n'; cat";
    let created = client::request(
        &path,
        &Request::New {
            name: Some("target".into()),
            command: vec!["sh".into(), "-c".into(), script.into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("start raw child");
    assert_eq!(created, Response::Value("target".into()));
    wait_for(&path, "target", "child-ready");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_remuda"));
    cmd.args(["attach", "target", "--mouse=false"]);
    cmd.env("REMUDA_RUNTIME_DIR", &dir);
    let viewer = Session::new(
        "viewer",
        Box::new(PtyAgent::spawn(cmd, Size::new(80, 24)).expect("spawn viewer")),
        Arc::new(SystemClock::new()),
    );
    let held = viewer.attach();
    let output = held.subscribe().expect("capture viewer output");
    let initial = collect_until_bytes(&output, b"\x1b[?25");
    assert!(!initial
        .windows(b"\x1b[?1000h".len())
        .any(|w| w == b"\x1b[?1000h"));
    assert!(!initial
        .windows(b"\x1b[?1006h".len())
        .any(|w| w == b"\x1b[?1006h"));

    let wheel = b"\x1b[<64;10;10M";
    held.write_raw(wheel)
        .expect("wheel reaches child while disabled");
    let received = collect_until_bytes(&output, wheel);
    assert!(received.windows(wheel.len()).any(|w| w == wheel));

    held.write_raw(&[0x1d])
        .expect("Ctrl-] reaches child when disabled");
    let received_toggle = collect_until_bytes(&output, &[0x1d]);
    assert!(
        received_toggle.contains(&0x1d),
        "disabled toggle key must reach the child"
    );
    assert!(!received_toggle
        .windows(b"\x1b[?1000h".len())
        .any(|w| w == b"\x1b[?1000h"));

    held.write_raw(&[client::DETACH]).expect("detach");
    drop(held);
    let restored = collect_until_bytes(
        &output,
        b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?2004l",
    );
    assert!(restored
        .windows(b"\x1b[?1000l".len())
        .any(|w| w == b"\x1b[?1000l"));
    assert!(restored
        .windows(b"\x1b[?1006l".len())
        .any(|w| w == b"\x1b[?1006l"));
}

#[test]
fn a_registered_schedule_actually_fires_through_a_real_daemon() {
    let path = scratch("schedule");
    let _daemon = daemon_at(&path);

    // `every` is seconds on native's own clock, kept tiny so the test's
    // PATIENCE window covers many ticks rather than racing a single one.
    let register = Request::Eval {
        code: r#"
            remuda.fired = 0
            remuda.schedule({
              name = "test-schedule",
              every = 0.01,
              run = function() remuda.fired = remuda.fired + 1 end,
            })
        "#
        .to_string(),
        name: None,
    };
    match client::request(&path, &register).expect("register") {
        Response::Value(_) => {}
        other => panic!("unexpected: {other:?}"),
    }

    let deadline = Instant::now() + PATIENCE;
    loop {
        let fired = client::request(
            &path,
            &Request::Eval {
                code: "return remuda.fired".to_string(),
                name: None,
            },
        )
        .expect("read fired");
        // >= 2, not != "0" — the name promises PERIODIC firing, and a
        // scheduler that fires once and stops must fail this test.
        if matches!(&fired, Response::Value(v) if v.parse::<u32>().is_ok_and(|n| n >= 2)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the schedule did not fire at least twice: {fired:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn eval(path: &Path, code: &str) -> String {
    match client::request(
        path,
        &Request::Eval {
            code: code.to_string(),
            name: None,
        },
    )
    .expect("eval")
    {
        Response::Value(v) => v,
        other => panic!("unexpected: {other:?}"),
    }
}

fn read_count(path: &Path, code: &str) -> u32 {
    eval(path, code).parse().expect("a number")
}

#[test]
fn a_session_exited_hook_fires_when_a_real_session_dies() {
    // `session_exited` is remuda's own first real event: fired once for each
    // session the daemon's ticker notices has died. Nothing calls
    // `remuda.emit("session_exited", ...)` anywhere yet, so this must fail red.
    let path = scratch("session-exited");
    let _daemon = daemon_at(&path);

    eval(
        &path,
        r#"
            remuda._session_exited_names = {}
            remuda.on("session_exited", function(name)
                table.insert(remuda._session_exited_names, name)
            end)
        "#,
    );

    // Exits on its own almost immediately, so the ticker's very next tick
    // (TICK_PERIOD is 1s, see daemon.rs) has something dead to reap well
    // inside PATIENCE.
    let response = client::request(
        &path,
        &Request::New {
            name: Some("short-lived".into()),
            command: vec!["sh".into(), "-c".into(), "exit 0".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("new");
    assert_eq!(response, Response::Value("short-lived".into()));

    // Negative control: a session that stays alive throughout must never be
    // named — this is fine to trivially hold today too, since nothing fires
    // the event for anyone yet.
    new_session(&path, "long-lived");

    let deadline = Instant::now() + PATIENCE;
    loop {
        let seen = eval(
            &path,
            "return table.concat(remuda._session_exited_names, ',')",
        );
        if seen.split(',').any(|n| n == "short-lived") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "session_exited never fired for short-lived. seen so far: {seen:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let seen = eval(
        &path,
        "return table.concat(remuda._session_exited_names, ',')",
    );
    assert!(
        !seen.split(',').any(|n| n == "long-lived"),
        "a still-alive session must never appear in session_exited names: {seen:?}"
    );
}

#[test]
fn a_session_exited_hook_fires_once_when_a_session_is_closed() {
    // `close` removes the entry itself, so the reaper never sees it die: the
    // close path must fire the event, and the reaper must not fire it again.
    let path = scratch("session-exited-close");
    let _daemon = daemon_at(&path);

    eval(
        &path,
        r#"
            remuda._session_exited_names = {}
            remuda.on("session_exited", function(name)
                table.insert(remuda._session_exited_names, name)
            end)
        "#,
    );
    new_session(&path, "closed");
    let response = client::request(
        &path,
        &Request::Close {
            name: "closed".into(),
        },
    )
    .expect("close");
    assert_eq!(response, Response::Ok);

    let count = "local n = 0 for _, v in ipairs(remuda._session_exited_names) do if v == 'closed' then n = n + 1 end end return n";
    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, count) == 0 {
        assert!(
            Instant::now() < deadline,
            "session_exited never fired for a closed session"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Two more ticks: a late reap must not fire a duplicate.
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(
        read_count(&path, count),
        1,
        "a closed session must fire exactly once"
    );
}

#[test]
fn a_session_exited_hook_still_fires_once_when_ls_reaps_before_the_tick() {
    // `Registry::reap()` removes what it finds and hands it only to whoever
    // calls first. A `List` that reaps well inside TICK_PERIOD must not make
    // the ticker's own later reap silently find nothing to emit for — and it
    // must not double-emit either, once every reap site funnels through one
    // notifying path.
    let path = scratch("session-exited-race");
    let _daemon = daemon_at(&path);

    eval(
        &path,
        r#"
            remuda._race_exited_names = {}
            remuda.on("session_exited", function(name)
                table.insert(remuda._race_exited_names, name)
            end)
        "#,
    );

    let response = client::request(
        &path,
        &Request::New {
            name: Some("race-short-lived".into()),
            command: vec!["sh".into(), "-c".into(), "exit 0".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("new");
    assert_eq!(response, Response::Value("race-short-lived".into()));

    // Well inside TICK_PERIOD (1s) — this reaps the session before the
    // ticker's own tick has a chance to.
    std::thread::sleep(Duration::from_millis(80));
    client::request(&path, &Request::List).expect("list");

    // Give the ticker a full period too, so a double-emit (both paths firing)
    // would have every chance to show up if the funnel were not idempotent.
    std::thread::sleep(Duration::from_millis(1200));

    let names = eval(&path, "return table.concat(remuda._race_exited_names, ',')");
    let count = names
        .split(',')
        .filter(|n| *n == "race-short-lived")
        .count();
    assert_eq!(
        count, 1,
        "expected exactly one session_exited for race-short-lived, got {count}: {names:?}"
    );
}

#[test]
fn a_hostile_session_name_reaches_the_hook_byte_for_byte() {
    // `Request::New`'s name has no validation at all beyond uniqueness (see
    // `spawn`/`Registry::register`) — a caller may hand over anything. The
    // emit path splices it into a Lua source string via `mcp::lua_string`, so
    // a quote or backslash proves that escaping is real, not merely untested.
    let path = scratch("session-exited-hostile");
    let _daemon = daemon_at(&path);
    let hostile = r#"it's a "test" \with\backslashes"#;

    eval(&path, "remuda._hostile_exited_names = {}");
    eval(
        &path,
        "remuda.on(\"session_exited\", function(name) \
            table.insert(remuda._hostile_exited_names, name) \
         end)",
    );

    let response = client::request(
        &path,
        &Request::New {
            name: Some(hostile.to_string()),
            command: vec!["sh".into(), "-c".into(), "exit 0".into()],
            size: Size::new(80, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("new");
    assert_eq!(response, Response::Value(hostile.to_string()));

    let deadline = Instant::now() + PATIENCE;
    loop {
        let count = read_count(&path, "return #remuda._hostile_exited_names");
        if count >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "session_exited never fired for the hostile name"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Exact match, not a substring or a count — proof the name arrived intact
    // rather than truncated or escaped-then-left-escaped by a naive splice.
    assert_eq!(
        eval(&path, "return remuda._hostile_exited_names[1]"),
        hostile,
        "the hostile name did not survive the emit path unchanged"
    );
    assert_eq!(
        read_count(&path, "return #remuda._hostile_exited_names"),
        1,
        "no injected code should have run more than once, nor duplicated the entry"
    );
}

#[test]
fn cancelling_one_same_label_schedule_leaves_the_other_firing() {
    // The gap measured on 09-13: a name-keyed table means a second
    // registrant under the same label silently replaces the first. A handle
    // fixes it — two schedules can share a label and coexist, and only the
    // handle that was actually cancelled stops.
    let path = scratch("schedule-cancel");
    let _daemon = daemon_at(&path);

    eval(
        &path,
        r#"
            remuda.fired_a, remuda.fired_b = 0, 0
            remuda.handle_a = remuda.schedule({
              name = "dup",
              every = 0.01,
              run = function() remuda.fired_a = remuda.fired_a + 1 end,
            })
            remuda.handle_b = remuda.schedule({
              name = "dup",
              every = 0.01,
              run = function() remuda.fired_b = remuda.fired_b + 1 end,
            })
        "#,
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.fired_a") < 2
        || read_count(&path, "return remuda.fired_b") < 2
    {
        assert!(
            Instant::now() < deadline,
            "both same-label schedules must fire independently"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    eval(&path, "remuda.cancel(remuda.handle_a)");
    let a_at_cancel = read_count(&path, "return remuda.fired_a");

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.fired_b") < a_at_cancel + 2 {
        assert!(
            Instant::now() < deadline,
            "the surviving schedule must keep firing"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        read_count(&path, "return remuda.fired_a"),
        a_at_cancel,
        "the cancelled schedule must not fire again"
    );
}

#[test]
fn one_throwing_schedule_does_not_starve_the_others() {
    // Handles are table keys, so iteration order is unknown: a throw that
    // escapes the loop skips whichever schedules happen to come after it.
    // `every` is far beyond the real ticker's clock, so only the ticks this
    // test drives by hand can fire anything.
    let path = scratch("schedule-throw");
    let _daemon = daemon_at(&path);

    eval(
        &path,
        r#"
            remuda.healthy_fired = 0
            remuda.schedule({ every = 1e9, run = function() error("boom") end })
            for _ = 1, 20 do
              remuda.schedule({
                every = 1e9,
                run = function() remuda.healthy_fired = remuda.healthy_fired + 1 end,
              })
            end
        "#,
    );

    assert_eq!(
        eval(
            &path,
            "return tostring(pcall(remuda._run_due_schedules, 2e9))"
        ),
        "true",
        "a throwing schedule must not abort the tick"
    );
    assert_eq!(
        read_count(&path, "return remuda.healthy_fired"),
        20,
        "every healthy schedule must fire despite the throwing one"
    );
}

#[test]
fn a_schedule_registered_during_a_tick_first_fires_on_the_next_tick() {
    // Adding keys to a table mid-`pairs` is undefined in Lua: new entries may
    // or may not be visited, or `next` may raise. A snapshot makes it exact.
    let path = scratch("schedule-mutate");
    let _daemon = daemon_at(&path);

    eval(
        &path,
        r#"
            remuda.late_fired = 0
            remuda.schedule({ every = 1e9, run = function()
              for _ = 1, 50 do
                remuda.schedule({
                  every = 1e9,
                  run = function() remuda.late_fired = remuda.late_fired + 1 end,
                })
              end
            end })
        "#,
    );

    assert_eq!(
        eval(
            &path,
            "return tostring(pcall(remuda._run_due_schedules, 2e9))"
        ),
        "true"
    );
    assert_eq!(
        read_count(&path, "return remuda.late_fired"),
        0,
        "schedules added during a tick must wait for the next one"
    );
    eval(&path, "remuda._run_due_schedules(4e9)");
    assert_eq!(read_count(&path, "return remuda.late_fired"), 50);
}

#[test]
fn event_counts_reads_zero_before_any_emit_and_n_after_real_fires() {
    // `remuda.event_counts()` does not exist yet — this must fail red with a
    // Lua "attempt to call a nil value" error, not a compile error.
    let path = scratch("event-counts");
    let _daemon = daemon_at(&path);

    eval(&path, r#"remuda.on("counter_test_event", function() end)"#);

    // A name `emit` has never been called with is absent, not zero — Lua's
    // `nil` is the only value that says so.
    assert_eq!(
        eval(&path, "return remuda.event_counts()['counter_test_event']"),
        "nil",
        "an event never emitted must be absent from event_counts(), not zero"
    );

    for _ in 0..3 {
        eval(&path, "remuda.emit('counter_test_event')");
    }

    assert_eq!(
        read_count(&path, "return remuda.event_counts()['counter_test_event']"),
        3,
        "event_counts() must count every real emit call"
    );
}

#[test]
fn schedule_fires_reads_zero_before_any_run_and_n_after_real_ticks() {
    // `remuda.schedule_fires()` does not exist yet — same nil-call failure
    // expected as `event_counts()` above.
    let path = scratch("schedule-fires");
    let _daemon = daemon_at(&path);

    eval(
        &path,
        r#"
            remuda.schedule({
              name = "counter_test_schedule",
              every = 1,
              run = function() end,
            })
        "#,
    );

    // Before any tick has elapsed, an unfired named schedule is absent too.
    assert_eq!(
        eval(
            &path,
            "return remuda.schedule_fires()['counter_test_schedule']"
        ),
        "nil",
        "a schedule that has never fired must be absent, not zero"
    );

    let deadline = Instant::now() + PATIENCE;
    loop {
        let fires = eval(
            &path,
            "return remuda.schedule_fires()['counter_test_schedule']",
        );
        if fires.parse::<u32>().is_ok_and(|n| n >= 3) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "counter_test_schedule did not fire at least 3 times: {fires:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // An unnamed schedule (`spec.name` left nil) must never appear as a key
    // at all — `t[nil] = x` is a Lua error, so the increment must be skipped
    // rather than crash the daemon. A side-channel counter proves it really
    // fired even though `schedule_fires()` must stay silent about it.
    eval(&path, "remuda._unnamed_fired = 0");
    eval(
        &path,
        r#"
            remuda.schedule({
              every = 1,
              run = function() remuda._unnamed_fired = remuda._unnamed_fired + 1 end,
            })
        "#,
    );

    let before_keys = eval(
        &path,
        "local n = 0 for _ in pairs(remuda.schedule_fires()) do n = n + 1 end return n",
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda._unnamed_fired") < 3 {
        assert!(
            Instant::now() < deadline,
            "the unnamed schedule never fired"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let after_keys = eval(
        &path,
        "local n = 0 for _ in pairs(remuda.schedule_fires()) do n = n + 1 end return n",
    );
    assert_eq!(
        before_keys, after_keys,
        "an unnamed schedule must add no key to schedule_fires(), even after firing"
    );
}

#[test]
fn mutating_the_returned_event_counts_table_does_not_change_internal_state() {
    // Both accessors must hand back a snapshot, the same guarantee
    // `remuda.emit`'s own hook snapshot already keeps for `remuda.hooks`
    // (see tools.lua) — a caller mutating what it was handed must never
    // reach back into the daemon's own counters.
    let path = scratch("counters-are-copies");
    let _daemon = daemon_at(&path);

    eval(&path, r#"remuda.on("copy_test_event", function() end)"#);
    eval(&path, "remuda.emit('copy_test_event')");
    eval(
        &path,
        "local t = remuda.event_counts() t['copy_test_event'] = 9999",
    );
    assert_eq!(
        read_count(&path, "return remuda.event_counts()['copy_test_event']"),
        1,
        "event_counts() must return a copy, not a live reference"
    );

    eval(
        &path,
        r#"
            remuda.schedule({
              name = "copy_test_schedule",
              every = 1,
              run = function() end,
            })
        "#,
    );
    let deadline = Instant::now() + PATIENCE;
    loop {
        let fires = eval(
            &path,
            "return remuda.schedule_fires()['copy_test_schedule']",
        );
        if fires.parse::<u32>().is_ok_and(|n| n >= 1) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "copy_test_schedule never fired: {fires:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let before = read_count(
        &path,
        "return remuda.schedule_fires()['copy_test_schedule']",
    );
    eval(
        &path,
        "local t = remuda.schedule_fires() t['copy_test_schedule'] = 9999",
    );
    assert_eq!(
        read_count(
            &path,
            "return remuda.schedule_fires()['copy_test_schedule']"
        ),
        before,
        "schedule_fires() must return a copy, not a live reference"
    );
}

#[test]
fn the_daemon_names_the_build_it_was_started_from() {
    let path = scratch("version");
    let _daemon = daemon_at(&path);

    match client::request(&path, &Request::Version).expect("version") {
        Response::Value(said) => assert_eq!(said, remuda_native::dist::BUILD_VERSION),
        other => panic!("unexpected: {other:?}"),
    }
}

// Unix only: the regression is SIGPIPE, and `true` is not a Windows command.
#[test]
#[cfg(unix)]
fn auto_started_daemon_survives_a_fast_process_exit() {
    let dir = scratch_dir("autostart-fast-process");

    let start = remuda_timed(
        &dir,
        &[
            "-s",
            "s",
            "-e",
            "remuda.process{argv = {'true'}, on_exit = 'fast-exit'}",
        ],
    );
    assert!(
        start.status.success(),
        "auto-started process call failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    // Give the daemon time to deliver the exit event before asking the same
    // auto-started instance a second question.
    std::thread::sleep(Duration::from_millis(100));
    let answer = remuda_timed(&dir, &["-s", "s", "-e", "return 1"]);
    assert!(
        answer.status.success(),
        "the daemon died after its fast child exited: {}",
        String::from_utf8_lossy(&answer.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&answer.stdout).trim(), "1");

    let _ = remuda(&dir, &["-s", "s", "stop", "-f"]);
}

/// A daemon's log is post-mortem evidence: the next auto-start must append to
/// it, not wipe it. Unix only: CLI auto-start with piped output hangs on Windows.
#[test]
#[cfg(unix)]
fn an_auto_start_appends_to_the_previous_daemons_log() {
    let dir = scratch_dir("log-append");
    let log = daemon::socket_path_in(&dir, "s").with_extension("log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "panic trace from the last daemon\n").unwrap();

    let out = remuda_timed(&dir, &["-s", "s", "-e", "return 1"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = remuda(&dir, &["-s", "s", "stop", "-f"]);

    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("panic trace from the last daemon"), "{text}");
    assert!(text.contains("--- remuda daemon start pid "), "{text}");
}

/// Appending must not grow forever: past ~1 MiB the old log moves to `.log.1`.
#[test]
#[cfg(unix)]
fn an_oversized_daemon_log_is_rotated_on_auto_start() {
    let dir = scratch_dir("log-rotate");
    let log = daemon::socket_path_in(&dir, "s").with_extension("log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let big = format!("old marker\n{}", "x".repeat(1_100_000));
    std::fs::write(&log, big).unwrap();

    let out = remuda_timed(&dir, &["-s", "s", "-e", "return 1"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = remuda(&dir, &["-s", "s", "stop", "-f"]);

    let rotated = std::fs::read_to_string(log.with_extension("log.1")).unwrap();
    assert!(rotated.starts_with("old marker"));
    let fresh = std::fs::read_to_string(&log).unwrap();
    assert!(fresh.len() < 10_000, "{} bytes", fresh.len());
    assert!(fresh.contains("--- remuda daemon start pid "), "{fresh}");
}

/// What a person types, with its own pipes and no terminal — so `restart`
/// reaches the "nothing to ask on" branch rather than blocking on a prompt.
fn remuda(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(args)
        .env("REMUDA_RUNTIME_DIR", dir)
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run remuda")
}

/// Bounded on purpose, like `Daemon::left_on_its_own` — an unbounded wait on
/// a command that hangs is the same failure this exists to catch, fast.
fn remuda_timed(dir: &Path, args: &[&str]) -> std::process::Output {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(args)
        .env("REMUDA_RUNTIME_DIR", dir)
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        // An auto-started daemon boots the user config; keep it off the real one.
        .env("HOME", dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn remuda");

    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{args:?} did not exit within PATIENCE"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().expect("collect output")
}

/// `restart` drives the SHIPPED BINARY, not `daemon::serve` on a thread: the
/// stop is a `process::exit`, so an in-process daemon would take the test
/// runner with it — which is also why this is the only honest way to test it.
#[test]
fn stop_stops_a_daemon_and_leaves_the_next_command_free_to_start_one() {
    let dir = scratch_dir("stop");
    let path = daemon::socket_path_in(&dir, "s");
    let mut daemon = Daemon::spawn(&dir);

    // Positive control: it is answering before we ask it to stop, so "the
    // socket is silent" below cannot pass on a daemon that never came up.
    assert!(
        String::from_utf8_lossy(&remuda(&dir, &["-s", "s", "ls"]).stdout).contains("no sessions"),
        "the daemon was not answering to begin with"
    );

    let out = remuda(&dir, &["-s", "s", "stop"]);
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "restart failed: {said}");
    assert!(said.contains("stopped the daemon"), "{said}");
    assert!(
        remuda_native::ipc::connect(&path).is_err(),
        "the daemon is still answering after restart"
    );
    assert!(daemon.left_on_its_own(), "it did not exit 0 on its own");
}

#[test]
fn stop_with_no_daemon_running_is_not_an_error() {
    let dir = scratch_dir("stop-empty");
    let out = remuda(&dir, &["-s", "s", "stop"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no daemon running"));
}

/// A live herd must not be thrown away by a command that was typed by habit.
/// With no terminal to ask on, the refusal names the flag rather than prompting
/// into a pipe that will never answer.
#[test]
fn stop_refuses_to_kill_a_live_session_without_being_told_twice() {
    let dir = scratch_dir("stop-live");
    let path = daemon::socket_path_in(&dir, "s");
    let mut daemon = Daemon::spawn(&dir);
    new_session(&path, "keeper");

    let out = remuda(&dir, &["-s", "s", "stop"]);
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success(),
        "it killed a live herd unasked: {said}"
    );
    assert!(
        said.contains("keeper"),
        "it did not name what it would lose: {said}"
    );
    assert!(
        remuda_native::ipc::connect(&path).is_ok(),
        "the daemon died despite refusing"
    );

    // Outside a hosted session, -f remains the no-prompt path for a live herd.
    let forced = remuda(&dir, &["-s", "s", "stop", "-f"]);
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(daemon.left_on_its_own(), "-f did not stop it");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn a_session_cannot_force_stop_its_own_daemon_without_an_explicit_override() {
    let dir = scratch_dir("stop-inside");
    let path = daemon::socket_path_in(&dir, "s");
    let mut daemon = Daemon::spawn(&dir);
    let binary = env!("CARGO_BIN_EXE_remuda");
    let alias = dir.join("runtime-alias");
    std::os::unix::fs::symlink(&dir, &alias).expect("runtime symlink");
    let env = std::collections::HashMap::from([(
        "REMUDA_RUNTIME_DIR".to_string(),
        alias.to_string_lossy().into_owned(),
    )]);
    let response = client::request(
        &path,
        &Request::New {
            name: Some("inside".into()),
            command: vec![
                "sh".into(),
                "-c".into(),
                format!("'{binary}' -s s stop -f --yes; sleep 30"),
            ],
            size: Size::new(100, 30),
            cwd: None,
            env: Some(env),
        },
    )
    .expect("create managed session");
    assert!(matches!(response, Response::Value(_)));

    assert_session_shutdown_identity_is_refused(&path, daemon.0.id());

    let deadline = Instant::now() + PATIENCE;
    let screen = loop {
        let screen = capture(&path, "inside");
        if screen.contains("cannot stop this daemon") {
            break screen;
        }
        assert!(
            Instant::now() < deadline,
            "stop command did not report its refusal: {screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        screen.contains("--i-am-inside"),
        "missing explicit override guidance: {screen}"
    );
    assert!(
        remuda_native::ipc::connect(&path).is_ok(),
        "the daemon died"
    );
    let stopped = client::request(
        &path,
        &Request::New {
            name: Some("override".into()),
            command: vec![
                binary.into(),
                "-s".into(),
                "s".into(),
                "stop".into(),
                "-f".into(),
                "--yes".into(),
                "--i-am-inside".into(),
            ],
            size: Size::new(100, 30),
            cwd: None,
            env: None,
        },
    )
    .expect("request override session");
    assert!(matches!(stopped, Response::Value(_)));
    assert!(
        daemon.left_on_its_own(),
        "daemon did not exit after override"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
fn assert_session_shutdown_identity_is_refused(path: &Path, daemon_id: u32) {
    let sessions = match client::request(path, &Request::List).expect("list sessions") {
        Response::Sessions(sessions) => sessions,
        response => panic!("unexpected list response: {response:?}"),
    };
    let session_id = sessions
        .iter()
        .find(|session| session.name == "inside")
        .expect("inside session")
        .id
        .clone();
    let raw_shutdown = client::request(
        path,
        &Request::Shutdown {
            requester_daemon_id: Some(daemon_id.to_string()),
            requester_session_id: Some(session_id),
            requester_session_name: Some("inside".into()),
            override_hosted: false,
        },
    )
    .expect("raw shutdown response");
    assert!(
        matches!(&raw_shutdown, Response::Error(reason) if reason.contains("one of its own sessions")),
        "raw shutdown was not refused: {raw_shutdown:?}"
    );
    let stale_identity = client::request(
        path,
        &Request::Shutdown {
            requester_daemon_id: Some(daemon_id.to_string()),
            requester_session_id: Some("stale-session-id".into()),
            requester_session_name: None,
            override_hosted: false,
        },
    )
    .expect("stale identity response");
    assert!(
        matches!(&stale_identity, Response::Error(reason) if reason.contains("one of its own sessions")),
        "unknown identity from this daemon was not refused: {stale_identity:?}"
    );
}

#[cfg(unix)]
#[test]
fn shutdown_uses_peer_ancestry_when_session_environment_is_stripped() {
    let dir = scratch_dir("stop-ancestry-red");
    let path = daemon::socket_path_in(&dir, "s");
    let mut daemon = Daemon::spawn(&dir);
    let runtime = dir.to_string_lossy();
    let path_text = path.to_string_lossy();
    let path_value = std::env::var("PATH").unwrap_or_default();
    let binary = env!("CARGO_BIN_EXE_remuda");
    let test_binary = std::env::current_exe().expect("integration test binary path");
    let cli = format!(
        "/usr/bin/env -i PATH='{}' REMUDA_RUNTIME_DIR='{}' '{}' -s s stop -f --yes; sleep 30",
        path_value, runtime, binary
    );
    let raw = format!(
        "/usr/bin/env -i PATH='{}' REMUDA_RUNTIME_DIR='{}' REMUDA_TEST_RAW_SHUTDOWN_SOCKET='{}' '{}' --exact raw_shutdown_descendant_helper --nocapture; sleep 30",
        path_value,
        runtime,
        path_text,
        test_binary.display()
    );
    for (name, command) in [("env-stop", cli), ("raw-stop", raw)] {
        let response = client::request(
            &path,
            &Request::New {
                name: Some(name.into()),
                command: vec!["sh".into(), "-c".into(), command],
                size: Size::new(100, 30),
                cwd: None,
                env: Some(std::collections::HashMap::from([(
                    "REMUDA_RUNTIME_DIR".into(),
                    runtime.to_string(),
                )])),
            },
        )
        .expect("start descendant requester");
        assert!(matches!(response, Response::Value(_)));
    }

    let deadline = Instant::now() + PATIENCE;
    let cli_screen = loop {
        let screen = capture(&path, "env-stop");
        if screen.contains("cannot stop this daemon") {
            break screen;
        }
        assert!(
            Instant::now() < deadline,
            "env -i requester was not refused: {screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(cli_screen.contains("--i-am-inside"), "{cli_screen}");

    let deadline = Instant::now() + PATIENCE;
    let raw_screen = loop {
        let screen = capture(&path, "raw-stop");
        if screen.contains("raw_shutdown_descendant_helper") {
            break screen;
        }
        assert!(
            Instant::now() < deadline,
            "raw Shutdown helper did not run: {screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(raw_screen.contains("ok"), "raw helper failed: {raw_screen}");
    assert!(
        remuda_native::ipc::connect(&path).is_ok(),
        "daemon was stopped"
    );

    let stopped = client::request(
        &path,
        &Request::Shutdown {
            requester_daemon_id: None,
            requester_session_id: None,
            requester_session_name: None,
            override_hosted: false,
        },
    )
    .expect("outside shutdown");
    assert_eq!(stopped, Response::Ok);
    assert!(
        daemon.left_on_its_own(),
        "outside caller did not stop daemon"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn raw_shutdown_descendant_helper() {
    let Ok(path) = std::env::var("REMUDA_TEST_RAW_SHUTDOWN_SOCKET") else {
        return;
    };
    let response = client::request(
        Path::new(&path),
        &Request::Shutdown {
            requester_daemon_id: None,
            requester_session_id: None,
            requester_session_name: None,
            override_hosted: false,
        },
    )
    .expect("raw descendant shutdown response");
    assert!(
        matches!(&response, Response::Error(reason) if reason.contains("one of its own sessions")),
        "raw descendant Shutdown was not refused: {response:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_session_can_stop_a_different_private_daemon() {
    let dir_a = scratch_dir("stop-foreign-a");
    let dir_b = scratch_dir("stop-foreign-b");
    let path_a = daemon::socket_path_in(&dir_a, "s");
    let mut daemon_a = Daemon::spawn(&dir_a);
    let mut daemon_b = Daemon::spawn(&dir_b);
    let env = std::collections::HashMap::from([(
        "REMUDA_RUNTIME_DIR".to_string(),
        dir_b.to_string_lossy().into_owned(),
    )]);
    let response = client::request(
        &path_a,
        &Request::New {
            name: Some("foreign-stop".into()),
            command: vec![
                env!("CARGO_BIN_EXE_remuda").into(),
                "-s".into(),
                "s".into(),
                "stop".into(),
                "-f".into(),
                "--yes".into(),
            ],
            size: Size::new(80, 24),
            cwd: None,
            env: Some(env),
        },
    )
    .expect("start foreign stop command");
    assert!(matches!(response, Response::Value(_)));
    assert!(
        daemon_b.left_on_its_own(),
        "different daemon was not stopped"
    );
    assert!(
        remuda_native::ipc::connect(&path_a).is_ok(),
        "the session's own daemon was stopped"
    );
    let stopped_a = client::request(
        &path_a,
        &Request::Shutdown {
            requester_daemon_id: None,
            requester_session_id: None,
            requester_session_name: None,
            override_hosted: false,
        },
    )
    .expect("stop session daemon");
    assert_eq!(stopped_a, Response::Ok);
    assert!(daemon_a.left_on_its_own(), "session daemon did not stop");
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

/// A name with no matching arm is a plain error naming the package, not a
/// panic or a silent no-op. Daemon pre-started for the same reason as above
/// (auto-start on Windows not covered here; see warmblood-kr/remuda#54).
#[test]
fn exec_of_an_unknown_package_fails_and_names_it() {
    let dir = scratch_dir("exec-unknown");
    let _daemon = Daemon::spawn(&dir);

    let out = remuda_timed(&dir, &["-s", "s", "exec", "definitely-not-a-real-package"]);
    assert!(
        !out.status.success(),
        "an unknown package should not succeed"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no such package"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Lua's long-bracket string form has no escape processing at all — safe
/// for embedding a raw filesystem path (backslashes included) into eval
/// source text without escaping it first.
fn lua_raw_string(s: &str) -> String {
    format!("[[{s}]]")
}

#[test]
fn process_delivers_stdout_lines_in_order_then_an_exit_event() {
    let dir = scratch_dir("process-lines");
    let _daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");
    let exe = lua_raw_string(env!("CARGO_BIN_EXE_remuda"));

    eval(&path, "remuda.t1_lines = {}");
    eval(&path, "remuda.t1_exit = nil");
    eval(
        &path,
        "remuda.on('t1-line', function(l) table.insert(remuda.t1_lines, l) end)",
    );
    eval(
        &path,
        "remuda.on('t1-exit', function(c) remuda.t1_exit = c end)",
    );
    eval(
        &path,
        &format!(
            "remuda.process{{argv = {{{exe}, '_print_lines', '5', '0'}}, on_line = 't1-line', on_exit = 't1-exit'}}"
        ),
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.t1_exit and 1 or 0") == 0 {
        assert!(Instant::now() < deadline, "exit event never arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        eval(&path, "return table.concat(remuda.t1_lines, ',')"),
        "1,2,3,4,5"
    );
}

#[test]
fn a_silent_process_yields_only_the_exit_event() {
    let dir = scratch_dir("process-silent");
    let _daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");
    let exe = lua_raw_string(env!("CARGO_BIN_EXE_remuda"));

    eval(&path, "remuda.silent_lines = 0");
    eval(&path, "remuda.silent_exit = nil");
    eval(
        &path,
        "remuda.on('silent-line', function() remuda.silent_lines = remuda.silent_lines + 1 end)",
    );
    eval(
        &path,
        "remuda.on('silent-exit', function() remuda.silent_exit = true end)",
    );
    eval(
        &path,
        &format!(
            "remuda.process{{argv = {{{exe}, '_print_lines', '0', '0'}}, on_line = 'silent-line', on_exit = 'silent-exit'}}"
        ),
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.silent_exit and 1 or 0") == 0 {
        assert!(Instant::now() < deadline, "exit event never arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(read_count(&path, "return remuda.silent_lines"), 0);
}

/// A pid this daemon spawned is gone: `kill(pid, 0)` — no signal delivered,
/// only whether one *could* be — is ESRCH once the pid is reaped. Anything
/// else (success, or a permission error) means it is still around.
#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Find a pid's own child, by exact pid, one time — never a glob/grep
/// pattern (this investigation's own history: zsh's globber has produced a
/// false "no matches" from `ps -ef | grep [r]emuda` more than once).
#[cfg(unix)]
fn child_pid_of(parent: i32, deadline: Instant) -> Option<i32> {
    loop {
        // `pgrep -P`, not `ps --ppid`: the latter is GNU-only.
        let out = std::process::Command::new("pgrep")
            .args(["-P", &parent.to_string()])
            .output()
            .expect("ps");
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(pid) = text.split_whitespace().next().and_then(|s| s.parse().ok()) {
            return Some(pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// [MEASURED, Linux] This is the actual leak (`process.rs`'s plain-pipe
/// children, "ALWAYS survive daemon death... zero process-group isolation,
/// zero signal handling anywhere") and this is `child_guard::harden`'s fix
/// for it: a real out-of-process daemon, SIGKILLed for real, and its DIRECT
/// `remuda.process` child is gone within PATIENCE — not by any daemon code
/// running (none does, on SIGKILL), but because the kernel itself delivers
/// PDEATHSIG the moment the daemon dies.
///
/// The GRANDCHILD (`sleep`, forked by the `sh` direct child before the kill)
/// is asserted to *survive* the same SIGKILL, on purpose: PDEATHSIG is
/// registered on the direct child alone and is cleared across fork(2), so it
/// cannot reach a process it never touched, and nothing else runs on a raw
/// SIGKILL to sweep the group. This pins the exact boundary named in
/// child_guard.rs's own doc comment and in steps/ "Known ceilings", rather
/// than asserting past it. See
/// `a_clean_shutdown_reaps_a_processs_whole_group_including_a_grandchild`
/// for the one path that DOES reach this grandchild.
///
/// Negative control, run by hand for this round (see steps/ writeup): with
/// `child_guard::harden`'s call in `process.rs::spawn` commented out, this
/// test's first `while pid_alive(direct_pid)` loop times out — nothing tells
/// the kernel to kill the direct child when the daemon dies. Restoring the
/// call makes it pass again.
#[cfg(target_os = "linux")]
#[test]
fn a_sigkilled_daemon_reaps_its_direct_process_child_but_not_an_already_forked_grandchild() {
    let dir = scratch_dir("orphan-reap");
    let daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");

    eval(&path, "remuda.og_lines = {}");
    eval(
        &path,
        "remuda.on('og-line', function(l) table.insert(remuda.og_lines, l) end)",
    );
    eval(
        &path,
        "remuda.process{argv = {'sh', '-c', 'echo $$; sleep 60'}, on_line = 'og-line'}",
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return #remuda.og_lines") == 0 {
        assert!(
            Instant::now() < deadline,
            "the direct child never printed its own pid"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let direct_pid: i32 = eval(&path, "return remuda.og_lines[1]")
        .trim()
        .parse()
        .expect("the printed $$ is a pid");

    let deadline = Instant::now() + PATIENCE;
    let grandchild_pid = child_pid_of(direct_pid, deadline)
        .unwrap_or_else(|| panic!("sleep never forked as a child of {direct_pid}"));
    assert!(
        pid_alive(direct_pid),
        "sanity: direct child must be alive before the kill"
    );
    assert!(
        pid_alive(grandchild_pid),
        "sanity: grandchild must be alive before the kill"
    );

    let daemon_pid = daemon.0.id() as libc::pid_t;
    assert_eq!(
        unsafe { libc::kill(daemon_pid, libc::SIGKILL) },
        0,
        "SIGKILL of the real daemon pid must succeed"
    );

    let deadline = Instant::now() + PATIENCE;
    while pid_alive(direct_pid) {
        assert!(
            Instant::now() < deadline,
            "the direct child ({direct_pid}) outlived a SIGKILLed daemon — child_guard::harden \
             did not reap it"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Pin the ceiling: give the kernel a beat, then confirm the grandchild
    // is still here — a raw SIGKILL of the daemon runs no code, so nothing
    // built this round could have reached it.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        pid_alive(grandchild_pid),
        "a grandchild dying too would mean either this test's premise changed or something \
         now reaps process groups on a signal this round never wired that to — worth knowing, \
         not assuming"
    );

    // Clean up the leak this test just proved exists, so it doesn't actually
    // linger on the machine that ran it.
    unsafe {
        libc::kill(grandchild_pid, libc::SIGKILL);
    }
}

/// The Codex app-server is detached into its own process group, so daemon PTY
/// teardown cannot HUP it. Its parent-death guard must still reap it when the
/// daemon dies and the session's remuda CLI disappears.
#[cfg(unix)]
#[test]
fn a_sigkilled_daemon_reaps_a_codex_app_server_in_its_session() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch_dir("codex-parent-death");
    let mut daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");
    let stub_dir = dir.join("bin");
    std::fs::create_dir_all(&stub_dir).unwrap();
    let pid_file = dir.join("app-server.pid");
    let stub = stub_dir.join("codex");
    std::fs::write(
        &stub,
        "#!/bin/sh\nif [ \"$1\" = app-server ]; then\n  trap '' HUP\n  echo $$ > \"$STUB_PIDFILE\"\nfi\nexec /bin/sleep 60\n",
    )
    .unwrap();
    let mut mode = std::fs::metadata(&stub).unwrap().permissions();
    mode.set_mode(0o755);
    std::fs::set_permissions(&stub, mode).unwrap();
    let env = std::collections::HashMap::from([
        ("PATH".to_string(), stub_dir.to_string_lossy().into_owned()),
        (
            "STUB_PIDFILE".to_string(),
            pid_file.to_string_lossy().into_owned(),
        ),
    ]);
    let response = client::request(
        &path,
        &Request::New {
            name: Some("codex".into()),
            command: vec![
                env!("CARGO_BIN_EXE_remuda").into(),
                "_codex_tui".into(),
                "--status".into(),
                dir.join("status").to_string_lossy().into_owned(),
            ],
            size: Size::new(80, 24),
            cwd: None,
            env: Some(env),
        },
    )
    .expect("start Codex TUI session");
    assert!(matches!(response, Response::Value(_)));

    let deadline = Instant::now() + PATIENCE;
    let app_server_pid = loop {
        if let Ok(pid) = std::fs::read_to_string(&pid_file) {
            break pid.trim().parse::<i32>().unwrap();
        }
        assert!(Instant::now() < deadline, "stub app-server never started");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        pid_alive(app_server_pid),
        "app-server exited before daemon death"
    );

    assert_eq!(
        unsafe { libc::kill(daemon.0.id() as libc::pid_t, libc::SIGKILL) },
        0,
        "kill private daemon"
    );
    let _ = daemon.0.wait();
    let deadline = Instant::now() + PATIENCE;
    while pid_alive(app_server_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let app_server_reaped = !pid_alive(app_server_pid);
    if !app_server_reaped {
        // Keep a regression failure from leaving the stub process behind.
        unsafe {
            libc::kill(app_server_pid, libc::SIGKILL);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        app_server_reaped,
        "Codex app-server ({app_server_pid}) outlived the killed daemon"
    );
}

/// [MEASURED, unix] The one path that DOES reach a grandchild: a clean
/// `remuda stop` sends `Request::Shutdown`, which runs
/// `reap_processes_before_exit` (daemon.rs) before the process exits —
/// `remuda.processes()` + `remuda._process_killpg(id)`, which `killpg`s the
/// whole process group `child_guard::harden` put the direct child in. Both
/// the direct child and the grandchild it already forked are gone.
#[cfg(unix)]
#[test]
fn a_clean_shutdown_reaps_a_processs_whole_group_including_a_grandchild() {
    let dir = scratch_dir("orphan-reap-clean");
    let _daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");

    eval(&path, "remuda.cg_lines = {}");
    eval(
        &path,
        "remuda.on('cg-line', function(l) table.insert(remuda.cg_lines, l) end)",
    );
    eval(
        &path,
        "remuda.process{argv = {'sh', '-c', 'echo $$; sleep 60'}, on_line = 'cg-line'}",
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return #remuda.cg_lines") == 0 {
        assert!(
            Instant::now() < deadline,
            "the direct child never printed its own pid"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let direct_pid: i32 = eval(&path, "return remuda.cg_lines[1]")
        .trim()
        .parse()
        .expect("the printed $$ is a pid");

    let deadline = Instant::now() + PATIENCE;
    let grandchild_pid = child_pid_of(direct_pid, deadline)
        .unwrap_or_else(|| panic!("sleep never forked as a child of {direct_pid}"));
    assert!(
        pid_alive(direct_pid) && pid_alive(grandchild_pid),
        "sanity: both alive pre-restart"
    );

    // No live pty session was ever created here, so `remuda stop` (no
    // `-f`) proceeds straight to `Request::Shutdown` without a confirmation
    // prompt — see `confirm_losses` in src/bin/remuda.rs.
    let out = remuda(&dir, &["-s", "s", "stop"]);
    assert!(
        out.status.success(),
        "restart failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let deadline = Instant::now() + PATIENCE;
    while pid_alive(direct_pid) || pid_alive(grandchild_pid) {
        assert!(
            Instant::now() < deadline,
            "a clean shutdown left something behind: direct {direct_pid} alive={}, grandchild \
             {grandchild_pid} alive={}",
            pid_alive(direct_pid),
            pid_alive(grandchild_pid)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_line_flood_does_not_starve_the_schedule_ticker() {
    let dir = scratch_dir("process-flood");
    let _daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");
    let exe = lua_raw_string(env!("CARGO_BIN_EXE_remuda"));
    const FLOOD_LINES: u32 = 3000;
    // A per-line delay, not a bare line count: `daemon.rs`'s own `TICK_PERIOD`
    // is a fixed 1 real second (see its doc comment), independent of the
    // Lua schedule's own `every`, so the ticker's very first wakeup cannot
    // come sooner than that regardless of how this test paces its flood. A
    // delay-free flood of any size a modern machine can push drains in well
    // under one second — measured at ~0.2s for 100k lines — which starves
    // the observation window, not the ticker: there is no failure to see if
    // the flood is already over before the first tick could possibly fire.
    // Pacing by wall-clock sleep (not raw throughput) makes the minimum
    // flood duration (here, >= 6s) independent of the machine's speed.
    const FLOOD_LINE_DELAY_MS: u32 = 2;
    let flood_deadline = Instant::now() + Duration::from_secs(60);

    eval(&path, "remuda.flood_count = 0");
    eval(
        &path,
        "remuda.on('flood-line', function() remuda.flood_count = remuda.flood_count + 1 end)",
    );
    eval(&path, "remuda.ticks = 0");
    eval(
        &path,
        "remuda.schedule{every = 0.05, run = function() remuda.ticks = remuda.ticks + 1 end}",
    );
    eval(
        &path,
        &format!(
            "remuda.process{{argv = {{{exe}, '_print_lines', '{FLOOD_LINES}', '{FLOOD_LINE_DELAY_MS}'}}, on_line = 'flood-line'}}"
        ),
    );

    let mut last_ticks = read_count(&path, "return remuda.ticks");
    let mut ticker_advanced_during_flood = false;
    loop {
        let count = read_count(&path, "return remuda.flood_count");
        if count >= FLOOD_LINES {
            break;
        }
        assert!(
            Instant::now() < flood_deadline,
            "flood never finished ({count}/{FLOOD_LINES})"
        );
        std::thread::sleep(Duration::from_millis(100));
        let ticks = read_count(&path, "return remuda.ticks");
        if ticks > last_ticks {
            ticker_advanced_during_flood = true;
        }
        last_ticks = ticks;
    }
    assert!(
        ticker_advanced_during_flood,
        "the schedule ticker never advanced during the flood"
    );
    assert_eq!(read_count(&path, "return remuda.flood_count"), FLOOD_LINES);

    // A named, generous bound — this asserts "not starved," not a specific
    // performance target.
    let consecutive_skips = read_count(&path, "return remuda.schedule_skips().consecutive");
    assert!(
        consecutive_skips < 20,
        "consecutive schedule skips too high under flood: {consecutive_skips}"
    );
}

#[test]
fn killing_a_process_mid_stream_yields_exit_and_nothing_after() {
    let dir = scratch_dir("process-kill");
    let _daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");
    let exe = lua_raw_string(env!("CARGO_BIN_EXE_remuda"));

    eval(&path, "remuda.km_lines = 0");
    eval(&path, "remuda.km_exit = nil");
    eval(
        &path,
        "remuda.on('km-line', function() remuda.km_lines = remuda.km_lines + 1 end)",
    );
    eval(
        &path,
        "remuda.on('km-exit', function() remuda.km_exit = true end)",
    );
    // A slow trickle so there is a real window to kill it mid-stream rather
    // than racing its own natural exit.
    eval(
        &path,
        &format!(
            "remuda.km_handle = remuda.process{{argv = {{{exe}, '_print_lines', '100000', '10'}}, on_line = 'km-line', on_exit = 'km-exit'}}"
        ),
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.km_lines") < 3 {
        assert!(
            Instant::now() < deadline,
            "process never started producing lines"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    eval(&path, "remuda.kill(remuda.km_handle)");

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.km_exit and 1 or 0") == 0 {
        assert!(
            Instant::now() < deadline,
            "exit event never arrived after kill"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let lines_at_exit = read_count(&path, "return remuda.km_lines");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        read_count(&path, "return remuda.km_lines"),
        lines_at_exit,
        "a line arrived after the exit event"
    );
}

/// The paced flood above (`a_line_flood_does_not_starve_the_schedule_ticker`)
/// is deliberately slow enough that `process.rs`'s 4096-line `BUFFER_CAP`
/// never fills, so backpressure itself is never exercised there. This test
/// writes as fast as `_print_lines` can (delay 0) and gives it 5x
/// `BUFFER_CAP` worth of lines, so the buffer must fill — and proves it did,
/// three independent ways:
///
/// 1. The child is *still running* (`remuda.processes()` still lists its id)
///    well after an unblocked 20000-tiny-line write would already have
///    exited on its own — it can only still be alive because its own
///    `write()` is blocked on a full pipe.
/// 2. Total wall-clock time to drain is far longer than an unpaced write of
///    20000 short lines takes with no consumer at all (well under 100ms,
///    unmeasured here, but self-evidently near-instant) — the elapsed time
///    is explainable only by the child being made to wait.
/// 3. The schedule ticker keeps advancing throughout, so none of the above
///    comes at the cost of starving the Image's own FIFO — the property the
///    paced flood test already covers, still held even under real pressure.
///
/// The `on_line` hook itself is a busy loop, not a sleep — Lua has no
/// builtin sleep, and this is simplest thing that reliably costs enough
/// real wall time per line to keep the buffer pinned near `BUFFER_CAP` for
/// long enough to observe, rather than draining in a blink.
#[test]
fn an_unpaced_flood_exercises_real_backpressure_and_the_child_blocks() {
    let dir = scratch_dir("process-unpaced-flood");
    let _daemon = Daemon::spawn(&dir);
    let path = daemon::socket_path_in(&dir, "s");
    let exe = lua_raw_string(env!("CARGO_BIN_EXE_remuda"));
    const FLOOD_LINES: u32 = 20_000;

    eval(&path, "remuda.up_count = 0");
    eval(&path, "remuda.up_last = 0");
    eval(&path, "remuda.up_broken = false");
    eval(&path, "remuda.up_exit = nil");
    eval(
        &path,
        // Ordering is checked inline, one line at a time, rather than
        // buffered into a 20000-entry table and checked after: `up_broken`
        // catches any gap or repeat the instant it happens, and `up_last`
        // at the end is proof every line 1..N arrived, not a sample of them.
        "remuda.on('up-line', function(l)
            local n = tonumber(l)
            if n ~= remuda.up_last + 1 then remuda.up_broken = true end
            remuda.up_last = n
            local busy = 0
            for i = 1, 10000 do busy = busy + i end
            remuda.up_count = remuda.up_count + 1
        end)",
    );
    eval(
        &path,
        "remuda.on('up-exit', function() remuda.up_exit = true end)",
    );
    eval(&path, "remuda.up_ticks = 0");
    eval(
        &path,
        "remuda.schedule{every = 0.05, run = function() remuda.up_ticks = remuda.up_ticks + 1 end}",
    );

    let spawned_at = Instant::now();
    eval(
        &path,
        &format!(
            "remuda.up_handle = remuda.process{{argv = {{{exe}, '_print_lines', '{FLOOD_LINES}', '0'}}, on_line = 'up-line', on_exit = 'up-exit'}}"
        ),
    );

    // Proof #1: still alive well after an unblocked flood of this size would
    // have finished. `BUFFER_CAP` (4096) plus a typical OS pipe (tens of
    // thousands of bytes, a few thousand lines of this size) is nowhere near
    // 20000 lines, so a child that is still running here is blocked on a
    // full pipe, not merely "still working".
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        read_count(
            &path,
            "return (function() \
                for _, id in ipairs(remuda.processes()) do \
                    if id == remuda.up_handle then return 1 end \
                end \
                return 0 \
             end)()"
        ),
        1,
        "the flooding process had already exited after 300ms — \
         backpressure did not hold it, so the buffer/pipe never filled"
    );

    let flood_deadline = Instant::now() + Duration::from_secs(60);
    let mut last_ticks = read_count(&path, "return remuda.up_ticks");
    let mut ticker_advanced_during_flood = false;
    loop {
        let count = read_count(&path, "return remuda.up_count");
        if count >= FLOOD_LINES {
            break;
        }
        assert!(
            Instant::now() < flood_deadline,
            "flood never finished ({count}/{FLOOD_LINES})"
        );
        std::thread::sleep(Duration::from_millis(50));
        let ticks = read_count(&path, "return remuda.up_ticks");
        if ticks > last_ticks {
            ticker_advanced_during_flood = true;
        }
        last_ticks = ticks;
    }

    // Proof #2: it took distinctly longer than an unpaced, unblocked write
    // of 20000 short lines could possibly take on its own. Generous enough
    // to survive a slow CI runner (including Windows), tight enough that
    // "no backpressure" (near-instant) cannot pass it by accident.
    let elapsed = spawned_at.elapsed();
    assert!(
        elapsed > Duration::from_millis(500),
        "the flood drained in {elapsed:?} — too fast to have been backpressured"
    );

    assert!(
        ticker_advanced_during_flood,
        "the schedule ticker never advanced during the unpaced flood"
    );

    assert_eq!(
        read_count(&path, "return remuda.up_broken and 1 or 0"),
        0,
        "a line arrived out of order or was skipped"
    );
    assert_eq!(
        read_count(&path, "return remuda.up_last"),
        FLOOD_LINES,
        "the last line received was not the expected final line"
    );

    let deadline = Instant::now() + PATIENCE;
    while read_count(&path, "return remuda.up_exit and 1 or 0") == 0 {
        assert!(Instant::now() < deadline, "exit event never arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The whole point of steps/035: a `~/.config/remuda/init.lua` present before
/// the daemon exists is evaluated automatically, with no `exec`/`eval` from
/// this test, every time a FRESH daemon boots. The instrument is a marker the
/// file itself writes, and the restart leg proves the load runs on every boot
/// from the same unchanged file, not just the first.
#[test]
#[cfg(unix)]
fn a_fresh_daemon_auto_loads_the_user_config_on_every_boot() {
    let dir = scratch_dir("boot-loader-positive");
    let home = dir.join("home");
    let marker = dir.join("init-ran.marker");
    let _ = std::fs::remove_file(&marker);
    std::fs::create_dir_all(home.join(".config/remuda")).expect("mkdir config dir");
    std::fs::write(
        home.join(".config/remuda/init.lua"),
        format!(
            "local f = io.open({:?}, 'w') f:write('ran') f:close()",
            marker.display().to_string()
        ),
    )
    .expect("write init.lua");

    let wait_for_marker = |leg: &str| {
        let deadline = Instant::now() + PATIENCE;
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "{leg}: the daemon never evaluated its user config at boot"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    let mut daemon = Daemon::spawn_with_home(&dir, &home);
    wait_for_marker("first boot");

    let out = remuda(&dir, &["-s", "s", "stop", "-f"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        daemon.left_on_its_own(),
        "the old daemon did not exit on its own"
    );

    // Same home, config untouched; only the evidence of the first run goes.
    std::fs::remove_file(&marker).expect("remove first-boot marker");
    let _daemon2 = Daemon::spawn_with_home(&dir, &home);
    wait_for_marker("after restart");
}

/// Negative control for the whole mechanism above (steps/035's own DoD
/// wording: "with the feature turned off, the same procedure must go RED —
/// if it does not go red, the instrument is not measuring the feature").
/// Absence of `~/.config/remuda/init.lua` IS "feature off" here, the same
/// way a vanilla Neovim/Hammerspoon install with no config is: other files
/// may sit under `~/.config/remuda` (here, butler credentials), but with no
/// init.lua a fresh daemon must never start anything on its own.
#[test]
#[cfg(unix)]
fn a_fresh_daemon_with_no_user_config_never_auto_starts_a_session() {
    let dir = scratch_dir("boot-loader-negative");
    let home = dir.join("home-empty");
    let butler_dir = home.join(".config/remuda/butler");
    std::fs::create_dir_all(&butler_dir).expect("mkdir conventional butler dir");
    std::fs::write(butler_dir.join("token"), "test-token\n").expect("write token");
    std::fs::write(
        butler_dir.join("config"),
        "http://127.0.0.1:1\n!room:example.org\n@butler:example.org\n\n",
    )
    .expect("write config");
    // Deliberately: no `~/.config/remuda/init.lua` written at all.

    let _daemon = Daemon::spawn_with_home(&dir, &home);

    // A couple of TICK_PERIODs' worth of margin to rule out a delayed load,
    // not just an instant-after check.
    std::thread::sleep(Duration::from_millis(1200));
    let listed = remuda(&dir, &["-s", "s", "ls"]);
    let listed_out = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed_out.contains("no sessions"),
        "a fresh daemon with no init.lua at all auto-registered something: {listed_out}"
    );
}

/// Regression test against the exact hazard `load_user_config`'s own doc
/// comment (daemon.rs) names: because `Image::spawn`'s `ready` chain (image.rs)
/// is checked on EVERY job forever, an `Err` inside it poisons the image
/// PERMANENTLY -- moving the user-config load back inside that chain (as
/// opposed to the separate, later `image.eval` call it uses today) would let
/// one colleague's own typo in their `~/.config/remuda/init.lua` brick every
/// other session on their daemon, forever, until a manual restart. A broken
/// file must be reported and then the daemon must go on being an entirely
/// ordinary, working daemon.
#[test]
#[cfg(unix)]
fn a_broken_user_config_is_reported_but_never_bricks_the_daemon() {
    let dir = scratch_dir("boot-loader-blast-radius");
    let home = dir.join("home");
    std::fs::create_dir_all(home.join(".config/remuda")).expect("mkdir config dir");
    std::fs::write(
        home.join(".config/remuda/init.lua"),
        "this is not valid lua $$$\n",
    )
    .expect("write broken init.lua");

    let _daemon = Daemon::spawn_with_home(&dir, &home);
    let path = daemon::socket_path_in(&dir, "s");

    // The daemon is still a normal, working daemon: `ls` answers cleanly --
    // if the hazard above ever regressed, this would instead come back as
    // an error containing "image failed to start".
    match client::request(&path, &Request::List).expect("list") {
        Response::Sessions(s) => assert!(s.is_empty(), "a fresh daemon holds nothing"),
        other => panic!("unexpected: {other:?}"),
    }

    // Direct proof the image itself is not poisoned, not merely that `ls`
    // (which never touches the `ready` chain either) happens to still work:
    // an entirely unrelated, ordinary session can still be created.
    new_session(&path, "plain");
    let listed = remuda(&dir, &["-s", "s", "ls"]);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("plain"),
        "an ordinary session could not be created after a broken user config \
         -- the image is poisoned"
    );
}
