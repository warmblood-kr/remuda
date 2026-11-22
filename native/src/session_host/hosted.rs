//! Opt-in host-backed daemon sessions (#713). With the opt-in set, `new`
//! reserves a launch in this daemon's private inventory and starts a
//! detached `remuda-session-host run-reserved`. The host owns the PTY and
//! outlives the daemon. At startup every daemon rediscovers registered hosts
//! from the inventory and registers checked handles. A host that is gone
//! answers Unavailable instead of being dropped or respawned. The default
//! spawn path stays the in-process PtyAgent; Windows (#214) is not covered.
use super::api::Identity as HostIdentity;
use super::birth::Bootstrap;
use super::entry::Launch;
use super::gate::{Drained, Gate};
use super::inventory::disk::Store;
use super::inventory::model::{Birth, Entry, Lease, Phase, Target};
use super::recovery::Owner;
use super::remote::{Announced, HostSession};
use remuda_core::session_operations::SessionHandle;
use remuda_core::{Registry, Size};
use serde_json::value::RawValue;
use std::collections::HashMap;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Set to "0" to use in-process sessions instead of the default host.
pub const OPT_IN: &str = "REMUDA_EXPERIMENTAL_SESSION_HOST";
/// The daemon's stable input identity, so a restarted daemon recovers acts
/// an earlier one reserved. The writer fence, not this id, excludes owners.
const CLIENT: [u8; 16] = *b"remuda-daemon-01";
const PUBLISH_DEADLINE: Duration = Duration::from_secs(10);
const TEARDOWN_DEADLINE: Duration = Duration::from_secs(10);
/// Left out of every session child's environment, hosted or in-process: the
/// session-host switch and the daemon's systemd/launchd service handles.
pub(crate) const DAEMON_ONLY: &[&str] = &[
    OPT_IN,
    "NOTIFY_SOCKET",
    "LISTEN_FDS",
    "LISTEN_PID",
    "LISTEN_FDNAMES",
    "WATCHDOG_USEC",
    "WATCHDOG_PID",
    "INVOCATION_ID",
    "JOURNAL_STREAM",
    "XPC_SERVICE_NAME",
];

/// The daemon's environment a hosted child starts from. Host-internal names
/// (`REMUDA_SESSION_HOST_*`) and variables that are not UTF-8 are left out:
/// the launch is UTF-8 JSON and refuses the internal prefix.
fn inherited() -> HashMap<String, String> {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| {
            !DAEMON_ONLY.contains(&key.as_str()) && !key.starts_with("REMUDA_SESSION_HOST_")
        })
        .collect()
}

pub fn opted_in() -> bool {
    hosting_enabled(
        cfg!(any(target_os = "linux", target_os = "macos")),
        std::env::var_os(OPT_IN).as_deref(),
    )
}

fn hosting_enabled(supported: bool, value: Option<&std::ffi::OsStr>) -> bool {
    supported && value != Some(std::ffi::OsStr::new("0"))
}

#[cfg(test)]
mod default_hosting_tests {
    use super::hosting_enabled;
    use std::ffi::OsStr;

    #[test]
    fn hosting_defaults_on_only_where_the_host_is_implemented() {
        assert!(hosting_enabled(true, None));
        assert!(hosting_enabled(true, Some(OsStr::new("1"))));
        assert!(!hosting_enabled(true, Some(OsStr::new("0"))));
        assert!(!hosting_enabled(false, None));
        assert!(!hosting_enabled(false, Some(OsStr::new("1"))));
    }
}

/// Private per-daemon state and endpoint roots. In the legacy layout both
/// roots are `<socket>.hosts/`. Opened lazily; nothing exists until used.
pub struct Hosting {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    socket: Option<PathBuf>,
    socket_root: PathBuf,
    hosts: Mutex<Option<Arc<Hosts>>>,
    gate: Arc<Gate>,
    /// Teardowns of closed sessions still running off the request thread.
    retiring: Arc<(Mutex<usize>, Condvar)>,
}
struct Hosts {
    owner: Arc<Owner>,
    root: PathBuf,
    socket_root: PathBuf,
    gate: Arc<Gate>,
    /// Opened handles by name, kept for teardown after the Registry drops one.
    open: Mutex<HashMap<String, Arc<HostSession>>>,
}

fn create_socket_root(path: &Path) -> io::Result<std::fs::File> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    super::unix::directory(path)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
const STATE_OVERRIDE: &str = "REMUDA_SESSION_HOST_STATE_DIR";

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn metadata_if_present(path: &Path) -> io::Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(path) {
        Ok(info) => Ok(Some(info)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn read_symlink(path: &Path) -> io::Result<Option<PathBuf>> {
    match std::fs::read_link(path) {
        Ok(target) => Ok(Some(target)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn managed_default_socket(socket: &Path) -> bool {
    std::env::var_os("REMUDA_RUNTIME_DIR").is_none()
        && (!cfg!(target_os = "macos") || std::env::var_os("XDG_RUNTIME_DIR").is_none())
        && socket.parent() == Some(crate::daemon::runtime_dir().join("remuda").as_path())
}

#[cfg(target_os = "macos")]
fn default_state_base() -> io::Result<PathBuf> {
    let home = std::env::home_dir()
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            io::Error::other("session host state requires an absolute home directory")
        })?;
    Ok(home
        .canonicalize()?
        .join("Library/Application Support/Remuda/session-hosts"))
}

#[cfg(target_os = "linux")]
#[path = "linux_state.rs"]
mod linux_state;

#[cfg(target_os = "linux")]
use linux_state::default_state_base;

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn is_default_state_root(root: &Path) -> bool {
    root.file_name()
        .is_some_and(|name| name.as_encoded_bytes().starts_with(b"default-"))
}

#[cfg(target_os = "macos")]
fn boot_namespace() -> io::Result<String> {
    let mut bytes = [0_u8; 37];
    let mut size = bytes.len();
    // SAFETY: the name is a NUL-terminated string and oldp can hold size bytes.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    boot_namespace_for(&bytes[..size.min(bytes.len())])
}

#[cfg(target_os = "macos")]
fn boot_namespace_for(bytes: &[u8]) -> io::Result<String> {
    let Some((0, uuid)) = bytes.split_last() else {
        return Err(io::Error::other("invalid macOS boot session UUID"));
    };
    if uuid.len() != 36
        || uuid.iter().enumerate().any(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte != b'-'
            } else {
                !byte.is_ascii_hexdigit()
            }
        })
    {
        return Err(io::Error::other("invalid macOS boot session UUID"));
    }
    Ok(format!("boot-{}", String::from_utf8_lossy(uuid)))
}

#[cfg(all(test, target_os = "macos"))]
#[test]
fn boot_namespaces_keep_inventory_paths_distinct() {
    let root = Path::new("/state/default-server");
    let first = boot_namespace_for(b"88289E9F-B575-4D8C-9529-0D704F52D68A\0").unwrap();
    let second = boot_namespace_for(b"88289E9F-B575-4D8C-9529-0D704F52D68B\0").unwrap();
    assert_ne!(
        root.join(first).join("inventory"),
        root.join(second).join("inventory")
    );
    assert!(boot_namespace_for(b"../../other-boot\0").is_err());
}

pub struct NewHosted<'a> {
    pub name: &'a str,
    pub session_id: &'a str,
    pub command: &'a [String],
    pub size: Size,
    pub cwd: Option<&'a str>,
    pub env: &'a HashMap<String, String>,
    /// Launch-time protection: remote input is refused for this instance
    /// before its first remote request, exactly as for an in-process one.
    pub remote_input_disabled: bool,
}

impl Hosting {
    pub fn for_socket(socket: &Path) -> Self {
        let mut name = socket.file_name().unwrap_or_default().to_os_string();
        name.push(".hosts");
        Self {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            socket: Some(socket.to_owned()),
            socket_root: socket.with_file_name(name),
            hosts: Mutex::new(None),
            gate: Arc::default(),
            retiring: Arc::default(),
        }
    }
    /// Hosting over an existing private directory, for tests.
    #[cfg(test)]
    pub(crate) fn at(root: PathBuf) -> Self {
        Self {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            socket: None,
            socket_root: root,
            hosts: Mutex::new(None),
            gate: Arc::default(),
            retiring: Arc::default(),
        }
    }
    pub(crate) fn split_state_root(&self) -> io::Result<Option<PathBuf>> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(socket) = &self.socket {
            return self.unix_split_state_root(socket);
        }
        Ok(None)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn unix_split_state_root(&self, socket: &Path) -> io::Result<Option<PathBuf>> {
        let pin = read_symlink(&self.socket_root.join("state-location"))?;
        let mut lock_name = socket.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock = PathBuf::from(lock_name);
        let lock_metadata = metadata_if_present(&lock)?;
        let alias = match lock_metadata.as_ref() {
            Some(info) if info.is_symlink() => read_symlink(&lock)?,
            _ => None,
        };
        // An old daemon never wrote a split-state pin or a lock alias.
        // Keep its namespace even if a cleaner removed inventory files.
        if pin.is_none()
            && alias.is_none()
            && (lock_metadata.is_some()
                || metadata_if_present(socket)?.is_some()
                || metadata_if_present(&self.socket_root)?.is_some())
        {
            if std::env::var_os(STATE_OVERRIDE).is_some() {
                return Err(io::Error::other(
                    "separate session host state requires a fresh daemon namespace",
                ));
            }
            return Ok(None);
        }
        let override_dir = std::env::var_os(STATE_OVERRIDE);
        let base = if let Some(base) = &override_dir {
            let base = Path::new(base);
            super::unix::directory(base)?;
            base.canonicalize()?
        } else if pin.is_some() || alias.is_some() || managed_default_socket(socket) {
            default_state_base()?
        } else {
            return Ok(None);
        };
        if metadata_if_present(&self.socket_root.join("inventory"))?.is_some() {
            return Err(io::Error::other(
                "legacy and protected host state both exist",
            ));
        }

        let parent = self.socket_root.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "missing socket directory")
        })?;
        let name = self
            .socket_root
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing socket name"))?;
        let identity = parent.canonicalize()?.join(name);
        let namespace = hex(&super::inventory::model::hash(
            identity.as_os_str().as_encoded_bytes(),
        ));
        let namespace = if override_dir.is_some() {
            namespace
        } else {
            format!("default-{namespace}")
        };
        let requested = base.join(namespace);
        if pin.as_ref().is_some_and(|path| *path != requested)
            || alias
                .as_ref()
                .is_some_and(|path| *path != requested.join("daemon.lock"))
        {
            return Err(io::Error::other(
                "session host state directory changed; restore the original setting",
            ));
        }
        if pin.is_some() {
            super::unix::directory(&requested)?;
        }
        Ok(Some(requested))
    }

    #[cfg(target_os = "linux")]
    fn state_root(&self) -> io::Result<PathBuf> {
        match self.split_state_root()? {
            Some(root) => linux_state::boot_state_root(&root, false),
            None => Ok(self.socket_root.clone()),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn state_root(&self) -> io::Result<PathBuf> {
        if let Some(root) = self.split_state_root()? {
            #[cfg(target_os = "macos")]
            if is_default_state_root(&root) {
                return Ok(root.join(boot_namespace()?));
            }
            return Ok(root);
        }
        Ok(self.socket_root.clone())
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn pin_split_state(&self, root: &Path) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        linux_state::boot_state_root(root, true)?;
        let socket_root = create_socket_root(&self.socket_root)?;
        let pin = self.socket_root.join("state-location");
        match std::os::unix::fs::symlink(root, &pin) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
        if std::fs::read_link(&pin)? != root {
            return Err(io::Error::other("session host state directory changed"));
        }
        // Persist the namespace before publishing its location.
        if let Some(parent) = root.parent() {
            super::unix::directory(parent)?.sync_all()?;
        }
        socket_root.sync_all()
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn prepare_split_state(root: &Path) -> io::Result<()> {
        if is_default_state_root(root) {
            let base = default_state_base()?;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(&base)?;
            super::unix::directory(&base)?.sync_all()?;
            #[cfg(target_os = "macos")]
            boot_namespace()?;
        }
        create_socket_root(root)?.sync_all()?;
        #[cfg(target_os = "linux")]
        linux_state::boot_state_root(root, false)?;
        Ok(())
    }

    fn hosts(&self, create: bool) -> io::Result<Option<Arc<Hosts>>> {
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hosts) = hosts.as_ref() {
            return Ok(Some(hosts.clone()));
        }
        let root = self.state_root()?;
        let inventory = root.join("inventory");
        let store = if inventory.exists() {
            Store::open(&inventory)
        } else if create {
            match std::fs::DirBuilder::new().mode(0o700).create(&root) {
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
                _ => {}
            }
            Store::create(&inventory)
        } else {
            return Ok(None);
        }
        .map_err(|error| io::Error::other(format!("session host inventory: {error:?}")))?;
        // One writer claim per daemon process: older owners are fenced from here.
        let owner = Owner::claim(store)
            .map_err(|error| io::Error::other(format!("session host claim: {error:?}")))?;
        let opened = Arc::new(Hosts {
            owner: Arc::new(owner),
            root,
            socket_root: self.socket_root.clone(),
            gate: Arc::clone(&self.gate),
            open: Mutex::new(HashMap::new()),
        });
        *hosts = Some(opened.clone());
        Ok(Some(opened))
    }

    /// Register every published host from an existing inventory. Exited ones
    /// are torn down instead; unpublished reservations are skipped. Returns
    /// the names registered.
    pub fn discover(&self, registry: &Registry) -> io::Result<Vec<String>> {
        let Some(hosts) = self.hosts(false)? else {
            return Ok(Vec::new());
        };
        let index = hosts
            .owner
            .index()
            .map_err(|error| io::Error::other(format!("session host inventory: {error:?}")))?;
        let mut names = Vec::new();
        for (name, entry) in &index.entries {
            let Ok(session) = hosts.open(name) else {
                continue;
            };
            if matches!(entry.phase, Phase::Exited { .. })
                || entry.metadata.contains_key(super::recovery::CLOSING)
            {
                let hosts = hosts.clone();
                let name = name.clone();
                std::thread::spawn(move || hosts.teardown(&name, session));
                continue;
            }
            let Ok(handle) = SessionHandle::hosted(session) else {
                continue;
            };
            // The inventory may change between the discovery snapshot and
            // opening the host. Never apply an older instance's policy to a
            // newly reused session name.
            let identity = handle.identity();
            if identity.host != Some(entry.identity.host)
                || identity.id != entry.identity.session
                || identity.instance != entry.identity.instance
            {
                continue;
            }
            let disabled = super::recovery::remote_input_disabled(entry);
            if registry
                .register_hosted_with_remote_input_disabled(handle, disabled)
                .is_ok()
            {
                names.push(name.clone());
            }
        }
        Ok(names)
    }

    /// Start one host-backed session and register its checked handle.
    pub fn spawn(&self, registry: &Registry, request: &NewHosted<'_>) -> Result<(), String> {
        let hosts = self
            .hosts(true)
            .map_err(|error| error.to_string())?
            .ok_or("session host inventory unavailable")?;
        let session = hosts.spawn(request)?;
        let handle = SessionHandle::hosted(session.clone()).map_err(|error| error.to_string())?;
        registry
            .register_hosted_with_remote_input_disabled(handle, request.remote_input_disabled)
            .map(|_| ())
            .map_err(|_| format!("name taken: {}", request.name))
    }

    /// Claim the inventory again under a newer epoch, then show every open
    /// host the new writer, fencing any daemon that claimed since. Returns
    /// each host's answer; none before this daemon opened its inventory.
    pub fn reclaim(&self) -> io::Result<Vec<(String, Announced)>> {
        let hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(hosts) = hosts else {
            return Ok(Vec::new());
        };
        hosts
            .owner
            .reclaim()
            .map_err(|error| io::Error::other(format!("session host reclaim: {error:?}")))?;
        let open: Vec<_> = hosts
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(name, session)| (name.clone(), session.clone()))
            .collect();
        let mut announced: Vec<_> = open
            .into_iter()
            .map(|(name, session)| (name, session.announce_writer()))
            .collect();
        announced.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(announced)
    }

    /// Read-only check of every registered host, for a daemon that must not
    /// claim yet (a reload candidate): no fence moves and no lease is taken.
    pub fn preflight(&self) -> io::Result<Vec<Checked>> {
        let inventory = self.state_root()?.join("inventory");
        if !inventory.exists() {
            return Ok(Vec::new());
        }
        let index = Store::open(&inventory)
            .and_then(|store| store.read())
            .map_err(|error| io::Error::other(format!("session host inventory: {error:?}")))?;
        Ok(index
            .entries
            .iter()
            .map(|(name, entry)| Checked {
                name: name.clone(),
                state: checked(entry),
            })
            .collect())
    }

    /// Stop admitting host-backed work, then wait up to `deadline` for acts,
    /// control calls and host starts already admitted. Hosts keep running.
    pub fn drain(&self, deadline: Duration) -> Drained {
        self.gate.drain(deadline)
    }

    /// For `stop`: close every host-backed session, then wait up to
    /// `deadline` for each host to stop. Returns how many were closed and the
    /// names still closing at the deadline (the next daemon finishes those).
    pub fn close_all(&self, registry: &Registry, deadline: Duration) -> (usize, Vec<String>) {
        let Ok(Some(hosts)) = self.hosts(false) else {
            return (0, Vec::new());
        };
        let open: Vec<_> = hosts
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(name, session)| (name.clone(), session.clone()))
            .collect();
        // A human attached, or detaching a moment ago, makes the host refuse
        // Close; stop ends that session anyway, as it ends an in-process one.
        let closed = open
            .iter()
            .filter(|(name, session)| match registry.close(name) {
                Some(Ok(true)) => true,
                // Retry even when nothing was revoked: a detach may have
                // released the grant between the refusal and the revoke.
                Some(Err(_)) if session.revoke_human().is_ok() => {
                    matches!(registry.close(name), Some(Ok(true)))
                }
                _ => false,
            })
            .count();
        let waits: Vec<_> = open
            .into_iter()
            .map(|(name, session)| {
                std::thread::spawn(move || session.teardown(deadline).err().map(|_| name))
            })
            .collect();
        let left = waits
            .into_iter()
            .filter_map(|wait| wait.join().ok().flatten())
            .collect();
        (closed, left)
    }

    /// Admit host-backed work again after a drain, for a reload rolled back.
    pub fn reopen(&self) {
        self.gate.reopen();
    }

    /// After the Registry closed `name`: stop its host and retire its entry,
    /// off the request thread. Unknown names are ignored.
    pub fn retire_closed(&self, name: &str) {
        let Ok(Some(hosts)) = self.hosts(false) else {
            return;
        };
        let session = hosts
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned();
        if let Some(session) = session {
            let name = name.to_owned();
            let retiring = Arc::clone(&self.retiring);
            *retiring.0.lock().unwrap_or_else(|e| e.into_inner()) += 1;
            std::thread::spawn(move || {
                hosts.teardown(&name, session);
                let (count, done) = &*retiring;
                *count.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
                done.notify_all();
            });
        }
    }

    /// Before a daemon exits on a signal: wait up to `deadline` for the hosts
    /// of sessions already closed to stop, so none outlives its close. Hosts
    /// of live sessions are not touched. Returns how many were still stopping.
    pub fn finish_retiring(&self, deadline: Duration) -> usize {
        let (count, done) = &*self.retiring;
        let count = count.lock().unwrap_or_else(|e| e.into_inner());
        let (count, _) = done
            .wait_timeout_while(count, deadline, |count| *count > 0)
            .unwrap_or_else(|e| e.into_inner());
        *count
    }
}

/// One inventory entry as a preflight saw it.
#[derive(Debug, PartialEq, Eq)]
pub struct Checked {
    pub name: String,
    pub state: CheckedState,
}
#[derive(Debug, PartialEq, Eq)]
pub enum CheckedState {
    /// The host answered Inspect for the expected identity.
    Running { build: String },
    /// Not published yet, exited, or closing: nothing to hand over.
    Settling,
    /// Published, but its host did not answer as expected.
    Unreachable(String),
}
fn checked(entry: &Entry) -> CheckedState {
    if entry.phase != Phase::Registered || entry.metadata.contains_key(super::recovery::CLOSING) {
        return CheckedState::Settling;
    }
    let Some(birth) = entry.birth.as_ref() else {
        return CheckedState::Unreachable("no endpoint recorded".into());
    };
    let inspected =
        super::entry::Client::open_observer(Path::new(birth.endpoint()), entry.identity.clone())
            .and_then(|mut client| client.request(&super::api::Request::Inspect));
    match inspected {
        Ok(super::api::Response::Info { info }) if info.identity == entry.identity => {
            CheckedState::Running {
                build: info.host_build,
            }
        }
        Ok(super::api::Response::Error { code }) => {
            CheckedState::Unreachable(format!("host refused inspect: {code}"))
        }
        Ok(other) => CheckedState::Unreachable(format!("unexpected reply {}", reply_kind(&other))),
        Err(error) => CheckedState::Unreachable(error.to_string()),
    }
}
fn reply_kind(response: &super::api::Response) -> &'static str {
    match response {
        super::api::Response::Info { .. } => "info for another identity",
        super::api::Response::Error { .. } => "error",
        _ => "other",
    }
}

impl Hosts {
    fn open(&self, name: &str) -> Result<Arc<HostSession>, String> {
        let session = Arc::new(
            HostSession::open(self.owner.clone(), name, CLIENT)
                .map_err(|e| e.to_string())?
                .gated(Arc::clone(&self.gate)),
        );
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_owned(), session.clone());
        Ok(session)
    }
    fn teardown(&self, name: &str, session: Arc<HostSession>) {
        if session.teardown(TEARDOWN_DEADLINE).is_ok() {
            let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
            if open.get(name).is_some_and(|s| Arc::ptr_eq(s, &session)) {
                open.remove(name);
            }
        }
    }
    fn spawn(self: &Arc<Self>, request: &NewHosted<'_>) -> Result<Arc<HostSession>, String> {
        let _pass = self.gate.enter().map_err(|error| error.to_string())?;
        let launch = launch(request)?;
        // Checked before the durable reservation, which a missing binary
        // would otherwise leave behind under this name.
        let exe = host_executable()?;
        // A name the registry no longer holds can still have an entry while
        // its closed host is torn down. Wait for that, as a close of an
        // in-process session frees its name at once.
        if !self.wait_released(request.name)? {
            return Err(format!(
                "session name {} is still held by a closing host-backed session",
                request.name
            ));
        }
        let raw = serde_json::to_string(&launch).map_err(|e| e.to_string())?;
        let identity = HostIdentity {
            host: random()?,
            session: request.session_id.to_owned(),
            instance: hex(&random::<16>()?),
        };
        let launch_id = random()?;
        let nonce = random()?;
        let key = hex(&random::<8>()?);
        let endpoint = self.root.join(&key);
        let socket = self.socket_root.join(key).join("socket");
        socket_fits(&socket)?;
        let endpoint_text = endpoint
            .to_str()
            .ok_or("session host path is not UTF-8")?
            .to_owned();
        let mut entry = Entry::reserved(identity.clone(), launch_id, &raw)
            .map_err(|error| format!("session host launch: {error:?}"))?;
        set_launch_remote_policy(&mut entry, request.remote_input_disabled)?;
        entry.birth = Some(
            Birth::new(endpoint_text, nonce)
                .map_err(|error| format!("session host birth: {error:?}"))?,
        );
        let store = self
            .owner
            .index()
            .map_err(|error| format!("session host inventory: {error:?}"))?
            .cursor()
            .store;
        // Durable reservation precedes the spawn; a crash leaves only a record.
        self.owner
            .reserve_launch(request.name.to_owned(), entry)
            .map_err(|error| format!("session host reservation: {error:?}"))?;
        let target = Target {
            name: request.name.to_owned(),
            identity,
        };
        let bootstrap = serde_json::to_vec(&Bootstrap {
            store: self.root.join("inventory"),
            socket: (self.root != self.socket_root).then_some(socket),
            lease: Lease::new(
                store,
                Target {
                    name: target.name.clone(),
                    identity: target.identity.clone(),
                },
                launch_id,
                nonce,
            ),
            launch: RawValue::from_string(raw).map_err(|e| e.to_string())?,
        })
        .map_err(|e| e.to_string())?;
        let exit = match start(&exe, &endpoint, &bootstrap) {
            Ok(exit) => exit,
            Err(StartError::NotSpawned(error)) => {
                return match self.owner.cancel_unspawned(target, launch_id) {
                    Ok(()) => Err(error),
                    Err(cancel) => Err(format!(
                        "{error}; could not confirm reservation cleanup: {cancel:?}"
                    )),
                };
            }
            Err(StartError::Ambiguous(error)) => return Err(error),
        };
        match self.published(request.name, &exit) {
            Ok(()) => self.open(request.name),
            // The child already ended. Like an in-process `new` of a short
            // command, the session still starts: it is registered, its last
            // screen can be read, and the reaper announces the exit and
            // retires its host.
            Err(Published::Exited) => self.open(request.name),
            Err(Published::Late) => Err(
                "session host did not publish in time; it may still start, and a daemon \
                 restart will rediscover it"
                    .into(),
            ),
            Err(Published::Store(error)) => Err(error),
            Err(Published::HostEnded(status)) if status.code() == Some(super::BUILD_MISMATCH) => {
                Err(format!(
                    "session host {} is not this daemon's build {}: the installed remuda \
                     changed while it started; restart the daemon to use it",
                    exe.display(),
                    crate::dist::BUILD_VERSION
                ))
            }
            Err(Published::HostEnded(status)) => Err(format!(
                "session host {} ended before it published ({status})",
                exe.display()
            )),
        }
    }
    /// True once `name` has no inventory entry, waiting up to the teardown
    /// deadline for a closing host to retire it.
    fn wait_released(&self, name: &str) -> Result<bool, String> {
        let until = Instant::now() + TEARDOWN_DEADLINE;
        loop {
            let held = self
                .owner
                .index()
                .map_err(|error| format!("session host inventory: {error:?}"))?
                .entries
                .contains_key(name);
            if !held {
                return Ok(true);
            }
            if Instant::now() >= until {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn published(&self, name: &str, exit: &mpsc::Receiver<ExitStatus>) -> Result<(), Published> {
        let until = Instant::now() + PUBLISH_DEADLINE;
        let mut ended = None;
        loop {
            let index = self
                .owner
                .index()
                .map_err(|error| Published::Store(format!("session host inventory: {error:?}")))?;
            match index.entries.get(name) {
                // The host records Registered just before it starts taking
                // input; wait for that too, so an attach or input sent right
                // after `new` is not refused as still starting.
                Some(entry) if entry.phase == Phase::Registered && accepting(entry) => {
                    return Ok(())
                }
                Some(entry) if matches!(entry.phase, Phase::Exited { .. }) => {
                    return Err(Published::Exited)
                }
                _ => {}
            }
            // Checked after reading the index, so a host that published and
            // then ended is not taken for one that never published.
            if let Some(status) = ended {
                return Err(Published::HostEnded(status));
            }
            if Instant::now() >= until {
                return Err(Published::Late);
            }
            match exit.recv_timeout(Duration::from_millis(20)) {
                Ok(status) => ended = Some(status),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                // The reaper could not wait for it: keep polling the index.
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
}
/// True once a registered host takes input. A host that cannot be asked
/// counts as accepting: the request that follows reports what is wrong.
fn set_launch_remote_policy(entry: &mut Entry, disabled: bool) -> Result<(), String> {
    let value = super::inventory::model::Namespace::new(1, if disabled { "true" } else { "false" })
        .map_err(|error| format!("session host policy: {error:?}"))?;
    entry
        .metadata
        .insert(super::recovery::REMOTE_INPUT_POLICY.into(), value);
    Ok(())
}

fn accepting(entry: &Entry) -> bool {
    use super::api::{PublicationStatus, Request, Response};
    let Some(birth) = entry.birth.as_ref() else {
        return true;
    };
    let inspected =
        super::entry::Client::open_observer(Path::new(birth.endpoint()), entry.identity.clone())
            .and_then(|mut client| client.request(&Request::Inspect));
    match inspected {
        Ok(Response::Info { info }) => !matches!(
            info.publication,
            Some(PublicationStatus::Starting | PublicationStatus::Unavailable)
        ),
        _ => true,
    }
}
enum Published {
    Exited,
    /// The host may still publish: an unknown outcome, not a clean failure.
    Late,
    Store(String),
    /// The host process ended without publishing; nothing of it is left running.
    HostEnded(ExitStatus),
}

fn launch(request: &NewHosted<'_>) -> Result<Launch, String> {
    let mut argv = request.command.to_vec();
    if argv.is_empty() {
        argv.push(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()));
    }
    let cwd = match request.cwd {
        Some(cwd) => cwd.to_owned(),
        None => std::env::current_dir()
            .map_err(|e| e.to_string())?
            .to_str()
            .ok_or("working directory is not UTF-8")?
            .to_owned(),
    };
    let mut env = inherited();
    if env.get("TERM").is_none_or(|term| term == "dumb") {
        env.insert("TERM".into(), "xterm-256color".into());
    }
    env.extend(request.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    let mut env: Vec<_> = env.into_iter().collect();
    env.sort();
    let agent: String = remuda_core::registry::slug(&argv[0])
        .chars()
        .take(32)
        .collect();
    let launch = Launch {
        name: request.name.to_owned(),
        argv,
        cwd,
        agent,
        rows: request.size.rows(),
        cols: request.size.cols(),
        env,
    };
    super::entry::validate(&launch).map_err(|error| format!("session host launch: {error}"))?;
    Ok(launch)
}

/// The host of this daemon's own build, beside the daemon's executable.
fn host_executable() -> Result<PathBuf, String> {
    let daemon = std::env::current_exe().map_err(|e| e.to_string())?;
    let daemon = super::host_build::profile_daemon(&daemon).unwrap_or(daemon);
    super::host_build::choose(
        &daemon,
        crate::dist::BUILD_VERSION,
        super::host_build::probe,
    )
}
/// Start the host. The receiver gets its exit status if it ends; a host that
/// ends before it publishes failed to start.
#[derive(Debug)]
enum StartError {
    NotSpawned(String),
    Ambiguous(String),
}

fn start(
    exe: &Path,
    endpoint: &Path,
    bootstrap: &[u8],
) -> Result<mpsc::Receiver<ExitStatus>, StartError> {
    let mut child = Command::new(exe)
        .arg("run-reserved")
        .arg(endpoint)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env(super::EXPECTED_BUILD, crate::dist::BUILD_VERSION)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| StartError::NotSpawned(format!("start {}: {error}", exe.display())))?;
    let written = child
        .stdin
        .take()
        .ok_or_else(|| Unwritten {
            text: "session host stdin".into(),
            broken_pipe: false,
        })
        .and_then(|mut stdin| {
            super::wire::write_bootstrap(&mut stdin, bootstrap).map_err(|e| Unwritten {
                broken_pipe: e.kind() == io::ErrorKind::BrokenPipe,
                text: e.to_string(),
            })
        });
    // The host calls setsid; this thread only reaps it if it exits first.
    let (ended, exit) = mpsc::channel();
    std::thread::spawn(move || {
        if let Ok(status) = child.wait() {
            let _ = ended.send(status);
        }
    });
    match written {
        Ok(()) => Ok(exit),
        // A host that exits before reading its bootstrap (another build) is
        // reported by its exit status, not by this lost write.
        Err(error) if error.broken_pipe => Ok(exit),
        Err(error) => Err(StartError::Ambiguous(error.text)),
    }
}
struct Unwritten {
    text: String,
    broken_pipe: bool,
}

fn random<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|error| format!("entropy: {error}"))?;
    Ok(bytes)
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod start_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_host_that_exits_before_its_bootstrap_is_reported_by_its_status() {
        let dir = std::env::temp_dir().join(format!("rhst-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("host");
        std::fs::write(&exe, "#!/bin/sh\nexit 3\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        // With its length prefix, more than a pipe holds: the write cannot
        // finish before the host is gone.
        let bootstrap = vec![b' '; super::super::wire::MAX_FRAME];
        let exit = start(&exe, &dir.join("endpoint"), &bootstrap).expect("exit receiver");
        let status = exit.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(status.code(), Some(super::super::BUILD_MISMATCH));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A host binds its socket 23 bytes deeper than its daemon's, so a runtime
/// directory the daemon fits in can still be too long for the host. Said
/// before anything is reserved, instead of the host exiting unexplained.
fn socket_fits(socket: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let len = socket.as_os_str().as_bytes().len();
    let limit = crate::staged_socket::SOCKET_PATH_LIMIT;
    if len > limit {
        return Err(format!(
            "session host socket path too long ({len} bytes, limit {limit}): {}\nNext: set a shorter REMUDA_RUNTIME_DIR",
            socket.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod socket_path_tests {
    #[test]
    fn a_host_socket_too_long_for_the_kernel_is_refused_with_the_remedy() {
        let limit = crate::staged_socket::SOCKET_PATH_LIMIT;
        let fits = format!("/{}", "a".repeat(limit - 1));
        assert!(super::socket_fits(std::path::Path::new(&fits)).is_ok());
        let long = format!("/{}", "a".repeat(limit));
        let error = super::socket_fits(std::path::Path::new(&long)).unwrap_err();
        assert!(error.contains("socket path too long"), "{error}");
        assert!(error.contains("REMUDA_RUNTIME_DIR"), "{error}");
    }
}
