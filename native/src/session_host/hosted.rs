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
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Daemon environment switch; anything but "1" keeps the default path.
pub const OPT_IN: &str = "REMUDA_EXPERIMENTAL_SESSION_HOST";
/// The daemon's stable input identity, so a restarted daemon recovers acts
/// an earlier one reserved. The writer fence, not this id, excludes owners.
const CLIENT: [u8; 16] = *b"remuda-daemon-01";
const PUBLISH_DEADLINE: Duration = Duration::from_secs(10);
const TEARDOWN_DEADLINE: Duration = Duration::from_secs(10);
/// Inherited from the daemon when set; the host clears everything else.
const INHERITED: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "TERM",
    "COLORTERM",
    "XDG_RUNTIME_DIR",
    "SSH_AUTH_SOCK",
];

pub fn opted_in() -> bool {
    std::env::var(OPT_IN).is_ok_and(|value| value == "1")
}

/// Private per-daemon state: `<socket>.hosts/` holds the inventory and one
/// directory per host endpoint. Opened lazily; nothing exists until used.
pub struct Hosting {
    root: PathBuf,
    hosts: Mutex<Option<Arc<Hosts>>>,
    gate: Arc<Gate>,
}
struct Hosts {
    owner: Arc<Owner>,
    root: PathBuf,
    gate: Arc<Gate>,
    /// Opened handles by name, kept for teardown after the Registry drops one.
    open: Mutex<HashMap<String, Arc<HostSession>>>,
}
pub struct NewHosted<'a> {
    pub name: &'a str,
    pub session_id: &'a str,
    pub command: &'a [String],
    pub size: Size,
    pub cwd: Option<&'a str>,
    pub env: &'a HashMap<String, String>,
}

impl Hosting {
    pub fn for_socket(socket: &Path) -> Self {
        let mut name = socket.file_name().unwrap_or_default().to_os_string();
        name.push(".hosts");
        Self {
            root: socket.with_file_name(name),
            hosts: Mutex::new(None),
            gate: Arc::default(),
        }
    }
    /// Hosting over an existing private directory, for tests.
    #[cfg(test)]
    pub(crate) fn at(root: PathBuf) -> Self {
        Self {
            root,
            hosts: Mutex::new(None),
            gate: Arc::default(),
        }
    }
    fn hosts(&self, create: bool) -> io::Result<Option<Arc<Hosts>>> {
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hosts) = hosts.as_ref() {
            return Ok(Some(hosts.clone()));
        }
        let inventory = self.root.join("inventory");
        let store = if inventory.exists() {
            Store::open(&inventory)
        } else if create {
            match std::fs::DirBuilder::new().mode(0o700).create(&self.root) {
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
            root: self.root.clone(),
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
            if registry.register_hosted(handle).is_ok() {
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
            .register_hosted(handle)
            .map(|_| ())
            .map_err(|_| format!("session name already in use: {}", request.name))
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
        let inventory = self.root.join("inventory");
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
            std::thread::spawn(move || hosts.teardown(&name, session));
        }
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
        if self
            .owner
            .index()
            .map_err(|error| format!("session host inventory: {error:?}"))?
            .entries
            .contains_key(request.name)
        {
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
        let endpoint = self.root.join(hex(&random::<8>()?));
        let endpoint_text = endpoint
            .to_str()
            .ok_or("session host path is not UTF-8")?
            .to_owned();
        let mut entry = Entry::reserved(identity.clone(), launch_id, &raw)
            .map_err(|error| format!("session host launch: {error:?}"))?;
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
            lease: Lease::new(store, target, launch_id, nonce),
            launch: RawValue::from_string(raw).map_err(|e| e.to_string())?,
        })
        .map_err(|e| e.to_string())?;
        let exit = start(&exe, &endpoint, &bootstrap)?;
        match self.published(request.name, &exit) {
            Ok(()) => self.open(request.name),
            Err(Published::Exited) => {
                // Stop the host and retire the entry, as for a closed session.
                if let Ok(session) = self.open(request.name) {
                    let name = request.name.to_owned();
                    let hosts = self.clone();
                    std::thread::spawn(move || hosts.teardown(&name, session));
                }
                Err("session host child exited at once".into())
            }
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
    fn published(&self, name: &str, exit: &mpsc::Receiver<ExitStatus>) -> Result<(), Published> {
        let until = Instant::now() + PUBLISH_DEADLINE;
        let mut ended = None;
        loop {
            let index = self
                .owner
                .index()
                .map_err(|error| Published::Store(format!("session host inventory: {error:?}")))?;
            match index.entries.get(name).map(|entry| entry.phase) {
                Some(Phase::Registered) => return Ok(()),
                Some(Phase::Exited { .. }) => return Err(Published::Exited),
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
    let mut env: HashMap<String, String> = INHERITED
        .iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| ((*key).into(), value)))
        .collect();
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
    super::host_build::choose(
        &daemon,
        crate::dist::BUILD_VERSION,
        super::host_build::probe,
    )
}
/// Start the host. The receiver gets its exit status if it ends; a host that
/// ends before it publishes failed to start.
fn start(
    exe: &Path,
    endpoint: &Path,
    bootstrap: &[u8],
) -> Result<mpsc::Receiver<ExitStatus>, String> {
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
        .map_err(|error| format!("start {}: {error}", exe.display()))?;
    let written = child
        .stdin
        .take()
        .ok_or_else(|| Unwritten {
            text: "session host stdin".into(),
            broken_pipe: false,
        })
        .and_then(|mut stdin| {
            super::wire::write_frame(&mut stdin, bootstrap).map_err(|e| Unwritten {
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
        Err(error) => Err(error.text),
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
