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
use super::inventory::disk::Store;
use super::inventory::model::{Birth, Entry, Lease, Phase, Target};
use super::recovery::Owner;
use super::remote::HostSession;
use remuda_core::session_operations::SessionHandle;
use remuda_core::{Registry, Size};
use serde_json::value::RawValue;
use std::collections::HashMap;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
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
}
struct Hosts {
    owner: Arc<Owner>,
    root: PathBuf,
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

impl Hosts {
    fn open(&self, name: &str) -> Result<Arc<HostSession>, String> {
        let session = Arc::new(
            HostSession::open(self.owner.clone(), name, CLIENT).map_err(|e| e.to_string())?,
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
        start(&exe, &endpoint, &bootstrap)?;
        match self.published(request.name) {
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
        }
    }
    fn published(&self, name: &str) -> Result<(), Published> {
        let until = Instant::now() + PUBLISH_DEADLINE;
        loop {
            let index = self
                .owner
                .index()
                .map_err(|error| Published::Store(format!("session host inventory: {error:?}")))?;
            match index.entries.get(name).map(|entry| entry.phase) {
                Some(Phase::Registered) => return Ok(()),
                Some(Phase::Exited { .. }) => return Err(Published::Exited),
                _ if Instant::now() >= until => return Err(Published::Late),
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}
enum Published {
    Exited,
    /// The host may still publish: an unknown outcome, not a clean failure.
    Late,
    Store(String),
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

/// The host binary ships beside the daemon's own executable.
fn host_executable() -> Result<PathBuf, String> {
    let exe = host_beside(&std::env::current_exe().map_err(|e| e.to_string())?);
    if !exe.is_file() {
        return Err(format!("session host binary not found: {}", exe.display()));
    }
    Ok(exe)
}
fn host_beside(daemon: &Path) -> PathBuf {
    daemon.with_file_name(format!(
        "remuda-session-host{}",
        std::env::consts::EXE_SUFFIX
    ))
}
fn start(exe: &Path, endpoint: &Path, bootstrap: &[u8]) -> Result<(), String> {
    let mut child = Command::new(exe)
        .arg("run-reserved")
        .arg(endpoint)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("start {}: {error}", exe.display()))?;
    let written = child
        .stdin
        .take()
        .ok_or_else(|| "session host stdin".to_owned())
        .and_then(|mut stdin| {
            super::wire::write_frame(&mut stdin, bootstrap).map_err(|e| e.to_string())
        });
    // The host calls setsid; this thread only reaps it if it exits first.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    written
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
mod tests {
    use super::*;

    #[test]
    fn host_binary_is_looked_up_beside_the_daemon_with_the_platform_suffix() {
        let daemon = Path::new("/opt/bin").join(format!("remuda{}", std::env::consts::EXE_SUFFIX));
        let want = format!("remuda-session-host{}", std::env::consts::EXE_SUFFIX);
        assert_eq!(host_beside(&daemon), Path::new("/opt/bin").join(want));
    }
}
