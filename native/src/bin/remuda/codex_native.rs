//! Codex native notice protocol, add-only slice.
//!
//! No product wiring yet: this module holds the pure decisions the endpoint
//! and the pin will use, so they can be tested without a socket. The pinned
//! identity is the first owner declaration; it is not proof of historical
//! ancestry or of which TUI is current.
//!
//! Version pin: installed `codex-cli 0.166.0`. `thread/queue/add`,
//! `thread/read` and `thread/loaded/list` are experimental there, so the
//! client must initialize with `capabilities.experimentalApi = true`.

// The endpoint and pin wiring lands next; until then this surface exists for
// its tests only.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

pub(crate) const PROTOCOL: &str = "codex-app-server-v1";
pub(crate) const PIN_SCHEMA: u64 = 1;
pub(crate) const PINNED_VERSION: &str = "0.166.0";
pub(crate) const MODE_ADD_ONLY: &str = "queue-add";
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_MESSAGES: usize = 64;
const MAX_PAGES: usize = 16;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// The identity observed on one accepted connection, recorded once and
/// revalidated on every later dispatch.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pin {
    pub schema: u64,
    pub protocol: String,
    pub nonce: String,
    pub session: String,
    pub socket: String,
    pub thread: String,
    pub peer_uid: u32,
    pub peer_pid: u32,
    pub peer_start_us: u64,
    pub socket_dev: u64,
    pub socket_ino: u64,
    pub generation: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LiveIdentity {
    pub uid: u32,
    pub pid: u32,
    pub start_us: u64,
    pub dev: u64,
    pub ino: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Accepted,
    NotWritten(&'static str),
    Uncertain(&'static str),
}

impl Outcome {
    pub(crate) fn to_json(self) -> Value {
        match self {
            Outcome::Accepted => json!({"outcome": "accepted_queue"}),
            Outcome::NotWritten(reason) => {
                json!({"outcome": "definitely_not_written", "reason": reason})
            }
            Outcome::Uncertain(reason) => json!({"outcome": "uncertain", "reason": reason}),
        }
    }
}

/// A successfully published binding. Removal is inode-based, so a replacement
/// at either name survives.
pub(crate) struct Published {
    pub generation: String,
    manifest: super::private_dir::CreatedFile,
    pin: super::private_dir::CreatedFile,
}

impl Published {
    pub(crate) fn remove(self) {
        self.manifest.remove();
        self.pin.remove();
    }
}

pub(crate) fn version_supported(version: &str) -> bool {
    version == PINNED_VERSION
}

/// Exact measured version, parsed from the initialize response's user agent
/// (`<client>/<version> (<platform>)`), not a loose prefix match.
pub(crate) fn server_version(initialize: &Value) -> Option<&str> {
    let user_agent = initialize.pointer("/result/userAgent")?.as_str()?;
    let after_client = user_agent.split_once('/')?.1;
    let version = after_client.split_whitespace().next()?;
    version_supported(version).then_some(version)
}

/// Canonical UUID: 8-4-4-4-12 hex digits.
pub(crate) fn thread_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => *byte == b'-',
        _ => byte.is_ascii_hexdigit(),
    })
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A deterministic fingerprint of the one accepted connection.
pub(crate) fn generation(live: LiveIdentity, socket: &Path, thread: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(live.dev.to_le_bytes());
    hasher.update(live.ino.to_le_bytes());
    hasher.update(live.pid.to_le_bytes());
    hasher.update(live.uid.to_le_bytes());
    hasher.update(live.start_us.to_le_bytes());
    hasher.update(socket.as_os_str().as_encoded_bytes());
    hasher.update(thread.as_bytes());
    hex(&hasher.finalize())
}

pub(crate) fn pin_for(
    socket: &Path,
    nonce: &str,
    session: &str,
    thread: &str,
    live: LiveIdentity,
) -> Pin {
    Pin {
        schema: PIN_SCHEMA,
        protocol: PROTOCOL.into(),
        nonce: nonce.to_string(),
        session: session.to_string(),
        socket: socket.to_string_lossy().into_owned(),
        thread: thread.to_string(),
        peer_uid: live.uid,
        peer_pid: live.pid,
        peer_start_us: live.start_us,
        socket_dev: live.dev,
        socket_ino: live.ino,
        generation: generation(live, socket, thread),
    }
}

/// The pin is the whole authority: a changed socket inode, peer, start time
/// or thread means the declaration no longer describes the live endpoint.
pub(crate) fn pin_matches(pin: &Pin, socket: &Path, thread: &str, live: LiveIdentity) -> bool {
    pin.schema == PIN_SCHEMA
        && pin.protocol == PROTOCOL
        && !pin.nonce.is_empty()
        && !pin.session.is_empty()
        && pin.socket == socket.to_string_lossy()
        && pin.thread == thread
        && pin.peer_uid == live.uid
        && pin.peer_pid == live.pid
        && pin.peer_start_us == live.start_us
        && pin.socket_dev == live.dev
        && pin.socket_ino == live.ino
        && pin.generation == generation(live, socket, thread)
}

pub(crate) fn read_matches(response: &Value, thread: &str) -> bool {
    let Some(found) = response.pointer("/result/thread") else {
        return false;
    };
    found.get("id").and_then(Value::as_str) == Some(thread)
        && found.get("sessionId").and_then(Value::as_str) == Some(thread)
        && found.get("ephemeral").and_then(Value::as_bool) == Some(false)
        && found.get("forkedFromId").is_none_or(Value::is_null)
        && found.get("parentThreadId").is_none_or(Value::is_null)
        && found.get("threadSource").and_then(Value::as_str) == Some("user")
        && found.get("canAcceptDirectInput").and_then(Value::as_bool) == Some(true)
}

/// The transport must match the JSON-RPC id before any payload decision.
pub(crate) fn rpc_matches(response: &Value, request_id: u64) -> bool {
    response.get("id").and_then(Value::as_u64) == Some(request_id)
}

/// `thread/loaded/list` proves the UUID is loaded in memory, not persisted
/// only. The installed schema is `{data:[uuid string], nextCursor}`.
pub(crate) fn loaded_matches(response: &Value, thread: &str) -> bool {
    let Some(rows) = response.pointer("/result/data").and_then(Value::as_array) else {
        return false;
    };
    rows.iter().any(|row| row.as_str() == Some(thread))
}

pub(crate) fn loaded_next_cursor(response: &Value) -> Option<&str> {
    response.pointer("/result/nextCursor")?.as_str()
}

/// `accepted_queue` needs the correlated admission receipt: a non-empty
/// submission id, our frozen client id echoed, and the exact canonical text.
pub(crate) fn receipt_matches(response: &Value, client_id: &str, text: &str) -> bool {
    let Some(queued) = response.pointer("/result/queuedSubmission") else {
        return false;
    };
    let id_ok = queued
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty());
    let echo_ok = queued.get("clientUserMessageId").and_then(Value::as_str) == Some(client_id);
    let Some(input) = queued.get("input").and_then(Value::as_array) else {
        return false;
    };
    let input_ok = input.len() == 1
        && input[0].get("type").and_then(Value::as_str) == Some("text")
        && input[0].get("text").and_then(Value::as_str) == Some(text)
        && input[0]
            .get("text_elements")
            .and_then(Value::as_array)
            .is_some_and(|elements| elements.is_empty());
    id_ok && echo_ok && input_ok
}

/// A well-formed reply carries exactly one of `result`/`error` and the id of
/// our request. The transport runs this before any initialize, read, list or
/// add decision; a mixed or wrongly identified reply is malformed.
pub(crate) fn correlated(response: &Value, request_id: u64) -> bool {
    let has_result = response.get("result").is_some();
    let has_error = response.get("error").is_some();
    has_result != has_error && rpc_matches(response, request_id)
}

pub(crate) fn result_for(response: &Value, request_id: u64) -> Option<&Value> {
    if correlated(response, request_id) {
        response.get("result")
    } else {
        None
    }
}

/// No source-proven pre-admission rejection exists for the installed build, so
/// any add error, mixed reply or unmatched receipt is `uncertain`.
pub(crate) fn add_outcome(
    response: &Value,
    request_id: u64,
    client_id: &str,
    text: &str,
) -> Outcome {
    if !correlated(response, request_id) {
        return Outcome::Uncertain("malformed_reply");
    }
    if response.get("error").is_some() {
        return Outcome::Uncertain("add_error");
    }
    if receipt_matches(response, client_id, text) {
        Outcome::Accepted
    } else {
        Outcome::Uncertain("receipt_mismatch")
    }
}

#[cfg(unix)]
fn peer_uid_pid(stream: &std::os::unix::net::UnixStream) -> Option<(u32, u32)> {
    use std::os::unix::io::AsRawFd;
    #[cfg(target_os = "macos")]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: getpeereid only writes the two out-pointers.
        let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        if rc != 0 {
            return None;
        }
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: buffer and length describe one pid_t; SOL_LOCAL/LOCAL_PEERPID is macOS.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                0,
                0x002,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        if rc != 0 || pid <= 0 {
            return None;
        }
        Some((uid, pid as u32))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: buffer and length describe one ucred.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        if rc != 0 || cred.pid <= 0 {
            return None;
        }
        Some((cred.uid, cred.pid as u32))
    }
}

#[cfg(unix)]
fn socket_stat(path: &Path) -> Option<(u64, u64, u32)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    // Codex publishes `app.sock` as a symlink to the daemon's real socket, so
    // follow it here; the resolved inode and the connected-FD peer are pinned.
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.file_type().is_socket() {
        return None;
    }
    // These sockets live outside the pinned manifest directory, so owner and
    // mode are the checks here; replacement is caught by the (dev, ino) pin.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return None;
    }
    Some((metadata.dev(), metadata.ino(), metadata.mode()))
}

/// Observe the declared socket's `(dev, ino)` without opening it.
#[cfg(unix)]
pub(crate) fn socket_fingerprint(path: &Path) -> Option<(u64, u64)> {
    socket_stat(path).map(|(dev, ino, _mode)| (dev, ino))
}

/// Observe the accepted connection and the declared socket path. `None` means
/// the declaration cannot be pinned from this process.
#[cfg(unix)]
pub(crate) fn socket_identity(
    stream: &std::os::unix::net::UnixStream,
    path: &Path,
) -> Option<LiveIdentity> {
    let (dev, ino, _mode) = socket_stat(path)?;
    let (uid, pid) = peer_uid_pid(stream)?;
    if uid != unsafe { libc::geteuid() } {
        return None;
    }
    let started = super::opencode_tui::process_identity(pid)?;
    Some(LiveIdentity {
        uid,
        pid,
        start_us: started.started_us,
        dev,
        ino,
    })
}

/// The shared frozen stdin grammar. It never names the endpoint or thread; the
/// caller cannot retarget a dispatch, and `deny_unknown_fields` rejects the
/// attempt outright.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub version: u64,
    pub binding: String,
    pub nonce: String,
    pub session: String,
    pub instance: String,
    pub request_id: String,
    pub text: String,
    pub message_ids: Vec<String>,
}

/// The strict private protocol manifest is the only source of the endpoint,
/// thread and mode. It is bound to one nonce/session/generation.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub schema: u64,
    pub protocol: String,
    pub nonce: String,
    pub session: String,
    pub mode: String,
    pub generation: String,
    pub socket: String,
    pub thread: String,
}

pub(crate) fn valid_token(value: &str) -> bool {
    token(value)
}

fn token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

pub(crate) fn validate_request(request: &Request) -> Result<(), &'static str> {
    if request.version != 1
        || request.binding.is_empty()
        || !Path::new(&request.binding).is_absolute()
        || !token(&request.nonce)
        || !token(&request.session)
        || !token(&request.instance)
        || !request.request_id.starts_with("msg_")
        || request.request_id.len() > 128
        || !request
            .request_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        || request.text.len() > 1024
        || request.text.contains(['\r', '\n'])
        || request.message_ids.is_empty()
        || request.message_ids.iter().any(|id| !token(id))
    {
        return Err("invalid_request");
    }
    if request.text != super::agent_notice::canonical_notice(&request.message_ids) {
        return Err("invalid_request");
    }
    Ok(())
}

/// The owner's adoption declaration: the socket and UUID read from a fresh
/// `/status` in the original TUI, plus the binding generation it belongs to.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdoptRequest {
    pub version: u64,
    pub binding: String,
    pub nonce: String,
    pub session: String,
    pub socket: String,
    pub thread: String,
}

pub(crate) fn validate_adopt(request: &AdoptRequest) -> Result<(), &'static str> {
    if request.version != 1
        || !Path::new(&request.binding).is_absolute()
        || !token(&request.nonce)
        || !token(&request.session)
        || !Path::new(&request.socket).is_absolute()
        || !thread_uuid(&request.thread)
    {
        return Err("invalid_request");
    }
    Ok(())
}

/// The manifest must describe the same declaration the pin recorded, so a
/// stale or swapped manifest cannot retarget an otherwise valid binding.
pub(crate) fn validate_manifest(
    request: &Request,
    manifest: &Manifest,
    pin: &Pin,
) -> Result<(), &'static str> {
    if manifest.schema != PIN_SCHEMA
        || manifest.protocol != PROTOCOL
        || manifest.nonce != request.nonce
        || manifest.session != request.session
        || manifest.mode != MODE_ADD_ONLY
        || !thread_uuid(&manifest.thread)
        || manifest.socket.is_empty()
        || !Path::new(&manifest.socket).is_absolute()
        || manifest.generation.is_empty()
        || manifest.generation != pin.generation
        || manifest.socket != pin.socket
        || manifest.thread != pin.thread
        || manifest.nonce != pin.nonce
        || manifest.session != pin.session
    {
        return Err("invalid_binding");
    }
    Ok(())
}

/// Read the pinned identity through the private-directory seam: the parent is
/// opened and checked once, the leaf is nofollow/nonblock, and the file's
/// owner/mode are verified on the opened descriptor with a hard size bound.
pub(crate) fn load_pin(path: &Path) -> Result<Pin, &'static str> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or("invalid_pin")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid_pin")?;
    let directory = super::private_dir::PrivateDir::open(parent).map_err(|_| "invalid_pin")?;
    let data = directory
        .read_limited(name, MAX_MANIFEST)
        .map_err(|_| "invalid_pin")?;
    let pin: Pin = serde_json::from_slice(&data).map_err(|_| "invalid_pin")?;
    if pin.schema != PIN_SCHEMA
        || pin.protocol != PROTOCOL
        || pin.nonce.is_empty()
        || pin.session.is_empty()
        || !thread_uuid(&pin.thread)
    {
        return Err("invalid_pin");
    }
    Ok(pin)
}

/// Create the pin with `O_EXCL` (a preexisting file is never adopted) and
/// remove only this invocation's inode if writing fails.
pub(crate) fn store_pin(
    path: &Path,
    pin: &Pin,
) -> Result<super::private_dir::CreatedFile, &'static str> {
    let text = serde_json::to_vec(pin).map_err(|_| "invalid_pin")?;
    store_pin_with(path, |file| {
        std::io::Write::write_all(file, &text)
            .and_then(|()| file.sync_all())
            .map_err(|_| "invalid_pin")
    })
}

pub(crate) fn store_pin_with<F>(
    path: &Path,
    write: F,
) -> Result<super::private_dir::CreatedFile, &'static str>
where
    F: FnOnce(&mut std::fs::File) -> Result<(), &'static str>,
{
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or("invalid_pin")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid_pin")?;
    let directory = super::private_dir::PrivateDir::open(parent).map_err(|_| "invalid_pin")?;
    let (mut file, id) = directory.create_new(name).map_err(|_| "invalid_pin")?;
    let created = super::private_dir::CreatedFile::new(directory, name.to_string(), id);
    if write(&mut file).is_err() {
        drop(file);
        created.remove();
        return Err("invalid_pin");
    }
    Ok(created)
}

/// `_codex_adopt`: pin the owner-declared socket and thread into the private
/// binding directory. The declaration is the authority; the kernel peer,
/// process start and socket inode are recorded and rechecked on every dispatch.
pub(crate) fn run_adopt(args: &[&str]) -> ExitCode {
    if args.iter().any(|arg| *arg == "--help" || *arg == "-h") {
        println!(
            "remuda _codex_adopt\nReads one frozen adoption request from stdin; returns typed JSON."
        );
        return ExitCode::SUCCESS;
    }
    if !args.is_empty() {
        return emit_status("unavailable", "invalid_args");
    }
    if !platform_supported() {
        return emit_status("unavailable", "unsupported_platform");
    }
    let text = match super::agent_notice::read_stdin() {
        Ok(text) => text,
        Err(reason) => return emit_status("unavailable", reason),
    };
    let request: AdoptRequest = match serde_json::from_str(&text) {
        Ok(request) => request,
        Err(_) => return emit_status("unavailable", "invalid_request"),
    };
    if let Err(reason) = validate_adopt(&request) {
        return emit_status("unavailable", reason);
    }
    match adopt(&request) {
        Ok(published) => emit_fields(json!({
            "outcome": "adopted",
            "generation": published.generation
        })),
        Err(reason) => emit_status("unavailable", reason),
    }
}

fn emit_status(outcome: &str, reason: &str) -> ExitCode {
    emit_fields(json!({"outcome": outcome, "reason": reason}))
}

fn emit_fields(value: Value) -> ExitCode {
    println!("{value}");
    ExitCode::SUCCESS
}

fn emit(outcome: Outcome) -> ExitCode {
    emit_fields(outcome.to_json())
}

/// Only macOS is measured end to end for this slice; Linux is not advertised
/// until the same peer, receipt and lifecycle evidence exists there.
const fn platform_supported() -> bool {
    cfg!(target_os = "macos")
}

/// Connect and observe one endpoint: the path fingerprint before the connect,
/// the connected FD's peer credentials and start generation, and the path
/// fingerprint again after the connect. A swap in that window fails here.
#[cfg(unix)]
fn connect_endpoint(
    socket: &Path,
) -> Result<(std::os::unix::net::UnixStream, LiveIdentity), &'static str> {
    let before = socket_fingerprint(socket).ok_or("endpoint_unverified")?;
    let stream = std::os::unix::net::UnixStream::connect(socket).map_err(|_| "dead_socket")?;
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let live = socket_identity(&stream, socket).ok_or("endpoint_unverified")?;
    if (live.dev, live.ino) != before {
        return Err("endpoint_unverified");
    }
    Ok((stream, live))
}

#[cfg(unix)]
fn fingerprint_still(socket: &Path, live: LiveIdentity) -> bool {
    socket_fingerprint(socket) == Some((live.dev, live.ino))
}

/// The non-mutating preflight shared by adoption and delivery: initialize,
/// require the exact measured build, then the declared UUID as the root-user
/// thread loaded on this peer. It writes no turn, queue item or key.
#[cfg(unix)]
fn preflight(
    stream: &mut std::os::unix::net::UnixStream,
    thread: &str,
) -> Result<u64, &'static str> {
    let initialize = json!({
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": {"name": "remuda", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"experimentalApi": true}
        }
    });
    let reply = exchange(stream, &initialize, 1)?;
    if result_for(&reply, 1).is_none() {
        return Err("initialize_refused");
    }
    if server_version(&reply).is_none() {
        return Err("version_unsupported");
    }
    super::codex_tui::websocket_send_text(
        stream,
        &json!({"method": "initialized", "params": {}}).to_string(),
    )
    .map_err(|_| "transport")?;

    let read = json!({
        "id": 2,
        "method": "thread/read",
        "params": {"threadId": thread}
    });
    let reply = exchange(stream, &read, 2)?;
    if result_for(&reply, 2).is_none() || !read_matches(&reply, thread) {
        return Err("thread_mismatch");
    }

    let mut cursor: Option<String> = None;
    let mut next_id = 3_u64;
    for _ in 0..MAX_PAGES {
        let id = next_id;
        next_id += 1;
        let list = match &cursor {
            Some(cursor) => json!({
                "id": id,
                "method": "thread/loaded/list",
                "params": {"cursor": cursor}
            }),
            None => json!({"id": id, "method": "thread/loaded/list", "params": {}}),
        };
        let reply = exchange(stream, &list, id)?;
        if result_for(&reply, id).is_none() {
            return Err("thread_mismatch");
        }
        // Stop as soon as the exact thread is found; later pages cannot make
        // it more true.
        if loaded_matches(&reply, thread) {
            return Ok(next_id);
        }
        match loaded_next_cursor(&reply) {
            Some(next) => cursor = Some(next.to_string()),
            None => return Err("thread_not_loaded"),
        }
    }
    Err("thread_not_loaded")
}

#[cfg(unix)]
pub(crate) fn adopt(request: &AdoptRequest) -> Result<Published, &'static str> {
    adopt_guarded(request, || true)
}

/// Adopt only while `permitted` still authorizes publication. The guard runs
/// immediately before each binding file is created and again before the result
/// is returned, so a launcher can withdraw authority during preflight.
pub(crate) fn adopt_guarded(
    request: &AdoptRequest,
    permitted: impl Fn() -> bool,
) -> Result<Published, &'static str> {
    let binding = Path::new(&request.binding);
    let pin_file = pin_path(binding);
    if std::fs::symlink_metadata(&pin_file).is_ok() || std::fs::symlink_metadata(binding).is_ok() {
        return Err("already_adopted");
    }
    if !permitted() {
        return Err("authority_lost");
    }
    let socket = Path::new(&request.socket);
    let (stream, live) = connect_endpoint(socket)?;
    let mut stream =
        super::codex_tui::websocket_handshake(stream, None).map_err(|_| "endpoint_unverified")?;
    // The owner-declared UUID must be live and loaded on this exact peer
    // before we publish any binding.
    preflight(&mut stream, &request.thread)?;
    if !fingerprint_still(socket, live) {
        return Err("endpoint_unverified");
    }
    let pin = pin_for(
        socket,
        &request.nonce,
        &request.session,
        &request.thread,
        live,
    );
    // Pin first, so a manifest never points at a missing pin.
    if !permitted() {
        return Err("authority_lost");
    }
    let created = store_pin(&pin_file, &pin)?;
    if !permitted() {
        created.remove();
        return Err("authority_lost");
    }
    let manifest = Manifest {
        schema: PIN_SCHEMA,
        protocol: PROTOCOL.into(),
        nonce: pin.nonce.clone(),
        session: pin.session.clone(),
        mode: MODE_ADD_ONLY.into(),
        generation: pin.generation.clone(),
        socket: pin.socket.clone(),
        thread: pin.thread.clone(),
    };
    let text = serde_json::to_vec(&manifest).map_err(|_| "invalid_binding")?;
    let written = store_pin_with(binding, |file| {
        std::io::Write::write_all(file, &text)
            .and_then(|()| file.sync_all())
            .map_err(|_| "invalid_binding")
    });
    let manifest = match written {
        Ok(manifest) => manifest,
        Err(reason) => {
            created.remove();
            return Err(reason);
        }
    };
    let published = Published {
        generation: pin.generation,
        manifest,
        pin: created,
    };
    if !permitted() {
        published.remove();
        return Err("authority_lost");
    }
    Ok(published)
}

/// Test-only stand-in for a completed adopt: two real created files that
/// `remove` can delete.
#[cfg(all(test, unix))]
pub(crate) fn fake_published(dir: &std::path::Path) -> Published {
    let write = |file: &mut std::fs::File| {
        std::io::Write::write_all(file, b"{}").map_err(|_| "invalid_pin")
    };
    let manifest = store_pin_with(&dir.join("manifest.json"), write).expect("manifest");
    let pin = store_pin_with(&dir.join("pin.json"), write).expect("pin");
    Published {
        generation: "fake".into(),
        manifest,
        pin,
    }
}

#[cfg(not(unix))]
pub(crate) fn adopt(_request: &AdoptRequest) -> Result<Published, &'static str> {
    Err("unsupported_platform")
}

/// `_codex_notice`: read one frozen request from stdin, deliver add-only
/// through the declared endpoint, and print one typed outcome. There is no
/// queueing, retry, deferral or PTY fallback here.
pub(crate) fn run(args: &[&str]) -> ExitCode {
    if args.iter().any(|arg| *arg == "--help" || *arg == "-h") {
        println!(
            "remuda _codex_notice\nReads one frozen notice request from stdin; returns typed JSON."
        );
        return ExitCode::SUCCESS;
    }
    if !args.is_empty() {
        return emit(Outcome::NotWritten("invalid_args"));
    }
    if !platform_supported() {
        return emit(Outcome::NotWritten("unsupported_platform"));
    }
    let text = match super::agent_notice::read_stdin() {
        Ok(text) => text,
        Err(reason) => return emit(Outcome::NotWritten(reason)),
    };
    let request: Request = match serde_json::from_str(&text) {
        Ok(request) => request,
        Err(_) => return emit(Outcome::NotWritten("invalid_request")),
    };
    if let Err(reason) = validate_request(&request) {
        return emit(Outcome::NotWritten(reason));
    }
    emit(dispatch(&request))
}

fn dispatch(request: &Request) -> Outcome {
    let path = Path::new(&request.binding);
    // Publication is the manifest write (the pin is written first). No leaf
    // under a valid private parent is a proven pre-write state; whether an
    // earlier publication vanished is Butler provenance, not filesystem proof.
    if super::agent_notice::binding_missing(path) {
        return Outcome::NotWritten("binding_missing");
    }
    let manifest = match load_manifest(path) {
        Ok(manifest) => manifest,
        Err(reason) => return Outcome::NotWritten(reason),
    };
    let pin = match load_pin(&pin_path(path)) {
        Ok(pin) => pin,
        Err(reason) => return Outcome::NotWritten(reason),
    };
    if let Err(reason) = validate_manifest(request, &manifest, &pin) {
        return Outcome::NotWritten(reason);
    }
    deliver(request, &manifest, &pin)
}

/// The pin sits beside the manifest in the same private directory; the request
/// cannot name either file.
fn pin_path(manifest_path: &Path) -> PathBuf {
    manifest_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.join("pin.json"))
        .unwrap_or_else(|| PathBuf::from("pin.json"))
}

fn load_manifest(path: &Path) -> Result<Manifest, &'static str> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or("invalid_binding")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid_binding")?;
    let directory = super::private_dir::PrivateDir::open(parent).map_err(|_| "invalid_binding")?;
    let data = directory
        .read_limited(name, MAX_MANIFEST)
        .map_err(|_| "invalid_binding")?;
    let manifest: Manifest = serde_json::from_slice(&data).map_err(|_| "invalid_binding")?;
    if manifest.protocol != PROTOCOL {
        return Err("invalid_binding");
    }
    Ok(manifest)
}

/// Every protocol reply is read through the one correlated, bounded reader.
#[cfg(unix)]
fn exchange(
    stream: &mut std::os::unix::net::UnixStream,
    payload: &Value,
    id: u64,
) -> Result<Value, &'static str> {
    super::codex_tui::websocket_send_text(stream, &payload.to_string()).map_err(|_| "transport")?;
    read_reply(stream, id)
}

#[cfg(unix)]
fn read_reply(stream: &mut std::os::unix::net::UnixStream, id: u64) -> Result<Value, &'static str> {
    for _ in 0..MAX_MESSAGES {
        let text = super::codex_tui::websocket_read_text(stream)
            .map_err(|_| "transport")?
            .ok_or("transport_closed")?;
        let Ok(message) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        // Notifications carry no id and are not our reply.
        if message.get("id").is_none() {
            continue;
        }
        if !correlated(&message, id) {
            return Err("malformed_reply");
        }
        return Ok(message);
    }
    Err("transport")
}

#[cfg(unix)]
fn deliver(request: &Request, manifest: &Manifest, pin: &Pin) -> Outcome {
    let socket = Path::new(&manifest.socket);
    let (stream, live) = match connect_endpoint(socket) {
        Ok(endpoint) => endpoint,
        Err(reason) => return Outcome::NotWritten(reason),
    };
    if !pin_matches(pin, socket, &manifest.thread, live) {
        return Outcome::NotWritten("pin_mismatch");
    }
    let mut stream = match super::codex_tui::websocket_handshake(stream, None) {
        Ok(stream) => stream,
        Err(_) => return Outcome::NotWritten("endpoint_unverified"),
    };
    let id = match preflight(&mut stream, &manifest.thread) {
        Ok(id) => id,
        Err(reason) => return Outcome::NotWritten(reason),
    };
    if !fingerprint_still(socket, live) {
        return Outcome::NotWritten("endpoint_unverified");
    }

    let add = json!({
        "id": id,
        "method": "thread/queue/add",
        "params": {
            "threadId": manifest.thread,
            "clientUserMessageId": request.request_id,
            "input": [{
                "type": "text",
                "text": request.text,
                "text_elements": []
            }]
        }
    });
    // The add may have reached the server even if the send or reply fails, so
    // every post-attempt failure is uncertain and is never retried.
    match exchange(&mut stream, &add, id) {
        Ok(reply) => add_outcome(&reply, id, &request.request_id, &request.text),
        Err(reason) => Outcome::Uncertain(reason),
    }
}

#[cfg(not(unix))]
fn deliver(_request: &Request, _manifest: &Manifest, _pin: &Pin) -> Outcome {
    Outcome::NotWritten("unsupported_platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    const THREAD: &str = "01a1ab7b-7f46-73f0-a024-10063f94f0c3";

    fn codex_native_binding_absent(binding: &str) -> bool {
        !pin_path(std::path::Path::new(binding)).exists()
    }

    fn live() -> LiveIdentity {
        LiveIdentity {
            uid: 501,
            pid: 677,
            start_us: 1_793_877_000,
            dev: 16_777_220,
            ino: 1_234_567,
        }
    }

    #[test]
    fn a_withdrawn_authority_publishes_nothing() {
        let request = AdoptRequest {
            version: 1,
            binding: format!("/tmp/remuda-guard-{}/manifest.json", std::process::id()),
            nonce: "n".into(),
            session: "s".into(),
            socket: format!("/tmp/remuda-guard-{}/missing.sock", std::process::id()),
            thread: THREAD.into(),
        };
        assert!(matches!(
            adopt_guarded(&request, || false),
            Err("authority_lost")
        ));
        assert!(!std::path::Path::new(&request.binding).exists());
        assert!(codex_native_binding_absent(&request.binding));
    }

    #[test]
    fn version_gate_is_the_exact_measured_build() {
        assert!(version_supported("0.166.0"));
        assert!(!version_supported("0.166"));
        assert!(!version_supported("0.166.1"));
        assert!(!version_supported("0.167.0"));
        assert!(!version_supported(""));
    }

    #[test]
    fn initialize_user_agent_yields_the_exact_version() {
        let init: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/codex/initialize.json"
        ))
        .expect("fixture");
        assert_eq!(server_version(&init), Some("0.166.0"));
        let other = json!({"result":{"userAgent":"x/0.167.0 (Linux)"}});
        assert_eq!(server_version(&other), None);
        assert_eq!(server_version(&json!({"result":{}})), None);
    }

    #[test]
    fn thread_must_be_a_canonical_uuid() {
        assert!(thread_uuid(THREAD));
        assert!(thread_uuid("01A1AB7B-7F46-73F0-A024-10063F94F0C3"));
        assert!(!thread_uuid(""));
        assert!(!thread_uuid("01a1ab7b7f4673f0a02410063f94f0c3"));
        assert!(!thread_uuid("01a1ab7b-7f46-73f0-a024-10063f94f0c"));
        assert!(!thread_uuid("../01a1ab7b-7f46-73f0-a024-10063f94f0c3"));
        assert!(!thread_uuid("01a1ab7b-7f46-73f0-a024-10063f94f0cz"));
        assert!(!thread_uuid("01a1ab7b/7f46-73f0-a024-10063f94f0c3"));
    }

    #[test]
    fn generation_covers_every_identity_field() {
        let socket = Path::new("/tmp/codex.sock");
        let base = generation(live(), socket, THREAD);
        assert_eq!(base.len(), 64);
        assert_eq!(base, generation(live(), socket, THREAD));

        let mut changed = live();
        changed.ino += 1;
        assert_ne!(base, generation(changed, socket, THREAD));
        let mut changed = live();
        changed.start_us += 1;
        assert_ne!(base, generation(changed, socket, THREAD));
        let mut changed = live();
        changed.uid += 1;
        assert_ne!(base, generation(changed, socket, THREAD));
        assert_ne!(
            base,
            generation(live(), Path::new("/tmp/other.sock"), THREAD)
        );
        assert_ne!(base, generation(live(), socket, "other"));
    }

    #[test]
    fn a_changed_endpoint_or_recycled_pid_breaks_the_pin() {
        let socket = Path::new("/tmp/codex.sock");
        let pin = pin_for(socket, "nonce-1", "session-1", THREAD, live());
        assert!(pin_matches(&pin, socket, THREAD, live()));

        let mut moved = live();
        moved.ino += 1;
        assert!(!pin_matches(&pin, socket, THREAD, moved));

        let mut recycled = live();
        recycled.pid += 1;
        recycled.start_us += 60_000_000;
        assert!(!pin_matches(&pin, socket, THREAD, recycled));

        assert!(!pin_matches(
            &pin,
            Path::new("/tmp/other.sock"),
            THREAD,
            live()
        ));
        assert!(!pin_matches(&pin, socket, "other", live()));
    }

    #[test]
    fn read_requires_the_captured_root_user_thread() {
        let ok: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/codex/thread_read_tui.json"
        ))
        .expect("fixture");
        let thread = ok
            .pointer("/result/thread/id")
            .and_then(Value::as_str)
            .unwrap();
        assert!(read_matches(&ok, thread));

        let mut ephemeral = ok.clone();
        *ephemeral.pointer_mut("/result/thread/ephemeral").unwrap() = json!(true);
        assert!(!read_matches(&ephemeral, thread));

        let mut forked = ok.clone();
        *forked.pointer_mut("/result/thread/forkedFromId").unwrap() = json!("other");
        assert!(!read_matches(&forked, thread));

        let mut owned = ok.clone();
        *owned.pointer_mut("/result/thread/threadSource").unwrap() = json!("system");
        assert!(!read_matches(&owned, thread));

        let mut direct = ok.clone();
        *direct
            .pointer_mut("/result/thread/canAcceptDirectInput")
            .unwrap() = json!(false);
        assert!(!read_matches(&direct, thread));
        assert!(!read_matches(&json!({"result":{}}), thread));
    }

    #[test]
    fn loaded_list_is_uuid_strings_with_a_cursor() {
        let ok: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/codex/loaded_list_tui.json"
        ))
        .expect("fixture");
        let thread = ok
            .pointer("/result/data/0")
            .and_then(Value::as_str)
            .unwrap();
        assert!(loaded_matches(&ok, thread));
        assert_eq!(loaded_next_cursor(&ok), None);
        assert!(!loaded_matches(
            &json!({"result":{"data":[{"id":THREAD}]}}),
            THREAD
        ));
        assert!(loaded_matches(&json!({"result":{"data":[THREAD]}}), THREAD));
        assert!(!loaded_matches(&json!({"result":{}}), THREAD));
        assert_eq!(
            loaded_next_cursor(&json!({"result":{"data":[],"nextCursor":"c1"}})),
            Some("c1")
        );
    }

    #[test]
    fn receipt_is_the_captured_single_text_input() {
        let ok: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/codex/queue_add_tui.json"
        ))
        .expect("fixture");
        let text = ok
            .pointer("/result/queuedSubmission/input/0/text")
            .and_then(Value::as_str)
            .unwrap();
        let client = ok
            .pointer("/result/queuedSubmission/clientUserMessageId")
            .and_then(Value::as_str)
            .unwrap();
        assert!(receipt_matches(&ok, client, text));
        assert!(!receipt_matches(&ok, "msg_other", text));
        assert!(!receipt_matches(&ok, client, "different"));

        let mut extra = ok.clone();
        extra
            .pointer_mut("/result/queuedSubmission/input")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"text","text":"extra","text_elements":[]}));
        assert!(!receipt_matches(&extra, client, text));

        let mut no_elements = ok.clone();
        no_elements
            .pointer_mut("/result/queuedSubmission/input/0")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("text_elements");
        assert!(!receipt_matches(&no_elements, client, text));

        let empty_id = json!({"result":{"queuedSubmission":{"id":"","clientUserMessageId":client,
            "input":[{"type":"text","text":text,"text_elements":[]}]}}});
        assert!(!receipt_matches(&empty_id, client, text));
        assert!(!receipt_matches(&json!({"result":{}}), client, text));
        assert!(rpc_matches(&ok, 2));
        assert!(!rpc_matches(&ok, 3));
    }

    #[cfg(unix)]
    #[test]
    fn socket_identity_reads_uid_pid_and_inode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("remuda-codex-native-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("temp dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("0700");
        let path = dir.join("app.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let stream = std::os::unix::net::UnixStream::connect(&path).expect("connect");
        // A shared-mode socket is refused even when the owner is correct.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("0755");
        assert!(socket_identity(&stream, &path).is_none());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("0600");
        let live = socket_identity(&stream, &path).expect("identity");
        let metadata = std::fs::metadata(&path).expect("stat");
        use std::os::unix::fs::MetadataExt;
        assert_eq!(live.uid, unsafe { libc::geteuid() });
        assert_eq!(live.pid, std::process::id());
        assert_eq!(live.dev, metadata.dev());
        assert_eq!(live.ino, metadata.ino());
        assert!(live.start_us > 0);

        let not_socket = dir.join("plain");
        std::fs::write(&not_socket, b"x").expect("plain");
        assert!(socket_identity(&stream, &not_socket).is_none());
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sample_request() -> Request {
        Request {
            version: 1,
            binding: "/private/tmp/remuda-binding/manifest.json".into(),
            nonce: "nonce-1".into(),
            session: "session-1".into(),
            instance: "instance-1".into(),
            request_id: "msg_01ABC".into(),
            text: super::super::agent_notice::canonical_notice(&["01ABC".into()]),
            message_ids: vec!["01ABC".into()],
        }
    }

    fn sample_manifest() -> Manifest {
        Manifest {
            schema: PIN_SCHEMA,
            protocol: PROTOCOL.into(),
            nonce: "nonce-1".into(),
            session: "session-1".into(),
            mode: MODE_ADD_ONLY.into(),
            generation: generation(live(), Path::new("/tmp/codex.sock"), THREAD),
            socket: "/tmp/codex.sock".into(),
            thread: THREAD.into(),
        }
    }

    #[test]
    fn request_grammar_is_exact() {
        assert!(validate_request(&sample_request()).is_ok());

        let mut relative = sample_request();
        relative.binding = "manifest.json".into();
        assert_eq!(validate_request(&relative), Err("invalid_request"));

        let mut retarget = sample_request();
        retarget.text = "Butler message 01ABC arrived.".into();
        assert_eq!(validate_request(&retarget), Err("invalid_request"));

        let mut ids = sample_request();
        ids.message_ids = vec!["01ABC".into(), "01DEF".into()];
        assert_eq!(validate_request(&ids), Err("invalid_request"));

        let mut request_id = sample_request();
        request_id.request_id = "probe".into();
        assert_eq!(validate_request(&request_id), Err("invalid_request"));

        let mut empty_nonce = sample_request();
        empty_nonce.nonce.clear();
        assert_eq!(validate_request(&empty_nonce), Err("invalid_request"));
    }

    #[test]
    fn the_caller_cannot_supply_a_socket_or_thread() {
        let with_socket = json!({
            "version": 1,
            "binding": "/private/tmp/remuda-binding/manifest.json",
            "nonce": "nonce-1",
            "session": "session-1",
            "instance": "instance-1",
            "request_id": "msg_01ABC",
            "text": super::super::agent_notice::canonical_notice(&["01ABC".into()]),
            "message_ids": ["01ABC"],
            "socket": "/tmp/app.sock",
        });
        assert!(serde_json::from_value::<Request>(with_socket).is_err());
        let thread = json!({
            "version": 1,
            "binding": "/private/tmp/remuda-binding/manifest.json",
            "nonce": "nonce-1",
            "session": "session-1",
            "instance": "instance-1",
            "request_id": "msg_01ABC",
            "text": super::super::agent_notice::canonical_notice(&["01ABC".into()]),
            "message_ids": ["01ABC"],
            "thread": THREAD,
        });
        assert!(serde_json::from_value::<Request>(thread).is_err());
    }

    #[test]
    fn the_manifest_alone_names_the_endpoint_and_mode() {
        let request = sample_request();
        let pin = pin_for(
            Path::new("/tmp/codex.sock"),
            "nonce-1",
            "session-1",
            THREAD,
            live(),
        );
        let manifest = sample_manifest();
        assert!(validate_manifest(&request, &manifest, &pin).is_ok());

        let mut swapped = manifest.clone();
        swapped.socket = "/tmp/other.sock".into();
        assert_eq!(
            validate_manifest(&request, &swapped, &pin),
            Err("invalid_binding")
        );

        let mut mode = manifest.clone();
        mode.mode = "queue-start".into();
        assert_eq!(
            validate_manifest(&request, &mode, &pin),
            Err("invalid_binding")
        );

        let mut stale = manifest.clone();
        stale.generation = "stale".into();
        assert_eq!(
            validate_manifest(&request, &stale, &pin),
            Err("invalid_binding")
        );

        let mut nonce = manifest.clone();
        nonce.nonce = "nonce-2".into();
        assert_eq!(
            validate_manifest(&request, &nonce, &pin),
            Err("invalid_binding")
        );

        let mut protocol = manifest.clone();
        protocol.protocol = "codex-uds-v1".into();
        assert_eq!(
            validate_manifest(&request, &protocol, &pin),
            Err("invalid_binding")
        );

        let mut extra = serde_json::to_value(&manifest).unwrap();
        extra["callback"] = json!("x");
        assert!(serde_json::from_value::<Manifest>(extra).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn pin_round_trips_and_never_adopts_a_preexisting_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("remuda-codex-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("0700");
        let path = dir.join("pin.json");
        let live = live();
        let pin = pin_for(
            Path::new("/tmp/codex.sock"),
            "nonce-1",
            "session-1",
            THREAD,
            live,
        );

        let _created = store_pin(&path, &pin).expect("store");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let loaded = load_pin(&path).expect("load");
        assert_eq!(loaded, pin);
        assert!(pin_matches(
            &loaded,
            Path::new("/tmp/codex.sock"),
            THREAD,
            live
        ));

        assert!(store_pin(&path, &pin).is_err());
        let mut replacement = pin.clone();
        replacement.generation = "replacement".into();
        let failed = store_pin_with(&path, |file| {
            std::io::Write::write_all(file, b"partial").map_err(|_| "invalid_pin")
        });
        assert!(failed.is_err());
        assert_eq!(load_pin(&path).expect("original survives"), pin);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn post_add_errors_and_malformed_replies_stay_uncertain() {
        let ok: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/codex/queue_add_tui.json"
        ))
        .expect("fixture");
        let text = ok
            .pointer("/result/queuedSubmission/input/0/text")
            .and_then(Value::as_str)
            .unwrap();
        let client = ok
            .pointer("/result/queuedSubmission/clientUserMessageId")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(add_outcome(&ok, 2, client, text), Outcome::Accepted);

        // A valid-looking receipt under another request id must not accept.
        assert_eq!(
            add_outcome(&ok, 3, client, text),
            Outcome::Uncertain("malformed_reply")
        );

        // A mixed result+error reply is malformed, never accepted.
        let mut mixed = ok.clone();
        mixed["error"] = json!({"code": -32603});
        assert_eq!(
            add_outcome(&mixed, 2, client, text),
            Outcome::Uncertain("malformed_reply")
        );

        // A reply with no id at all cannot be correlated to our add.
        let mut no_id = ok.clone();
        no_id.as_object_mut().unwrap().remove("id");
        assert_eq!(
            add_outcome(&no_id, 2, client, text),
            Outcome::Uncertain("malformed_reply")
        );

        for error in [
            json!({"id": 2, "error": {"code": -32600}}),
            json!({"id": 2, "error": null}),
            json!({"id": 2, "error": {"message": "opaque"}}),
            json!({"id": 2, "error": {"code": -32603}}),
        ] {
            assert_eq!(
                add_outcome(&error, 2, client, text),
                Outcome::Uncertain("add_error")
            );
        }
        assert_eq!(
            add_outcome(&json!({"error": {"code": -32603}}), 2, client, text),
            Outcome::Uncertain("malformed_reply")
        );
        assert_eq!(Outcome::Accepted.to_json()["outcome"], "accepted_queue");

        assert_eq!(
            add_outcome(&json!({"id": 2, "result": {}}), 2, client, text),
            Outcome::Uncertain("receipt_mismatch")
        );
        assert_eq!(
            Outcome::Uncertain("transport").to_json()["outcome"],
            "uncertain"
        );
        assert_eq!(
            Outcome::NotWritten("dead_socket").to_json()["outcome"],
            "definitely_not_written"
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_pin_rejects_an_extra_field() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("remuda-codex-pin-extra-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("0700");
        let path = dir.join("pin.json");
        let pin = pin_for(
            Path::new("/tmp/codex.sock"),
            "nonce-1",
            "session-1",
            THREAD,
            live(),
        );
        let mut data = serde_json::to_value(&pin).unwrap();
        data["socket_owner"] = json!("invented");
        std::fs::write(&path, data.to_string()).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("0600");
        assert!(load_pin(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn adopt_writes_a_bound_pin_and_manifest_once() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("remuda-codex-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("0700");
        let socket_path = dir.join("app.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind");
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
            .expect("0600");
        let server =
            std::thread::spawn(move || serve(listener, THREAD, "", "", Behavior::PreflightOnly));
        let binding = dir.join("manifest.json");
        let request = AdoptRequest {
            version: 1,
            binding: binding.to_string_lossy().into_owned(),
            nonce: "nonce-1".into(),
            session: "session-1".into(),
            socket: socket_path.to_string_lossy().into_owned(),
            thread: THREAD.into(),
        };
        assert!(validate_adopt(&request).is_ok());

        let published = adopt(&request).expect("adopt");
        let generation = published.generation.clone();
        assert_eq!(generation.len(), 64);
        let methods = server.join().expect("server");
        assert!(methods.iter().any(|method| method == "initialize"));
        assert!(methods.iter().any(|method| method == "thread/read"));
        assert!(methods.iter().any(|method| method == "thread/loaded/list"));
        assert!(!methods.iter().any(|method| method == "thread/queue/add"));
        let pin = load_pin(&dir.join("pin.json")).expect("pin");
        let manifest = load_manifest(&binding).expect("manifest");
        assert!(validate_manifest(&sample_request(), &manifest, &pin).is_ok());
        assert_eq!(pin.generation, generation);
        assert_eq!(pin.socket, socket_path.to_string_lossy());
        assert_eq!(pin.thread, THREAD);
        assert_eq!(manifest.mode, MODE_ADD_ONLY);

        // Re-adoption never overwrites an existing declaration.
        assert!(adopt(&request).is_err());
        assert_eq!(
            load_pin(&dir.join("pin.json"))
                .expect("pin survives")
                .generation,
            generation
        );

        // Identity-based removal takes only the published inodes.
        published.remove();
        assert!(!binding.exists());
        assert!(!dir.join("pin.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn adoption_requires_an_absolute_socket_and_uuid() {
        let request = AdoptRequest {
            version: 1,
            binding: "/private/tmp/b/manifest.json".into(),
            nonce: "nonce-1".into(),
            session: "session-1".into(),
            socket: "app.sock".into(),
            thread: THREAD.into(),
        };
        assert_eq!(validate_adopt(&request), Err("invalid_request"));
        let mut bad_thread = request;
        bad_thread.socket = "/tmp/app.sock".into();
        bad_thread.thread = "../../app".into();
        assert_eq!(validate_adopt(&bad_thread), Err("invalid_request"));
    }

    #[cfg(unix)]
    fn socket_live(path: &Path) -> LiveIdentity {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).expect("stat");
        let started = super::super::opencode_tui::process_identity(std::process::id())
            .expect("process identity");
        LiveIdentity {
            uid: unsafe { libc::geteuid() },
            pid: std::process::id(),
            start_us: started.started_us,
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    #[cfg(unix)]
    fn ws_accept(stream: &mut std::os::unix::net::UnixStream) {
        use std::io::{Read, Write};
        let mut request = Vec::new();
        let mut byte = [0; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("handshake byte");
            request.push(byte[0]);
        }
        let text = String::from_utf8(request).expect("ascii handshake");
        let key = text
            .lines()
            .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
            .expect("key")
            .trim();
        let accept =
            remuda_native::cluster::encoding::encode_base64(&super::super::codex_tui::sha1(
                format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
            ));
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        stream.write_all(response.as_bytes()).expect("upgrade");
    }

    #[cfg(unix)]
    fn ws_read(stream: &mut std::os::unix::net::UnixStream) -> Value {
        use std::io::Read;
        let mut header = [0; 2];
        stream.read_exact(&mut header).expect("frame header");
        assert_eq!(header[1] & 0x80, 0x80, "client frames must be masked");
        let mut length = (header[1] & 0x7f) as u64;
        if length == 126 {
            let mut extended = [0; 2];
            stream.read_exact(&mut extended).expect("length");
            length = u16::from_be_bytes(extended) as u64;
        } else if length == 127 {
            let mut extended = [0; 8];
            stream.read_exact(&mut extended).expect("length");
            length = u64::from_be_bytes(extended);
        }
        let mut mask = [0; 4];
        stream.read_exact(&mut mask).expect("mask");
        let mut payload = vec![0; length as usize];
        stream.read_exact(&mut payload).expect("payload");
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
        serde_json::from_slice(&payload).expect("json")
    }

    #[cfg(unix)]
    fn ws_write(stream: &mut std::os::unix::net::UnixStream, value: &Value) {
        use std::io::Write;
        let body = value.to_string();
        let body = body.as_bytes();
        let mut frame = vec![0x81];
        match body.len() {
            len @ 0..=125 => frame.push(len as u8),
            len @ 126..=65535 => {
                frame.push(126);
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            }
            len => {
                frame.push(127);
                frame.extend_from_slice(&(len as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(body);
        stream.write_all(&frame).expect("frame");
    }

    #[cfg(unix)]
    #[derive(Clone, Copy, PartialEq)]
    enum Behavior {
        AcceptAdd,
        WrongAddId,
        MixedEnvelope,
        CloseAfterAdd,
        PreflightOnly,
    }

    #[cfg(unix)]
    fn serve(
        listener: std::os::unix::net::UnixListener,
        thread: &str,
        text: &str,
        client: &str,
        behavior: Behavior,
    ) -> Vec<String> {
        let (mut stream, _) = listener.accept().expect("accept");
        ws_accept(&mut stream);
        let mut methods = Vec::new();
        loop {
            let message = ws_read(&mut stream);
            let Some(method) = message.get("method").and_then(Value::as_str) else {
                continue;
            };
            methods.push(method.to_string());
            let Some(id) = message.get("id").and_then(Value::as_u64) else {
                if behavior == Behavior::PreflightOnly && method == "initialized" {
                    continue;
                }
                continue;
            };
            if method == "thread/queue/add" && behavior == Behavior::CloseAfterAdd {
                break;
            }
            let reply = match method {
                "initialize" => json!({
                    "id": id,
                    "result": {"userAgent": "codex-cli/0.166.0 (Mac OS X; arm64)"}
                }),
                "thread/read" => json!({
                    "id": id,
                    "result": {"thread": {
                        "id": thread, "sessionId": thread, "ephemeral": false,
                        "forkedFromId": null, "parentThreadId": null,
                        "threadSource": "user", "canAcceptDirectInput": true
                    }}
                }),
                "thread/loaded/list" => json!({
                    "id": id,
                    "result": {"data": [thread], "nextCursor": null}
                }),
                "thread/queue/add" => {
                    let mut reply = json!({
                        "id": if behavior == Behavior::WrongAddId { id + 1 } else { id },
                        "result": {"queuedSubmission": {
                            "id": "q1", "clientUserMessageId": client,
                            "input": [{"type": "text", "text": text, "text_elements": []}]
                        }}
                    });
                    if behavior == Behavior::MixedEnvelope {
                        reply["error"] = json!({"code": -32603, "message": "opaque"});
                    }
                    reply
                }
                _ => json!({"id": id, "error": {"code": -32601, "message": "unknown"}}),
            };
            ws_write(&mut stream, &reply);
            if method == "thread/queue/add" && behavior != Behavior::PreflightOnly {
                break;
            }
            if method == "thread/loaded/list" && behavior == Behavior::PreflightOnly {
                break;
            }
        }
        methods
    }

    #[cfg(unix)]
    fn transport_fixture(
        name: &str,
        behavior: Behavior,
    ) -> (Outcome, Vec<String>, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "remuda-codex-transport-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("0700");
        let socket = dir.join("app.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).expect("0600");
        let request = sample_request();
        let server = std::thread::spawn({
            let text = request.text.clone();
            let client = request.request_id.clone();
            move || serve(listener, THREAD, &text, &client, behavior)
        });
        let live = socket_live(&socket);
        let pin = pin_for(&socket, "nonce-1", "session-1", THREAD, live);
        let manifest = Manifest {
            schema: PIN_SCHEMA,
            protocol: PROTOCOL.into(),
            nonce: "nonce-1".into(),
            session: "session-1".into(),
            mode: MODE_ADD_ONLY.into(),
            generation: pin.generation.clone(),
            socket: socket.to_string_lossy().into_owned(),
            thread: THREAD.into(),
        };
        let outcome = deliver(&request, &manifest, &pin);
        let methods = server.join().expect("server");
        (outcome, methods, dir)
    }

    #[cfg(unix)]
    #[test]
    fn transport_accepts_only_a_correlated_add_receipt() {
        let (outcome, methods, dir) = transport_fixture("ok", Behavior::AcceptAdd);
        assert_eq!(outcome, Outcome::Accepted);
        assert_eq!(adds(&methods), 1);
        assert!(!methods.iter().any(|method| method == "thread/queue/start"));
        assert!(!methods.iter().any(|method| method == "thread/queue/update"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn transport_rejects_a_valid_looking_receipt_under_another_id() {
        let (outcome, methods, dir) = transport_fixture("wrong-id", Behavior::WrongAddId);
        assert_eq!(outcome, Outcome::Uncertain("malformed_reply"));
        assert_eq!(adds(&methods), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn transport_rejects_a_mixed_result_and_error_envelope() {
        let (outcome, methods, dir) = transport_fixture("mixed", Behavior::MixedEnvelope);
        assert_eq!(outcome, Outcome::Uncertain("malformed_reply"));
        assert_eq!(adds(&methods), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn transport_treats_eof_after_the_add_as_uncertain() {
        let (outcome, methods, dir) = transport_fixture("eof", Behavior::CloseAfterAdd);
        assert_eq!(outcome, Outcome::Uncertain("transport_closed"));
        assert_eq!(adds(&methods), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn adds(methods: &[String]) -> usize {
        methods
            .iter()
            .filter(|method| method.as_str() == "thread/queue/add")
            .count()
    }
}
