//! Bounded, VPN-address-only server for one-shot Noise IK requests.
#![allow(clippy::disallowed_types)]

use super::{frame, replay};
use crate::cluster::{self, identity, join_token::JoinTokenStore, NodeState};
use remuda_core::protocol::{Request, Response};
use remuda_core::WallClock;
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
#[cfg(all(test, unix))]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

pub const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
pub const MAX_HEADER_COUNT: usize = 64;
pub const MAX_BODY_BYTES: usize = 65_535;
pub const MAX_GLOBAL_REQUESTS: usize = 64;
pub const MAX_PEER_REQUESTS: usize = 8;
pub const MAX_PREAUTH_PER_IP: usize = 4;
pub const MAX_HELD_REQUEST: Duration = Duration::from_secs(30);
const SOCKET_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_READ_TIME: Duration = Duration::from_secs(5);
const LOCAL_DAEMON_REQUEST_TIMEOUT: Duration = Duration::from_secs(25);
const OBSERVE_INTERVAL: Duration = Duration::from_secs(5);
const ACCEPT_POLL: Duration = Duration::from_millis(20);
const ACCEPT_RESOURCE_BACKOFF: Duration = Duration::from_millis(250);
const REPLAY_CAPACITY: usize = 65_536;
static LISTENER_ERROR_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Explicit listener address and opt-in for wildcard binding.
/// On dual-stack IPv6 systems, an opted-in `[::]` bind may also accept IPv4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListenerConfig {
    pub bind_addr: SocketAddr,
    pub allow_unspecified: bool,
}

impl ListenerConfig {
    pub fn new(bind_addr: SocketAddr) -> Self {
        Self {
            bind_addr,
            allow_unspecified: false,
        }
    }
}

/// One bound cluster listener. Calls to `serve_until` are intended for host lifecycle control.
pub struct Listener {
    socket: TcpListener,
    state: Arc<ListenerState>,
    authorize: MemberAuthorizer,
    dispatch: FrameDispatcher,
}

type MemberAuthorizer = Arc<dyn Fn(&[u8]) -> io::Result<()> + Send + Sync>;
type FrameDispatcher = Arc<dyn Fn(&[u8]) -> io::Result<Vec<u8>> + Send + Sync>;

struct ListenerState {
    responder_private: Zeroizing<Vec<u8>>,
    replay: Mutex<replay::ReplayWindow>,
    limiter: Arc<RequestLimiter>,
    join_tokens: JoinTokenStore,
}

struct MemberRegistryCache {
    cached: Mutex<(String, cluster::Registry)>,
}

#[derive(Default)]
struct RequestLimiter {
    active_global: AtomicUsize,
    active_ips: Mutex<HashMap<std::net::IpAddr, usize>>,
    active_peers: Mutex<HashMap<String, usize>>,
}

struct GlobalPermit(Arc<RequestLimiter>);

struct PeerPermit {
    limiter: Arc<RequestLimiter>,
    fingerprint: String,
}

struct IpPermit {
    limiter: Arc<RequestLimiter>,
    address: std::net::IpAddr,
}

struct InboundRequest {
    body: Vec<u8>,
}

#[derive(Clone, Copy)]
struct ConnectionLimits {
    outer_hold: Duration,
    post_dispatch_hold: Duration,
    idle_read: Duration,
    total_read: Duration,
}

/// Refuse wildcard binds unless the operator opted in explicitly.
pub fn validate_bind_address(address: SocketAddr, allow_unspecified: bool) -> io::Result<()> {
    if address.ip().is_unspecified() && !allow_unspecified {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "wildcard listener bind requires explicit public-bind opt-in",
        ));
    }
    Ok(())
}

/// Bind the configured address after confirming that this node is initialized.
pub fn bind(config: ListenerConfig, daemon_path: &Path) -> io::Result<Listener> {
    validate_bind_address(config.bind_addr, config.allow_unspecified)?;
    let (identity_node, registry) = cluster::nodes()?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cluster is not initialized; run `remuda cluster init`",
        )
    })?;
    if !registry
        .authorized_nodes
        .iter()
        .any(|entry| entry.node_fp == identity_node.node_fp && entry.state == NodeState::Admitted)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local node is not admitted in its registry",
        ));
    }
    let responder_private = identity::load_static_private_key()?;
    let revision = cluster::registry::registry_revision_token()?;
    let registry_cache = Arc::new(MemberRegistryCache {
        cached: Mutex::new((revision, registry)),
    });
    let join_tokens = JoinTokenStore::open(Arc::new(crate::SystemWallClock::new()))?;
    let socket = TcpListener::bind(config.bind_addr)?;
    socket.set_nonblocking(true)?;
    let authorizer_cache = registry_cache.clone();
    let authorize: MemberAuthorizer =
        Arc::new(move |peer_static| authorizer_cache.authorize(peer_static));
    let dispatch_path = daemon_path.to_path_buf();
    let dispatch: FrameDispatcher =
        Arc::new(move |payload| dispatch_payload(payload, &dispatch_path));
    Ok(Listener {
        socket,
        state: Arc::new(ListenerState {
            responder_private,
            replay: Mutex::new(replay::ReplayWindow::new(REPLAY_CAPACITY)),
            limiter: Arc::new(RequestLimiter::default()),
            join_tokens,
        }),
        authorize,
        dispatch,
    })
}

/// Bind and serve requests until the listener fails.
pub fn serve(config: ListenerConfig, daemon_path: &Path) -> io::Result<()> {
    bind(config, daemon_path)?.serve()
}

impl Listener {
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Run until stopped; token rollback/expiry state is observed every five seconds.
    pub fn serve_until(&self, stop: &AtomicBool) -> io::Result<()> {
        std::thread::scope(|scope| {
            let (observe_error_tx, observe_error_rx) = std::sync::mpsc::channel::<io::Error>();
            let (observer_stop_tx, observer_stop_rx) = std::sync::mpsc::channel();
            scope.spawn(move || {
                if let Err(error) = self.state.join_tokens.observe() {
                    let _ = observe_error_tx.send(error);
                    return;
                }
                loop {
                    match observer_stop_rx.recv_timeout(OBSERVE_INTERVAL) {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if let Err(error) = self.state.join_tokens.observe() {
                                let _ = observe_error_tx.send(error);
                                break;
                            }
                        }
                    }
                }
            });
            let state = self.state.clone();
            let authorize = self.authorize.clone();
            let dispatch = self.dispatch.clone();
            let limits = ConnectionLimits {
                outer_hold: MAX_HELD_REQUEST,
                post_dispatch_hold: MAX_HELD_REQUEST,
                idle_read: SOCKET_IDLE_TIMEOUT,
                total_read: MAX_REQUEST_READ_TIME,
            };
            let result = serve_socket_until(
                &self.socket,
                stop,
                || match observe_error_rx.try_recv() {
                    Ok(error) => Err(error),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                    | Err(std::sync::mpsc::TryRecvError::Disconnected) => Ok(()),
                },
                move |stream, remote_addr| {
                    spawn_connection_handler(
                        stream,
                        remote_addr,
                        state.clone(),
                        authorize.clone(),
                        dispatch.clone(),
                        limits,
                    )
                },
            );
            let _ = observer_stop_tx.send(());
            result
        })
    }

    pub fn serve(&self) -> io::Result<()> {
        self.serve_until(&AtomicBool::new(false))
    }
}

fn serve_socket_until<T, Tick, Handle>(
    socket: &TcpListener,
    stop: &AtomicBool,
    mut tick: Tick,
    mut handle: Handle,
) -> io::Result<()>
where
    Tick: FnMut() -> io::Result<()>,
    Handle: FnMut(TcpStream, SocketAddr) -> io::Result<T>,
{
    while !stop.load(Ordering::Acquire) {
        tick()?;
        match socket.accept() {
            Ok((stream, remote_addr)) => {
                if let Err(error) = prepare_accepted_stream(&stream) {
                    log_listener_error("accepted stream setup", &error);
                    continue;
                }
                if let Err(error) = handle(stream, remote_addr) {
                    log_listener_error("connection handling", &error);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
            }
            Err(error) => {
                let resource_pressure = is_accept_resource_exhaustion(&error);
                log_listener_error("accept", &error);
                std::thread::sleep(if resource_pressure {
                    ACCEPT_RESOURCE_BACKOFF
                } else {
                    ACCEPT_POLL
                });
            }
        }
    }
    Ok(())
}

fn prepare_accepted_stream(stream: &TcpStream) -> io::Result<()> {
    stream.set_nonblocking(false)
}

fn is_accept_resource_exhaustion(error: &io::Error) -> bool {
    match error.raw_os_error() {
        #[cfg(unix)]
        Some(23 | 24) => true, // ENFILE / EMFILE
        #[cfg(windows)]
        Some(10024) => true, // WSAEMFILE
        _ => false,
    }
}

fn log_listener_error(context: &str, error: &io::Error) {
    if LISTENER_ERROR_COUNT
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(64)
    {
        eprintln!("remuda: cluster listener {context} failed: {error}");
    }
}

impl RequestLimiter {
    fn acquire_ip(self: &Arc<Self>, address: std::net::IpAddr) -> Option<Arc<IpPermit>> {
        let mut ips = self
            .active_ips
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let active = ips.entry(address).or_default();
        if *active >= MAX_PREAUTH_PER_IP {
            return None;
        }
        *active += 1;
        Some(Arc::new(IpPermit {
            limiter: self.clone(),
            address,
        }))
    }

    fn acquire_global(self: &Arc<Self>) -> Option<Arc<GlobalPermit>> {
        self.active_global
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_GLOBAL_REQUESTS).then_some(count + 1)
            })
            .ok()
            .map(|_| Arc::new(GlobalPermit(self.clone())))
    }

    fn acquire_peer(self: &Arc<Self>, fingerprint: &str) -> Option<Arc<PeerPermit>> {
        let mut peers = self
            .active_peers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let active = peers.entry(fingerprint.to_owned()).or_default();
        if *active >= MAX_PEER_REQUESTS {
            return None;
        }
        *active += 1;
        Some(Arc::new(PeerPermit {
            limiter: self.clone(),
            fingerprint: fingerprint.to_owned(),
        }))
    }
}

impl Drop for GlobalPermit {
    fn drop(&mut self) {
        self.0.active_global.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for PeerPermit {
    fn drop(&mut self) {
        let mut peers = self
            .limiter
            .active_peers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(active) = peers.get_mut(&self.fingerprint) {
            *active -= 1;
            if *active == 0 {
                peers.remove(&self.fingerprint);
            }
        }
    }
}

impl Drop for IpPermit {
    fn drop(&mut self) {
        let mut ips = self
            .limiter
            .active_ips
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(active) = ips.get_mut(&self.address) {
            *active -= 1;
            if *active == 0 {
                ips.remove(&self.address);
            }
        }
    }
}

fn effective_hold_timeout(requested: Duration) -> Duration {
    requested.min(MAX_HELD_REQUEST)
}

fn parse_http_request<R: BufRead>(mut reader: R) -> io::Result<InboundRequest> {
    let mut total_header_bytes = 0usize;
    let request_line = read_header_line(&mut reader, MAX_REQUEST_LINE_BYTES)?;
    total_header_bytes += request_line.len();
    let request_line = std::str::from_utf8(&request_line)
        .map_err(|_| invalid_http("request line is not ASCII"))?;
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method != "POST" || path != "/cluster" || version != "HTTP/1.1" || parts.next().is_some() {
        return Err(invalid_http("unsupported HTTP request line"));
    }

    let mut content_length = None;
    let mut header_count = 0usize;
    loop {
        let line = read_header_line(&mut reader, MAX_REQUEST_LINE_BYTES)?;
        total_header_bytes = total_header_bytes.saturating_add(line.len());
        if total_header_bytes > MAX_HEADER_BYTES {
            return Err(invalid_http("HTTP headers exceed byte cap"));
        }
        if line.is_empty() {
            break;
        }
        header_count += 1;
        if header_count > MAX_HEADER_COUNT {
            return Err(invalid_http("HTTP request exceeds header count cap"));
        }
        let (name, value) = parse_header(&line)?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(invalid_http("Transfer-Encoding is not supported"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some()
                || value.is_empty()
                || !value.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(invalid_http("invalid or duplicate Content-Length"));
            }
            let length = value
                .parse::<usize>()
                .map_err(|_| invalid_http("invalid Content-Length"))?;
            if length > MAX_BODY_BYTES {
                return Err(invalid_http("HTTP body exceeds byte cap"));
            }
            content_length = Some(length);
        }
    }
    let length = content_length.ok_or_else(|| invalid_http("Content-Length is required"))?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(InboundRequest { body })
}

fn parse_socket_request_with_timeout(
    stream: &TcpStream,
    max_read_time: Duration,
) -> io::Result<InboundRequest> {
    let wake_stream = stream.try_clone()?;
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let timer = std::thread::Builder::new()
        .name("remuda-cluster-read-deadline".into())
        .spawn(move || {
            if done_rx.recv_timeout(max_read_time).is_err() {
                let _ = wake_stream.shutdown(Shutdown::Both);
            }
        })?;
    let result = stream
        .try_clone()
        .and_then(|reader| parse_http_request(std::io::BufReader::new(reader)));
    let _ = done_tx.send(());
    let _ = timer.join();
    result
}

fn read_header_line<R: BufRead>(reader: &mut R, max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    let read = reader
        .take((max_bytes + 1) as u64)
        .read_until(b'\n', &mut line)?;
    if read == 0 || line.len() > max_bytes || !line.ends_with(b"\r\n") {
        return Err(invalid_http("HTTP line is incomplete or exceeds byte cap"));
    }
    line.truncate(line.len() - 2);
    if line
        .iter()
        .any(|byte| *byte == 0 || (*byte < 0x20 && *byte != b'\t'))
    {
        return Err(invalid_http("HTTP line contains a control byte"));
    }
    Ok(line)
}

fn parse_header(line: &[u8]) -> io::Result<(&str, &str)> {
    if line.first().is_some_and(u8::is_ascii_whitespace) {
        return Err(invalid_http("folded HTTP headers are not supported"));
    }
    let colon = line
        .iter()
        .position(|byte| *byte == b':')
        .ok_or_else(|| invalid_http("malformed HTTP header"))?;
    let name = std::str::from_utf8(&line[..colon])
        .map_err(|_| invalid_http("HTTP header name is not ASCII"))?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
    {
        return Err(invalid_http("invalid HTTP header name"));
    }
    let value = std::str::from_utf8(&line[colon + 1..])
        .map_err(|_| invalid_http("HTTP header value is not ASCII"))?
        .trim_matches([' ', '\t']);
    Ok((name, value))
}

fn invalid_http(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn authorize_remote_request(request: &Request) -> io::Result<()> {
    match request {
        // Enable after #252 (PTY write timeout) merges.
        Request::Input { .. } => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote front refuses Input",
            ));
        }
        // Enable only after the listener's explicit Close security review.
        Request::Close { .. } => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote front refuses Close",
            ));
        }
        _ => {}
    }
    crate::remote_front::authorize(request)
        .map_err(|reason| io::Error::new(io::ErrorKind::PermissionDenied, reason))
}

impl MemberRegistryCache {
    fn authorize(&self, peer_static: &[u8]) -> io::Result<()> {
        let revision = cluster::registry::registry_revision_token()?;
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        refresh_registry_snapshot(&mut cached, revision, cluster::load_registry)?;
        authorize_key_in_registry(&cached.1, peer_static)
    }
}

fn refresh_registry_snapshot(
    cached: &mut (String, cluster::Registry),
    revision: String,
    load: impl FnOnce() -> io::Result<cluster::Registry>,
) -> io::Result<()> {
    if cached.0 != revision {
        cached.1 = load()?;
        cached.0 = revision;
    }
    Ok(())
}

fn authorize_key_in_registry(registry: &cluster::Registry, peer_static: &[u8]) -> io::Result<()> {
    let fingerprint = cluster::encoding::fingerprint(peer_static);
    let member = registry
        .authorized_nodes
        .iter()
        .find(|entry| entry.node_fp == fingerprint)
        .ok_or_else(permission_denied)?;
    if member.state != NodeState::Admitted
        || member.static_pubkey != cluster::encoding::encode_base64(peer_static)
    {
        return Err(permission_denied());
    }
    Ok(())
}

fn permission_denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "remote node is not admitted",
    )
}

fn dispatch_and_reauthorize<A, D>(
    peer_static: &[u8],
    mut authorize: A,
    max_hold: Duration,
    dispatch: D,
) -> io::Result<Vec<u8>>
where
    A: FnMut(&[u8]) -> io::Result<()>,
    D: FnOnce() -> io::Result<Vec<u8>>,
{
    authorize(peer_static)?;
    let started = Instant::now();
    let response = dispatch()?;
    if started.elapsed() > effective_hold_timeout(max_hold) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "held cluster request exceeded server timeout",
        ));
    }
    authorize(peer_static)?;
    Ok(response)
}

fn dispatch_payload(payload: &[u8], daemon_path: &Path) -> io::Result<Vec<u8>> {
    let request = crate::remote_front::decode_frame(payload)
        .map_err(|reason| io::Error::new(io::ErrorKind::InvalidData, reason))?;
    authorize_remote_request(&request)?;
    crate::remote_front::forward_frame_with_timeout(
        daemon_path,
        payload,
        LOCAL_DAEMON_REQUEST_TIMEOUT.min(MAX_HELD_REQUEST),
    )
    .map_err(|reason| io::Error::other(format!("local request failed: {reason}")))
}

fn spawn_connection_handler(
    stream: TcpStream,
    remote_addr: SocketAddr,
    state: Arc<ListenerState>,
    authorize: MemberAuthorizer,
    dispatch: FrameDispatcher,
    limits: ConnectionLimits,
) -> io::Result<()> {
    let Some(ip_permit) = state.limiter.acquire_ip(remote_addr.ip()) else {
        return write_http_response(stream, 429, b"source address request capacity reached");
    };
    let Some(global_permit) = state.limiter.acquire_global() else {
        return write_http_response(stream, 503, b"request capacity reached");
    };
    std::thread::Builder::new()
        .name("remuda-cluster-listener".into())
        .spawn(move || {
            let _ip_permit = ip_permit;
            handle_connection_with(stream, state, global_permit, authorize, dispatch, limits);
        })
        .map(|_| ())
}

fn handle_connection_with(
    stream: TcpStream,
    state: Arc<ListenerState>,
    global_permit: Arc<GlobalPermit>,
    authorize: MemberAuthorizer,
    dispatch: FrameDispatcher,
    limits: ConnectionLimits,
) {
    let _ = stream.set_read_timeout(Some(limits.idle_read));
    let _ = stream.set_write_timeout(Some(limits.idle_read));
    let inbound = match parse_socket_request_with_timeout(&stream, limits.total_read) {
        Ok(request) => request,
        Err(_) => return ignore_response_error(write_http_response(stream, 400, b"bad request")),
    };
    let opened = match frame::open_request(&state.responder_private, &inbound.body) {
        Ok(request) => request,
        Err(_) => return ignore_response_error(write_http_response(stream, 400, b"bad frame")),
    };
    let peer_fp = cluster::encoding::fingerprint(&opened.peer_static);
    if authorize(&opened.peer_static).is_err() {
        return ignore_response_error(write_http_response(stream, 403, b"not admitted"));
    }
    let now = crate::SystemWallClock::new().unix_seconds() as i64;
    let replay = state
        .replay
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .check_and_insert(&peer_fp, opened.ephemeral, opened.timestamp_seconds, now);
    if replay.is_err() {
        return ignore_response_error(write_http_response(stream, 409, b"replayed or stale frame"));
    }
    let Some(peer_permit) = state.limiter.acquire_peer(&peer_fp) else {
        return ignore_response_error(write_http_response(
            stream,
            429,
            b"peer request capacity reached",
        ));
    };
    let payload = opened.payload.clone();
    let peer_static = opened.peer_static.clone();
    let dispatch_worker = dispatch.clone();
    let authorize_worker = authorize.clone();
    let worker_global = global_permit.clone();
    let worker_peer = peer_permit.clone();
    let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("remuda-cluster-dispatch".into())
        .spawn(move || {
            let _global = worker_global;
            let _peer = worker_peer;
            let result = dispatch_and_reauthorize(
                &peer_static,
                |key| authorize_worker(key),
                limits.post_dispatch_hold,
                || dispatch_worker(&payload),
            );
            let _ = reply_tx.send(result);
        });
    if worker.is_err() {
        let response = encode_error("unable to start request worker");
        return send_encrypted_response(stream, opened, &response);
    }
    let response = match reply_rx.recv_timeout(effective_hold_timeout(limits.outer_hold)) {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => encode_error(&error.to_string()),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => encode_error("request timed out"),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            encode_error("request worker failed")
        }
    };
    if authorize(&opened.peer_static).is_err() {
        let refused = encode_error("remote node was revoked while the request was held");
        return send_encrypted_response(stream, opened, &refused);
    }
    send_encrypted_response(stream, opened, &response)
}

fn encode_error(reason: &str) -> Vec<u8> {
    serde_json::to_vec(&Response::error(reason)).unwrap_or_else(|_| b"null".to_vec())
}

fn send_encrypted_response(stream: TcpStream, opened: frame::OpenedRequest, payload: &[u8]) {
    let payload = bounded_response_payload(payload);
    let result = frame::seal_response(opened, &payload)
        .and_then(|body| write_http_response(stream, 200, &body));
    ignore_response_error(result);
}

fn bounded_response_payload(payload: &[u8]) -> Vec<u8> {
    if payload.len() > frame::MAX_RESPONSE_PAYLOAD {
        encode_error("cluster response exceeds the Noise frame limit")
    } else {
        payload.to_vec()
    }
}

fn write_http_response(mut stream: TcpStream, status: u16, body: &[u8]) -> io::Result<()> {
    let label = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        409 => "Conflict",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {label}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

fn ignore_response_error(result: io::Result<()>) {
    let _ = result;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{AuthorizedNode, Registry};
    use remuda_core::protocol::Request;
    use std::io;
    use std::io::Cursor;
    use std::sync::{mpsc, Arc};

    #[cfg(unix)]
    struct SocketTestServer {
        address: SocketAddr,
        responder_public: Vec<u8>,
        responder_private: Vec<u8>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<io::Result<()>>>,
        state_dir: PathBuf,
    }

    #[cfg(unix)]
    impl SocketTestServer {
        fn start(
            responder_private: Vec<u8>,
            responder_public: Vec<u8>,
            authorize: MemberAuthorizer,
            dispatch: FrameDispatcher,
            limits: ConnectionLimits,
        ) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let responder_private_for_test = responder_private.clone();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let state_dir = std::env::temp_dir().join(format!(
                "remuda-listener-test-{}-{}",
                std::process::id(),
                LISTENER_ERROR_COUNT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&state_dir).unwrap();
            std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            let join_tokens =
                JoinTokenStore::open_at(&state_dir, Arc::new(crate::SystemWallClock::new()))
                    .unwrap();
            let state = Arc::new(ListenerState {
                responder_private: Zeroizing::new(responder_private),
                replay: Mutex::new(replay::ReplayWindow::new(REPLAY_CAPACITY)),
                limiter: Arc::new(RequestLimiter::default()),
                join_tokens,
            });
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = stop.clone();
            let thread = std::thread::spawn(move || {
                serve_socket_until(
                    &listener,
                    &thread_stop,
                    || Ok(()),
                    move |stream, remote_addr| {
                        spawn_connection_handler(
                            stream,
                            remote_addr,
                            state.clone(),
                            authorize.clone(),
                            dispatch.clone(),
                            limits,
                        )
                    },
                )
            });
            Self {
                address,
                responder_public,
                responder_private: responder_private_for_test,
                stop,
                thread: Some(thread),
                state_dir,
            }
        }

        fn start_production(
            responder_private: Vec<u8>,
            responder_public: Vec<u8>,
            authorize: MemberAuthorizer,
            dispatch: FrameDispatcher,
        ) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let responder_private_for_test = responder_private.clone();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let state_dir = std::env::temp_dir().join(format!(
                "remuda-listener-production-test-{}-{}",
                std::process::id(),
                LISTENER_ERROR_COUNT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&state_dir).unwrap();
            std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            let join_tokens =
                JoinTokenStore::open_at(&state_dir, Arc::new(crate::SystemWallClock::new()))
                    .unwrap();
            let state = Arc::new(ListenerState {
                responder_private: Zeroizing::new(responder_private),
                replay: Mutex::new(replay::ReplayWindow::new(REPLAY_CAPACITY)),
                limiter: Arc::new(RequestLimiter::default()),
                join_tokens,
            });
            let listener = Listener {
                socket: listener,
                state,
                authorize,
                dispatch,
            };
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = stop.clone();
            let thread = std::thread::spawn(move || listener.serve_until(&thread_stop));
            Self {
                address,
                responder_public,
                responder_private: responder_private_for_test,
                stop,
                thread: Some(thread),
                state_dir,
            }
        }

        fn exchange(&self, sealed: frame::SealedRequest) -> (u16, Vec<u8>) {
            let mut stream = TcpStream::connect(self.address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            write_http_request(&mut stream, &sealed.message);
            let (status, body) = read_http_response(&mut stream).unwrap();
            if status == 200 {
                (status, frame::open_response(sealed, &body).unwrap())
            } else {
                (status, body)
            }
        }

        fn send_raw(&self, body: &[u8], headers: &[u8]) -> io::Result<(u16, Vec<u8>)> {
            let mut stream = TcpStream::connect(self.address)?;
            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
            write!(stream, "POST /cluster HTTP/1.1\r\nHost: test\r\n")?;
            stream.write_all(headers)?;
            stream.write_all(b"\r\n")?;
            stream.write_all(body)?;
            stream.flush()?;
            read_http_response(&mut stream)
        }
    }

    #[cfg(unix)]
    impl Drop for SocketTestServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            let _ = std::fs::remove_dir_all(&self.state_dir);
        }
    }

    #[cfg(unix)]
    fn write_http_request(stream: &mut TcpStream, body: &[u8]) {
        write!(
            stream,
            "POST /cluster HTTP/1.1\r\nHost: test\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
    }

    #[cfg(unix)]
    fn read_http_response(stream: &mut TcpStream) -> io::Result<(u16, Vec<u8>)> {
        let mut reader = std::io::BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let status = line
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing HTTP status"))?;
        let mut length = None;
        loop {
            line.clear();
            reader.read_line(&mut line)?;
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                length = value.trim().parse::<usize>().ok();
            }
        }
        let mut body = vec![
            0;
            length.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing response Content-Length",
                )
            })?
        ];
        reader.read_exact(&mut body)?;
        Ok((status, body))
    }

    #[cfg(unix)]
    fn socket_server(
        dispatch: impl Fn(&[u8]) -> io::Result<Vec<u8>> + Send + Sync + 'static,
        held_timeout: Duration,
        post_dispatch_hold_limit: Duration,
        idle_timeout: Duration,
        read_timeout: Duration,
    ) -> (SocketTestServer, snow::Keypair, Arc<Mutex<Registry>>) {
        let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let peer = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let registry = Arc::new(Mutex::new(Registry {
            authorized_nodes: vec![AuthorizedNode {
                node_fp: crate::cluster::encoding::fingerprint(&peer.public),
                static_pubkey: crate::cluster::encoding::encode_base64(&peer.public),
                state: NodeState::Admitted,
                version: 1,
                by: "test".into(),
            }],
        }));
        let authorizer_registry = registry.clone();
        let authorize: MemberAuthorizer = Arc::new(move |key| {
            authorize_key_in_registry(&authorizer_registry.lock().unwrap(), key)
        });
        let server = SocketTestServer::start(
            responder.private,
            responder.public,
            authorize,
            Arc::new(dispatch),
            ConnectionLimits {
                outer_hold: held_timeout,
                post_dispatch_hold: post_dispatch_hold_limit,
                idle_read: idle_timeout,
                total_read: read_timeout,
            },
        );
        (server, peer, registry)
    }

    #[cfg(unix)]
    fn production_socket_server() -> (SocketTestServer, snow::Keypair) {
        let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let peer = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let server = SocketTestServer::start_production(
            responder.private,
            responder.public,
            Arc::new(|_| Ok(())),
            Arc::new(|_| Ok(serde_json::to_vec(&Response::Ok).unwrap())),
        );
        (server, peer)
    }

    #[cfg(unix)]
    fn sealed_list_request(
        peer: &snow::Keypair,
        server: &SocketTestServer,
    ) -> frame::SealedRequest {
        let now = crate::SystemWallClock::new().unix_seconds() as i64;
        frame::seal_request(
            &peer.private,
            &server.responder_public,
            now,
            &serde_json::to_vec(&Request::List).unwrap(),
        )
        .unwrap()
    }

    #[cfg(unix)]
    fn sealed_payload_request(
        peer: &snow::Keypair,
        server: &SocketTestServer,
        payload: &[u8],
    ) -> frame::SealedRequest {
        let now = crate::SystemWallClock::new().unix_seconds() as i64;
        frame::seal_request(&peer.private, &server.responder_public, now, payload).unwrap()
    }

    #[cfg(unix)]
    fn socket_test_timeout() -> Duration {
        Duration::from_secs(1)
    }

    #[test]
    fn unspecified_bind_requires_explicit_public_opt_in() {
        assert!(validate_bind_address("127.0.0.1:0".parse().unwrap(), false).is_ok());
        assert!(validate_bind_address("0.0.0.0:0".parse().unwrap(), false).is_err());
        assert!(validate_bind_address("[::]:0".parse().unwrap(), false).is_err());
        assert!(validate_bind_address("0.0.0.0:0".parse().unwrap(), true).is_ok());
    }

    #[test]
    fn remote_input_is_refused_until_pty_timeout_merges() {
        assert!(authorize_remote_request(&Request::List).is_ok());
        let close_error = authorize_remote_request(&Request::Close {
            name: "session".into(),
            instance_id: None,
            confirm: None,
        })
        .unwrap_err();
        assert_eq!(close_error.to_string(), "remote front refuses Close");
        let error = authorize_remote_request(&Request::Input {
            name: "session".into(),
            instance_id: "instance".into(),
            client_id: "00000000000000000000000000000001".into(),
            seq: 1,
            bytes: b"hello\r".to_vec(),
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "remote front refuses Input");
    }

    #[test]
    fn remote_dispatch_keeps_eval_and_shutdown_out_of_the_wire_surface() {
        assert!(authorize_remote_request(&Request::Eval {
            code: "return 1".into(),
            name: None,
        })
        .is_err());
        assert!(authorize_remote_request(&Request::Shutdown {
            requester_daemon_id: None,
            requester_session_id: None,
            requester_session_name: None,
            override_hosted: false,
        })
        .is_err());
    }

    #[test]
    fn bounded_http_parser_accepts_one_content_length_post() {
        let request = b"POST /cluster HTTP/1.1\r\nHost: node\r\nContent-Length: 4\r\n\r\ntest";
        let parsed = parse_http_request(Cursor::new(request)).unwrap();
        assert_eq!(parsed.body, b"test");
    }

    #[test]
    fn bounded_http_parser_names_missing_content_length() {
        let request = b"POST /cluster HTTP/1.1\r\nHost: node\r\n\r\n";
        let error = parse_http_request(Cursor::new(request)).err().unwrap();
        assert_eq!(error.to_string(), "Content-Length is required");
    }

    #[test]
    fn accepted_stream_reads_a_body_sent_after_a_split_write() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .write_all(b"POST /cluster HTTP/1.1\r\nHost: node\r\nContent-Length: 4\r\n\r\n")
                .unwrap();
            std::thread::sleep(Duration::from_millis(200));
            stream.write_all(b"test").unwrap();
        });

        let (stream, _) = loop {
            match listener.accept() {
                Ok(pair) => break pair,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        };
        prepare_accepted_stream(&stream).unwrap();
        let parsed = parse_socket_request_with_timeout(&stream, MAX_REQUEST_READ_TIME).unwrap();
        client.join().unwrap();
        assert_eq!(parsed.body, b"test");
    }

    #[cfg(unix)]
    #[test]
    fn production_serve_until_reads_a_body_sent_after_a_split_write() {
        let (server, _) = production_socket_server();
        let mut stream = TcpStream::connect(server.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .write_all(b"POST /cluster HTTP/1.1\r\nHost: node\r\nContent-Length: 4\r\n\r\n")
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        stream.write_all(b"test").unwrap();
        assert_eq!(
            read_http_response(&mut stream).unwrap(),
            (400, b"bad frame".to_vec())
        );
    }

    #[cfg(unix)]
    #[test]
    fn production_serve_until_caps_pre_auth_connections_per_source_ip() {
        let (server, _) = production_socket_server();
        let held = (0..MAX_PREAUTH_PER_IP)
            .map(|_| TcpStream::connect(server.address).unwrap())
            .collect::<Vec<_>>();
        std::thread::sleep(Duration::from_millis(100));
        let mut fifth = TcpStream::connect(server.address).unwrap();
        fifth.set_read_timeout(Some(socket_test_timeout())).unwrap();
        let (status, body) = read_http_response(&mut fifth).unwrap();
        assert_eq!(status, 429);
        assert_eq!(body, b"source address request capacity reached");
        drop(held);
    }

    #[cfg(unix)]
    #[test]
    fn accept_loop_survives_a_connection_handler_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let loop_stop = stop.clone();
        let limiter = Arc::new(RequestLimiter::default());
        let held = (0..MAX_GLOBAL_REQUESTS)
            .map(|_| limiter.acquire_global().unwrap())
            .collect::<Vec<_>>();
        let (first_tx, first_rx) = mpsc::channel();
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let write_errors = Arc::new(AtomicUsize::new(0));
        let write_errors_for_server = write_errors.clone();
        let server = std::thread::spawn(move || {
            let mut requests = 0;
            serve_socket_until(
                &listener,
                &loop_stop,
                || Ok(()),
                move |stream, _| {
                    requests += 1;
                    if requests == 1 {
                        first_tx.send(()).unwrap();
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    let response = if limiter.acquire_global().is_none() {
                        write_http_response(stream, 503, b"request capacity reached")
                    } else {
                        drop(stream);
                        Ok(())
                    };
                    if response.is_err() {
                        write_errors_for_server.fetch_add(1, Ordering::SeqCst);
                    }
                    if requests == 2 {
                        accepted_tx.send(()).unwrap();
                    }
                    response
                },
            )
        });

        let first = TcpStream::connect(address).unwrap();
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        unsafe {
            libc::setsockopt(
                std::os::fd::AsRawFd::as_raw_fd(&first),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                std::ptr::addr_of!(linger).cast(),
                std::mem::size_of_val(&linger) as libc::socklen_t,
            );
        }
        first_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(first);
        drop(TcpStream::connect(address).unwrap());
        accepted_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(write_errors.load(Ordering::SeqCst) >= 1);
        stop.store(true, Ordering::Release);
        server.join().unwrap().unwrap();
        drop(held);
    }

    #[test]
    fn bounded_http_parser_rejects_transfer_encoding_and_duplicate_lengths() {
        let chunked =
            b"POST /cluster HTTP/1.1\r\nHost: node\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
        assert!(parse_http_request(Cursor::new(chunked)).is_err());
        let unknown = b"POST /cluster HTTP/1.1\r\nHost: node\r\nTransfer-Encoding: gzip\r\nContent-Length: 0\r\n\r\n";
        assert!(parse_http_request(Cursor::new(unknown)).is_err());
        let duplicate = b"POST /cluster HTTP/1.1\r\nHost: node\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n";
        assert!(parse_http_request(Cursor::new(duplicate)).is_err());
    }

    #[test]
    fn bounded_http_parser_enforces_line_header_and_body_caps() {
        let mut long_line = vec![b'a'; MAX_REQUEST_LINE_BYTES + 1];
        long_line.extend_from_slice(b"\r\n");
        assert!(parse_http_request(Cursor::new(long_line)).is_err());

        let mut many_headers = b"POST /cluster HTTP/1.1\r\nHost: node\r\n".to_vec();
        for _ in 0..=MAX_HEADER_COUNT {
            many_headers.extend_from_slice(b"X-Test: y\r\n");
        }
        many_headers.extend_from_slice(b"Content-Length: 0\r\n\r\n");
        assert!(parse_http_request(Cursor::new(many_headers)).is_err());

        let too_large = format!(
            "POST /cluster HTTP/1.1\r\nHost: node\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert!(parse_http_request(Cursor::new(too_large)).is_err());

        let oversized_headers = format!(
            "POST /cluster HTTP/1.1\r\nHost: node\r\nX-Large: {}\r\nContent-Length: 0\r\n\r\n",
            "x".repeat(MAX_HEADER_BYTES)
        );
        assert!(parse_http_request(Cursor::new(oversized_headers)).is_err());
    }

    #[test]
    fn authorization_is_rechecked_after_a_held_request_before_data_returns() {
        let pair = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let fingerprint = crate::cluster::encoding::fingerprint(&pair.public);
        let registry = Arc::new(Mutex::new(Registry {
            authorized_nodes: vec![AuthorizedNode {
                node_fp: fingerprint.clone(),
                static_pubkey: crate::cluster::encoding::encode_base64(&pair.public),
                state: NodeState::Admitted,
                version: 1,
                by: fingerprint,
            }],
        }));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let checking = registry.clone();
        let peer_static = pair.public.clone();
        let worker = std::thread::spawn(move || {
            dispatch_and_reauthorize(
                &peer_static,
                |key| authorize_key_in_registry(&checking.lock().unwrap(), key),
                MAX_HELD_REQUEST,
                || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(b"private session data".to_vec())
                },
            )
        });
        entered_rx.recv().unwrap();
        registry.lock().unwrap().authorized_nodes[0].state = NodeState::Revoked;
        release_tx.send(()).unwrap();
        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn future_sync_hold_limit_is_server_owned() {
        assert_eq!(
            effective_hold_timeout(std::time::Duration::from_secs(300)),
            std::time::Duration::from_secs(30)
        );
        assert_eq!(
            effective_hold_timeout(std::time::Duration::from_secs(5)),
            std::time::Duration::from_secs(5)
        );
    }

    #[test]
    fn oversized_result_becomes_a_clear_encrypted_error_payload() {
        let payload = bounded_response_payload(&vec![b'x'; frame::MAX_RESPONSE_PAYLOAD + 1]);
        assert!(payload.len() < frame::MAX_RESPONSE_PAYLOAD);
        let response: Response = serde_json::from_slice(&payload).unwrap();
        assert!(matches!(response, Response::Error(message) if message.contains("frame limit")));
    }

    #[test]
    fn request_limiter_caps_global_and_per_peer_work() {
        let limiter = Arc::new(RequestLimiter::default());
        let mut global = (0..MAX_GLOBAL_REQUESTS)
            .map(|_| limiter.acquire_global().unwrap())
            .collect::<Vec<_>>();
        assert!(limiter.acquire_global().is_none());
        drop(global.pop());
        assert!(limiter.acquire_global().is_some());

        let mut peer = (0..MAX_PEER_REQUESTS)
            .map(|_| limiter.acquire_peer("peer-a").unwrap())
            .collect::<Vec<_>>();
        assert!(limiter.acquire_peer("peer-a").is_none());
        assert!(limiter.acquire_peer("peer-b").is_some());
        drop(peer.pop());
        assert!(limiter.acquire_peer("peer-a").is_some());
    }

    #[test]
    fn request_limiter_caps_pre_auth_connections_per_source_ip() {
        let limiter = Arc::new(RequestLimiter::default());
        let source: std::net::IpAddr = "192.0.2.10".parse().unwrap();
        let permits = (0..MAX_PREAUTH_PER_IP)
            .map(|_| limiter.acquire_ip(source).unwrap())
            .collect::<Vec<_>>();
        assert!(limiter.acquire_ip(source).is_none());
        assert!(limiter.acquire_ip("192.0.2.11".parse().unwrap()).is_some());
        drop(permits);
        assert!(limiter.acquire_ip(source).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn socket_accepts_a_fresh_ik_request_and_refuses_its_replay() {
        let (server, peer, _) = socket_server(
            |payload| dispatch_payload(payload, Path::new("unused-daemon-path")),
            socket_test_timeout(),
            socket_test_timeout(),
            socket_test_timeout(),
            socket_test_timeout(),
        );
        let close = Request::Close {
            name: "session-to-protect".into(),
            instance_id: None,
            confirm: None,
        };
        let sealed = sealed_payload_request(&peer, &server, &serde_json::to_vec(&close).unwrap());
        let duplicate_body = sealed.message.clone();
        let (status, payload) = server.exchange(sealed);
        assert_eq!(status, 200);
        assert!(matches!(
            serde_json::from_slice::<Response>(&payload).unwrap(),
            Response::Error(message) if message.contains("Close")
        ));

        let content_length = format!("Content-Length: {}\r\n", duplicate_body.len());
        let (status, _) = server
            .send_raw(&duplicate_body, content_length.as_bytes())
            .unwrap();
        assert_eq!(status, 409);
    }

    #[cfg(unix)]
    #[test]
    fn socket_rejects_low_order_unknown_and_mismatched_static_keys_before_dispatch() {
        let dispatched = Arc::new(AtomicUsize::new(0));
        let dispatched_for_worker = dispatched.clone();
        let (server, peer, registry) = socket_server(
            move |_| {
                dispatched_for_worker.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::to_vec(&Response::Ok).unwrap())
            },
            socket_test_timeout(),
            socket_test_timeout(),
            socket_test_timeout(),
            socket_test_timeout(),
        );
        let low_order = frame::request_with_low_order_key(&server.responder_public, false).unwrap();
        assert!(low_order.len() > 32);
        let error = frame::open_request(&server.responder_private, &low_order)
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "low-order Noise DH result");
        let content_length = format!("Content-Length: {}\r\n", low_order.len());
        let (status, body) = server
            .send_raw(&low_order, content_length.as_bytes())
            .unwrap();
        assert_eq!((status, body), (400, b"bad frame".to_vec()));

        let unknown = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let unknown_request = sealed_payload_request(
            &unknown,
            &server,
            &serde_json::to_vec(&Request::List).unwrap(),
        );
        assert_eq!(server.exchange(unknown_request).0, 403);

        let wrong_key = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        registry.lock().unwrap().authorized_nodes[0].static_pubkey =
            crate::cluster::encoding::encode_base64(&wrong_key.public);
        let mismatched = sealed_list_request(&peer, &server);
        assert_eq!(server.exchange(mismatched).0, 403);
        assert_eq!(dispatched.load(Ordering::SeqCst), 0);
    }

    #[cfg(unix)]
    #[test]
    fn socket_denials_and_malformed_json_never_echo_request_payloads() {
        let dispatched = Arc::new(AtomicUsize::new(0));
        let dispatched_for_worker = dispatched.clone();
        let (server, peer, _) = socket_server(
            move |payload| {
                let request =
                    crate::remote_front::decode_frame(payload).map_err(io::Error::other)?;
                authorize_remote_request(&request)?;
                dispatched_for_worker.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::to_vec(&Response::Ok).unwrap())
            },
            socket_test_timeout(),
            socket_test_timeout(),
            socket_test_timeout(),
            socket_test_timeout(),
        );
        let secret = "SECRET_PAYLOAD_XYZ";
        let refused = [
            Request::Eval {
                code: secret.to_owned(),
                name: None,
            },
            Request::Send {
                name: "session".into(),
                bytes: secret.as_bytes().to_vec(),
            },
            Request::Close {
                name: "session".into(),
                instance_id: None,
                confirm: None,
            },
        ];
        for request in refused {
            let sealed =
                sealed_payload_request(&peer, &server, &serde_json::to_vec(&request).unwrap());
            let (status, response) = server.exchange(sealed);
            assert_eq!(status, 200);
            let response = String::from_utf8(response).unwrap();
            assert!(response.starts_with("{\"Error\":\"remote front refuses "));
            assert!(!response.contains(secret));
        }

        let malformed =
            sealed_payload_request(&peer, &server, br#"{"Eval":{"code":"SECRET_PAYLOAD_XYZ""#);
        let (status, response) = server.exchange(malformed);
        assert_eq!(status, 200);
        let response = String::from_utf8(response).unwrap();
        assert!(response.contains("invalid remote request"));
        assert!(!response.contains(secret));
        assert_eq!(dispatched.load(Ordering::SeqCst), 0);
    }

    #[cfg(unix)]
    #[test]
    fn socket_remote_input_is_refused_without_dispatch() {
        let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let peer = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let server = SocketTestServer::start_production(
            responder.private,
            responder.public,
            Arc::new(|_| Ok(())),
            Arc::new(|payload| dispatch_payload(payload, Path::new("unused-daemon-path"))),
        );
        let request = Request::Input {
            name: "session".into(),
            instance_id: "instance".into(),
            client_id: "00000000000000000000000000000001".into(),
            seq: 1,
            bytes: b"hello\r".to_vec(),
        };
        let sealed = sealed_payload_request(&peer, &server, &serde_json::to_vec(&request).unwrap());
        let (status, response) = server.exchange(sealed);
        assert_eq!(status, 200);
        let response: Response = serde_json::from_slice(&response).unwrap();
        assert!(
            matches!(&response, Response::Error(message) if message == "remote front refuses Input"),
            "unexpected response: {response:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn socket_rechecks_membership_before_releasing_a_held_response() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let worker_release_rx = release_rx.clone();
        let (server, peer, registry) = socket_server(
            move |_| {
                entered_tx.send(()).unwrap();
                worker_release_rx.lock().unwrap().recv().unwrap();
                Ok(serde_json::to_vec(&Response::Value("held secret".into())).unwrap())
            },
            socket_test_timeout(),
            socket_test_timeout(),
            socket_test_timeout(),
            socket_test_timeout(),
        );
        let address = server.address;
        let sealed = sealed_list_request(&peer, &server);
        let message = sealed.message.clone();
        let request_thread = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(socket_test_timeout()))
                .unwrap();
            write_http_request(&mut stream, &message);
            let (status, body) = read_http_response(&mut stream).unwrap();
            (status, body)
        });
        entered_rx.recv_timeout(socket_test_timeout()).unwrap();
        registry.lock().unwrap().authorized_nodes[0].state = NodeState::Revoked;
        release_tx.send(()).unwrap();
        let (status, encrypted) = request_thread.join().unwrap();
        assert_eq!(status, 200);
        let response = frame::open_response(sealed, &encrypted).unwrap();
        let response: Response = serde_json::from_slice(&response).unwrap();
        assert!(matches!(response, Response::Error(message) if message.contains("revoked")));
    }

    #[cfg(unix)]
    #[test]
    fn socket_held_requests_obey_outer_deadline_and_post_dispatch_hold_check() {
        let (server, peer, _) = socket_server(
            |_| {
                std::thread::sleep(Duration::from_millis(250));
                Ok(serde_json::to_vec(&Response::Ok).unwrap())
            },
            Duration::from_millis(60),
            Duration::from_secs(1),
            socket_test_timeout(),
            socket_test_timeout(),
        );
        let started = Instant::now();
        let (_, payload) = server.exchange(sealed_list_request(&peer, &server));
        assert!(started.elapsed() < Duration::from_millis(200));
        let response: Response = serde_json::from_slice(&payload).unwrap();
        assert!(matches!(response, Response::Error(message) if message.contains("timed out")));

        let (server, peer, _) = socket_server(
            |_| {
                std::thread::sleep(Duration::from_millis(80));
                Ok(serde_json::to_vec(&Response::Ok).unwrap())
            },
            Duration::from_secs(1),
            Duration::from_millis(30),
            socket_test_timeout(),
            socket_test_timeout(),
        );
        let (_, payload) = server.exchange(sealed_list_request(&peer, &server));
        let response: Response = serde_json::from_slice(&payload).unwrap();
        assert!(
            matches!(response, Response::Error(message) if message.contains("held cluster request exceeded"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn socket_http_requires_content_length_and_rejects_an_over_cap_body() {
        let (server, _peer, _) = socket_server(
            |_| Ok(serde_json::to_vec(&Response::Ok).unwrap()),
            socket_test_timeout(),
            socket_test_timeout(),
            socket_test_timeout(),
            socket_test_timeout(),
        );
        assert_eq!(
            server.send_raw(b"", b"").unwrap(),
            (400, b"bad request".to_vec())
        );
        let over_cap = vec![0; MAX_BODY_BYTES + 1];
        let header = format!("Content-Length: {}\r\n", over_cap.len());
        let full_body_result = server.send_raw(&over_cap, header.as_bytes());
        assert!(full_body_result.is_err() || full_body_result.unwrap().0 == 400);

        let mut stream = TcpStream::connect(server.address).unwrap();
        stream
            .set_read_timeout(Some(socket_test_timeout()))
            .unwrap();
        write!(
            stream,
            "POST /cluster HTTP/1.1\r\nHost: test\r\nContent-Length: {}\r\n\r\n",
            over_cap.len()
        )
        .unwrap();
        stream.write_all(b"x").unwrap();
        let started = Instant::now();
        assert_eq!(read_http_response(&mut stream).unwrap().0, 400);
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[cfg(unix)]
    #[test]
    fn socket_idle_read_deadline_closes_a_slowloris_request() {
        let (server, _peer, _) = socket_server(
            |_| Ok(serde_json::to_vec(&Response::Ok).unwrap()),
            socket_test_timeout(),
            socket_test_timeout(),
            Duration::from_millis(80),
            Duration::from_secs(2),
        );
        let started = Instant::now();
        let mut stream = TcpStream::connect(server.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .write_all(b"POST /cluster HTTP/1.1\r\nHost: test\r\nContent-Length: 4\r\n\r\n")
            .unwrap();
        let mut tail = Vec::new();
        let _ = stream.read_to_end(&mut tail);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[cfg(unix)]
    #[test]
    fn socket_total_read_deadline_stops_a_slow_drip_that_beats_idle_timeout() {
        let (server, _peer, _) = socket_server(
            |_| Ok(serde_json::to_vec(&Response::Ok).unwrap()),
            socket_test_timeout(),
            socket_test_timeout(),
            Duration::from_millis(150),
            Duration::from_millis(300),
        );
        let started = Instant::now();
        let mut stream = TcpStream::connect(server.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let request = b"POST /cluster HTTP/1.1\r\nHost: test\r\nContent-Length: 4\r\n\r\ntest";
        for byte in request {
            if stream.write_all(&[*byte]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let mut tail = Vec::new();
        let _ = stream.read_to_end(&mut tail);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn member_authorization_refuses_unknown_and_revoked_keys() {
        let pair = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let fingerprint = crate::cluster::encoding::fingerprint(&pair.public);
        let mut registry = Registry::default();
        registry.authorized_nodes.push(AuthorizedNode {
            node_fp: fingerprint.clone(),
            static_pubkey: crate::cluster::encoding::encode_base64(&pair.public),
            state: NodeState::Admitted,
            version: 1,
            by: fingerprint,
        });
        assert!(authorize_key_in_registry(&registry, &pair.public).is_ok());
        assert!(authorize_key_in_registry(&registry, &[8; 32]).is_err());
        registry.authorized_nodes[0].state = NodeState::Revoked;
        assert!(authorize_key_in_registry(&registry, &pair.public).is_err());
    }

    #[test]
    fn member_registry_snapshot_reloads_when_its_revision_changes() {
        let pair = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let fingerprint = crate::cluster::encoding::fingerprint(&pair.public);
        let admitted = Registry {
            authorized_nodes: vec![AuthorizedNode {
                node_fp: fingerprint.clone(),
                static_pubkey: crate::cluster::encoding::encode_base64(&pair.public),
                state: NodeState::Admitted,
                version: 1,
                by: fingerprint,
            }],
        };
        let mut cached = ("revision-1".to_owned(), admitted);
        refresh_registry_snapshot(&mut cached, "revision-1".into(), || {
            panic!("unchanged registry revision must stay cached")
        })
        .unwrap();
        let mut revoked = cached.1.clone();
        revoked.authorized_nodes[0].state = NodeState::Revoked;
        refresh_registry_snapshot(&mut cached, "revision-2".into(), || Ok(revoked)).unwrap();
        assert!(authorize_key_in_registry(&cached.1, &pair.public).is_err());
    }
}
