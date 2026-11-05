use base64::Engine as _;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use remuda_native::net::http_client::{perform, HttpRequest};

use super::private_dir::{CreatedFile, PrivateDir};

const SCHEMA: u64 = 1;
const PROTOCOL: &str = "opencode-http-v1";
const START_TIMEOUT: Duration = Duration::from_secs(10);
const WATCHER_ACK_TIMEOUT: Duration = Duration::from_secs(2);
/// OpenCode resolves `OPENCODE_PASSWORD` before `OPENCODE_SERVER_PASSWORD`
/// (packages/cli/src/env.ts), so an inherited primary password would win over
/// the generated one. Both children get the generated value for both names.
const AUTH_ENV_KEYS: [&str; 2] = ["OPENCODE_PASSWORD", "OPENCODE_SERVER_PASSWORD"];

fn configure_auth(command: &mut Command, password: &str) {
    for key in AUTH_ENV_KEYS {
        command.env(key, password);
    }
}

fn cli_spec() -> remuda_native::cli_parse::Spec {
    use remuda_native::cli_parse::{OptionSpec, Spec, VerbSpec};
    Spec {
        name: "remuda".into(),
        options: vec![],
        verbs: vec![VerbSpec {
            name: "_opencode_tui".into(),
            about:
                "Run an owned OpenCode server and the original TUI on one fixed session (Unix only)"
                    .into(),
            usage: None,
            args: vec![],
            next: "remuda _opencode_tui --binding PATH [--cwd DIR] [--status PATH]".into(),
            options: vec![
                OptionSpec {
                    long: "binding".into(),
                    short: None,
                    value: Some("PATH".into()),
                    help: "Private per-launch binding manifest".into(),
                    global: false,
                    required: true,
                    repeat_policy: Default::default(),
                },
                OptionSpec {
                    long: "cwd".into(),
                    short: None,
                    value: Some("DIR".into()),
                    help: "Session directory".into(),
                    global: false,
                    required: false,
                    repeat_policy: Default::default(),
                },
                OptionSpec {
                    long: "status".into(),
                    short: None,
                    value: Some("PATH".into()),
                    help: "Optional status file".into(),
                    global: false,
                    required: false,
                    repeat_policy: Default::default(),
                },
                OptionSpec {
                    long: "capability".into(),
                    short: None,
                    value: None,
                    help: "Print native-notice capability JSON and exit".into(),
                    global: false,
                    required: false,
                    repeat_policy: Default::default(),
                },
            ],
        }],
    }
}

fn parse_args(args: &[&str]) -> Result<(PathBuf, PathBuf, Option<PathBuf>), ExitCode> {
    let mut words = vec!["_opencode_tui"];
    words.extend(args.iter().copied());
    let report = remuda_native::cli_parse::parse(&cli_spec(), &words);
    if report.kind.as_deref() == Some("help") {
        println!("{}", report.text);
        return Err(ExitCode::SUCCESS);
    }
    if !report.ok {
        eprintln!(
            "{}",
            remuda_native::cli_parse::parse(&cli_spec(), &["_opencode_tui", "--help"]).text
        );
        return Err(ExitCode::FAILURE);
    }
    let binding = report
        .values
        .get("binding")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let cwd = report
        .values
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let status = report
        .values
        .get("status")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    match binding {
        Some(binding) => Ok((binding, cwd, status)),
        None => Err(ExitCode::FAILURE),
    }
}

const UNSUPPORTED: &str = "OpenCode native binding requires macOS or Linux";

/// The wrapper can only claim a native binding where it can read a process's
/// start time and parent: libproc on macOS, `/proc` on Linux. Any other Unix
/// (or Windows) advertises false and keeps the unbound PTY.
pub(crate) fn platform_supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
}

fn capability() -> Value {
    json!({
        "native_notice": platform_supported(),
        "protocol": PROTOCOL,
        "platform": std::env::consts::OS,
    })
}

pub fn run(args: &[&str]) -> ExitCode {
    if args.contains(&"--capability") {
        println!("{}", capability());
        return ExitCode::SUCCESS;
    }
    if !platform_supported() {
        eprintln!("remuda: OpenCode TUI: {UNSUPPORTED}");
        return ExitCode::FAILURE;
    }
    let (binding, cwd, status) = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    match launch(binding, cwd, status) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("remuda: OpenCode TUI: {error}");
            ExitCode::FAILURE
        }
    }
}

struct Cleanup {
    server: Option<Child>,
    watcher: Option<Child>,
    manifest: Option<CreatedFile>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        stop_child_group(self.server.take());
        if let Some(mut watcher) = self.watcher.take() {
            let _ = watcher.kill();
            let _ = watcher.wait();
        }
        // Only the inode this invocation created is removed; a preexisting or
        // replacement file is left alone.
        if let Some(manifest) = self.manifest.take() {
            manifest.remove();
        }
    }
}

fn launch(binding: PathBuf, cwd: PathBuf, status: Option<PathBuf>) -> Result<ExitCode, String> {
    let nonce = env_value("REMUDA_BUTLER_BINDING_NONCE")?;
    let session = env_value("REMUDA_SESSION_ID")?;
    let cwd = cwd
        .canonicalize()
        .map_err(|error| format!("session directory: {error}"))?;
    let parent = binding
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| "binding path has no parent directory".to_string())?;
    let name = binding
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "binding path needs a UTF-8 file name".to_string())?;
    // One pinned, checked directory descriptor owns every later create, read
    // and removal of the manifest.
    let directory = PrivateDir::open(parent)?;
    let password = random_hex(24)?;
    let mut cleanup = Cleanup {
        server: None,
        watcher: None,
        manifest: None,
    };
    cleanup.server = Some(spawn_server(&password)?);
    let server_pid = cleanup.server.as_ref().expect("server just stored").id();
    // Fence the owned child BEFORE any blocking startup IO or publication.
    let generation = attach_watch(&mut cleanup, server_pid)?;
    let url = read_server(cleanup.server.as_mut().expect("server just stored"))?;
    let info = server_info(&url, &password)?;
    if info.pid != server_pid || !info.urls.iter().any(|candidate| candidate == &url) {
        return Err("OpenCode server identity does not match the owned child".into());
    }
    let conversation = new_conversation()?;
    create_session(&url, &password, &conversation, &cwd)?;
    let manifest = manifest_json(ManifestSpec {
        nonce: &nonce,
        session: &session,
        generation: &generation,
        server_pid,
        url: &url,
        conversation: &conversation,
        password: &password,
        version: &info.version,
    });
    cleanup.manifest = Some(create_manifest(&directory, &name, &manifest)?);
    if let Some(path) = status.as_ref() {
        let _ = fs::write(path, "MODEL:? CTX:? CTXWIN:? CTXPCT:?\n");
    }
    let code = run_tui(&url, &conversation, &cwd, &password);
    drop(cleanup);
    Ok(code)
}

fn env_value(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is required in binding mode"))
}

fn spawn_server(password: &str) -> Result<Child, String> {
    let mut command = Command::new("opencode");
    command
        .args(["serve", "--hostname", "127.0.0.1", "--port", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    configure_auth(&mut command, password);
    remuda_native::child_guard::harden(&mut command);
    command
        .spawn()
        .map_err(|error| format!("cannot start OpenCode server: {error}"))
}

fn read_server(server: &mut Child) -> Result<String, String> {
    let stdout = server
        .stdout
        .take()
        .ok_or_else(|| "OpenCode server has no stdout".to_string())?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let _ = tx.send(line);
        }
    });
    let deadline = Instant::now() + START_TIMEOUT;
    let mut url = None;
    while url.is_none() && Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left.min(Duration::from_millis(200))) {
            Ok(line) => {
                if let Some(value) = line.strip_prefix("server listening on ") {
                    let value = value.trim().to_string();
                    if !loopback_endpoint(&value) {
                        return Err("OpenCode server published a non-loopback URL".into());
                    }
                    url = Some(value);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if server.try_wait().ok().flatten().is_some() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    url.ok_or_else(|| "OpenCode server did not publish its URL".into())
}

pub(crate) fn loopback_endpoint(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    let host = match url.host() {
        Some(url::Host::Ipv4(address)) => address == Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(address)) => address == Ipv6Addr::LOCALHOST,
        _ => false,
    };
    host && url.scheme() == "http"
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && matches!(url.path(), "" | "/")
        && url.port().is_some()
}

#[derive(serde::Deserialize)]
struct ServerInfo {
    pid: u32,
    version: String,
    urls: Vec<String>,
}

fn server_info(url: &str, password: &str) -> Result<ServerInfo, String> {
    let response = request(
        url.to_string() + "/api/info",
        "GET",
        password,
        String::new(),
    )?;
    if response.status != 200 {
        return Err(format!(
            "OpenCode server info refused: HTTP {}",
            response.status
        ));
    }
    serde_json::from_slice(&response.body)
        .map_err(|_| "OpenCode server info is invalid JSON".to_string())
}

/// The fixed conversation is always `ses_` plus 32 lowercase hex characters.
/// `_agent_notice` interpolates it into a URL path, so anything else is refused.
pub(crate) fn fixed_conversation(value: &str) -> bool {
    value.strip_prefix("ses_").is_some_and(|hex| {
        hex.len() == 32
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn new_conversation() -> Result<String, String> {
    Ok(format!("ses_{}", random_hex(16)?))
}

pub(crate) fn generation_for(wrapper_started_us: u64, server_started_us: u64) -> String {
    format!("{wrapper_started_us:x}-{server_started_us:x}")
}

pub(crate) fn generation_matches(value: &str) -> bool {
    let Some((wrapper, server)) = value.split_once('-') else {
        return false;
    };
    [wrapper, server].iter().all(|part| {
        !part.is_empty() && part.len() <= 16 && part.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn create_session(url: &str, password: &str, conversation: &str, cwd: &Path) -> Result<(), String> {
    let body =
        json!({"id": conversation, "location": {"directory": cwd.to_string_lossy().to_string()}});
    let response = request(
        url.to_string() + "/api/session",
        "POST",
        password,
        body.to_string(),
    )?;
    if response.status != 200 {
        return Err(format!(
            "OpenCode session create refused: HTTP {}",
            response.status
        ));
    }
    let value: Value = serde_json::from_slice(&response.body)
        .map_err(|_| "OpenCode session create returned invalid JSON".to_string())?;
    if value.pointer("/data/id").and_then(Value::as_str) != Some(conversation) {
        return Err("OpenCode session create did not return the fixed session".into());
    }
    Ok(())
}

fn request(
    url: String,
    method: &str,
    password: &str,
    body: String,
) -> Result<remuda_native::net::HttpResponse, String> {
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"));
    perform(
        HttpRequest {
            method: method.into(),
            url,
            headers: vec![
                ("Authorization".into(), format!("Basic {auth}").into_bytes()),
                ("Content-Type".into(), b"application/json".to_vec()),
            ],
            body: body.into_bytes(),
            timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(2),
            max_bytes: 64 * 1024,
            ca_file: None,
            pin: None,
            pin_only: false,
        },
        None,
    )
    .map_err(|error| format!("OpenCode HTTP request failed: {error}"))
}

struct ManifestSpec<'a> {
    nonce: &'a str,
    session: &'a str,
    generation: &'a str,
    server_pid: u32,
    url: &'a str,
    conversation: &'a str,
    password: &'a str,
    version: &'a str,
}

fn manifest_json(spec: ManifestSpec<'_>) -> Value {
    json!({
        "schema": SCHEMA,
        "protocol": PROTOCOL,
        "nonce": spec.nonce,
        "session": spec.session,
        "wrapper_pid": std::process::id(),
        "server_pid": spec.server_pid,
        "server_version": spec.version,
        "generation": spec.generation,
        "endpoint": spec.url,
        "conversation": spec.conversation,
        "auth": {"scheme": "basic", "user": "opencode", "password": spec.password},
    })
}

fn create_manifest(
    directory: &PrivateDir,
    name: &str,
    manifest: &Value,
) -> Result<CreatedFile, String> {
    let text =
        serde_json::to_vec(manifest).map_err(|_| "cannot encode binding manifest".to_string())?;
    write_manifest_with(directory, name, |file| {
        file.write_all(&text).and_then(|()| file.sync_all())
    })
}

/// Create the manifest and, if writing or syncing fails after creation, remove
/// exactly the inode created here so no truncated manifest is left behind.
fn write_manifest_with<F>(
    directory: &PrivateDir,
    name: &str,
    write: F,
) -> Result<CreatedFile, String>
where
    F: FnOnce(&mut std::fs::File) -> std::io::Result<()>,
{
    let (mut file, id) = directory.create_new(name)?;
    let created = CreatedFile::new(directory.clone(), name.to_string(), id);
    if write(&mut file).is_err() {
        drop(file);
        created.remove();
        return Err("cannot write binding manifest".into());
    }
    Ok(created)
}

fn run_tui(url: &str, conversation: &str, cwd: &Path, password: &str) -> ExitCode {
    let mut command = Command::new("opencode");
    command
        .arg("--server")
        .arg(url)
        .arg("--session")
        .arg(conversation)
        .arg(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    configure_auth(&mut command, password);
    match command.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("remuda: OpenCode TUI: cannot start: {error}");
            ExitCode::FAILURE
        }
    }
}

/// A process's stable identity: who its parent is, and when it started.
/// Start time in microseconds rejects a recycled pid, which `kill(pid, 0)`
/// cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    pub parent: u32,
    pub started_us: u64,
}

#[cfg(target_os = "macos")]
pub(crate) fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is exactly the requested flavor's size and outlives the call.
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    Some(ProcessIdentity {
        parent: info.pbi_ppid,
        started_us: info
            .pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbi_start_tvusec),
    })
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    if pid == 0 {
        return None;
    }
    // `comm` may contain spaces and parentheses, so split after the last ')'.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    // Field 4 (ppid) and field 22 (starttime) of proc(5) stat.
    let parent = fields.get(1)?.parse().ok()?;
    let start_ticks: u64 = fields.get(19)?.parse().ok()?;
    // SAFETY: sysconf has no preconditions and only reads a constant.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks <= 0 {
        return None;
    }
    let per_tick_us = 1_000_000_u64 / ticks as u64;
    Some(ProcessIdentity {
        parent,
        started_us: start_ticks.saturating_mul(per_tick_us),
    })
}

#[cfg(not(unix))]
pub(crate) fn process_identity(_pid: u32) -> Option<ProcessIdentity> {
    None
}

/// Start the parent watcher and wait for its acknowledgement, then derive the
/// launch generation from the live wrapper and server identities.
#[cfg(unix)]
fn attach_watch(cleanup: &mut Cleanup, server_pid: u32) -> Result<String, String> {
    let watcher = start_parent_watch(server_pid)?;
    cleanup.watcher = Some(watcher);
    let wrapper = process_identity(std::process::id())
        .ok_or_else(|| "cannot read the wrapper process identity".to_string())?;
    let server = process_identity(server_pid)
        .ok_or_else(|| "cannot read the server process identity".to_string())?;
    if server.parent != std::process::id() {
        return Err("OpenCode server is not a child of this wrapper".into());
    }
    Ok(generation_for(wrapper.started_us, server.started_us))
}

#[cfg(not(unix))]
fn attach_watch(_cleanup: &mut Cleanup, _server_pid: u32) -> Result<String, String> {
    Err(UNSUPPORTED.into())
}

#[cfg(unix)]
fn start_parent_watch(server_pid: u32) -> Result<Child, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("cannot locate the watcher binary: {error}"))?;
    let mut command = Command::new(executable);
    command
        .args([
            "_opencode_watch",
            &std::process::id().to_string(),
            &server_pid.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start OpenCode server watcher: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "OpenCode watcher has no acknowledgement pipe".to_string())?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let read = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(matches!(read, Ok(count) if count > 0) && line.trim() == "ready");
    });
    match rx.recv_timeout(WATCHER_ACK_TIMEOUT) {
        Ok(true) => Ok(child),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            Err("OpenCode server watcher did not acknowledge startup".into())
        }
    }
}

#[cfg(not(unix))]
fn start_parent_watch(_server_pid: u32) -> Result<Child, String> {
    Err(UNSUPPORTED.into())
}

#[cfg(unix)]
fn stop_child_group(child: Option<Child>) {
    let Some(mut child) = child else { return };
    let group = -(child.id() as libc::pid_t);
    // SAFETY: the child was started with process_group(0), so its pid is the group id.
    unsafe {
        libc::kill(group, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(not(unix))]
fn stop_child_group(child: Option<Child>) {
    if let Some(mut child) = child {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(unix)]
pub fn watch_parent(parent_pid: i32, server_pid: i32) -> ExitCode {
    if parent_pid <= 1 || server_pid <= 1 {
        return ExitCode::FAILURE;
    }
    // The wrapper can die between spawning the server and this process starting.
    // Refuse before acknowledging in that case; the wrapper then tears down.
    // SAFETY: getppid only reads this process's parent.
    if unsafe { libc::getppid() } != parent_pid {
        // SAFETY: server_pid is the owned child's process group leader.
        unsafe {
            libc::kill(-server_pid, libc::SIGKILL);
        }
        return ExitCode::FAILURE;
    }
    // The wrapper blocks on this line before any startup IO or publication.
    let mut out = std::io::stdout();
    let _ = out.write_all(b"ready\n");
    let _ = out.flush();
    loop {
        // SAFETY: read this process's parent and signal the server's process group.
        unsafe {
            if libc::getppid() != parent_pid {
                libc::kill(-server_pid, libc::SIGKILL);
                return ExitCode::SUCCESS;
            }
            if libc::kill(-server_pid, 0) == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return ExitCode::SUCCESS;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(not(unix))]
pub fn watch_parent(_parent_pid: i32, _server_pid: i32) -> ExitCode {
    eprintln!("remuda: OpenCode watcher requires Unix");
    ExitCode::FAILURE
}

fn random_hex(bytes: usize) -> Result<String, String> {
    let mut data = vec![0_u8; bytes];
    getrandom::fill(&mut data).map_err(|_| "randomness unavailable".to_string())?;
    Ok(data.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn private_test_dir() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("remuda-manifest-{}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).expect("temp dir");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
        dir
    }

    #[test]
    fn capability_advertises_native_only_where_identity_is_supported() {
        let supported = cfg!(any(target_os = "macos", target_os = "linux"));
        assert_eq!(platform_supported(), supported);
        assert_eq!(capability()["native_notice"].as_bool(), Some(supported));
        assert_eq!(capability()["protocol"], PROTOCOL);
    }

    #[test]
    #[cfg(unix)]
    fn manifest_write_failure_removes_only_its_own_partial_file() {
        use std::io::Write as _;
        let dir = private_test_dir();
        let directory = PrivateDir::open(&dir).expect("private dir");
        let path = dir.join("manifest.json");

        let refused = write_manifest_with(&directory, "manifest.json", |file| {
            file.write_all(b"{\"partial\":")?;
            Err(std::io::Error::other("injected sync failure"))
        });
        assert!(refused.is_err());
        assert!(!path.exists(), "a failed write leaves no partial manifest");

        fs::write(&path, b"preexisting").expect("preexisting");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
        assert!(write_manifest_with(&directory, "manifest.json", |_| Ok(())).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"preexisting");
    }

    #[test]
    #[cfg(unix)]
    fn a_created_manifest_is_private_and_removable_by_identity() {
        use std::io::Write as _;
        let dir = private_test_dir();
        let directory = PrivateDir::open(&dir).expect("private dir");
        let created = write_manifest_with(&directory, "manifest.json", |file| {
            file.write_all(b"{\"schema\":1}")
        })
        .expect("create manifest");
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.join("manifest.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        created.remove();
        assert!(!dir.join("manifest.json").exists());
    }

    #[test]
    fn manifest_never_claims_an_instance() {
        let manifest = manifest_json(ManifestSpec {
            nonce: "n",
            session: "s",
            generation: "g",
            server_pid: 1,
            url: "http://127.0.0.1:1",
            conversation: "ses_x",
            password: "p",
            version: "2.2.5",
        });
        assert!(manifest.get("instance").is_none());
        assert_eq!(manifest["nonce"], "n");
        assert_eq!(manifest["auth"]["password"], "p");
    }

    #[test]
    fn loopback_validation_is_exact() {
        assert!(loopback_endpoint("http://127.0.0.1:1234"));
        assert!(loopback_endpoint("http://[::1]:1234"));
        assert!(!loopback_endpoint("http://example.com:1234"));
        assert!(!loopback_endpoint("https://127.0.0.1:1234"));
        assert!(!loopback_endpoint("http://127.0.0.1:1234/path"));
        assert!(!loopback_endpoint("http://user@127.0.0.1:1234"));
        assert!(!loopback_endpoint("http://127.0.0.1:1234?query"));
    }

    #[test]
    fn fixed_session_id_has_the_required_prefix() {
        let id = format!("ses_{}", random_hex(16).unwrap());
        assert!(id.starts_with("ses_"));
        assert_eq!(id.len(), 36);
        assert!(fixed_conversation(&id));
        assert_eq!(new_conversation().unwrap().len(), 36);
    }

    #[test]
    fn conversation_token_grammar_is_exact() {
        assert!(fixed_conversation("ses_0123456789abcdef0123456789abcdef"));
        assert!(!fixed_conversation("ses_"));
        assert!(!fixed_conversation("ses_x"));
        assert!(!fixed_conversation("ses_0123456789ABCDEF0123456789ABCDEF"));
        assert!(!fixed_conversation("ses_0123456789abcdef0123456789abcde"));
        assert!(!fixed_conversation("ses_0123456789abcdef0123456789abcdef0"));
        assert!(!fixed_conversation("ses_../../etc/passwd"));
        assert!(!fixed_conversation("ses_0123456789abcdef0123456789abcde?x"));
        assert!(!fixed_conversation("ses_0123456789abcdef0123456789abcde."));
        assert!(!fixed_conversation("ses_0123456789abcdef0123456789abcde/"));
    }

    #[test]
    fn generation_encodes_both_process_starts() {
        assert_eq!(generation_for(0x10, 0x20), "10-20");
        assert!(generation_matches(&generation_for(1, 2)));
        assert!(!generation_matches(""));
        assert!(!generation_matches("10"));
        assert!(!generation_matches("10-"));
        assert!(!generation_matches("-20"));
        assert!(!generation_matches("10-20-30"));
        assert!(!generation_matches("10-xyz"));
        assert!(!generation_matches("random-label"));
    }

    #[cfg(unix)]
    #[test]
    fn process_identity_reads_this_process() {
        let identity = process_identity(std::process::id()).expect("self identity");
        // SAFETY: getppid only reads this process's parent.
        assert_eq!(identity.parent, unsafe { libc::getppid() } as u32);
        assert!(identity.started_us > 0);
        assert_eq!(process_identity(0), None);
    }

    #[cfg(unix)]
    #[test]
    fn auth_env_overrides_an_inherited_primary_password() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("printf '%s|%s' \"$OPENCODE_PASSWORD\" \"$OPENCODE_SERVER_PASSWORD\"")
            .env("OPENCODE_PASSWORD", "inherited-conflict");
        configure_auth(&mut command, "generated-owner");
        let output = command.output().expect("sh");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "generated-owner|generated-owner"
        );
    }
}
