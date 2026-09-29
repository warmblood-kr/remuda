//! The daemon: it owns the sessions, and it outlives every client.
//!
//! 정수님, 2026-09-10: *"일종의 tmux 같은 pty manager, which support daemon
//! mode. with session list so that user can select a session to attach."*
//!
//! That "outlives" is the whole reason a daemon exists rather than a library
//! call. A session is where an agent gets logged in by a human and then works
//! for hours; if it died when the viewer's terminal closed, attaching could
//! never be the credential path it has to be.
//!
//! One connection carries one request. After an accepted `Attach` the same
//! connection stops speaking the protocol and becomes a raw byte pipe, in both
//! directions, until either end hangs up.

use crate::image::Image;
use crate::ipc::{self, Listener, Stream, TryClone};
use crate::process_ancestry;
use crate::pty::PtyAgent;
#[cfg(unix)]
use interprocess::local_socket::traits::Listener as _;
#[cfg(windows)]
use interprocess::local_socket::traits::ListenerExt;
#[cfg(unix)]
use interprocess::local_socket::traits::Stream as LocalStream;
#[cfg(unix)]
use interprocess::local_socket::ListenerNonblockingMode;
use remuda_core::agent::Result as AgentResult;
use remuda_core::protocol::{collapse_runs, Request, Response, StyledScreen};
use remuda_core::{Clock, Registry, Session, Size};
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::Mutex;

use crate::SystemClock;
use portable_pty::CommandBuilder;

/// Each protocol connection carries one request; cap all simultaneous long
/// polls so they cannot consume an unbounded number of daemon worker threads.
pub(crate) const MAX_CONCURRENT_SYNCS: usize = 16;
static ACTIVE_SYNCS: AtomicUsize = AtomicUsize::new(0);

struct SyncPermit;

impl SyncPermit {
    fn acquire() -> Option<Self> {
        ACTIVE_SYNCS
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |active| {
                (active < MAX_CONCURRENT_SYNCS).then_some(active + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

fn acquire_sync_permit() -> Result<SyncPermit, Response> {
    SyncPermit::acquire().ok_or(Response::SyncAtCapacity)
}

impl Drop for SyncPermit {
    fn drop(&mut self) {
        ACTIVE_SYNCS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Where a node's socket lives. Prefer `$XDG_RUNTIME_DIR`; Android falls back
/// to its process temp directory, while other Unix systems keep their old path.
pub fn socket_path(server: &str) -> PathBuf {
    socket_path_in(&runtime_dir(), server)
}

/// Runtime root used by the client and daemon entry points.
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("REMUDA_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(default_runtime_dir)
}

/// The same derivation with the runtime directory supplied — what the daemon
/// tests use, so they exercise the shipped naming instead of a hand-built path
/// that only resembles it.
pub fn socket_path_in(base: &Path, server: &str) -> PathBuf {
    #[cfg(unix)]
    {
        base.join("remuda").join(format!("{server}.sock"))
    }
    // A named pipe has no directory to live in, so the runtime directory folds
    // into the pipe's NAME. That is what keeps `REMUDA_RUNTIME_DIR` isolating
    // one daemon from another on Windows the way a directory does on unix.
    #[cfg(windows)]
    {
        PathBuf::from(format!(
            r"\\.\pipe\remuda-{:016x}-{server}",
            fingerprint(base.as_os_str())
        ))
    }
}

fn default_runtime_dir() -> PathBuf {
    let xdg_runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    let temp_dir = std::env::temp_dir();
    #[cfg(unix)]
    {
        let who = std::env::var("USER").unwrap_or_else(|_| "nobody".into());
        runtime_base_for(
            cfg!(target_os = "android"),
            xdg_runtime_dir.as_deref(),
            &temp_dir,
            &who,
        )
    }
    #[cfg(windows)]
    {
        let who = std::env::var("USERNAME").unwrap_or_else(|_| "nobody".into());
        runtime_base_for(false, xdg_runtime_dir.as_deref(), &temp_dir, &who)
    }
}

fn runtime_base_for(
    is_android: bool,
    xdg_runtime_dir: Option<&std::ffi::OsStr>,
    temp_dir: &Path,
    user: &str,
) -> PathBuf {
    if let Some(path) = xdg_runtime_dir {
        return PathBuf::from(path);
    }
    if is_android {
        return temp_dir.to_path_buf();
    }
    #[cfg(unix)]
    {
        PathBuf::from(format!("/tmp/remuda-{user}"))
    }
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\remuda\{user}"))
    }
}

/// FNV-1a over the runtime directory, so an arbitrarily long path still yields
/// a pipe name inside the 256-character limit. Only used on Windows.
#[cfg(windows)]
fn fingerprint(text: &std::ffi::OsStr) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The shell a bare `new` starts. `$SHELL` first on every host: a person's
/// shell is their own choice, and nothing here improves on it.
pub fn default_shell() -> String {
    shell_or_default(std::env::var("SHELL").ok())
}

/// `powershell.exe` and not `pwsh.exe`: 5.1 is in-box on every supported
/// Windows and 7 is a separate install, so `pwsh` risks prefilling the prompt
/// with a binary that is not on the machine — worse than the `cmd.exe` it replaces.
fn shell_or_default(configured: Option<String>) -> String {
    configured.unwrap_or_else(|| {
        if cfg!(windows) {
            "powershell.exe"
        } else {
            "sh"
        }
        .to_string()
    })
}

/// Whether a session that ended keeps its entry. Off by default: 정수님,
/// 2026-09-10, asked that a session go away by itself when its program exits.
/// Read in the DAEMON's environment, so changing it takes a `remuda stop`.
pub(crate) fn keep_exited() -> bool {
    std::env::var("REMUDA_KEEP_EXITED").is_ok_and(|v| v == "1")
}

/// Where a user's own auto-loaded config lives (XDG `$XDG_CONFIG_HOME`,
/// else `$HOME/.config`) — mirrors `remuda.rs`'s `history_path` shape, not
/// `dist.rs`'s `base_dir` (see steps/035 for why).
fn user_config_path() -> Option<PathBuf> {
    let config_home = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(config_home.join("remuda").join("init.lua"))
}

/// Evaluate the user's `~/.config/remuda/init.lua`, once, at daemon boot.
/// Absence is silent; a broken file is reported but never poisons the
/// image the way an `Err` in `Image::spawn`'s `ready` chain would (steps/035).
fn load_user_config(image: &Image) {
    let Some(path) = user_config_path() else {
        return;
    };
    let Ok(source) = std::fs::read_to_string(&path) else {
        return;
    };
    if let Err(e) = image.eval(&source, Some(&path.display().to_string())) {
        eprintln!("remuda: error loading {}: {e}", path.display());
    }
}

/// Serve until the listener dies. Caution: connect before unlinking — an
/// unconditional unlink displaces a *live* peer, which then keeps running
/// unreachable and holds its pty children forever.
pub fn serve(path: &Path) -> std::io::Result<()> {
    serve_inner(path, None)
}

/// Serve a CLI-derived path, protecting Remuda's own runtime directory while
/// validating any user-selected socket parent without changing its mode.
pub fn serve_with_runtime(path: &Path, runtime: &Path) -> std::io::Result<()> {
    serve_inner(path, Some(runtime))
}

fn serve_inner(path: &Path, runtime: Option<&Path>) -> std::io::Result<()> {
    let _socket_lock = SocketLock::acquire(path, runtime)?;
    if ipc::connect(path).is_ok() {
        return Err(std::io::Error::other(format!(
            "a daemon is already listening at {} — pick a different name (remuda -s <name>) \
             or stop it first",
            path.display()
        )));
    }
    // Before the bind: once a client can see the socket, a signal must find
    // the handler, not the default action. Signals queue in the pair until
    // `serve_unix` polls this together with the listener.
    #[cfg(unix)]
    let signals = catch_signals()?;
    let listener: Listener = ipc::listen(path)?;
    #[cfg(unix)]
    let listener = prepare_unix_listener(listener, path)?;
    #[cfg(windows)]
    let listener = listener;
    let socket_owner = Arc::new(SocketOwnership::capture(path)?);

    let registry = Arc::new(Registry::new());
    // The image starts with the daemon and lives exactly as long (step 007).
    // It is started *after* the bind, so the `remuda` table it binds points at
    // a socket that is already accepting — the interpreter's first call cannot
    // race the listener it will talk to.
    let counters = Arc::new(crate::tick::Counters::default());
    let image = Image::spawn(path, Arc::clone(&registry), Arc::clone(&counters));
    // Its own thread, not a synchronous call here — measured, not foreseen:
    // an inline call deadlocks the moment a real `init.lua` does what
    // butler's actually does, calling `remuda.new` (or `send`/`close`/...).
    // Every one of those bindings is `ask()` in script.rs, a real IPC round
    // trip back to THIS daemon's own socket — answered only by the
    // `listener.incoming()` loop below. Calling `load_user_config` inline,
    // before that loop starts, blocks this very thread on a reply that only
    // this same (now-blocked) thread could ever produce. Off on its own
    // thread instead, the load can lag an instant behind the daemon's first
    // accepted connection (a `remuda ls` run in that instant would race it —
    // never poisoned, just possibly early), but it can never wedge startup.
    // Never inside `Image::spawn`'s own `ready` chain either: `tools.lua`
    // (image.rs) is compiled-in and safe to let poison `ready` forever on
    // failure, but this file is user-authored, on the reading machine, not
    // this repo's — a typo here must degrade to "no butler session", never
    // brick every future eval this daemon ever answers.
    {
        let image = image.clone();
        std::thread::spawn(move || load_user_config(&image));
    }
    spawn_ticker(image.clone(), Arc::clone(&counters), Arc::clone(&registry));
    #[cfg(unix)]
    {
        serve_unix(
            listener,
            (path, runtime),
            signals,
            registry,
            image,
            counters,
            socket_owner,
        )
    }
    #[cfg(windows)]
    {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            spawn_connection(stream, &registry, &image, &counters, &socket_owner);
        }
        Ok(())
    }
}

#[cfg(unix)]
fn serve_unix(
    mut listener: Listener,
    socket: (&Path, Option<&Path>),
    mut signals: std::os::unix::net::UnixStream,
    registry: Arc<Registry>,
    image: Image,
    counters: Arc<crate::tick::Counters>,
    socket_owner: Arc<SocketOwnership>,
) -> ! {
    use std::io::Read as _;
    use std::os::fd::AsRawFd;

    // Auto-started (#107), the daemon leads its own session and a HUP is a
    // stray one. Run by hand in a terminal it does not, and a HUP means that
    // terminal really hung up — stop in order rather than write to a dead tty.
    // SAFETY: getsid/getpid only read this process's ids.
    let detached = unsafe { libc::getsid(0) == libc::getpid() };
    let mut signal_bytes = [0u8; 1];
    'poll_loop: loop {
        let mut watched = [
            libc::pollfd {
                fd: unix_listener_fd(&listener),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: signals.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `watched` points to two initialized pollfd values for the
        // duration of this blocking call. A negative timeout waits indefinitely.
        let ready = unsafe { libc::poll(watched.as_mut_ptr(), watched.len() as _, -1) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            eprintln!("remuda daemon: poll failed: {error}");
            continue;
        }

        // Handle signals first when both descriptors are ready. In particular,
        // SIGUSR1 can replace the listener before accepting a queued probe.
        if watched[1].revents & libc::POLLIN != 0 {
            match signals.read(&mut signal_bytes) {
                Ok(0) => continue,
                Ok(count) => {
                    for byte in &signal_bytes[..count] {
                        let signal = libc::c_int::from(*byte);
                        if signal == libc::SIGUSR1 {
                            rebind_after_sigusr1(&mut listener, socket.0, socket.1, &socket_owner);
                            // The old listener's readiness bits cannot describe
                            // the replacement listener. Poll both fds again.
                            continue 'poll_loop;
                        }
                        let name = match signal {
                            libc::SIGTERM => "SIGTERM",
                            libc::SIGINT => "SIGINT",
                            _ if detached => {
                                let _ = writeln!(
                                    std::io::stderr(),
                                    "remuda daemon: SIGHUP ignored — use `remuda stop` to stop it"
                                );
                                continue;
                            }
                            _ => "SIGHUP",
                        };
                        let _ = writeln!(std::io::stderr(), "remuda daemon: {name}, shutting down");
                        reap_processes_before_exit(&image);
                        socket_owner.cleanup();
                        std::process::exit(0);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    eprintln!("remuda daemon: signal socket read failed: {error}");
                    continue;
                }
            }
        }

        if watched[0].revents & libc::POLLIN != 0 {
            match listener.accept() {
                Ok(stream) => {
                    if let Err(error) = stream.set_nonblocking(false) {
                        eprintln!("remuda daemon: could not restore blocking client mode: {error}");
                        continue;
                    }
                    spawn_connection(stream, &registry, &image, &counters, &socket_owner);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::ConnectionAborted
                    ) => {}
                Err(error) => eprintln!("remuda daemon: accept failed: {error}"),
            }
        }
    }
}

#[cfg(unix)]
fn prepare_unix_listener(mut listener: Listener, path: &Path) -> std::io::Result<Listener> {
    use std::os::unix::fs::PermissionsExt;

    // SocketOwnership performs inode-conditional cleanup, so an old listener
    // must never unlink a replacement path when it is dropped.
    listener.do_not_reclaim_name_on_drop();
    listener.set_nonblocking(ListenerNonblockingMode::Accept)?;
    // Both initial binds and SIGUSR1 rebinds occur inside a validated private
    // directory before the socket mode is tightened.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

#[cfg(unix)]
fn unix_listener_fd(listener: &Listener) -> std::os::fd::RawFd {
    use std::os::fd::AsRawFd;
    match listener {
        interprocess::local_socket::Listener::UdSocket(listener) => listener.inner().as_raw_fd(),
    }
}

#[cfg(unix)]
fn rebind_after_sigusr1(
    listener: &mut Listener,
    path: &Path,
    runtime: Option<&Path>,
    socket_owner: &SocketOwnership,
) {
    // Our current listener already owns this exact path, so rebinding it would
    // only perform an unnecessary self-connect and report a misleading error.
    if socket_owner.owns_path() {
        return;
    }
    if let Err(error) = prepare_socket_path(path, runtime) {
        eprintln!(
            "remuda daemon: could not validate socket directory {} before rebind: {error}",
            path.display()
        );
        return;
    }
    match ipc::listen(path) {
        Ok(replacement) => {
            let replacement = match prepare_unix_listener(replacement, path) {
                Ok(listener) => listener,
                Err(error) => {
                    eprintln!(
                        "remuda daemon: could not prepare rebound socket at {}: {error}",
                        path.display()
                    );
                    return;
                }
            };
            let refreshed = socket_owner.refresh();
            *listener = replacement;
            match refreshed {
                Ok(()) => eprintln!(
                    "remuda daemon: rebound socket at {} after SIGUSR1",
                    path.display()
                ),
                Err(error) => eprintln!(
                    "remuda daemon: could not record rebound socket at {}: {error}",
                    path.display()
                ),
            }
        }
        Err(error) => eprintln!(
            "remuda daemon: could not rebind socket at {} after SIGUSR1: {error}",
            path.display()
        ),
    }
}

fn spawn_connection(
    stream: Stream,
    registry: &Arc<Registry>,
    image: &Image,
    counters: &Arc<crate::tick::Counters>,
    socket_owner: &Arc<SocketOwnership>,
) {
    let registry = Arc::clone(registry);
    let image = image.clone();
    let counters = Arc::clone(counters);
    let socket_owner = Arc::clone(socket_owner);
    std::thread::spawn(move || {
        let _ = handle(stream, &registry, &image, &counters, socket_owner);
    });
}

/// Serialize stale-socket removal and bind for one daemon name. The lock file
/// stays in the runtime directory; unlinking it would let contenders lock
/// different inodes while one daemon still owns the old file.
struct SocketLock {
    #[cfg(unix)]
    _file: std::fs::File,
}

#[cfg(unix)]
const SOCKET_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
#[cfg(unix)]
const SOCKET_LOCK_NOTICE_AFTER: std::time::Duration = std::time::Duration::from_secs(1);

impl SocketLock {
    fn acquire(socket: &Path, runtime: Option<&Path>) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

            prepare_socket_path(socket, runtime).map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("prepare socket path {}: {error}", socket.display()),
                )
            })?;
            let mut lock_name = socket.as_os_str().to_os_string();
            lock_name.push(".lock");
            let lock_path = PathBuf::from(lock_name);
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .mode(0o600)
                .open(&lock_path)?;
            std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))?;
            let started = std::time::Instant::now();
            let deadline = started + SOCKET_LOCK_WAIT;
            let mut announced = false;
            loop {
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        socket_lock_timeout_message(&lock_path, socket_lock_holder(&lock_path)),
                    ));
                }
                let result =
                    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if result == 0 {
                    use std::io::{Seek, SeekFrom};
                    file.set_len(0)?;
                    file.seek(SeekFrom::Start(0))?;
                    writeln!(file, "{}", std::process::id())?;
                    file.flush()?;
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    if ipc::connect(socket).is_ok() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::AddrInUse,
                            "Remuda daemon is already listening",
                        ));
                    }
                    let holder = socket_lock_holder(&lock_path);
                    let elapsed = started.elapsed();
                    if !announced && elapsed >= SOCKET_LOCK_NOTICE_AFTER {
                        eprintln!(
                            "remuda: waiting for the socket lock held by another remuda daemon (pid {})",
                            holder.map_or_else(|| "unknown".to_string(), |pid| pid.to_string())
                        );
                        announced = true;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                } else if error.kind() == std::io::ErrorKind::Interrupted {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                } else {
                    return Err(error);
                }
            }
            Ok(Self { _file: file })
        }
        #[cfg(windows)]
        {
            let _ = socket;
            let _ = runtime;
            Ok(Self {})
        }
    }
}

#[cfg(unix)]
pub fn prepare_socket_path(socket: &Path, runtime: Option<&Path>) -> std::io::Result<()> {
    let socket = normalize_socket_path(socket)?;
    let parent = socket.parent().unwrap_or_else(|| Path::new("."));
    let Some(runtime) = runtime else {
        std::fs::create_dir_all(parent)?;
        return validate_socket_directory(parent, unsafe { libc::geteuid() }, false);
    };

    let runtime = normalize_socket_path(runtime)?;
    let managed = runtime.join("remuda");
    if parent == managed || parent.starts_with(&managed) {
        prepare_runtime_base(&runtime)?;
        prepare_managed_socket_directory(&managed)?;
        if parent != managed {
            // Nested -s paths live below our private runtime directory, but
            // their own mode remains user controlled.
            std::fs::create_dir_all(parent)?;
            validate_socket_directory(parent, unsafe { libc::geteuid() }, false)?;
        }
    } else {
        // Absolute and escaping -s paths are user-owned locations. Never
        // tighten them; inspect before the client creates the startup log.
        std::fs::create_dir_all(parent)?;
        validate_socket_directory(parent, unsafe { libc::geteuid() }, false)?;
    }
    Ok(())
}

#[cfg(unix)]
fn prepare_runtime_base(path: &Path) -> std::io::Result<()> {
    let path = normalize_socket_path(path)?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    create_private_directory_if_absent(&path)?;
    validate_socket_directory(&path, unsafe { libc::geteuid() }, false)
}

#[cfg(unix)]
fn prepare_managed_socket_directory(path: &Path) -> std::io::Result<()> {
    let path = normalize_socket_path(path)?;
    create_private_directory_if_absent(&path)?;
    validate_socket_directory(&path, unsafe { libc::geteuid() }, true)
}

/// Remove path spellings that can hide the directory actually opened by the
/// kernel. Parent traversal is rejected so validation cannot be bypassed by
/// comparing a path before resolving `..`.
#[cfg(unix)]
fn normalize_socket_path(path: &Path) -> std::io::Result<PathBuf> {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("socket path {} contains '..'; refusing", path.display()),
                ));
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    if normalized.as_os_str().is_empty() {
        normalized.push(".");
    }
    Ok(normalized)
}

#[cfg(unix)]
fn create_private_directory_if_absent(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => {
            // Umask can only remove permissions. Make the owner mode exact
            // before any contents are created; the new leaf is not traversable
            // by other users during this brief adjustment.
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn validate_socket_directory(
    path: &Path,
    expected_uid: libc::uid_t,
    tighten: bool,
) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let path = normalize_socket_path(path)?;

    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("socket directory {} is a symlink; refusing", path.display()),
        ));
    }

    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
    let directory = options.open(&path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("socket parent {} is not a directory", path.display()),
        ));
    }
    if metadata.uid() != expected_uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "socket directory {} is owned by uid {}, expected {}; choose a private REMUDA_RUNTIME_DIR",
                path.display(),
                metadata.uid(),
                expected_uid
            ),
        ));
    }
    let mode = metadata.permissions().mode() & 0o7777;
    if mode & 0o022 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "socket directory {} is group- or world-writable (mode {mode:04o}); remove those write permissions before starting remuda",
                path.display()
            ),
        ));
    }
    if tighten && mode != 0o700 {
        // Tighten through the opened directory handle; this also removes any
        // special permission bits and avoids following a replacement symlink.
        directory
            .set_permissions(std::fs::Permissions::from_mode(0o700))
            .map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("tighten socket directory {}: {error}", path.display()),
                )
            })?;
    }
    Ok(())
}

#[cfg(windows)]
pub fn prepare_socket_path(_socket: &Path, _runtime: Option<&Path>) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn socket_lock_holder(lock_path: &Path) -> Option<u32> {
    std::fs::read_to_string(lock_path).ok()?.trim().parse().ok()
}

#[cfg(unix)]
fn socket_lock_timeout_message(lock_path: &Path, holder: Option<u32>) -> String {
    match holder {
        Some(pid) => format!(
            "timed out waiting for the socket lock held by remuda daemon pid {pid}; \
             if it is stopped, run `kill -CONT {pid}` to resume it; if it is stuck, \
             verify it is this daemon, run `kill {pid}`, then retry"
        ),
        None => format!(
            "timed out waiting for the socket lock (holder pid unavailable; inspect {})",
            lock_path.display()
        ),
    }
}

/// The socket inode this daemon actually bound. Cleanup is conditional so a
/// replacement endpoint installed at the same pathname belongs to its creator.
struct SocketOwnership {
    path: PathBuf,
    #[cfg(unix)]
    identity: Mutex<(u64, u64)>,
}

impl SocketOwnership {
    fn capture(path: &Path) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::symlink_metadata(path)?;
            Ok(Self {
                path: path.to_path_buf(),
                identity: Mutex::new((metadata.dev(), metadata.ino())),
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                path: path.to_path_buf(),
            })
        }
    }

    fn cleanup(&self) {
        #[cfg(windows)]
        let _ = &self.path;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let Ok(identity) = self.identity.lock() else {
                return;
            };
            if std::fs::symlink_metadata(&self.path)
                .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == *identity)
            {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }

    #[cfg(unix)]
    fn refresh(&self) -> std::io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(&self.path)?;
        let mut identity = self
            .identity
            .lock()
            .map_err(|_| std::io::Error::other("socket ownership lock poisoned"))?;
        *identity = (metadata.dev(), metadata.ino());
        Ok(())
    }

    #[cfg(unix)]
    fn owns_path(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        let Ok(identity) = self.identity.lock() else {
            return false;
        };
        std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == *identity)
    }
}

impl Drop for SocketOwnership {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// A fixed period, not configurable yet. Fires every period whether or not
/// anything is registered — an unconditional wakeup rate paid by every
/// daemon, not just a latency ceiling for schedules; see steps/031.
const TICK_PERIOD: std::time::Duration = std::time::Duration::from_secs(1);

/// The one path every reap site (the ticker, `Request::List`, `ls()`) must
/// go through: `Registry::reap()` hands its answer to whoever calls first, so
/// notifying anywhere else would race it and silently drop the event.
pub(crate) fn reap_and_notify(registry: &Registry, image: &Image) -> Vec<String> {
    let dead = registry.reap_with_exit_info();
    for (name, id, reason, exit_info) in &dead {
        notify_exited(image, name, id, reason, exit_info.as_ref());
    }
    dead.into_iter().map(|(name, _, _, _)| name).collect()
}

fn close(registry: &Registry, image: &Image, name: &str) -> Option<AgentResult<()>> {
    let session = registry.get(name)?;
    let closed = registry.close(name)?;
    if let Ok(true) = closed {
        // If the reaper removed it first, close returns false and the reaper
        // owns the single notification using the marker's `closed` reason.
        notify_exited(
            image,
            name,
            session.id(),
            "closed",
            session.exit_info().as_ref(),
        );
    }
    Some(closed.map(drop))
}

/// `close` stops tracking a session itself, so the reaper never sees it die:
/// whichever of the two removes the entry fires the one `session_exited`.
fn notify_exited(
    image: &Image,
    name: &str,
    id: &str,
    reason: &str,
    exit_info: Option<&remuda_core::agent::ExitInfo>,
) {
    image.wait_session_output_monitor(id);
    let mut fields = vec![format!("reason={}", crate::mcp::lua_string(reason))];
    if let Some(exit_info) = exit_info {
        if let Some(exit_code) = exit_info.exit_code {
            fields.push(format!("exit_code={exit_code}"));
        }
        if reason != "closed" {
            if let Some(signal) = exit_info.signal {
                fields.push(format!("signal={signal}"));
            }
            if let Some(signal_name) = &exit_info.signal_name {
                fields.push(format!(
                    "signal_name={}",
                    crate::mcp::lua_string(signal_name)
                ));
            }
        }
    }
    let details = format!("{{{}}}", fields.join(", "));
    let _ = image.submit(
        &format!(
            "remuda.emit('session_exited', {}, {})",
            crate::mcp::lua_string(name),
            details,
        ),
        None,
    );
}

/// Wake the image once a period with `remuda._run_due_schedules(now)`, and
/// reap dead sessions, firing `session_exited` for each. Its own thread, so a
/// wedged schedule stalls only the tick, never the listener loop.
fn spawn_ticker(image: Image, counters: Arc<crate::tick::Counters>, registry: Arc<Registry>) {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let for_submit = Arc::clone(&clock);
    std::thread::spawn(move || {
        let ticker = crate::tick::Ticker::new(
            move || {
                let now = for_submit.now().as_secs_f64();
                if !keep_exited() {
                    reap_and_notify(&registry, &image);
                }
                image.submit(&format!("remuda._run_due_schedules({now})"), None)
            },
            clock,
            TICK_PERIOD,
            counters,
        );
        ticker.run_forever();
    });
}

/// The daemon is the one place that sees every round trip, including a
/// same-process loopback a script's own Eval makes back to itself.
fn record_request(counters: &crate::tick::Counters, request: &Request) {
    match request {
        Request::List => counters.counter("request_list").record_hit(),
        Request::Eval { .. } => counters.counter("request_eval").record_hit(),
        Request::CaptureStyled { .. } => counters.counter("request_capture_styled").record_hit(),
        Request::Sync { .. } => counters.counter("request_sync").record_hit(),
        _ => {}
    }
}

/// The handler writes signal numbers to a socketpair polled with the listener.
/// Catching signals preserves default dispositions in pty children.
#[cfg(unix)]
fn catch_signals() -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicI32, Ordering};
    static WRITE_FD: AtomicI32 = AtomicI32::new(-1);
    extern "C" fn on_signal(signal: libc::c_int) {
        let byte = signal as u8;
        // SAFETY: write(2) is async-signal-safe; the fd outlives the process.
        unsafe {
            libc::write(
                WRITE_FD.load(Ordering::Relaxed),
                (&raw const byte).cast(),
                1,
            )
        };
    }
    let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
    WRITE_FD.store(writer.as_raw_fd(), Ordering::Relaxed);
    std::mem::forget(writer);
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGUSR1] {
        // SAFETY: installs a handler that only calls write(2).
        unsafe {
            libc::signal(
                signal,
                on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            )
        };
    }
    Ok(reader)
}

/// Best-effort group-wide reap before a clean `Request::Shutdown` exits —
/// covers a grandchild PDEATHSIG alone can't reach (see child_guard.rs).
/// Only THIS death mode runs any code at all; SIGTERM/SIGKILL run none.
fn reap_processes_before_exit(image: &Image) {
    image.shutdown_pending_replies();
    let _ = image.eval(
        "for _, id in ipairs(remuda.processes()) do remuda._process_killpg(id) end",
        Some("@remuda/shutdown-reap"),
    );
    image.stop_modules_bounded();
}

fn capture_styled(
    stream: &Stream,
    registry: &Registry,
    name: &str,
    scrollback: usize,
) -> std::io::Result<()> {
    match registry.screen_snapshot_version_at(name, scrollback) {
        None => reply(stream, &Response::error(format!("no such session: {name}"))),
        Some(Err(e)) => reply(stream, &Response::error(e)),
        Some(Ok(versioned)) => {
            // Runs on the wire, not cells — see steps/022 for the 44x+
            // measured on a real screen. Cells, counters and cursor all come
            // from one parser snapshot, so new output cannot skew the anchor.
            let rows = versioned
                .snapshot
                .cells
                .iter()
                .map(|row| collapse_runs(row))
                .collect();
            reply(
                stream,
                &Response::StyledScreen {
                    rows,
                    instance_id: versioned.instance_id,
                    output_version: versioned.output_version,
                    wrapped: versioned.snapshot.wrapped,
                    scrollback_len: versioned.snapshot.scrollback_len,
                    scrollback_total: versioned.snapshot.scrollback_total,
                    cursor: versioned.snapshot.cursor,
                },
            )
        }
    }
}

fn handle(
    stream: Stream,
    registry: &Registry,
    image: &Image,
    counters: &crate::tick::Counters,
    socket_owner: Arc<SocketOwnership>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let Some(request) = read_request(&stream, &mut reader)? else {
        return Ok(());
    };

    record_request(counters, &request);

    match request {
        // Where a session that ended stops being listed. Here rather than on a
        // timer because listing is the only moment the answer is looked at, and
        // a reaper thread would need a clock this layer is not given.
        Request::List => {
            if !keep_exited() {
                reap_and_notify(registry, image);
            }
            reply(&stream, &Response::Sessions(registry.list()))
        }

        Request::Version => reply(&stream, &Response::Value(crate::dist::BUILD_VERSION.into())),

        // Answer before going. A client left guessing from a hung-up socket
        // cannot tell "it stopped" from "it never heard me".
        request @ Request::Shutdown { .. } => {
            handle_shutdown(stream, registry, image, socket_owner, request)
        }

        Request::New {
            name,
            command,
            size,
            cwd,
            env,
        } => handle_new(
            stream,
            registry,
            image,
            NewSessionRequest {
                name,
                command,
                size,
                cwd,
                env,
            },
        ),

        Request::SendLine { name, text } => {
            respond(&stream, &name, registry.send_line(&name, &text), |()| {
                Response::Ok
            })
        }

        Request::Input {
            name,
            instance_id,
            client_id,
            seq,
            bytes,
        } => input(
            &stream,
            registry,
            &name,
            &instance_id,
            &client_id,
            seq,
            &bytes,
        ),

        Request::Send { name, bytes } => {
            respond(&stream, &name, registry.send(&name, &bytes), |()| {
                Response::Ok
            })
        }

        Request::Feed { name, steps } => {
            respond(&stream, &name, registry.feed(&name, &steps), |()| {
                Response::Ok
            })
        }

        Request::Resize { name, size } => {
            respond(&stream, &name, registry.resize(&name, size), |()| {
                Response::Ok
            })
        }

        Request::Capture { name } => respond(
            &stream,
            &name,
            registry.screen_text(&name),
            Response::Screen,
        ),

        Request::CaptureStyled { name, scrollback } => {
            capture_styled(&stream, registry, &name, scrollback)
        }

        Request::Sync {
            name,
            instance_id,
            since,
            timeout_ms,
        } => handle_sync(&stream, registry, &name, instance_id, since, timeout_ms),

        Request::MouseState { name } => mouse_state(&stream, registry, &name),

        Request::Attach { name } => attach(stream, reader, registry, &name, false),
        Request::AttachTracked { name } => attach(stream, reader, registry, &name, true),
        Request::AttachStatus { name, generation } => {
            let response = registry.get(&name).map_or_else(
                || Response::error(format!("no such session: {name}")),
                |session| Response::AttachStatus {
                    taken_over: session.was_attachment_taken_over(generation),
                },
            );
            reply(&stream, &response)
        }

        Request::Close { name } => respond(&stream, &name, close(registry, image, &name), |()| {
            Response::Ok
        }),

        Request::ListDir { path: dir } => reply(&stream, &list_dir(&dir)),
        Request::Mkdir { path: dir } => reply(&stream, &mkdir(&dir)),
        Request::RemoveDirAll { path: dir } => reply(&stream, &remove_dir_all(&dir)),

        Request::Eval { code, name } => handle_eval(stream, reader, image, &code, name.as_deref()),
    }
}

fn handle_eval(
    stream: Stream,
    reader: BufReader<Stream>,
    image: &Image,
    code: &str,
    name: Option<&str>,
) -> std::io::Result<()> {
    match image.eval_request(code, name) {
        Ok(value) => match image.pending_replies().pending_id(&value) {
            Some(id) => deferred_reply(stream, reader, image, id),
            None if value.len() > crate::reply_limit::MAX_REPLY_BYTES => reply(
                &stream,
                &Response::error(format!(
                    "synchronous reply exceeds the {} MiB output limit ({} bytes)",
                    crate::reply_limit::MAX_REPLY_BYTES / (1024 * 1024),
                    value.len()
                )),
            ),
            None => reply(&stream, &Response::Value(value)),
        },
        // Lua's own message, which already carries the line and a traceback —
        // the same treatment `remuda run` gives a script file.
        Err(error) => reply(&stream, &Response::error(error)),
    }
}

fn deferred_reply(
    stream: Stream,
    reader: BufReader<Stream>,
    image: &Image,
    id: u64,
) -> std::io::Result<()> {
    #[cfg(unix)]
    let mut reader = reader;
    #[cfg(unix)]
    if let Err(error) = stream.set_nonblocking(true) {
        image.pending_replies().abandon(id);
        return Err(error);
    }
    let result = image.pending_replies().wait(id, || {
        #[cfg(unix)]
        {
            let mut extra = [0u8; 1];
            match reader.read(&mut extra) {
                Ok(0) => true,
                Ok(_) => false,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => false,
                Err(_) => true,
            }
        }
        #[cfg(windows)]
        {
            crate::ipc::peer_disconnected(reader.get_ref()).unwrap_or(true)
        }
    });
    #[cfg(unix)]
    let _ = stream.set_nonblocking(false);
    match result {
        Ok(result) => {
            let response = match result.completion {
                Ok(crate::pending::Completion::Result(result)) => Some(Response::CommandResult {
                    exit_code: result.exit_code,
                    stdout_base64: crate::cluster::encoding::encode_base64(&result.stdout),
                    stderr_base64: crate::cluster::encoding::encode_base64(&result.stderr),
                }),
                Ok(crate::pending::Completion::Failure(error)) => Some(Response::error(error)),
                Err(error) if error == "client disconnected" => None,
                Err(error) => Some(Response::error(error)),
            };
            let sent = response.map_or(Ok(()), |response| reply(&stream, &response));
            if let Some(ack) = result.shutdown_ack {
                let _ = ack.send(());
            }
            sent
        }
        Err(error) => reply(&stream, &Response::error(error)),
    }
}

fn handle_sync(
    stream: &Stream,
    registry: &Registry,
    name: &str,
    expected_instance: Option<String>,
    since: u64,
    timeout_ms: u64,
) -> std::io::Result<()> {
    let Some(session) = registry.get(name) else {
        return reply(stream, &Response::error(format!("no such session: {name}")));
    };
    if expected_instance
        .as_deref()
        .is_some_and(|expected| expected != session.instance_id())
    {
        return reply(stream, &Response::WrongInstance);
    }
    let _permit = match acquire_sync_permit() {
        Ok(permit) => permit,
        Err(response) => return reply(stream, &response),
    };
    let result = remuda_core::sync::wait(&session, since, timeout_ms);
    if let Some(expected) = expected_instance.as_deref() {
        if registry
            .get(name)
            .is_none_or(|current| current.instance_id() != expected)
        {
            return reply(stream, &Response::WrongInstance);
        }
    }
    let versioned = match result {
        Ok(snapshot) => snapshot,
        Err(error) => return reply(stream, &Response::error(error)),
    };
    let rows = versioned
        .snapshot
        .cells
        .iter()
        .map(|row| collapse_runs(row))
        .collect();
    reply(
        stream,
        &Response::Sync {
            instance_id: session.instance_id().to_owned(),
            output_version: versioned.output_version.unwrap_or(0),
            snapshot: StyledScreen {
                rows,
                wrapped: versioned.snapshot.wrapped,
                scrollback_len: versioned.snapshot.scrollback_len,
                scrollback_total: versioned.snapshot.scrollback_total,
                cursor: versioned.snapshot.cursor,
            },
        },
    )
}

fn handle_shutdown(
    stream: Stream,
    registry: &Registry,
    image: &Image,
    socket_owner: Arc<SocketOwnership>,
    request: Request,
) -> std::io::Result<()> {
    let Request::Shutdown {
        requester_daemon_id,
        requester_session_id,
        requester_session_name,
        override_hosted,
    } = request
    else {
        unreachable!("handle_shutdown only accepts Shutdown requests");
    };
    let own_daemon_id = std::process::id().to_string();
    let known_session_name = requester_session_id
        .as_deref()
        .and_then(|id| registry.name_for_id(id));
    let caller_claims_this_daemon = requester_daemon_id.as_deref() == Some(own_daemon_id.as_str())
        && (requester_session_id.is_some() || requester_session_name.is_some());
    if !override_hosted && (known_session_name.is_some() || caller_claims_this_daemon) {
        let identity = known_session_name
            .or(requester_session_name)
            .or(requester_session_id)
            .unwrap_or_else(|| "unknown".into());
        return refuse_hosted_shutdown(&stream, &identity);
    }
    if !override_hosted {
        match process_ancestry::peer_pid(&stream) {
            Ok(Some(peer_pid)) => match process_ancestry::is_self_or_descendant(
                peer_pid,
                &registry.live_process_ids(),
            ) {
                process_ancestry::Ancestry::Inside => {
                    return refuse_hosted_shutdown(&stream, "session process ancestry")
                }
                process_ancestry::Ancestry::Outside => {}
                process_ancestry::Ancestry::Unreadable { pid, error } => eprintln!(
                    "remuda: shutdown ancestry stopped at unreadable pid {pid} ({error}); treating requester as outside"
                ),
            },
            Ok(None) | Err(_)
                if process_ancestry::missing_peer_requires_refusal(caller_claims_this_daemon) =>
            {
                return refuse_hosted_shutdown(&stream, "self-reported session identity")
            }
            Ok(None) => eprintln!(
                "remuda: shutdown peer process ID unavailable; treating requester as outside"
            ),
            Err(error) => eprintln!(
                "remuda: shutdown peer process ID unavailable ({error}); treating requester as outside"
            ),
        }
    }
    image.shutdown_pending_replies();
    reply(&stream, &Response::Ok)?;
    reap_processes_before_exit(image);
    socket_owner.cleanup();
    std::process::exit(0);
}

fn refuse_hosted_shutdown(stream: &Stream, identity: &str) -> std::io::Result<()> {
    reply(
        stream,
        &Response::error(format!(
            "cannot stop this daemon from one of its own sessions ({identity}); pass --i-am-inside to override"
        )),
    )
}

struct NewSessionRequest {
    name: Option<String>,
    command: Vec<String>,
    size: Size,
    cwd: Option<String>,
    env: Option<HashMap<String, String>>,
}

fn handle_new(
    stream: Stream,
    registry: &Registry,
    image: &Image,
    request: NewSessionRequest,
) -> std::io::Result<()> {
    let NewSessionRequest {
        name,
        command,
        size,
        cwd,
        env,
    } = request;
    let name = match name {
        Some(given) => given,
        None => registry.unique_name(&remuda_core::registry::slug(
            command
                .first()
                .map_or_else(default_shell, String::clone)
                .as_str(),
        )),
    };
    let mut session_env = env.unwrap_or_default();
    let session_id = Session::new_id();
    session_env.insert("REMUDA_DAEMON_ID".into(), std::process::id().to_string());
    session_env.insert("REMUDA_SESSION_ID".into(), session_id.clone());
    session_env.insert("REMUDA_SESSION_NAME".into(), name.clone());
    match spawn(
        &name,
        &session_id,
        &command,
        size,
        cwd.as_deref(),
        Some(&session_env),
    ) {
        Err(e) => reply(&stream, &Response::error(e)),
        Ok(session) => {
            let session_id = session.id().to_string();
            let notifier = image.session_output_notifier(&name, &session_id);
            match registry.register(session) {
                Ok(session) => {
                    if let Some(output) = session.subscribe_output_wakeup() {
                        monitor_session_output(output, notifier);
                    } else {
                        notifier.flush(session.output_version().unwrap_or(0));
                        notifier.finish_monitor();
                    }
                    reply(&stream, &Response::Value(name))
                }
                Err(_) => {
                    image.discard_session_output_monitor(&session_id, &notifier);
                    reply(&stream, &Response::error(format!("name taken: {name}")))
                }
            }
        }
    }
}

/// Wait for PTY output, then submit one coalesced wake into the Lua image.
/// The deadline starts with the first chunk so continuous output cannot starve
/// a notification by continually restarting a quiet-period timer.
fn monitor_session_output(
    output: remuda_core::agent::OutputWakeup,
    notifier: crate::image::SessionOutputNotifier,
) {
    const COALESCE: std::time::Duration = std::time::Duration::from_millis(50);
    std::thread::spawn(move || {
        // The PTY reader starts with the child, before `handle_new` can
        // register the session and attach this monitor. Catch any output that
        // arrived in that gap; output racing this snapshot will also leave a
        // wake queued on `output`.
        let mut last_notified = output.version_after_wake();
        if last_notified > 0 {
            notifier.notify(last_notified);
        }
        loop {
            if output.recv().is_err() {
                notifier.flush(output.version_after_wake());
                notifier.finish_monitor();
                return;
            }
            let deadline = std::time::Instant::now() + COALESCE;
            let mut disconnected = false;
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match output.recv_timeout(remaining) {
                    Ok(()) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            let version = output.version_after_wake();
            if disconnected {
                notifier.flush(version);
                notifier.finish_monitor();
                return;
            }
            if version > last_notified {
                notifier.notify(version);
                last_notified = version;
            }
        }
    });
}

fn read_request(
    stream: &Stream,
    reader: &mut impl std::io::BufRead,
) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    match serde_json::from_str(&line) {
        Ok(request) => Ok(Some(request)),
        Err(error) => {
            reply(stream, &Response::error(format!("bad request: {error}")))?;
            Ok(None)
        }
    }
}

fn input(
    stream: &Stream,
    registry: &Registry,
    name: &str,
    instance_id: &str,
    client_id: &str,
    seq: u64,
    bytes: &[u8],
) -> std::io::Result<()> {
    let client_id = match remuda_core::input::validate_batch(client_id, seq, bytes) {
        Ok(client_id) => client_id,
        Err(error) => return reply(stream, &Response::error(error)),
    };
    let result = registry.apply_input_batch(
        name,
        remuda_core::input::InputBatch {
            instance_id,
            client_id,
            seq,
            bytes,
        },
    );
    match result {
        None => reply(stream, &Response::error(format!("no such session: {name}"))),
        Some(Err(remuda_core::input::InputError::RateLimited)) => {
            reply(stream, &Response::RateLimited)
        }
        Some(Err(remuda_core::input::InputError::Busy)) => reply(stream, &Response::Busy),
        Some(Err(error)) => reply(stream, &Response::error(error.to_string())),
        Some(Ok(remuda_core::input::InputOutcome::Ack { duplicate })) => {
            reply(stream, &Response::Ack { duplicate })
        }
        Some(Ok(remuda_core::input::InputOutcome::Uncertain)) => {
            reply(stream, &Response::Uncertain)
        }
        Some(Ok(remuda_core::input::InputOutcome::WrongInstance)) => {
            reply(stream, &Response::WrongInstance)
        }
        Some(Ok(remuda_core::input::InputOutcome::Exited)) => {
            reply(stream, &Response::error("session exited"))
        }
    }
}

fn mouse_state(stream: &Stream, registry: &Registry, name: &str) -> std::io::Result<()> {
    respond(
        stream,
        name,
        registry.get(name).map(|session| Ok(session.mouse_state())),
        Response::MouseState,
    )
}

/// The `None`/`Some(Err)`/`Some(Ok)` shape several `Request` arms share: no
/// such session, a refused or failed op, or an answer built from what it
/// returned.
fn respond<T>(
    stream: &Stream,
    name: &str,
    result: Option<AgentResult<T>>,
    ok: impl FnOnce(T) -> Response,
) -> std::io::Result<()> {
    match result {
        None => reply(stream, &Response::error(format!("no such session: {name}"))),
        Some(Err(remuda_core::AgentError::Busy)) => reply(stream, &Response::Busy),
        Some(Err(remuda_core::AgentError::WriteTimeout { .. })) => {
            reply(stream, &Response::WriteTimeout)
        }
        Some(Err(e)) => reply(stream, &Response::error(e)),
        Some(Ok(v)) => reply(stream, &ok(v)),
    }
}

fn list_dir(path: &str) -> Response {
    match std::fs::read_dir(path) {
        Ok(entries) => {
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            names.sort();
            Response::Entries(names)
        }
        Err(e) => Response::error(e.to_string()),
    }
}

fn mkdir(path: &str) -> Response {
    match std::fs::create_dir_all(path) {
        Ok(()) => Response::Ok,
        Err(e) => Response::error(e.to_string()),
    }
}

fn remove_dir_all(path: &str) -> Response {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Response::Ok,
        Err(e) => Response::error(e.to_string()),
    }
}

fn spawn(
    name: &str,
    session_id: &str,
    command: &[String],
    size: Size,
    cwd: Option<&str>,
    env: Option<&std::collections::HashMap<String, String>>,
) -> Result<Session, String> {
    let mut argv = command.to_vec();
    if argv.is_empty() {
        argv.push(default_shell());
    }
    let mut builder = CommandBuilder::new(&argv[0]);
    for arg in &argv[1..] {
        builder.arg(arg);
    }
    match cwd {
        Some(cwd) => builder.cwd(cwd),
        None => {
            if let Ok(cwd) = std::env::current_dir() {
                builder.cwd(cwd);
            }
        }
    }
    // The daemon inherits its whole environment, and it is often auto-started
    // from something with no terminal — so `TERM` reaches the agent unset or
    // `dumb` and its TUI degrades for a reason nobody can see from inside.
    if std::env::var("TERM").map(|t| t == "dumb").unwrap_or(true) {
        builder.env("TERM", "xterm-256color");
    }
    if let Some(env) = env {
        for (k, v) in env {
            builder.env(k, v);
        }
    }

    let agent = PtyAgent::spawn(builder, size).map_err(|e| e.to_string())?;
    Ok(Session::new_with_id(
        name,
        session_id,
        Box::new(agent),
        Arc::new(SystemClock::new()),
    ))
}

/// Hand this connection over to a human. A later attach displaces this one;
/// the old connection receives a printable notice before it is closed.
fn attach(
    stream: Stream,
    mut reader: BufReader<Stream>,
    registry: &Registry,
    name: &str,
    tracked: bool,
) -> std::io::Result<()> {
    let Some(session) = registry.get(name) else {
        return reply(
            &stream,
            &Response::error(format!("no such session: {name}")),
        );
    };
    let held = session.attach();
    let acknowledgement = if tracked {
        Response::AttachStarted {
            generation: held.generation(),
        }
    } else {
        Response::Ok
    };
    reply(&stream, &acknowledgement)?;

    // Paint what is already on screen before streaming anything new, or the
    // viewer sees a blank terminal until the program next redraws.
    let mut out = stream.try_clone()?;
    if let Ok(painted) = held.screen_bytes() {
        out.write_all(&painted)?;
        out.flush()?;
    }

    // Scoped threads, so the single exclusive guard can be shared with the
    // input pump rather than cloned or re-taken. There is still exactly one
    // `Attached` in existence, which is the invariant that makes raw writes
    // safe in the first place.
    // The two pumps block on *different* things — one on the socket, one on the
    // pty — so neither can be woken by the other's end-of-stream. A shared flag
    // plus a bounded wait is what lets either side end the attachment.
    //
    // Measured, not foreseen: without this, detaching left the output pump
    // parked on recv() from an idle shell, the scope never closed, the guard
    // was never dropped, and the session stayed locked to a viewer that had
    // already gone. The core got "a human is attached" forever.
    let done = std::sync::atomic::AtomicBool::new(false);
    let done = &done;
    let input_failed = std::sync::atomic::AtomicBool::new(false);
    let input_failed = &input_failed;
    // Checked before every read of the key pump below, not just its first —
    // a cancel that arrives before a read is pending is a documented no-op
    // on Windows, so the flag (not the cancel alone) is what actually stops
    // the loop. See steps/029.
    let stop = std::sync::atomic::AtomicBool::new(false);
    let stop = &stop;

    std::thread::scope(|scope| {
        // Keystrokes in, on their own thread: reading a socket blocks, and the
        // output pump must not wait on the human to type.
        let held = &held;
        let key_thread = scope.spawn(move || {
            let mut buf = [0u8; 4096];
            'keys: while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // Preserve this exact read buffer until the session
                        // or fails. Busy means no write was queued; every other
                        // error may follow a partial write and ends this pump.
                        match forward_attach_input(
                            || {
                                held.write_raw_while(&buf[..n], &|| {
                                    stop.load(std::sync::atomic::Ordering::SeqCst)
                                        || held.is_displaced()
                                })
                            },
                            || {
                                stop.load(std::sync::atomic::Ordering::SeqCst)
                                    || held.is_displaced()
                            },
                        ) {
                            Ok(()) => {}
                            Err(remuda_core::AgentError::Exited) => break 'keys,
                            Err(remuda_core::AgentError::Attached)
                                if stop.load(std::sync::atomic::Ordering::SeqCst)
                                    || held.is_displaced() =>
                            {
                                break 'keys;
                            }
                            Err(_) => {
                                input_failed.store(true, std::sync::atomic::Ordering::SeqCst);
                                break 'keys;
                            }
                        }
                    }
                }
            }
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        if let Some(rx) = held.subscribe() {
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                if held.is_displaced() {
                    break;
                }
                match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(chunk) => {
                        if out.write_all(&chunk).is_err() || out.flush().is_err() {
                            break;
                        }
                    }
                    // Timeout: nothing was printed, which is the normal state of
                    // an idle agent. Loop back and re-check whether we are done.
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        continue;
                    }
                    // The sender is gone: the process exited.
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        }
        if held.is_displaced() {
            // Keep this plain text so older clients display a useful reason
            // before observing the ordinary EOF, even if input ended the pump.
            let _ = out.write_all(b"\r\n[remuda] attached elsewhere, detached\r\n");
            let _ = out.flush();
        }
        if input_failed.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = report_attach_input_failure(&mut out);
        }
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        // Unblocks the key thread's read so the scope can close.
        ipc::stop_reader(&stream, stop, || key_thread.is_finished());
    });
    Ok(())
}

const ATTACH_INPUT_FAILURE_NOTICE: &str =
    "\r\n[remuda] input stopped after a PTY write error; some bytes may have been delivered partially or lost\r\n";

fn report_attach_input_failure(output: &mut impl Write) -> std::io::Result<()> {
    output.write_all(ATTACH_INPUT_FAILURE_NOTICE.as_bytes())?;
    output.flush()
}

fn forward_attach_input(
    mut write: impl FnMut() -> AgentResult<()>,
    mut stopping: impl FnMut() -> bool,
) -> AgentResult<()> {
    loop {
        if stopping() {
            return Err(remuda_core::agent::AgentError::Attached);
        }
        match write() {
            Err(remuda_core::agent::AgentError::Busy) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            result => return result,
        }
    }
}

struct LimitedReplyWriter<'a> {
    bytes: &'a mut Vec<u8>,
    limit: usize,
}

impl std::io::Write for LimitedReplyWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "serialized daemon reply exceeds the wire limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn reply(mut stream: &Stream, response: &Response) -> std::io::Result<()> {
    let wire_limit = crate::reply_limit::max_reply_wire_bytes();
    let mut line = Vec::with_capacity(1024);
    let serialized = serde_json::to_writer(
        LimitedReplyWriter {
            bytes: &mut line,
            limit: wire_limit - 1,
        },
        response,
    );
    if serialized.is_err() {
        line.clear();
        serde_json::to_writer(
            LimitedReplyWriter {
                bytes: &mut line,
                limit: wire_limit - 1,
            },
            &Response::error("daemon reply exceeds the maximum serialized size"),
        )?;
    }
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::{
        acquire_sync_permit, forward_attach_input, report_attach_input_failure, runtime_base_for,
        shell_or_default, SyncPermit, ATTACH_INPUT_FAILURE_NOTICE, MAX_CONCURRENT_SYNCS,
    };
    use remuda_core::agent::{AgentError, Color, StyledCell};
    use remuda_core::protocol::collapse_runs;
    use remuda_core::protocol::Response;
    use std::ffi::OsStr;
    use std::path::Path;
    use std::sync::Mutex;

    static SYNC_PERMIT_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn sync_concurrency_is_bounded() {
        let _lock = SYNC_PERMIT_TEST_LOCK.lock().unwrap();
        let mut permits: Vec<_> = (0..MAX_CONCURRENT_SYNCS)
            .map(|_| SyncPermit::acquire().expect("permit within sync capacity"))
            .collect();
        assert!(
            SyncPermit::acquire().is_none(),
            "excess Sync must be refused"
        );
        drop(permits.pop());
        let reusable = SyncPermit::acquire().expect("a released slot must be reusable");
        drop(reusable);
        drop(permits);
    }

    #[test]
    fn daemon_sync_capacity_refusal_is_typed_on_wire() {
        let _lock = SYNC_PERMIT_TEST_LOCK.lock().unwrap();
        let permits: Vec<_> = (0..MAX_CONCURRENT_SYNCS)
            .map(|_| SyncPermit::acquire().expect("fill daemon Sync capacity"))
            .collect();
        let response = match acquire_sync_permit() {
            Ok(_) => panic!("over-cap Sync must be refused"),
            Err(response) => response,
        };
        assert_eq!(response, Response::SyncAtCapacity);
        let mut wire = serde_json::to_vec(&response).expect("serialize protocol response");
        wire.push(b'\n');
        let decoded: Response =
            serde_json::from_slice(&wire[..wire.len() - 1]).expect("decode daemon response frame");
        assert_eq!(decoded, Response::SyncAtCapacity);
        drop(permits);
    }

    #[test]
    fn attach_input_does_not_replay_after_terminal_write_error_and_has_a_human_notice() {
        let mut attempts = 0;
        let result = forward_attach_input(
            || {
                attempts += 1;
                Err(AgentError::Io("injected partial write failure".into()))
            },
            || false,
        );
        assert!(matches!(result, Err(AgentError::Io(_))));
        assert_eq!(
            attempts, 1,
            "a possibly partial write must never be replayed"
        );
        assert!(ATTACH_INPUT_FAILURE_NOTICE.contains("input stopped"));
        assert!(ATTACH_INPUT_FAILURE_NOTICE.contains("may have been delivered"));
        let mut notice = Vec::new();
        report_attach_input_failure(&mut notice).unwrap();
        assert_eq!(notice, ATTACH_INPUT_FAILURE_NOTICE.as_bytes());
    }

    #[test]
    fn attach_input_retries_busy_only_until_the_original_buffer_is_accepted() {
        let mut attempts = 0;
        let result = forward_attach_input(
            || {
                attempts += 1;
                if attempts == 1 {
                    Err(AgentError::Busy)
                } else {
                    Ok(())
                }
            },
            || false,
        );
        assert!(result.is_ok());
        assert_eq!(attempts, 2);
    }

    #[test]
    fn android_runtime_base_uses_termux_temp_dir_when_xdg_is_unset() {
        assert_eq!(
            runtime_base_for(
                true,
                None,
                Path::new("/data/data/com.termux/files/usr/tmp"),
                "nobody",
            ),
            Path::new("/data/data/com.termux/files/usr/tmp")
        );
    }

    #[test]
    fn android_runtime_base_prefers_xdg_runtime_dir() {
        assert_eq!(
            runtime_base_for(
                true,
                Some(OsStr::new("/run/user/1000")),
                Path::new("/data/data/com.termux/files/usr/tmp"),
                "nobody",
            ),
            Path::new("/run/user/1000")
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_android_runtime_base_keeps_the_per_user_tmp_path() {
        assert_eq!(
            runtime_base_for(false, None, Path::new("/termux/tmp"), "jeongsoo"),
            Path::new("/tmp/remuda-jeongsoo")
        );
    }

    #[cfg(unix)]
    fn socket_test_dir(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "remuda-socket-policy-{}-{label}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn user_socket_parent_is_validated_without_changing_its_mode() {
        use super::prepare_socket_path;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let root = socket_test_dir("custom-parent");
        let runtime = root.join("runtime");
        let custom = root.join("chosen");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::set_permissions(&custom, std::fs::Permissions::from_mode(0o755)).unwrap();
        let before = std::fs::metadata(&custom).unwrap().mode() & 0o7777;

        prepare_socket_path(&custom.join("X.sock"), Some(&runtime)).unwrap();

        assert_eq!(std::fs::metadata(&custom).unwrap().mode() & 0o7777, before);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn nested_socket_name_keeps_the_managed_runtime_directory_private() {
        use super::prepare_socket_path;
        use std::os::unix::fs::PermissionsExt;

        let root = socket_test_dir("nested-name");
        let runtime = root.join("runtime");
        let socket = runtime.join("remuda/sub/X.sock");

        prepare_socket_path(&socket, Some(&runtime)).unwrap();

        assert_eq!(
            std::fs::metadata(runtime.join("remuda"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn writable_socket_directory_is_refused() {
        use super::validate_socket_directory;
        use std::os::unix::fs::PermissionsExt;

        let path = socket_test_dir("writable-leaf");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).unwrap();
        let uid = unsafe { libc::geteuid() };

        let error = validate_socket_directory(&path, uid, true).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("group- or world-writable"));
        std::fs::remove_dir_all(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn writable_runtime_base_is_refused_before_managed_directory_creation() {
        use super::prepare_socket_path;
        use std::os::unix::fs::PermissionsExt;

        let root = socket_test_dir("writable-base");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        let error = prepare_socket_path(&root.join("remuda/s.sock"), Some(&root)).unwrap_err();

        assert!(error.to_string().contains("group- or world-writable"));
        assert!(!root.join("remuda").exists());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn foreign_owned_base_is_refused() {
        use super::validate_socket_directory;

        let path = socket_test_dir("foreign-base");
        let uid = unsafe { libc::geteuid() };

        let error = validate_socket_directory(&path, uid.wrapping_add(1), false).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("owned by uid"));
        std::fs::remove_dir_all(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_socket_directory_has_a_specific_refusal_message() {
        use super::validate_socket_directory;

        let root = socket_test_dir("symlink");
        let target = root.join("target");
        let link = root.join("link");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let uid = unsafe { libc::geteuid() };

        let error = validate_socket_directory(&link, uid, true).unwrap_err();

        assert!(error.to_string().contains("is a symlink; refusing"));
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn non_android_windows_runtime_base_keeps_the_per_user_pipe_prefix() {
        assert_eq!(
            runtime_base_for(false, None, Path::new(r"C:\Temp"), "jeongsoo"),
            Path::new(r"\\remuda\jeongsoo")
        );
    }

    fn cell(text: &str, fg: Color) -> StyledCell {
        StyledCell {
            text: text.to_string(),
            fg,
            ..Default::default()
        }
    }

    /// [MEASURED] A real screen collapses to a fraction of the per-cell wire
    /// size — the fix for the 113x multiplier steps/020 introduced. See
    /// steps/022.
    #[test]
    fn collapsing_to_runs_shrinks_the_wire_size_a_real_screen_produces() {
        // 80x24: one coloured prompt-shaped run of text on row 0, everything
        // else default — the realistic case this fix targets, a handful of
        // style runs per row, not one independent style per cell.
        let mut row0: Vec<StyledCell> = Vec::new();
        for c in "user@host".chars() {
            row0.push(cell(&c.to_string(), Color::Idx(2)));
        }
        row0.push(cell(":", Color::Default));
        for c in "~/project".chars() {
            row0.push(cell(&c.to_string(), Color::Idx(4)));
        }
        while row0.len() < 80 {
            row0.push(cell(" ", Color::Default));
        }
        let mut screen: Vec<Vec<StyledCell>> = vec![row0];
        for _ in 1..24 {
            screen.push(vec![cell(" ", Color::Default); 80]);
        }

        let per_cell_bytes = serde_json::to_string(&screen).unwrap().len();
        let runs: Vec<_> = screen.iter().map(|r| collapse_runs(r)).collect();
        let run_bytes = serde_json::to_string(&runs).unwrap().len();

        assert!(
            run_bytes * 10 < per_cell_bytes,
            "expected at least a 10x reduction, got {per_cell_bytes} -> {run_bytes}"
        );
    }

    #[test]
    fn a_shell_the_person_already_chose_is_never_second_guessed() {
        assert_eq!(
            shell_or_default(Some("/usr/bin/fish".into())),
            "/usr/bin/fish"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_with_no_shell_set_falls_back_to_sh() {
        assert_eq!(shell_or_default(None), "sh");
    }

    /// Runs the prefill rather than asserting its spelling: the failure this
    /// guards is a prompt naming a binary the machine does not have.
    #[cfg(windows)]
    #[test]
    fn the_windows_prefill_is_powershell_and_it_is_really_there() {
        let shell = shell_or_default(None);
        assert_eq!(shell, "powershell.exe");
        // `42` cannot appear in an echo of the question (PRINCIPLES §4).
        let out = std::process::Command::new(&shell)
            .args(["-NoProfile", "-Command", "Write-Output (6*7)"])
            .output()
            .expect("the prefilled shell must be runnable, not merely plausible");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "42");
    }
}
