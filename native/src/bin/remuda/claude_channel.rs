//! Dedicated opt-in Claude channel server.
//!
//! Claude Code spawns a channel as an MCP server over stdio. This module is that
//! server: it declares only `claude/channel`, waits for the MCP initialized
//! notification, publishes a private ingress record, and writes channel
//! notifications through one serialized stdout writer. The ordinary
//! `remuda mcp` server is untouched.
//!
//! The first slice is intentionally one-way. It has no reply tool and no
//! permission relay. A pipe write is not admission: after any possible
//! notification byte the ingress answer is `uncertain` with
//! `channel_emitted_unconfirmed`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::opencode_tui::process_identity;
use super::private_dir::{CreateError, CreatedFile, PrivateDir};

/// The strict manifest protocol this slice writes.
pub const PROTOCOL: &str = "claude-channel-v1";
/// The one MCP protocol era this slice has actually tested.
pub const SUPPORTED_PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_LINE: usize = 64 * 1024;
const MAX_TEXT: usize = 1024;
const MAX_QUEUE: usize = 64;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// The canonical pointer is supplied by the caller so this module never grows
/// a second copy of the inbox wording.
pub type CanonicalPointer = fn(&[String]) -> String;

/// One channel server launch.
#[derive(Clone)]
pub struct Config {
    pub binding_dir: PathBuf,
    pub nonce: String,
    pub session: String,
    pub canonical: CanonicalPointer,
}

/// The verb entry point. Codex owns the `remuda.rs` registration; this
/// function only reads the owned child environment and runs the server.
pub fn run(args: &[&str]) -> std::process::ExitCode {
    if args.iter().any(|arg| *arg == "--help" || *arg == "-h") {
        println!(
            "remuda _claude_channel\nRun the opt-in Claude channel MCP server on stdin/stdout.\n\
             The binding directory, nonce and session come from the owned child environment."
        );
        return std::process::ExitCode::SUCCESS;
    }
    if !args.is_empty() {
        eprintln!("remuda: _claude_channel takes no arguments");
        return std::process::ExitCode::FAILURE;
    }
    let (binding_dir, nonce, session) = match owned_environment() {
        Ok(values) => values,
        Err(reason) => {
            eprintln!("remuda: Claude channel: {reason}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let config = Config {
        binding_dir,
        nonce,
        session,
        canonical: super::agent_notice::canonical_notice,
    };
    match serve(BufReader::new(std::io::stdin()), std::io::stdout(), config) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("remuda: Claude channel: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn owned_environment() -> Result<(PathBuf, String, String), String> {
    let dir = std::env::var("REMUDA_CLAUDE_BINDING_DIR")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or("REMUDA_CLAUDE_BINDING_DIR is required")?;
    let nonce = std::env::var("REMUDA_BUTLER_BINDING_NONCE")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or("REMUDA_BUTLER_BINDING_NONCE is required")?;
    let session = std::env::var("REMUDA_BUTLER_SESSION")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or("REMUDA_BUTLER_SESSION is required")?;
    Ok((PathBuf::from(dir), nonce, session))
}

/// Serve one channel over the given stdio streams. Public for the unit and
/// stdio tests; production calls [`run`].
enum ReaderEvent {
    Line(String),
    Eof,
    Error,
}

pub fn serve<R, W>(reader: R, writer: W, config: Config) -> std::io::Result<()>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let shutdown = Arc::new(AtomicBool::new(false));
    let (writer_tx, writer_rx) = mpsc::sync_channel::<WriterJob>(MAX_QUEUE);
    let writer_thread = thread::spawn({
        let shutdown = shutdown.clone();
        move || writer_loop(writer, writer_rx, shutdown)
    });
    let (reader_tx, reader_rx) = mpsc::sync_channel::<ReaderEvent>(MAX_QUEUE);
    let reader_thread = thread::spawn(move || reader_loop(reader, reader_tx));
    let mut session = Session::new(config, writer_tx.clone(), shutdown.clone());
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        match reader_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ReaderEvent::Line(line)) => session.handle_line(&line),
            Ok(ReaderEvent::Eof | ReaderEvent::Error) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    shutdown.store(true, Ordering::SeqCst);
    let ingress = session.ingress.take();
    let published = session.published.take();
    drop(session);
    if let Some(ingress) = ingress {
        let _ = ingress.join();
    }
    drop(writer_tx);
    // A stdout write can block in the OS; do not hold the lifecycle on it.
    if writer_thread.is_finished() {
        let _ = writer_thread.join();
    }
    drop(reader_thread);
    if let Some(published) = published {
        published.cleanup();
    }
    Ok(())
}

fn reader_loop<R: BufRead>(mut reader: R, events: SyncSender<ReaderEvent>) {
    loop {
        match read_bounded_line(&mut reader, MAX_LINE) {
            Ok(Some(line)) => {
                if events.send(ReaderEvent::Line(line)).is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = events.send(ReaderEvent::Eof);
                return;
            }
            Err(_) => {
                let _ = events.send(ReaderEvent::Error);
                return;
            }
        }
    }
}

/// Read one newline-terminated line without allocating past `max` bytes.
fn read_bounded_line<R: BufRead>(reader: &mut R, max: usize) -> std::io::Result<Option<String>> {
    let mut buffer = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if buffer.is_empty() {
                return Ok(None);
            }
            break;
        }
        if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
            let take = position + 1;
            if buffer.len() + take > max {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "line too long",
                ));
            }
            buffer.extend_from_slice(&available[..take]);
            reader.consume(take);
            break;
        }
        let take = available.len().min(max + 1 - buffer.len());
        if take == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line too long",
            ));
        }
        buffer.extend_from_slice(&available[..take]);
        reader.consume(take);
        if buffer.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line too long",
            ));
        }
    }
    String::from_utf8(buffer)
        .map(Some)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "not utf-8"))
}

struct Session {
    config: Config,
    writer: SyncSender<WriterJob>,
    shutdown: Arc<AtomicBool>,
    negotiation: Option<Negotiation>,
    initialize_response_ok: bool,
    initialized: bool,
    published: Option<Published>,
    ingress: Option<JoinHandle<()>>,
}

impl Session {
    fn new(config: Config, writer: SyncSender<WriterJob>, shutdown: Arc<AtomicBool>) -> Self {
        Self {
            config,
            writer,
            shutdown,
            negotiation: None,
            initialize_response_ok: false,
            initialized: false,
            published: None,
            ingress: None,
        }
    }

    fn handle_line(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        let request: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                self.reply_error(Value::Null, -32700, &format!("parse error: {error}"));
                return;
            }
        };
        let Some(object) = request.as_object() else {
            self.reply_error(Value::Null, -32600, "invalid request: not an object");
            return;
        };
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            self.reply_error(Value::Null, -32600, "invalid request: jsonrpc must be 2.0");
            return;
        }
        let id = object.get("id").cloned();
        if let Some(id) = &id {
            if !(id.is_string() || id.is_number() || id.is_null()) {
                self.reply_error(Value::Null, -32600, "invalid request: bad id");
                return;
            }
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            if let Some(id) = id {
                self.reply_error(id, -32600, "invalid request: missing method");
            }
            return;
        };
        match id {
            None => self.notification(method, object.get("params")),
            Some(id) => self.request(id, method, object.get("params")),
        }
    }

    fn request(&mut self, id: Value, method: &str, params: Option<&Value>) {
        match method {
            "initialize" => {
                if self.negotiation.is_some() || self.initialized {
                    self.reply_error(id, -32600, "invalid request: initialize already completed");
                    return;
                }
                let Some(requested) = params
                    .and_then(Value::as_object)
                    .and_then(|params| params.get("protocolVersion"))
                    .and_then(Value::as_str)
                else {
                    self.reply_error(
                        id,
                        -32602,
                        "invalid params: initialize needs protocolVersion",
                    );
                    return;
                };
                let negotiation = negotiate(requested);
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": negotiation.version,
                        "capabilities": {"experimental": {"claude/channel": {}}},
                        "serverInfo": {
                            "name": "remuda-channel",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "instructions": "Butler messages arrive as <channel source=\"remuda-channel\">. \
                            Read them with MCP butler_inbox (or remuda butler inbox). \
                            No reply is expected on this channel.",
                    }
                });
                self.initialize_response_ok = self.send_reply_wait(reply);
                self.negotiation = Some(negotiation);
            }
            "ping" => self.send_reply(json!({"jsonrpc": "2.0", "id": id, "result": {}})),
            "tools/list" => self.send_reply(json!({
                "jsonrpc": "2.0", "id": id, "result": {"tools": []}
            })),
            "tools/call" => self.reply_error(id, -32601, "method not found: tools/call"),
            other => self.reply_error(id, -32601, &format!("method not found: {other}")),
        }
    }

    fn notification(&mut self, method: &str, _params: Option<&Value>) {
        if method != "notifications/initialized" || self.initialized || !self.initialize_response_ok
        {
            return;
        }
        let Some(negotiation) = self.negotiation else {
            return;
        };
        self.initialized = true;
        if !negotiation.accepted {
            eprintln!(
                "remuda: Claude channel: unsupported MCP protocol era; ingress stays unavailable"
            );
            return;
        }
        match publish(&self.config, negotiation.version) {
            Ok(mut published) => {
                let context = IngressContext {
                    binding_dir: self.config.binding_dir.clone(),
                    capability: published.capability_token.clone(),
                    transport_id: published.transport_id.clone(),
                    nonce: self.config.nonce.clone(),
                    session: self.config.session.clone(),
                    canonical: self.config.canonical,
                };
                let listener = published.listener.take();
                if let Some(listener) = listener {
                    let writer = self.writer.clone();
                    let shutdown = self.shutdown.clone();
                    self.ingress = Some(thread::spawn(move || {
                        ingress_loop(listener, context, writer, shutdown)
                    }));
                }
                self.published = Some(published);
            }
            Err(reason) => {
                eprintln!("remuda: Claude channel: cannot publish ingress: {reason}");
            }
        }
    }

    fn send_reply(&self, value: Value) {
        let line = value.to_string();
        if self.writer.try_send(WriterJob::line(line)).is_err() {
            self.fail_lifecycle();
        }
    }

    /// The initialize response must actually reach the writer before ingress
    /// can be published. A failed, full or timed-out response is not readiness.
    fn send_reply_wait(&self, value: Value) -> bool {
        let (done, outcome) = mpsc::channel();
        let job = WriterJob {
            line: value.to_string(),
            done: Some(done),
        };
        if self.writer.try_send(job).is_err() {
            self.fail_lifecycle();
            return false;
        }
        let written = matches!(
            outcome.recv_timeout(WRITE_TIMEOUT),
            Ok(WriteOutcome::Written)
        );
        if !written {
            self.fail_lifecycle();
        }
        written
    }

    /// A full or disconnected reply queue means the stdout lifecycle is stuck;
    /// stop reading stdin instead of blocking the event loop on `send`.
    fn fail_lifecycle(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    fn reply_error(&self, id: Value, code: i32, message: &str) {
        self.send_reply(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Negotiation {
    version: &'static str,
    accepted: bool,
}

/// Echo the requested era only when it is the one this slice tested. Older
/// 2024/2025 requests may negotiate to the tested era; an unknown modern era
/// is answered honestly but does not publish ingress.
fn negotiate(requested: &str) -> Negotiation {
    if requested == SUPPORTED_PROTOCOL_VERSION {
        return Negotiation {
            version: SUPPORTED_PROTOCOL_VERSION,
            accepted: true,
        };
    }
    let legacy = requested.starts_with("2024-") || requested.starts_with("2025-");
    Negotiation {
        version: SUPPORTED_PROTOCOL_VERSION,
        accepted: legacy,
    }
}

struct WriterJob {
    line: String,
    done: Option<mpsc::Sender<WriteOutcome>>,
}

impl WriterJob {
    fn line(line: String) -> Self {
        Self { line, done: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteOutcome {
    Written,
    Closed { bytes: usize },
    Partial { bytes: usize },
    Uncertain { bytes: usize },
}

fn writer_loop<W: Write>(
    mut writer: W,
    jobs: mpsc::Receiver<WriterJob>,
    shutdown: Arc<AtomicBool>,
) {
    for job in jobs {
        let outcome = write_line(&mut writer, &job.line);
        if let Some(done) = job.done {
            let _ = done.send(outcome);
        }
        if !matches!(outcome, WriteOutcome::Written) {
            shutdown.store(true, Ordering::SeqCst);
            break;
        }
    }
}

/// Write one newline-framed JSON line. Track how many bytes reached the
/// transport so a failure before the first byte is provably not written.
fn write_line<W: Write>(writer: &mut W, line: &str) -> WriteOutcome {
    let bytes = line.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        match writer.write(&bytes[offset..]) {
            Ok(0) => return WriteOutcome::Closed { bytes: offset },
            Ok(written) => offset += written,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                return if offset == 0 {
                    WriteOutcome::Closed { bytes: 0 }
                } else {
                    WriteOutcome::Partial { bytes: offset }
                }
            }
        }
    }
    if writer.write_all(b"\n").is_err() || writer.flush().is_err() {
        return WriteOutcome::Uncertain { bytes: offset };
    }
    WriteOutcome::Written
}

/// The private ingress record. The capability secret is in a separate 0600
/// file; this record never carries it.
struct Published {
    listener: Option<std::os::unix::net::UnixListener>,
    socket_path: PathBuf,
    socket_id: FileId,
    manifest: CreatedFile,
    capability: CreatedFile,
    bridge: CreatedFile,
    capability_token: String,
    transport_id: String,
}

impl Published {
    fn cleanup(mut self) {
        drop(self.listener.take());
        remove_if_id(&self.socket_path, self.socket_id);
        self.manifest.remove();
        self.capability.remove();
        self.bridge.remove();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
}

fn publish(config: &Config, protocol_version: &str) -> Result<Published, String> {
    let directory = PrivateDir::open(&config.binding_dir)?;
    let transport_id = random_hex(16)?;
    let capability = random_hex(32)?;
    let socket_path = config.binding_dir.join("ingress.sock");
    let capability_path = config.binding_dir.join("capability");
    reject_existing(&socket_path)?;

    let listener = std::os::unix::net::UnixListener::bind(&socket_path)
        .map_err(|error| format!("cannot bind ingress socket: {error}"))?;
    if let Err(error) = set_mode(&socket_path, 0o600) {
        let _ = std::fs::remove_file(&socket_path);
        return Err(error);
    }
    let socket_id = match file_id(&socket_path) {
        Ok(id) => id,
        Err(error) => {
            let _ = std::fs::remove_file(&socket_path);
            return Err(error);
        }
    };

    let capability_file = match create_file(&directory, "capability", capability.as_bytes()) {
        Ok(file) => file,
        Err(error) => {
            remove_if_id(&socket_path, socket_id);
            return Err(error);
        }
    };

    let bridge = match process_identity(std::process::id()) {
        Some(identity) => identity,
        None => {
            remove_if_id(&socket_path, socket_id);
            capability_file.remove();
            return Err("cannot read the bridge process identity".into());
        }
    };
    let parent_started_us = process_identity(bridge.parent).map(|parent| parent.started_us);
    let manifest = json!({
        "schema": 1,
        "protocol": PROTOCOL,
        "nonce": config.nonce,
        "session": config.session,
        "endpoint": format!("unix://{}", socket_path.display()),
        "bridge_pid": std::process::id(),
        "bridge_started_us": bridge.started_us,
        "socket_dev": socket_id.dev,
        "socket_ino": socket_id.ino,
        "transport_id": transport_id,
        "protocol_version": protocol_version,
        "state": "mcp_initialized",
        "capability_file": capability_path.to_string_lossy().to_string(),
    });
    let manifest_file = match create_json_file(&directory, "channel.json", &manifest) {
        Ok(file) => file,
        Err(error) => {
            remove_if_id(&socket_path, socket_id);
            capability_file.remove();
            return Err(error);
        }
    };
    // Observed parent identity is a diagnostic only. It is never a guessed TUI
    // identity and never part of the strict channel manifest.
    let bridge_record = json!({
        "bridge_pid": std::process::id(),
        "bridge_started_us": bridge.started_us,
        "parent_pid": bridge.parent,
        "parent_started_us": parent_started_us,
    });
    let bridge_file = match create_json_file(&directory, "bridge.json", &bridge_record) {
        Ok(file) => file,
        Err(error) => {
            remove_if_id(&socket_path, socket_id);
            capability_file.remove();
            manifest_file.remove();
            return Err(error);
        }
    };

    Ok(Published {
        listener: Some(listener),
        socket_path,
        socket_id,
        manifest: manifest_file,
        capability: capability_file,
        bridge: bridge_file,
        capability_token: capability,
        transport_id,
    })
}

fn reject_existing(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(format!("{} already exists", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn create_file(directory: &PrivateDir, name: &str, bytes: &[u8]) -> Result<CreatedFile, String> {
    // Write the full leaf under a temporary name, then publish the final name
    // with one rename. A reader never sees a created-but-empty manifest.
    let suffix = random_hex(8).unwrap_or_else(|_| std::process::id().to_string());
    let temp = format!(".{name}.tmp.{suffix}");
    let (mut file, id) = directory
        .create_new_checked(&temp)
        .map_err(|error| error.to_string())?;
    let staged = CreatedFile::new(directory.clone(), temp.clone(), id);
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        staged.remove();
        return Err(format!("cannot write {name}: {error}"));
    }
    drop(file);
    match directory.leaf_absent(name) {
        Ok(true) => {}
        Ok(false) => {
            staged.remove();
            return Err(format!("{name} already exists"));
        }
        Err(error) => {
            staged.remove();
            return Err(error);
        }
    }
    if let Err(error) = directory.rename(&temp, name) {
        staged.remove();
        return Err(error);
    }
    let published = CreatedFile::new(directory.clone(), name.to_string(), id);
    if let Err(error) = directory.sync() {
        published.remove();
        return Err(error);
    }
    Ok(published)
}

fn create_json_file(
    directory: &PrivateDir,
    name: &str,
    value: &Value,
) -> Result<CreatedFile, String> {
    let text = serde_json::to_vec(value).map_err(|_| format!("cannot encode {name}"))?;
    create_file(directory, name, &text)
}

fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("cannot set mode on {}: {error}", path.display()))
}

fn file_id(path: &Path) -> Result<FileId, String> {
    use std::os::unix::fs::MetadataExt;
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(FileId {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

fn remove_if_id(path: &Path, expected: FileId) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    use std::os::unix::fs::MetadataExt;
    if metadata.dev() == expected.dev && metadata.ino() == expected.ino {
        let _ = std::fs::remove_file(path);
    }
}

fn random_hex(bytes: usize) -> Result<String, String> {
    let mut data = vec![0_u8; bytes];
    getrandom::fill(&mut data).map_err(|_| "randomness unavailable".to_string())?;
    Ok(data.iter().map(|byte| format!("{byte:02x}")).collect())
}

struct IngressContext {
    binding_dir: PathBuf,
    capability: String,
    transport_id: String,
    nonce: String,
    session: String,
    canonical: CanonicalPointer,
}

fn ingress_loop(
    listener: std::os::unix::net::UnixListener,
    context: IngressContext,
    writer: SyncSender<WriterJob>,
    shutdown: Arc<AtomicBool>,
) {
    let _ = listener.set_nonblocking(true);
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => handle_ingress(stream, &context, &writer),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL)
            }
            Err(_) => break,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IngressRequest {
    version: u64,
    capability: String,
    transport_id: String,
    nonce: String,
    session: String,
    request_id: String,
    text: String,
    message_ids: Vec<String>,
}

#[derive(Serialize)]
struct IngressAnswer {
    outcome: &'static str,
    reason: &'static str,
}

type ReserveClaim = fn(&Path, &IngressRequest, &str) -> Result<Claim, ClaimError>;

fn handle_ingress(
    stream: std::os::unix::net::UnixStream,
    context: &IngressContext,
    writer: &SyncSender<WriterJob>,
) {
    handle_ingress_with(stream, context, writer, reserve_claim)
}

fn handle_ingress_with(
    mut stream: std::os::unix::net::UnixStream,
    context: &IngressContext,
    writer: &SyncSender<WriterJob>,
    reserve: ReserveClaim,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    if !peer_is_same_user(&stream) {
        answer(&mut stream, "definitely_not_written", "peer");
        return;
    }
    let request = match read_ingress(&stream) {
        Ok(request) => request,
        Err(reason) => {
            answer(&mut stream, "definitely_not_written", reason);
            return;
        }
    };
    if let Err(reason) = validate_ingress(&request, context) {
        answer(&mut stream, "definitely_not_written", reason);
        return;
    }
    let claim = match reserve(&context.binding_dir, &request, &context.transport_id) {
        Ok(claim) => claim,
        Err(ClaimError::Duplicate) => {
            answer(&mut stream, "uncertain", "duplicate_claim");
            return;
        }
        Err(ClaimError::Failed) => {
            answer(&mut stream, "definitely_not_written", "claim_failed");
            return;
        }
    };
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": request.text,
            "meta": {
                "event_id": request.request_id,
                "transport_id": context.transport_id,
            }
        }
    })
    .to_string();
    let (done_tx, done_rx) = mpsc::channel();
    let job = WriterJob {
        line: notification,
        done: Some(done_tx),
    };
    match writer.try_send(job) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            claim.release();
            answer(&mut stream, "definitely_not_written", "writer_busy");
            return;
        }
        Err(TrySendError::Disconnected(_)) => {
            claim.release();
            answer(&mut stream, "definitely_not_written", "writer_closed");
            return;
        }
    }
    let write = done_rx.recv_timeout(WRITE_TIMEOUT);
    if matches!(write.as_ref(), Ok(WriteOutcome::Closed { bytes: 0 })) {
        claim.release();
    }
    let (outcome, reason) = answer_for_write(write);
    answer(&mut stream, outcome, reason);
}

fn read_ingress(stream: &std::os::unix::net::UnixStream) -> Result<IngressRequest, &'static str> {
    let mut reader = BufReader::new(stream);
    let line = read_bounded_line(&mut reader, MAX_LINE)
        .map_err(|_| "invalid_request")?
        .ok_or("invalid_request")?;
    serde_json::from_str(line.trim_end_matches(['\n', '\r'])).map_err(|_| "invalid_request")
}

fn validate_ingress(
    request: &IngressRequest,
    context: &IngressContext,
) -> Result<(), &'static str> {
    if request.version != 1
        || !constant_time_eq(&request.capability, &context.capability)
        || request.transport_id != context.transport_id
        || request.nonce != context.nonce
        || request.session != context.session
    {
        return Err("identity");
    }
    let mut seen = HashSet::new();
    if request.text.len() > MAX_TEXT
        || request.text.contains('\r')
        || request.text.contains('\n')
        || request.message_ids.is_empty()
        || request
            .message_ids
            .iter()
            .any(|id| !is_token(id) || !seen.insert(id))
        || !valid_request_id(&request.request_id)
        || request.text != (context.canonical)(&request.message_ids)
    {
        return Err("invalid_request");
    }
    Ok(())
}

fn valid_request_id(value: &str) -> bool {
    value.starts_with("msg_")
        && value.len() <= 128
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[derive(Debug)]
enum ClaimError {
    Duplicate,
    Failed,
}

struct Claim {
    file: CreatedFile,
}

impl Claim {
    fn release(self) {
        self.file.remove();
    }
}

fn claims_directory(binding_dir: &Path) -> Result<PrivateDir, ClaimError> {
    let path = binding_dir.join("claims");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(ClaimError::Failed);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&path).map_err(|_| ClaimError::Failed)?;
            set_mode(&path, 0o700).map_err(|_| ClaimError::Failed)?;
        }
        Err(_) => return Err(ClaimError::Failed),
    }
    PrivateDir::open(&path).map_err(|_| ClaimError::Failed)
}

fn reserve_claim(
    binding_dir: &Path,
    request: &IngressRequest,
    transport_id: &str,
) -> Result<Claim, ClaimError> {
    reserve_claim_with(binding_dir, request, transport_id, |directory| {
        directory.sync()
    })
}

fn reserve_claim_with<F>(
    binding_dir: &Path,
    request: &IngressRequest,
    transport_id: &str,
    sync: F,
) -> Result<Claim, ClaimError>
where
    F: Fn(&PrivateDir) -> Result<(), String>,
{
    let directory = claims_directory(binding_dir)?;
    // A newly created `claims` entry is only durable once the binding parent
    // itself is synced through its pinned descriptor.
    let parent = PrivateDir::open(binding_dir).map_err(|_| ClaimError::Failed)?;
    sync(&parent).map_err(|_| ClaimError::Failed)?;
    let name = format!("{}.json", request.request_id);
    let (mut file, id) = match directory.create_new_checked(&name) {
        Ok(created) => created,
        Err(CreateError::AlreadyExists) => return Err(ClaimError::Duplicate),
        Err(CreateError::Refused(_)) => return Err(ClaimError::Failed),
    };
    let created = CreatedFile::new(directory.clone(), name, id);
    let claim = json!({
        "request_id": request.request_id,
        "transport_id": transport_id,
        "state": "reserved",
    });
    let text = serde_json::to_vec(&claim).map_err(|_| ClaimError::Failed)?;
    if file
        .write_all(&text)
        .and_then(|()| file.sync_all())
        .is_err()
    {
        created.remove();
        return Err(ClaimError::Failed);
    }
    if sync(&directory).is_err() {
        created.remove();
        return Err(ClaimError::Failed);
    }
    Ok(Claim { file: created })
}

fn answer_for_write(
    outcome: Result<WriteOutcome, mpsc::RecvTimeoutError>,
) -> (&'static str, &'static str) {
    match outcome {
        Ok(
            WriteOutcome::Written | WriteOutcome::Partial { .. } | WriteOutcome::Uncertain { .. },
        ) => ("uncertain", "channel_emitted_unconfirmed"),
        Ok(WriteOutcome::Closed { bytes: 0 }) => ("definitely_not_written", "writer_closed"),
        Ok(WriteOutcome::Closed { .. }) => ("uncertain", "channel_emitted_unconfirmed"),
        Err(_) => ("uncertain", "writer_timeout"),
    }
}

fn answer(
    stream: &mut std::os::unix::net::UnixStream,
    outcome: &'static str,
    reason: &'static str,
) {
    let text = serde_json::to_string(&IngressAnswer { outcome, reason })
        .unwrap_or_else(|_| "{\"outcome\":\"uncertain\",\"reason\":\"encoding\"}".to_string());
    let _ = stream.write_all(text.as_bytes());
    let _ = stream.write_all(b"\n");
    let _ = stream.flush();
}

#[cfg(target_os = "macos")]
fn peer_is_same_user(stream: &std::os::unix::net::UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let ok = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0;
    ok && uid == unsafe { libc::geteuid() }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn peer_is_same_user(stream: &std::os::unix::net::UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ok = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            &mut len,
        )
    } == 0;
    ok && cred.uid == unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn peer_is_same_user(_stream: &std::os::unix::net::UnixStream) -> bool {
    false
}

// ---------------------------------------------------------------------------
// `_claude_notice`: the one-shot helper Butler calls to deliver one frozen
// Request1 notice through an already-published channel ingress.
// ---------------------------------------------------------------------------

const MAX_REQUEST: u64 = 64 * 1024;
const MAX_CAPABILITY: usize = 256;
const NOTICE_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// The frozen common notice request. This is the external contract; the private
/// ingress shape below is internal to the bridge.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoticeRequest {
    version: u64,
    binding: String,
    nonce: String,
    session: String,
    instance: String,
    request_id: String,
    text: String,
    message_ids: Vec<String>,
}

/// The strict `claude-channel-v1` manifest. Unknown fields reject.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelManifest {
    schema: u64,
    protocol: String,
    nonce: String,
    session: String,
    endpoint: String,
    bridge_pid: u32,
    bridge_started_us: u64,
    socket_dev: u64,
    socket_ino: u64,
    transport_id: String,
    protocol_version: String,
    state: String,
    capability_file: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoticeAnswer {
    outcome: String,
    reason: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Delivery {
    DefinitelyNotWritten(&'static str),
    Uncertain,
    Bridge(&'static str, &'static str),
}

/// Entry point for `remuda _claude_notice`. Reads one Request1 document from
/// stdin and prints one typed outcome. Never retries.
pub fn run_notice(args: &[&str]) -> std::process::ExitCode {
    if args.iter().any(|arg| *arg == "--help" || *arg == "-h") {
        println!(
            "remuda _claude_notice\nReads one frozen notice request from stdin; returns typed JSON.\n\
             `binding` is the private claude-channel-v1 channel.json path."
        );
        return std::process::ExitCode::SUCCESS;
    }
    if !args.is_empty() {
        return notice_outcome("definitely_not_written", "invalid_args");
    }
    let text = match super::agent_notice::read_stdin() {
        Ok(text) => text,
        Err(reason) => return notice_outcome("definitely_not_written", reason),
    };
    let request: NoticeRequest = match serde_json::from_str(&text) {
        Ok(request) => request,
        Err(_) => return notice_outcome("definitely_not_written", "invalid_request"),
    };
    if let Err(reason) = validate_notice_request(&request) {
        return notice_outcome("definitely_not_written", reason);
    }
    let manifest = match load_channel_manifest(&request.binding) {
        Ok(manifest) => manifest,
        Err(reason) => return notice_outcome("definitely_not_written", reason),
    };
    if let Err(reason) = validate_channel_manifest(&request, &manifest) {
        return notice_outcome("definitely_not_written", reason);
    }
    match deliver_notice(&request, &manifest) {
        Delivery::DefinitelyNotWritten(reason) => notice_outcome("definitely_not_written", reason),
        Delivery::Uncertain => notice_outcome("uncertain", "channel_emitted_unconfirmed"),
        Delivery::Bridge(outcome, reason) => notice_outcome(outcome, reason),
    }
}

fn notice_outcome(outcome: &str, reason: &str) -> std::process::ExitCode {
    println!("{}", json!({"outcome": outcome, "reason": reason}));
    std::process::ExitCode::SUCCESS
}

fn validate_notice_request(request: &NoticeRequest) -> Result<(), &'static str> {
    let mut seen = HashSet::new();
    if request.version != 1
        || request.binding.is_empty()
        || request.nonce.is_empty()
        || request.session.is_empty()
        || request.instance.is_empty()
        || request.message_ids.is_empty()
    {
        return Err("invalid_request");
    }
    if request.text.len() > MAX_TEXT
        || request.text.contains('\r')
        || request.text.contains('\n')
        || request.text != super::agent_notice::canonical_notice(&request.message_ids)
        || request
            .message_ids
            .iter()
            .any(|id| !is_token(id) || !seen.insert(id))
        || !valid_request_id(&request.request_id)
    {
        return Err("invalid_request");
    }
    Ok(())
}

/// A genuinely absent manifest is `binding_missing` only when the private
/// parent is valid and `openat` says ENOENT for the leaf. A missing or invalid
/// parent, a dangling symlink or a wrong-owner leaf stays `invalid_binding`.
fn load_channel_manifest(binding: &str) -> Result<ChannelManifest, &'static str> {
    let path = Path::new(binding);
    if !path.is_absolute() {
        return Err("invalid_binding");
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or("invalid_binding")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid_binding")?;
    if super::agent_notice::binding_missing(path) {
        return Err("binding_missing");
    }
    let directory = PrivateDir::open(parent).map_err(|_| "invalid_binding")?;
    let bytes = directory
        .read_limited(name, MAX_REQUEST)
        .map_err(|_| "invalid_binding")?;
    serde_json::from_slice(&bytes).map_err(|_| "invalid_binding")
}

fn validate_channel_manifest(
    request: &NoticeRequest,
    manifest: &ChannelManifest,
) -> Result<(), &'static str> {
    let binding = Path::new(&request.binding);
    if !binding.is_absolute() {
        return Err("invalid_binding");
    }
    let parent = binding
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or("invalid_binding")?;
    let socket = manifest
        .endpoint
        .strip_prefix("unix://")
        .ok_or("invalid_binding")?;
    let socket_path = Path::new(socket);
    let capability = Path::new(&manifest.capability_file);
    if manifest.schema != 1
        || manifest.protocol != PROTOCOL
        || manifest.nonce != request.nonce
        || manifest.session != request.session
        || manifest.state != "mcp_initialized"
        || manifest.protocol_version != SUPPORTED_PROTOCOL_VERSION
        || manifest.bridge_pid == 0
        || manifest.bridge_started_us == 0
        || !is_hex(&manifest.transport_id, 32)
        || !socket_path.is_absolute()
        || socket_path.parent() != Some(parent)
        || !capability.is_absolute()
        || capability.parent() != Some(parent)
        || capability.file_name().is_none()
    {
        return Err("invalid_binding");
    }
    Ok(())
}

fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn checked_socket(manifest: &ChannelManifest) -> Result<&str, &'static str> {
    use std::os::unix::fs::MetadataExt;
    let socket = manifest
        .endpoint
        .strip_prefix("unix://")
        .ok_or("invalid_binding")?;
    let metadata = std::fs::symlink_metadata(socket).map_err(|_| "invalid_binding")?;
    if !socket_is_private(&metadata)
        || metadata.dev() != manifest.socket_dev
        || metadata.ino() != manifest.socket_ino
    {
        return Err("invalid_binding");
    }
    Ok(socket)
}

fn socket_is_private(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    metadata.file_type().is_socket()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o777 == 0o600
}

fn load_capability(
    request: &NoticeRequest,
    manifest: &ChannelManifest,
) -> Result<String, &'static str> {
    let binding = Path::new(&request.binding);
    let parent = binding.parent().ok_or("invalid_binding")?;
    let name = Path::new(&manifest.capability_file)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid_binding")?;
    let directory = PrivateDir::open(parent).map_err(|_| "invalid_binding")?;
    let bytes = directory
        .read_limited(name, MAX_CAPABILITY as u64)
        .map_err(|_| "capability")?;
    let capability = String::from_utf8(bytes).map_err(|_| "capability")?;
    if capability.is_empty() || !capability.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("capability");
    }
    Ok(capability)
}

/// Mutations start at the first request byte handed to the bridge socket.
/// Before that every refusal is `definitely_not_written`; after it, no answer
/// can be trusted as proof of zero notification bytes.
fn deliver_notice(request: &NoticeRequest, manifest: &ChannelManifest) -> Delivery {
    let socket = match checked_socket(manifest) {
        Ok(socket) => socket,
        Err(reason) => return Delivery::DefinitelyNotWritten(reason),
    };
    let bridge = match process_identity(manifest.bridge_pid) {
        Some(identity) => identity,
        None => return Delivery::DefinitelyNotWritten("bridge_gone"),
    };
    if bridge.started_us != manifest.bridge_started_us {
        return Delivery::DefinitelyNotWritten("stale_binding");
    }
    let capability = match load_capability(request, manifest) {
        Ok(capability) => capability,
        Err(reason) => return Delivery::DefinitelyNotWritten(reason),
    };
    let mut stream = match std::os::unix::net::UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(_) => return Delivery::DefinitelyNotWritten("connect_failed"),
    };
    let _ = stream.set_read_timeout(Some(NOTICE_ACK_TIMEOUT));
    let _ = stream.set_write_timeout(Some(NOTICE_ACK_TIMEOUT));
    if !peer_is_same_user(&stream) {
        return Delivery::DefinitelyNotWritten("peer");
    }
    let peer = match peer_pid(&stream) {
        Some(peer) => peer,
        None => return Delivery::DefinitelyNotWritten("peer"),
    };
    let peer_identity = match process_identity(peer) {
        Some(identity) => identity,
        None => return Delivery::DefinitelyNotWritten("peer"),
    };
    if peer != manifest.bridge_pid || peer_identity.started_us != manifest.bridge_started_us {
        return Delivery::DefinitelyNotWritten("peer");
    }
    // Recheck the path fingerprint after the peer checks: the listener must
    // still be the recorded socket immediately before the first request byte.
    if checked_socket(manifest).is_err() {
        return Delivery::DefinitelyNotWritten("invalid_binding");
    }
    let body = json!({
        "version": 1,
        "capability": capability,
        "transport_id": &manifest.transport_id,
        "nonce": &request.nonce,
        "session": &request.session,
        "request_id": &request.request_id,
        "text": &request.text,
        "message_ids": &request.message_ids,
    })
    .to_string();
    let mut line = body.into_bytes();
    line.push(b'\n');
    let sent = match write_counted(&mut stream, &line) {
        Ok(sent) => sent,
        Err(_) => return Delivery::DefinitelyNotWritten("transport"),
    };
    if sent == 0 {
        return Delivery::DefinitelyNotWritten("transport");
    }
    if sent < line.len() {
        return Delivery::Uncertain;
    }
    let mut reader = BufReader::new(stream);
    let answer = match read_bounded_line(&mut reader, MAX_LINE) {
        Ok(Some(answer)) => answer,
        _ => return Delivery::Uncertain,
    };
    classify_answer(&answer)
}

fn classify_answer(answer: &str) -> Delivery {
    let Ok(value) = serde_json::from_str::<NoticeAnswer>(answer.trim_end_matches(['\n', '\r']))
    else {
        return Delivery::Uncertain;
    };
    match value.outcome.as_str() {
        "definitely_not_written" => match known_not_written(&value.reason) {
            Some(reason) => Delivery::Bridge("definitely_not_written", reason),
            None => Delivery::Uncertain,
        },
        _ => Delivery::Uncertain,
    }
}

/// Only reasons the bridge can prove before any notification byte are accepted
/// as `definitely_not_written`. Anything else stays uncertain.
fn known_not_written(reason: &str) -> Option<&'static str> {
    match reason {
        "peer" => Some("peer"),
        "invalid_request" => Some("invalid_request"),
        "identity" => Some("identity"),
        "claim_failed" => Some("claim_failed"),
        "writer_busy" => Some("writer_busy"),
        "writer_closed" => Some("writer_closed"),
        _ => None,
    }
}

fn write_counted(
    stream: &mut std::os::unix::net::UnixStream,
    bytes: &[u8],
) -> std::io::Result<usize> {
    let mut offset = 0;
    while offset < bytes.len() {
        match stream.write(&bytes[offset..]) {
            Ok(0) => return Ok(offset),
            Ok(written) => offset += written,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                if offset == 0 {
                    return Err(error);
                }
                return Ok(offset);
            }
        }
    }
    Ok(offset)
}

#[cfg(target_os = "macos")]
fn peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut pid: libc::pid_t = 0;
    let mut length = std::mem::size_of_val(&pid) as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return None;
    }
    u32::try_from(pid).ok()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ok = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            &mut length,
        )
    } == 0;
    if !ok {
        return None;
    }
    u32::try_from(cred.pid).ok()
}

#[cfg(not(unix))]
fn peer_pid(_stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicU64;
    use std::sync::Mutex;
    use std::time::Instant;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn private_dir() -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "remuda-claude-channel-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("temp dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod 0700");
        dir
    }

    fn canonical(ids: &[String]) -> String {
        super::super::agent_notice::canonical_notice(ids)
    }

    fn config(dir: &Path) -> Config {
        Config {
            binding_dir: dir.to_path_buf(),
            nonce: "nonce".into(),
            session: "session".into(),
            canonical,
        }
    }

    fn ingress(capability: &str, transport: &str, text: &str) -> IngressRequest {
        IngressRequest {
            version: 1,
            capability: capability.into(),
            transport_id: transport.into(),
            nonce: "nonce".into(),
            session: "session".into(),
            request_id: "msg_01".into(),
            text: text.into(),
            message_ids: vec!["01".into()],
        }
    }

    #[test]
    fn negotiation_echoes_tested_era_and_accepts_legacy() {
        assert_eq!(
            negotiate("2025-06-18"),
            Negotiation {
                version: "2025-06-18",
                accepted: true
            }
        );
        assert_eq!(
            negotiate("2024-11-05"),
            Negotiation {
                version: "2025-06-18",
                accepted: true
            }
        );
        assert_eq!(
            negotiate("2026-01-01"),
            Negotiation {
                version: "2025-06-18",
                accepted: false
            }
        );
    }

    #[test]
    fn canonical_notice_matches_the_contract_wording() {
        assert_eq!(
            super::super::agent_notice::canonical_notice(&["01ABC".into()]),
            "Butler message 01ABC arrived. Read it: MCP butler_inbox (or remuda butler inbox)"
        );
        assert_eq!(
            super::super::agent_notice::canonical_notice(&["A".into(), "B".into()]),
            "2 new Butler messages arrived. Read them: MCP butler_inbox (or remuda butler inbox)"
        );
    }

    #[test]
    fn unknown_method_returns_32601() {
        let (writer, replies) = mpsc::sync_channel(8);
        let mut session = Session::new(
            config(Path::new("/nonexistent")),
            writer,
            Arc::new(AtomicBool::new(false)),
        );
        session.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"nope"}"#);
        let job = replies.try_recv().expect("one reply");
        let value: Value = serde_json::from_str(&job.line).unwrap();
        assert_eq!(value["error"]["code"], -32601);
    }

    #[test]
    fn ingress_validation_rejects_each_identity_field() {
        let context = IngressContext {
            binding_dir: PathBuf::from("/tmp"),
            capability: "cap".into(),
            transport_id: "transport".into(),
            nonce: "nonce".into(),
            session: "session".into(),
            canonical,
        };
        let text = canonical(&["01".into()]);
        assert!(validate_ingress(&ingress("cap", "transport", &text), &context).is_ok());
        for bad in [
            ingress("wrong", "transport", &text),
            ingress("cap", "wrong", &text),
            IngressRequest {
                nonce: "wrong".into(),
                ..ingress("cap", "transport", &text)
            },
            IngressRequest {
                session: "wrong".into(),
                ..ingress("cap", "transport", &text)
            },
        ] {
            assert_eq!(validate_ingress(&bad, &context), Err("identity"));
        }
        assert_eq!(
            validate_ingress(&ingress("cap", "transport", "wrong"), &context),
            Err("invalid_request")
        );
    }

    #[test]
    fn claim_is_reserved_once() {
        let dir = private_dir();
        let request = ingress("cap", "transport", &canonical(&["01".into()]));
        assert!(reserve_claim(&dir, &request, "transport").is_ok());
        assert!(matches!(
            reserve_claim(&dir, &request, "transport"),
            Err(ClaimError::Duplicate)
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn released_claim_allows_a_safe_retry() {
        let dir = private_dir();
        let request = ingress("cap", "transport", &canonical(&["01".into()]));
        let claim = reserve_claim(&dir, &request, "transport").expect("first claim");
        claim.release();
        assert!(reserve_claim(&dir, &request, "transport").is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn zero_byte_failure_is_definitely_not_written() {
        struct Failing {
            left: usize,
        }
        impl Write for Failing {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                if self.left == 0 {
                    return Err(std::io::Error::other("closed"));
                }
                let take = self.left.min(data.len());
                self.left -= take;
                Ok(take)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            write_line(&mut Failing { left: 0 }, "abc"),
            WriteOutcome::Closed { bytes: 0 }
        );
        assert_eq!(
            write_line(&mut Failing { left: 2 }, "abc"),
            WriteOutcome::Partial { bytes: 2 }
        );
    }

    #[test]
    fn private_directory_must_be_0700() {
        let dir = private_dir();
        assert!(PrivateDir::open(&dir).is_ok());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(PrivateDir::open(&dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn publish_writes_strict_manifest_and_cleanup_removes_it() {
        let dir = private_dir();
        let published = publish(&config(&dir), "2025-06-18").expect("publish");
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(dir.join("channel.json")).unwrap()).unwrap();
        assert_eq!(manifest["schema"], 1);
        assert_eq!(manifest["protocol"], PROTOCOL);
        assert_eq!(manifest["state"], "mcp_initialized");
        assert_eq!(manifest["nonce"], "nonce");
        assert_eq!(manifest["session"], "session");
        assert!(manifest["capability_file"]
            .as_str()
            .unwrap()
            .ends_with("capability"));
        assert_eq!(
            std::fs::metadata(dir.join("capability"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let bridge: Value =
            serde_json::from_slice(&std::fs::read(dir.join("bridge.json")).unwrap()).unwrap();
        assert_eq!(bridge["bridge_pid"], manifest["bridge_pid"]);
        assert_eq!(bridge["bridge_started_us"], manifest["bridge_started_us"]);
        assert!(bridge["parent_pid"].as_u64().is_some());
        published.cleanup();
        assert!(!dir.join("channel.json").exists());
        assert!(!dir.join("capability").exists());
        assert!(!dir.join("ingress.sock").exists());
        assert!(!dir.join("bridge.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stdio_initialize_then_ingress_emits_one_channel_notification() {
        use std::os::unix::net::UnixStream;

        let dir = private_dir();
        let (server_stdin, mut client_stdin) = UnixStream::pair().unwrap();
        let (server_stdout, client_stdout) = UnixStream::pair().unwrap();
        let handle = thread::spawn({
            let dir = dir.clone();
            move || serve(BufReader::new(server_stdin), server_stdout, config(&dir))
        });
        client_stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\"}}\n")
            .unwrap();
        let mut reader = BufReader::new(client_stdout.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            reply["result"]["capabilities"]["experimental"]["claude/channel"],
            json!({})
        );

        client_stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .unwrap();
        let manifest_path = dir.join("channel.json");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !manifest_path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(manifest_path.exists(), "manifest was not published");
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        let endpoint = manifest["endpoint"].as_str().unwrap();
        let socket = endpoint.strip_prefix("unix://").unwrap();
        let capability = std::fs::read_to_string(dir.join("capability")).unwrap();
        let transport = manifest["transport_id"].as_str().unwrap();
        let text = canonical(&["01".into()]);

        let mut ingress = UnixStream::connect(socket).unwrap();
        let request = json!({
            "version": 1,
            "capability": capability,
            "transport_id": transport,
            "nonce": "nonce",
            "session": "session",
            "request_id": "msg_01",
            "text": text,
            "message_ids": ["01"],
        });
        ingress
            .write_all(format!("{request}\n").as_bytes())
            .unwrap();
        let mut answer = String::new();
        BufReader::new(ingress.try_clone().unwrap())
            .read_line(&mut answer)
            .unwrap();
        let answer: Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(answer["outcome"], "uncertain");
        assert_eq!(answer["reason"], "channel_emitted_unconfirmed");

        let mut notification = String::new();
        reader.read_line(&mut notification).unwrap();
        let notification: Value = serde_json::from_str(&notification).unwrap();
        assert_eq!(notification["method"], "notifications/claude/channel");
        assert_eq!(notification["params"]["content"], text);
        assert_eq!(notification["params"]["meta"]["event_id"], "msg_01");

        drop(client_stdin);
        let _ = handle.join().unwrap();
        assert!(!manifest_path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn writer_channel() -> (
        SyncSender<WriterJob>,
        Arc<Mutex<Vec<Value>>>,
        JoinHandle<()>,
    ) {
        let (sender, receiver) = mpsc::sync_channel::<WriterJob>(16);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let handle = thread::spawn(move || {
            while let Ok(job) = receiver.recv() {
                if let Ok(value) = serde_json::from_str::<Value>(&job.line) {
                    sink.lock().unwrap().push(value);
                }
                if let Some(done) = job.done {
                    let _ = done.send(WriteOutcome::Written);
                }
            }
        });
        (sender, seen, handle)
    }

    fn initialize_line(id: u64) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"initialize","params":{{"protocolVersion":"2025-06-18"}}}}"#
        )
    }

    const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

    fn stop(mut session: Session) {
        session.shutdown.store(true, Ordering::SeqCst);
        if let Some(ingress) = session.ingress.take() {
            let _ = ingress.join();
        }
        if let Some(published) = session.published.take() {
            published.cleanup();
        }
        drop(session);
    }

    fn wait_for_lines(seen: &Arc<Mutex<Vec<Value>>>, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while seen.lock().unwrap().len() < count && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn premature_initialized_does_not_poison_startup() {
        let dir = private_dir();
        let (writer, _seen, writer_handle) = writer_channel();
        let mut session = Session::new(config(&dir), writer, Arc::new(AtomicBool::new(false)));

        session.handle_line(INITIALIZED);
        assert!(!session.initialized);
        assert!(!dir.join("channel.json").exists());

        session.handle_line(&initialize_line(1));
        session.handle_line(INITIALIZED);
        assert!(session.initialized);
        assert!(dir.join("channel.json").exists());

        stop(session);
        let _ = writer_handle.join();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_initialize_cannot_ready_the_channel() {
        let dir = private_dir();
        let (writer, seen, writer_handle) = writer_channel();
        let mut session = Session::new(config(&dir), writer, Arc::new(AtomicBool::new(false)));

        session.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
        session.handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":[]}"#);
        session.handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"initialize"}"#);
        assert!(session.negotiation.is_none());
        assert!(!session.initialize_response_ok);

        session.handle_line(INITIALIZED);
        assert!(!session.initialized);
        assert!(!dir.join("channel.json").exists());

        wait_for_lines(&seen, 3);
        let seen = seen.lock().unwrap();
        assert!(seen.iter().all(|value| value["error"]["code"] == -32602));
        drop(seen);
        stop(session);
        let _ = writer_handle.join();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn repeated_initialize_is_rejected_after_the_first() {
        let dir = private_dir();
        let (writer, seen, writer_handle) = writer_channel();
        let mut session = Session::new(config(&dir), writer, Arc::new(AtomicBool::new(false)));

        session.handle_line(&initialize_line(1));
        assert!(session.initialize_response_ok);
        let version = session.negotiation;
        session.handle_line(&initialize_line(2));
        assert_eq!(session.negotiation, version);
        wait_for_lines(&seen, 2);
        assert!(seen
            .lock()
            .unwrap()
            .iter()
            .any(|value| value["id"] == 2 && value["error"]["code"] == -32600));

        session.handle_line(INITIALIZED);
        session.handle_line(&initialize_line(3));
        wait_for_lines(&seen, 3);
        assert!(seen
            .lock()
            .unwrap()
            .iter()
            .any(|value| value["id"] == 3 && value["error"]["code"] == -32600));

        stop(session);
        let _ = writer_handle.join();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_jsonrpc_frames_are_rejected() {
        let (writer, seen, writer_handle) = writer_channel();
        let mut session = Session::new(
            config(Path::new("/nonexistent")),
            writer,
            Arc::new(AtomicBool::new(false)),
        );
        session.handle_line("[1,2]");
        session.handle_line(r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#);
        session.handle_line(r#"{"jsonrpc":"2.0","id":{},"method":"ping"}"#);
        session.handle_line(r#"{"jsonrpc":"2.0","id":2}"#);
        session.handle_line("{");

        wait_for_lines(&seen, 5);
        let seen = seen.lock().unwrap();
        let codes: Vec<i64> = seen
            .iter()
            .filter_map(|value| value["error"]["code"].as_i64())
            .collect();
        assert_eq!(codes, vec![-32600, -32600, -32600, -32600, -32700]);
        drop(seen);
        stop(session);
        let _ = writer_handle.join();
    }

    #[test]
    fn failed_initialize_response_never_publishes() {
        let dir = private_dir();
        let (writer, receiver) = mpsc::sync_channel::<WriterJob>(8);
        drop(receiver);
        let mut session = Session::new(config(&dir), writer, Arc::new(AtomicBool::new(false)));

        session.handle_line(&initialize_line(1));
        assert!(!session.initialize_response_ok);
        session.handle_line(INITIALIZED);
        assert!(!session.initialized);
        assert!(!dir.join("channel.json").exists());

        stop(session);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn initialize_response_that_wrote_nothing_is_not_ready() {
        let dir = private_dir();
        let (writer, receiver) = mpsc::sync_channel::<WriterJob>(8);
        let writer_handle = thread::spawn(move || {
            while let Ok(job) = receiver.recv() {
                if let Some(done) = job.done {
                    let _ = done.send(WriteOutcome::Closed { bytes: 0 });
                }
            }
        });
        let mut session = Session::new(config(&dir), writer, Arc::new(AtomicBool::new(false)));

        session.handle_line(&initialize_line(1));
        assert!(!session.initialize_response_ok);
        session.handle_line(INITIALIZED);
        assert!(!session.initialized);
        assert!(!dir.join("channel.json").exists());

        stop(session);
        let _ = writer_handle.join();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn duplicate_startup_preserves_the_first_generation() {
        let dir = private_dir();
        let first = publish(&config(&dir), "2025-06-18").expect("first publish");
        let names = ["channel.json", "capability", "bridge.json"];
        let before: Vec<Vec<u8>> = names
            .iter()
            .map(|name| std::fs::read(dir.join(name)).unwrap())
            .collect();

        assert!(publish(&config(&dir), "2025-06-18").is_err());
        for (name, expected) in names.iter().zip(before) {
            assert_eq!(
                std::fs::read(dir.join(name)).unwrap(),
                expected,
                "{name} changed on duplicate startup"
            );
        }
        first.cleanup();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cleanup_preserves_a_replacement_leaf() {
        let dir = private_dir();
        let published = publish(&config(&dir), "2025-06-18").expect("publish");
        let names = ["channel.json", "capability", "bridge.json"];
        for name in names {
            let path = dir.join(name);
            // Keep our inode allocated so a filesystem cannot reuse the same
            // inode number for the replacement.
            std::fs::hard_link(&path, dir.join(format!("{name}.keep"))).expect("keep inode");
            std::fs::remove_file(&path).expect("remove ours");
            std::fs::write(&path, b"replacement").expect("replacement");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod 0600");
        }

        published.cleanup();
        for name in names {
            assert_eq!(
                std::fs::read(dir.join(name)).unwrap(),
                b"replacement",
                "{name} was deleted"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_symlinked_or_replaced_claim_directory_is_refused() {
        let dir = private_dir();
        let other = private_dir();
        let request = ingress("cap", "transport", &canonical(&["01".into()]));

        std::os::unix::fs::symlink(&other, dir.join("claims")).expect("symlink claims");
        assert!(matches!(
            reserve_claim(&dir, &request, "transport"),
            Err(ClaimError::Failed)
        ));

        std::fs::remove_file(dir.join("claims")).expect("unlink symlink");
        std::fs::write(dir.join("claims"), b"not a directory").expect("regular file");
        assert!(matches!(
            reserve_claim(&dir, &request, "transport"),
            Err(ClaimError::Failed)
        ));

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(other);
    }

    #[test]
    fn claim_fsync_failure_removes_the_claim() {
        let dir = private_dir();
        let request = ingress("cap", "transport", &canonical(&["01".into()]));
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let result = reserve_claim_with(&dir, &request, "transport", |_directory| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(())
            } else {
                Err("fsync failed".to_string())
            }
        });
        assert!(matches!(result, Err(ClaimError::Failed)));
        assert!(!dir.join("claims").join("msg_01.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_claim_answers_without_emitting_a_notification() {
        use std::os::unix::net::UnixStream;

        let dir = private_dir();
        let context = IngressContext {
            binding_dir: dir.clone(),
            capability: "cap".into(),
            transport_id: "transport".into(),
            nonce: "nonce".into(),
            session: "session".into(),
            canonical,
        };
        let request = ingress("cap", "transport", &canonical(&["01".into()]));
        let (server, mut client) = UnixStream::pair().unwrap();
        client
            .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
            .unwrap();
        let (writer, jobs) = mpsc::sync_channel(8);
        handle_ingress_with(server, &context, &writer, |dir, request, transport| {
            reserve_claim_with(dir, request, transport, |_| Err("fsync failed".to_string()))
        });

        assert!(
            jobs.try_recv().is_err(),
            "a failed claim emitted a notification"
        );
        let mut answer = String::new();
        BufReader::new(client).read_line(&mut answer).unwrap();
        let answer: Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(answer["outcome"], "definitely_not_written");
        assert_eq!(answer["reason"], "claim_failed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn line_reader_enforces_a_hard_byte_bound() {
        let mut exact = BufReader::new(&b"aaaaaaaa"[..]);
        assert_eq!(
            read_bounded_line(&mut exact, 8).unwrap(),
            Some("aaaaaaaa".into())
        );

        let mut over = BufReader::new(&b"aaaaaaaaa"[..]);
        assert!(read_bounded_line(&mut over, 8).is_err());

        let mut framed = BufReader::new(&b"aaaa\nbbbb"[..]);
        assert_eq!(
            read_bounded_line(&mut framed, 5).unwrap(),
            Some("aaaa\n".into())
        );

        let mut too_long = BufReader::new(&b"aaaaa\n"[..]);
        assert!(read_bounded_line(&mut too_long, 5).is_err());
    }

    #[test]
    fn duplicate_message_ids_are_rejected() {
        let context = IngressContext {
            binding_dir: PathBuf::from("/tmp"),
            capability: "cap".into(),
            transport_id: "transport".into(),
            nonce: "nonce".into(),
            session: "session".into(),
            canonical,
        };
        let ids = vec!["01".to_string(), "01".to_string()];
        let request = IngressRequest {
            message_ids: ids.clone(),
            text: canonical(&ids),
            ..ingress("cap", "transport", "placeholder")
        };
        assert_eq!(validate_ingress(&request, &context), Err("invalid_request"));
    }

    #[test]
    fn large_ordered_id_sets_have_no_artificial_cap() {
        let context = IngressContext {
            binding_dir: PathBuf::from("/tmp"),
            capability: "cap".into(),
            transport_id: "transport".into(),
            nonce: "nonce".into(),
            session: "session".into(),
            canonical,
        };
        let ids: Vec<String> = (0..200).map(|index| format!("id{index}")).collect();
        let request = IngressRequest {
            message_ids: ids.clone(),
            text: canonical(&ids),
            ..ingress("cap", "transport", "placeholder")
        };
        assert!(validate_ingress(&request, &context).is_ok());
    }

    #[test]
    fn serve_returns_on_eof_even_when_the_writer_blocks() {
        use std::os::unix::net::UnixStream;

        struct Blocking {
            gate: Arc<AtomicBool>,
        }
        impl Write for Blocking {
            fn write(&mut self, _data: &[u8]) -> std::io::Result<usize> {
                while !self.gate.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(std::io::Error::other("released"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let dir = private_dir();
        let (server_stdin, mut client_stdin) = UnixStream::pair().unwrap();
        let gate = Arc::new(AtomicBool::new(false));
        let handle = thread::spawn({
            let dir = dir.clone();
            let gate = gate.clone();
            move || {
                serve(
                    BufReader::new(server_stdin),
                    Blocking { gate },
                    config(&dir),
                )
            }
        });
        client_stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        drop(client_stdin);

        let deadline = Instant::now() + Duration::from_secs(2);
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_finished(), "serve waited for a blocked writer");

        gate.store(true, Ordering::SeqCst);
        let _ = handle.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn serve_returns_on_eof_after_a_ping_flood_with_a_blocked_writer() {
        use std::os::unix::net::UnixStream;

        struct Blocking {
            gate: Arc<AtomicBool>,
        }
        impl Write for Blocking {
            fn write(&mut self, _data: &[u8]) -> std::io::Result<usize> {
                while !self.gate.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(std::io::Error::other("released"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let dir = private_dir();
        let (server_stdin, mut client_stdin) = UnixStream::pair().unwrap();
        let gate = Arc::new(AtomicBool::new(false));
        let handle = thread::spawn({
            let dir = dir.clone();
            let gate = gate.clone();
            move || {
                serve(
                    BufReader::new(server_stdin),
                    Blocking { gate },
                    config(&dir),
                )
            }
        });
        let mut flood = String::new();
        for id in 0..(MAX_QUEUE + 32) {
            flood.push_str(&format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"ping\"}}\n"
            ));
        }
        client_stdin.write_all(flood.as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(50));
        drop(client_stdin);

        let deadline = Instant::now() + Duration::from_secs(2);
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            handle.is_finished(),
            "serve blocked on a full reply queue instead of returning on EOF"
        );

        gate.store(true, Ordering::SeqCst);
        let _ = handle.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    fn notice_request(dir: &Path, request_id: &str) -> NoticeRequest {
        NoticeRequest {
            version: 1,
            binding: dir.join("channel.json").to_string_lossy().into(),
            nonce: "nonce".into(),
            session: "session".into(),
            instance: "instance".into(),
            request_id: request_id.into(),
            text: canonical(&["01".into()]),
            message_ids: vec!["01".into()],
        }
    }

    #[test]
    fn notice_reports_binding_missing_only_for_an_absent_manifest() {
        let dir = private_dir();
        let binding = dir.join("channel.json");
        assert_eq!(
            load_channel_manifest(&binding.to_string_lossy()).unwrap_err(),
            "binding_missing"
        );

        std::fs::write(&binding, b"not json").unwrap();
        std::fs::set_permissions(&binding, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            load_channel_manifest(&binding.to_string_lossy()).unwrap_err(),
            "invalid_binding"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn notice_manifest_requires_the_exact_protocol_shape() {
        let dir = private_dir();
        let published = publish(&config(&dir), "2025-06-18").expect("publish");
        let request = notice_request(&dir, "msg_01");
        let manifest = load_channel_manifest(&request.binding).expect("strict manifest must load");
        validate_channel_manifest(&request, &manifest).expect("valid manifest");

        let mut bad = manifest.clone();
        bad.protocol = "opencode-http-v1".into();
        assert_eq!(
            validate_channel_manifest(&request, &bad),
            Err("invalid_binding")
        );

        let mut bad = manifest.clone();
        bad.nonce = "other".into();
        assert_eq!(
            validate_channel_manifest(&request, &bad),
            Err("invalid_binding")
        );

        let mut bad = manifest.clone();
        bad.state = "listening".into();
        assert_eq!(
            validate_channel_manifest(&request, &bad),
            Err("invalid_binding")
        );

        let mut bad = manifest.clone();
        bad.protocol_version = "2026-01-01".into();
        assert_eq!(
            validate_channel_manifest(&request, &bad),
            Err("invalid_binding")
        );

        let mut bad = manifest.clone();
        bad.endpoint = format!("unix://{}/nested/elsewhere.sock", dir.display());
        assert_eq!(
            validate_channel_manifest(&request, &bad),
            Err("invalid_binding")
        );

        let mut bad = manifest.clone();
        bad.capability_file = "/tmp/capability".into();
        assert_eq!(
            validate_channel_manifest(&request, &bad),
            Err("invalid_binding")
        );

        let mut bad = manifest.clone();
        bad.bridge_pid = 0;
        assert_eq!(
            validate_channel_manifest(&request, &bad),
            Err("invalid_binding")
        );

        published.cleanup();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn notice_refuses_a_stale_bridge_or_wrong_socket() {
        let dir = private_dir();
        let published = publish(&config(&dir), "2025-06-18").expect("publish");
        let request = notice_request(&dir, "msg_01");
        let manifest = load_channel_manifest(&request.binding).expect("strict manifest must load");

        let mut stale = manifest.clone();
        stale.bridge_started_us = stale.bridge_started_us.wrapping_add(1);
        assert_eq!(
            deliver_notice(&request, &stale),
            Delivery::DefinitelyNotWritten("stale_binding")
        );

        let mut gone = manifest.clone();
        gone.bridge_pid = u32::MAX;
        assert_eq!(
            deliver_notice(&request, &gone),
            Delivery::DefinitelyNotWritten("bridge_gone")
        );

        let mut replaced = manifest.clone();
        replaced.socket_ino ^= 1;
        assert_eq!(
            deliver_notice(&request, &replaced),
            Delivery::DefinitelyNotWritten("invalid_binding")
        );

        published.cleanup();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn notice_end_to_end_uses_request1_and_ingress() {
        use std::os::unix::net::UnixStream;

        let dir = private_dir();
        let (server_stdin, mut client_stdin) = UnixStream::pair().unwrap();
        let (server_stdout, client_stdout) = UnixStream::pair().unwrap();
        let handle = thread::spawn({
            let dir = dir.clone();
            move || serve(BufReader::new(server_stdin), server_stdout, config(&dir))
        });
        client_stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\"}}\n")
            .unwrap();
        let mut reader = BufReader::new(client_stdout.try_clone().unwrap());
        let mut reply = String::new();
        reader.read_line(&mut reply).unwrap();
        client_stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .unwrap();
        let manifest_path = dir.join("channel.json");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !manifest_path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(manifest_path.exists(), "manifest was not published");

        let request = notice_request(&dir, "msg_01");
        let manifest = load_channel_manifest(&request.binding).expect("strict manifest must load");
        validate_channel_manifest(&request, &manifest).expect("valid manifest");

        assert_eq!(deliver_notice(&request, &manifest), Delivery::Uncertain);
        let mut notification = String::new();
        reader.read_line(&mut notification).unwrap();
        let notification: Value = serde_json::from_str(&notification).unwrap();
        assert_eq!(notification["method"], "notifications/claude/channel");
        assert_eq!(notification["params"]["meta"]["event_id"], "msg_01");

        // The uncertain claim is durable: a repeat must not emit again.
        assert_eq!(deliver_notice(&request, &manifest), Delivery::Uncertain);

        drop(client_stdin);
        let _ = handle.join().unwrap();
        assert!(!manifest_path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn notice_malformed_ack_is_uncertain() {
        use std::os::unix::net::UnixListener;

        let dir = private_dir();
        let socket_path = dir.join("ingress.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        set_mode(&socket_path, 0o600).unwrap();
        let socket_id = file_id(&socket_path).unwrap();
        let capability_path = dir.join("capability");
        std::fs::write(&capability_path, b"abcd").unwrap();
        std::fs::set_permissions(&capability_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let identity = process_identity(std::process::id()).expect("own identity");
        let manifest = ChannelManifest {
            schema: 1,
            protocol: PROTOCOL.into(),
            nonce: "nonce".into(),
            session: "session".into(),
            endpoint: format!("unix://{}", socket_path.display()),
            bridge_pid: std::process::id(),
            bridge_started_us: identity.started_us,
            socket_dev: socket_id.dev,
            socket_ino: socket_id.ino,
            transport_id: "0123456789abcdef0123456789abcdef".into(),
            protocol_version: SUPPORTED_PROTOCOL_VERSION.into(),
            state: "mcp_initialized".into(),
            capability_file: capability_path.to_string_lossy().into(),
        };
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            stream.write_all(b"not json\n").unwrap();
        });

        let request = notice_request(&dir, "msg_01");
        assert_eq!(deliver_notice(&request, &manifest), Delivery::Uncertain);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
