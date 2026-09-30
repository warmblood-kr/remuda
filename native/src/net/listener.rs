//! Bounded, VPN-address-only server for one-shot Noise IK requests.
#![allow(clippy::disallowed_types)]

use super::{frame, replay};
use crate::cluster::{self, identity, join_token::JoinTokenStore, NodeState};
use remuda_core::protocol::{Request, Response};
use remuda_core::WallClock;
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

pub const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
pub const MAX_HEADER_COUNT: usize = 64;
pub const MAX_BODY_BYTES: usize = 65_535;
#[cfg(test)]
pub const MAX_GLOBAL_REQUESTS: usize = 1;
#[cfg(not(test))]
pub const MAX_GLOBAL_REQUESTS: usize = 64;
#[cfg(test)]
pub const MAX_PREAUTH_REQUESTS: usize = 1;
#[cfg(not(test))]
pub const MAX_PREAUTH_REQUESTS: usize = 16;
pub const MAX_PEER_REQUESTS: usize = 8;
pub const MAX_PREAUTH_PER_IP: usize = 4;
pub const MAX_JOIN_ATTEMPTS_PER_IP: usize = 10;
const JOIN_ATTEMPT_WINDOW: Duration = Duration::from_secs(60);
const MAX_JOIN_ATTEMPT_IPS: usize = 4096;
pub const MAX_HELD_REQUEST: Duration = Duration::from_secs(30);
const MAX_REGISTRY_UPDATE_JSON_BYTES: usize = 48 * 1024;
const MAX_REGISTRY_PAGE_ENTRIES: usize = 32;
const MAX_REGISTRY_RATE_BUCKETS: usize = cluster::registry::MAX_REGISTRY_ENTRIES;
const SOCKET_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_READ_TIME: Duration = Duration::from_secs(5);
const LOCAL_DAEMON_REQUEST_TIMEOUT: Duration = Duration::from_secs(25);
const OBSERVE_INTERVAL: Duration = Duration::from_secs(5);
const ACCEPT_POLL: Duration = Duration::from_millis(20);
const ACCEPT_RESOURCE_BACKOFF: Duration = Duration::from_millis(250);
const REPLAY_CAPACITY: usize = 65_536;
const REMOTE_INPUT_BYTES_PER_SECOND: usize = 256 * 1024;
const REMOTE_INPUT_WINDOW: Duration = Duration::from_secs(1);
const MAX_REMOTE_INPUT_PEERS: usize = 1024;
static LISTENER_ERROR_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Listener address and opt-in for wildcard binding.
/// An opted-in IPv6 wildcard may accept IPv4 on dual-stack systems.
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
    control_source: ControlSource,
}

type MemberAuthorizer = Arc<dyn Fn(&[u8]) -> io::Result<()> + Send + Sync>;
type JoinAdmitter = Arc<dyn Fn(&[u8], Option<&str>) -> io::Result<()> + Send + Sync>;
type FrameDispatcher = Arc<dyn Fn(&[u8], &[u8]) -> io::Result<Vec<u8>> + Send + Sync>;

struct ListenerState {
    responder_private: Zeroizing<Vec<u8>>,
    replay: Mutex<replay::ReplayWindow>,
    limiter: Arc<RequestLimiter>,
    join_tokens: JoinTokenStore,
    admit_join: JoinAdmitter,
}

#[derive(Clone)]
enum ControlSource {
    DefaultStateDir,
    #[cfg(all(test, unix))]
    StateDir(PathBuf),
}

impl ControlSource {
    #[cfg(all(test, unix))]
    fn from_state_dir(path: impl Into<PathBuf>) -> Self {
        Self::StateDir(path.into())
    }

    fn enabled(&self) -> io::Result<bool> {
        match self {
            Self::DefaultStateDir => cluster::control::enabled(),
            #[cfg(all(test, unix))]
            Self::StateDir(path) => cluster::control::enabled_at(path),
        }
    }
}

struct MemberRegistryCache {
    cached: Mutex<(String, cluster::Registry)>,
}

#[derive(Default)]
struct RequestLimiter {
    active_global: AtomicUsize,
    active_ips: Mutex<HashMap<std::net::IpAddr, usize>>,
    active_peers: Mutex<HashMap<String, usize>>,
    join_attempts: Mutex<HashMap<std::net::IpAddr, VecDeque<Instant>>>,
    registry_requests: Mutex<HashMap<String, RegistryTokenBucket>>,
    remote_inputs: Mutex<RemoteInputRateLimiter>,
}

#[derive(Default)]
struct RemoteInputRateLimiter {
    windows: HashMap<String, InputWindow>,
}

struct InputWindow {
    started: Instant,
    bytes: usize,
}

impl RemoteInputRateLimiter {
    fn check(&mut self, peer_fp: &str, bytes: usize, now: Instant) -> bool {
        self.windows.retain(|_, window| {
            now.saturating_duration_since(window.started) < REMOTE_INPUT_WINDOW
        });
        if bytes == 0 || bytes > REMOTE_INPUT_BYTES_PER_SECOND {
            return false;
        }
        match self.windows.get_mut(peer_fp) {
            Some(window) if now.saturating_duration_since(window.started) < REMOTE_INPUT_WINDOW => {
                if window.bytes.saturating_add(bytes) > REMOTE_INPUT_BYTES_PER_SECOND {
                    return false;
                }
                window.bytes += bytes;
            }
            Some(window) => {
                window.started = now;
                window.bytes = bytes;
            }
            None => {
                if self.windows.len() >= MAX_REMOTE_INPUT_PEERS {
                    return false;
                }
                self.windows.insert(
                    peer_fp.to_owned(),
                    InputWindow {
                        started: now,
                        bytes,
                    },
                );
            }
        }
        true
    }
}

struct RegistryTokenBucket {
    tokens: f64,
    last_refill: Instant,
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

#[derive(Clone)]
struct ConnectionPolicy {
    limits: ConnectionLimits,
    control_source: ControlSource,
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
    let request_limiter = Arc::new(RequestLimiter::default());
    let dispatch_limiter = request_limiter.clone();
    let dispatch: FrameDispatcher = Arc::new(move |payload, peer_static| {
        dispatch_payload(payload, peer_static, &dispatch_path, &dispatch_limiter)
    });
    let listener = Listener {
        socket,
        state: Arc::new(ListenerState {
            responder_private,
            replay: Mutex::new(replay::ReplayWindow::new(REPLAY_CAPACITY)),
            limiter: request_limiter,
            join_tokens,
            admit_join: Arc::new(cluster::admit_join_locked),
        }),
        authorize,
        dispatch,
        control_source: ControlSource::DefaultStateDir,
    };
    Ok(listener)
}

/// Bind and serve requests until the listener fails.
pub fn serve(config: ListenerConfig, daemon_path: &Path) -> io::Result<()> {
    bind(config, daemon_path)?.serve()
}

impl Listener {
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Run until stopped; token state and registry changes are observed every five seconds.
    pub fn serve_until(&self, stop: &AtomicBool) -> io::Result<()> {
        std::thread::scope(|scope| {
            let (observe_error_tx, observe_error_rx) = std::sync::mpsc::channel::<io::Error>();
            let (observer_stop_tx, observer_stop_rx) = std::sync::mpsc::channel();
            scope.spawn(move || {
                if let Err(error) = self.state.join_tokens.observe() {
                    let _ = observe_error_tx.send(error);
                    return;
                }
                let mut observed_registry = match cluster::registry::registry_revision_token() {
                    Ok(revision) => Some(revision),
                    Err(error) => {
                        eprintln!("remuda: unable to inspect cluster registry revision: {error}");
                        None
                    }
                };
                loop {
                    match observer_stop_rx.recv_timeout(OBSERVE_INTERVAL) {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if let Err(error) = self.state.join_tokens.observe() {
                                let _ = observe_error_tx.send(error);
                                break;
                            }
                            match cluster::registry::registry_revision_token() {
                                Ok(revision) if observed_registry.as_ref() != Some(&revision) => {
                                    observed_registry = Some(revision);
                                    cluster::replication::registry_changed();
                                }
                                Ok(revision) => observed_registry = Some(revision),
                                Err(error) => eprintln!(
                                    "remuda: unable to inspect cluster registry revision: {error}"
                                ),
                            }
                        }
                    }
                }
            });
            let state = self.state.clone();
            let authorize = self.authorize.clone();
            let dispatch = self.dispatch.clone();
            let control_source = self.control_source.clone();
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
                        ConnectionPolicy {
                            control_source: control_source.clone(),
                            limits,
                        },
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
    fn acquire_registry_request(&self, fingerprint: &str, priority: bool) -> bool {
        const BURST: f64 = 10.0;
        const REFILL_PER_SECOND: f64 = 1.0;
        // Background pulls may use nine tokens from the burst. Keep one
        // available for a registry update carrying an admission or tombstone.
        const UPDATE_RESERVE: f64 = 1.0;
        let now = Instant::now();
        let mut buckets = self
            .registry_requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        buckets
            .retain(|_, bucket| now.duration_since(bucket.last_refill) < Duration::from_secs(60));
        if !buckets.contains_key(fingerprint) && buckets.len() >= MAX_REGISTRY_RATE_BUCKETS {
            return false;
        }
        let bucket = buckets
            .entry(fingerprint.to_owned())
            .or_insert(RegistryTokenBucket {
                tokens: BURST,
                last_refill: now,
            });
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * REFILL_PER_SECOND).min(BURST);
        bucket.last_refill = now;
        if bucket.tokens < 1.0 || (!priority && bucket.tokens < 1.0 + UPDATE_RESERVE) {
            return false;
        }
        bucket.tokens -= 1.0;
        true
    }

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

    fn allow_join_attempt(&self, address: std::net::IpAddr, now: Instant) -> bool {
        let mut attempts = self
            .join_attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        attempts.retain(|_, times| {
            while times
                .front()
                .is_some_and(|time| now.saturating_duration_since(*time) >= JOIN_ATTEMPT_WINDOW)
            {
                times.pop_front();
            }
            !times.is_empty()
        });
        if !attempts.contains_key(&address) && attempts.len() >= MAX_JOIN_ATTEMPT_IPS {
            if let Some(oldest_ip) = attempts
                .iter()
                .min_by_key(|(_, times)| times.front().copied())
                .map(|(ip, _)| *ip)
            {
                attempts.remove(&oldest_ip);
            }
        }
        let bucket = attempts.entry(address).or_default();
        if bucket.len() >= MAX_JOIN_ATTEMPTS_PER_IP {
            return false;
        }
        bucket.push_back(now);
        true
    }

    fn allow_remote_input(&self, fingerprint: &str, bytes: usize) -> bool {
        self.remote_inputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .check(fingerprint, bytes, Instant::now())
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

#[cfg(test)]
fn authorize_remote_request(request: &Request) -> io::Result<()> {
    crate::remote_front::authorize(request)
        .map_err(|reason| io::Error::new(io::ErrorKind::PermissionDenied, reason))
}

fn authorize_remote_request_with_source(
    request: &Request,
    control_source: &ControlSource,
) -> Result<(), Response> {
    let enabled = if matches!(request, Request::Input { .. } | Request::Close { .. }) {
        control_source.enabled().map_err(|_| {
            let operation = if matches!(request, Request::Input { .. }) {
                "Input"
            } else {
                "Close"
            };
            Response::error(format!(
                "remote control setting unavailable; refusing {operation}"
            ))
        })?
    } else {
        true
    };
    authorize_remote_request_with_control(request, enabled)
}

fn authorize_remote_request_with_control(
    request: &Request,
    allow_remote_control: bool,
) -> Result<(), Response> {
    match request {
        Request::Input { .. } if !allow_remote_control => {
            return Err(Response::RemoteControlDisabled);
        }
        Request::Close { .. } if !allow_remote_control => {
            return Err(Response::RemoteControlDisabled);
        }
        _ => {}
    }
    // Registry replication is a cluster-listener protocol operation handled
    // directly by dispatch_payload. It is intentionally unavailable through
    // the local remote-control front, whose allowlist serves a different API.
    if matches!(
        request,
        Request::ClusterRegistrySync { .. } | Request::ClusterRegistryUpdate { .. }
    ) {
        return Ok(());
    }
    crate::remote_front::authorize(request).map_err(Response::error)
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

fn dispatch_payload(
    payload: &[u8],
    peer_static: &[u8],
    daemon_path: &Path,
    limiter: &RequestLimiter,
) -> io::Result<Vec<u8>> {
    let request: Request = serde_json::from_slice(payload)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid cluster request"))?;
    match request {
        Request::ClusterRegistrySync { digest, offset } => {
            let peer_fp = cluster::encoding::fingerprint(peer_static);
            if !limiter.acquire_registry_request(&peer_fp, false) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registry replication rate limit exceeded",
                ));
            }
            let (identity, registry) = cluster::nodes()?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "cluster is not initialized")
            })?;
            let current_digest = registry.digest()?;
            let unchanged = digest.as_deref() == Some(current_digest.as_str());
            if offset > registry.authorized_nodes.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "registry page offset is out of bounds",
                ));
            }
            let start = offset.min(registry.authorized_nodes.len());
            let end = start
                .saturating_add(MAX_REGISTRY_PAGE_ENTRIES)
                .min(registry.authorized_nodes.len());
            let page = if unchanged {
                Vec::new()
            } else {
                registry.authorized_nodes[start..end].to_vec()
            };
            let entries_json = cluster::registry::RegistryUpdate::encode_entries_json(&page)?;
            let next_offset = (!unchanged && end < registry.authorized_nodes.len()).then_some(end);
            serde_json::to_vec(&Response::ClusterRegistryPage {
                sender_fp: identity.node_fp,
                digest: current_digest,
                offset,
                entries_json,
                next_offset,
                unchanged,
            })
            .map_err(|_| io::Error::other("registry response encode failed"))
        }
        Request::ClusterRegistryUpdate { update_json } => {
            let peer_fp = cluster::encoding::fingerprint(peer_static);
            if !limiter.acquire_registry_request(&peer_fp, true) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registry replication rate limit exceeded",
                ));
            }
            if update_json.len() > MAX_REGISTRY_UPDATE_JSON_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registry update exceeds page byte cap",
                ));
            }
            let update = cluster::registry::RegistryUpdate::decode(update_json.as_bytes())?;
            let outcome = cluster::registry::apply_registry_update(&update, peer_static)?;
            let (_, registry) = cluster::nodes()?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "cluster is not initialized")
            })?;
            serde_json::to_vec(&Response::ClusterRegistryAck {
                digest: registry.digest()?,
                applied: !outcome.applied.is_empty(),
            })
            .map_err(|_| io::Error::other("registry acknowledgement encode failed"))
        }
        request => {
            if let Err(response) =
                authorize_remote_request_with_source(&request, &ControlSource::DefaultStateDir)
            {
                return serde_json::to_vec(&response).map_err(io::Error::other);
            }
            crate::remote_front::forward_frame_with_timeout(
                daemon_path,
                payload,
                LOCAL_DAEMON_REQUEST_TIMEOUT.min(MAX_HELD_REQUEST),
            )
            .map_err(|reason| io::Error::other(format!("local request failed: {reason}")))
        }
    }
}

#[cfg(test)]
fn dispatch_payload_with_control_source(
    payload: &[u8],
    daemon_path: &Path,
    control_source: &ControlSource,
) -> io::Result<Vec<u8>> {
    let request = crate::remote_front::decode_frame(payload)
        .map_err(|reason| io::Error::new(io::ErrorKind::InvalidData, reason))?;
    if let Err(response) = authorize_remote_request_with_source(&request, control_source) {
        return serde_json::to_vec(&response).map_err(io::Error::other);
    }
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
    policy: ConnectionPolicy,
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
            handle_connection_with(
                stream,
                remote_addr,
                state,
                global_permit,
                authorize,
                dispatch,
                policy,
            );
        })
        .map(|_| ())
}

fn handle_connection_with(
    stream: TcpStream,
    remote_addr: SocketAddr,
    state: Arc<ListenerState>,
    global_permit: Arc<GlobalPermit>,
    authorize: MemberAuthorizer,
    dispatch: FrameDispatcher,
    policy: ConnectionPolicy,
) {
    let ConnectionPolicy {
        control_source,
        limits,
    } = policy;
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
    let admitted = authorize(&opened.peer_static).is_ok();
    let now = crate::SystemWallClock::new().unix_seconds() as i64;
    let replay = state
        .replay
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .check_and_insert(&peer_fp, opened.ephemeral, opened.timestamp_seconds, now);
    if replay.is_err() {
        return ignore_response_error(write_http_response(stream, 409, b"replayed or stale frame"));
    }
    if !admitted {
        return handle_unknown_peer_join(stream, remote_addr, state, opened);
    }
    // The cluster listener has two protocol verbs which the local
    // remote-control front must refuse. Decode and authorize them here,
    // after the member check above, instead of using that front's combined
    // decode-and-authorize boundary.
    let request = match decode_cluster_request(&opened.payload) {
        Ok(request) => request,
        Err(reason) => {
            let response = encode_error(&reason);
            return send_encrypted_response(stream, opened, &response);
        }
    };
    if let Err(response) = authorize_remote_request_with_source(&request, &control_source) {
        let response = serde_json::to_vec(&response).unwrap_or_else(|_| b"null".to_vec());
        return send_encrypted_response(stream, opened, &response);
    }
    if let Request::Input { bytes, .. } = &request {
        if !state.limiter.allow_remote_input(&peer_fp, bytes.len()) {
            let response =
                serde_json::to_vec(&Response::RateLimited).unwrap_or_else(|_| b"null".to_vec());
            return send_encrypted_response(stream, opened, &response);
        }
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
                || dispatch_worker(&payload, &peer_static),
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

fn handle_unknown_peer_join(
    stream: TcpStream,
    remote_addr: SocketAddr,
    state: Arc<ListenerState>,
    opened: frame::OpenedRequest,
) {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct JoinRequest {
        join: JoinToken,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct JoinToken {
        token: String,
        #[serde(default)]
        endpoint: Option<String>,
    }
    let request: JoinRequest = match serde_json::from_slice(&opened.payload) {
        Ok(request) => request,
        Err(_) => return ignore_response_error(write_http_response(stream, 403, b"not admitted")),
    };
    if !state
        .limiter
        .allow_join_attempt(remote_addr.ip(), Instant::now())
    {
        return ignore_response_error(write_http_response(
            stream,
            429,
            b"join attempt rate limit reached",
        ));
    }
    let result = state
        .join_tokens
        .verify_consume_with(&request.join.token, || {
            (state.admit_join)(&opened.peer_static, request.join.endpoint.as_deref())
        });
    let response = if result.is_ok() {
        b"{\"joined\":true}".as_slice()
    } else {
        b"{\"joined\":false}".as_slice()
    };
    send_encrypted_response(stream, opened, response);
    if result.is_ok() {
        // Do not make the one-shot join reply wait on existing peers. Queue
        // replication after the joiner has received its admission result.
        cluster::registry_changed();
    }
}

fn encode_error(reason: &str) -> Vec<u8> {
    serde_json::to_vec(&Response::error(reason)).unwrap_or_else(|_| b"null".to_vec())
}

fn decode_cluster_request(payload: &[u8]) -> Result<Request, String> {
    if payload.len() > crate::remote_front::MAX_FRAME_BYTES {
        return Err(format!(
            "remote frame exceeds {} bytes",
            crate::remote_front::MAX_FRAME_BYTES
        ));
    }
    serde_json::from_slice(payload).map_err(|_| "invalid remote request".to_owned())
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
    struct TestControlSettings(PathBuf);

    #[cfg(unix)]
    impl TestControlSettings {
        fn new(contents: &[u8]) -> Self {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let path = std::env::temp_dir().join(format!(
                "remuda-listener-settings-{}-{}",
                std::process::id(),
                LISTENER_ERROR_COUNT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut settings = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path.join("settings.json"))
                .unwrap();
            settings.write_all(contents).unwrap();
            Self(path)
        }

        fn source(&self) -> ControlSource {
            ControlSource::from_state_dir(&self.0)
        }
    }

    #[cfg(unix)]
    impl Drop for TestControlSettings {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    struct SocketTestServer {
        address: SocketAddr,
        responder_public: Vec<u8>,
        responder_private: Vec<u8>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<io::Result<()>>>,
        state_dir: PathBuf,
        state: Arc<ListenerState>,
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
            Self::start_with_admitter_and_control_source(
                responder_private,
                responder_public,
                authorize,
                dispatch,
                limits,
                ControlSource::DefaultStateDir,
                Arc::new(|_, _| {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "admission refused",
                    ))
                }),
            )
        }

        fn start_with_admitter(
            responder_private: Vec<u8>,
            responder_public: Vec<u8>,
            authorize: MemberAuthorizer,
            dispatch: FrameDispatcher,
            limits: ConnectionLimits,
            admit_join: JoinAdmitter,
        ) -> Self {
            Self::start_with_admitter_and_control_source(
                responder_private,
                responder_public,
                authorize,
                dispatch,
                limits,
                ControlSource::DefaultStateDir,
                admit_join,
            )
        }

        fn start_with_admitter_and_control_source(
            responder_private: Vec<u8>,
            responder_public: Vec<u8>,
            authorize: MemberAuthorizer,
            dispatch: FrameDispatcher,
            limits: ConnectionLimits,
            control_source: ControlSource,
            admit_join: JoinAdmitter,
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
                admit_join,
            });
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = stop.clone();
            let thread_state = state.clone();
            let thread = std::thread::spawn(move || {
                serve_socket_until(
                    &listener,
                    &thread_stop,
                    || Ok(()),
                    move |stream, remote_addr| {
                        spawn_connection_handler(
                            stream,
                            remote_addr,
                            thread_state.clone(),
                            authorize.clone(),
                            dispatch.clone(),
                            ConnectionPolicy {
                                control_source: control_source.clone(),
                                limits,
                            },
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
                state,
            }
        }

        fn start_production(
            responder_private: Vec<u8>,
            responder_public: Vec<u8>,
            authorize: MemberAuthorizer,
            dispatch: FrameDispatcher,
        ) -> Self {
            Self::start_production_with_control_source(
                responder_private,
                responder_public,
                authorize,
                dispatch,
                ControlSource::DefaultStateDir,
            )
        }

        fn start_production_with_control_source(
            responder_private: Vec<u8>,
            responder_public: Vec<u8>,
            authorize: MemberAuthorizer,
            dispatch: FrameDispatcher,
            control_source: ControlSource,
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
                admit_join: Arc::new(cluster::admit_join_locked),
            });
            let test_state = state.clone();
            let listener = Listener {
                socket: listener,
                state: state.clone(),
                authorize,
                dispatch,
                control_source,
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
                state: test_state,
            }
        }

        fn exhaust_remote_input_budget(&self, peer_public: &[u8]) {
            let peer_fp = cluster::encoding::fingerprint(peer_public);
            assert!(self
                .state
                .limiter
                .allow_remote_input(&peer_fp, REMOTE_INPUT_BYTES_PER_SECOND));
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
        dispatch: impl Fn(&[u8], &[u8]) -> io::Result<Vec<u8>> + Send + Sync + 'static,
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
                delivered_by: None,
                format_major: 1,
                format_minor: 0,
                optional_fields: std::collections::BTreeMap::new(),
                endpoint: None,
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
            Arc::new(|_, _| Ok(serde_json::to_vec(&Response::Ok).unwrap())),
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

    #[cfg(unix)]
    #[test]
    fn ninth_concurrent_remote_sync_is_refused_by_listener_dispatch() {
        use interprocess::local_socket::traits::ListenerExt as _;

        let _capacity_lock = crate::remote_front::SYNC_CAPACITY_TEST_LOCK.lock().unwrap();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base =
            PathBuf::from("/tmp").join(format!("rsync-capacity-{}-{stamp}", std::process::id()));
        let daemon_path = crate::daemon::socket_path_in(&base, "fake");
        let daemon_listener = crate::ipc::listen(&daemon_path).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let fake_daemon = std::thread::spawn(move || {
            let mut held = Vec::new();
            for _ in 0..crate::remote_front::MAX_REMOTE_SYNCS {
                let stream = daemon_listener.incoming().next().unwrap().unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let mut request = Vec::new();
                reader.read_until(b'\n', &mut request).unwrap();
                assert!(matches!(
                    serde_json::from_slice::<Request>(&request).unwrap(),
                    Request::Sync { .. }
                ));
                held.push(reader);
            }
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            let mut response = serde_json::to_vec(&Response::Ok).unwrap();
            response.push(b'\n');
            for mut reader in held {
                reader.get_mut().write_all(&response).unwrap();
            }
        });
        let payload = serde_json::to_vec(&Request::Sync {
            name: "dev".into(),
            instance_id: None,
            since: 0,
            timeout_ms: 20_000,
        })
        .unwrap();
        let mut workers = Vec::new();
        for _ in 0..crate::remote_front::MAX_REMOTE_SYNCS {
            let path = daemon_path.clone();
            let payload = payload.clone();
            workers.push(std::thread::spawn(move || {
                dispatch_payload_with_control_source(
                    &payload,
                    &path,
                    &ControlSource::DefaultStateDir,
                )
            }));
        }
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let ninth = dispatch_payload_with_control_source(
            &payload,
            &daemon_path,
            &ControlSource::DefaultStateDir,
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<Response>(&ninth).unwrap(),
            Response::SyncAtCapacity
        );
        release_tx.send(()).unwrap();
        for worker in workers {
            assert_eq!(
                serde_json::from_slice::<Response>(&worker.join().unwrap().unwrap()).unwrap(),
                Response::Ok
            );
        }
        fake_daemon.join().unwrap();
        drop(daemon_path);
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    fn production_socket_close_response(
        setting: &[u8],
        request: Request,
        dispatch_response: Response,
        expected: Response,
        expected_dispatches: usize,
    ) {
        let settings = TestControlSettings::new(setting);
        let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let peer = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let dispatched = Arc::new(AtomicUsize::new(0));
        let seen = dispatched.clone();
        let payload = serde_json::to_vec(&dispatch_response).unwrap();
        let server = SocketTestServer::start_production_with_control_source(
            responder.private,
            responder.public,
            Arc::new(|_| Ok(())),
            Arc::new(move |_, _| {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(payload.clone())
            }),
            settings.source(),
        );
        let sealed = sealed_payload_request(&peer, &server, &serde_json::to_vec(&request).unwrap());
        let (status, response) = server.exchange(sealed);
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_slice::<Response>(&response).unwrap(),
            expected
        );
        assert_eq!(dispatched.load(Ordering::SeqCst), expected_dispatches);
    }

    #[cfg(unix)]
    #[test]
    fn remote_close_with_control_off_returns_typed_refusal() {
        production_socket_close_response(
            br#"{"allow_remote_control":false}"#,
            Request::Close {
                name: "session".into(),
                instance_id: Some("id".into()),
                confirm: Some(true),
            },
            Response::Ok,
            Response::RemoteControlDisabled,
            0,
        );
    }

    #[cfg(unix)]
    #[test]
    fn remote_confirmed_close_with_control_on_reaches_dispatch() {
        production_socket_close_response(
            br#"{"allow_remote_control":true}"#,
            Request::Close {
                name: "session".into(),
                instance_id: Some("id".into()),
                confirm: Some(true),
            },
            Response::Ok,
            Response::Ok,
            1,
        );
    }

    #[cfg(unix)]
    #[test]
    fn remote_stale_confirmed_close_reaches_daemon_and_is_refused() {
        production_socket_close_response(
            br#"{"allow_remote_control":true}"#,
            Request::Close {
                name: "session".into(),
                instance_id: Some("stale-instance".into()),
                confirm: Some(true),
            },
            Response::error("session restarted; close was refused"),
            Response::error("session restarted; close was refused"),
            1,
        );
    }

    #[cfg(unix)]
    #[test]
    fn remote_close_without_confirmation_is_refused_before_dispatch() {
        production_socket_close_response(
            br#"{"allow_remote_control":true}"#,
            Request::Close {
                name: "session".into(),
                instance_id: Some("id".into()),
                confirm: None,
            },
            Response::Ok,
            Response::error("remote front refuses Close"),
            0,
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_remote_close_without_identity_or_confirmation_is_refused() {
        production_socket_close_response(
            br#"{"allow_remote_control":true}"#,
            Request::Close {
                name: "session".into(),
                instance_id: None,
                confirm: None,
            },
            Response::Ok,
            Response::error("remote front refuses Close"),
            0,
        );
    }

    #[cfg(unix)]
    #[test]
    fn socket_remote_input_dispatches_the_idempotent_batch() {
        let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let peer = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let request = Request::Input {
            name: "session".into(),
            instance_id: "instance".into(),
            client_id: "00000000000000000000000000000001".into(),
            seq: 1,
            bytes: vec![u8::MAX; crate::remote_front::MAX_REMOTE_INPUT_BATCH_BYTES],
        };
        let expected = request.clone();
        let dispatched = Arc::new(AtomicUsize::new(0));
        let seen = dispatched.clone();
        let settings = TestControlSettings::new(br#"{"allow_remote_control":true}"#);
        let server = SocketTestServer::start_production_with_control_source(
            responder.private,
            responder.public,
            Arc::new(|_| Ok(())),
            Arc::new(move |payload, _| {
                assert_eq!(
                    crate::remote_front::decode_frame(payload).unwrap(),
                    expected
                );
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::to_vec(&Response::Ack { duplicate: false }).unwrap())
            }),
            settings.source(),
        );
        let sealed = sealed_payload_request(&peer, &server, &serde_json::to_vec(&request).unwrap());
        let (status, response) = server.exchange(sealed);
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_slice::<Response>(&response).unwrap(),
            Response::Ack { duplicate: false }
        );
        assert_eq!(dispatched.load(Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[test]
    fn socket_refuses_input_over_remote_batch_limit_with_typed_error() {
        let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let peer = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let dispatched = Arc::new(AtomicUsize::new(0));
        let seen = dispatched.clone();
        let settings = TestControlSettings::new(br#"{"allow_remote_control":true}"#);
        let server = SocketTestServer::start_production_with_control_source(
            responder.private,
            responder.public,
            Arc::new(|_| Ok(())),
            Arc::new(move |_, _| {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::to_vec(&Response::Ack { duplicate: false }).unwrap())
            }),
            settings.source(),
        );
        let request = Request::Input {
            name: "session".into(),
            instance_id: "instance".into(),
            client_id: "00000000000000000000000000000001".into(),
            seq: 1,
            bytes: vec![u8::MAX; crate::remote_front::MAX_REMOTE_INPUT_BATCH_BYTES + 1],
        };
        let sealed = sealed_payload_request(&peer, &server, &serde_json::to_vec(&request).unwrap());
        let (status, response) = server.exchange(sealed);
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_slice::<Response>(&response).unwrap(),
            Response::Error(format!(
                "remote Input batch exceeds {} bytes",
                crate::remote_front::MAX_REMOTE_INPUT_BATCH_BYTES
            ))
        );
        assert_eq!(dispatched.load(Ordering::SeqCst), 0);
    }

    #[cfg(unix)]
    #[test]
    fn socket_refuses_rate_limited_input_with_typed_error() {
        let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let peer = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let dispatched = Arc::new(AtomicUsize::new(0));
        let seen = dispatched.clone();
        let settings = TestControlSettings::new(br#"{"allow_remote_control":true}"#);
        let server = SocketTestServer::start_production_with_control_source(
            responder.private,
            responder.public,
            Arc::new(|_| Ok(())),
            Arc::new(move |_, _| {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::to_vec(&Response::Ack { duplicate: false }).unwrap())
            }),
            settings.source(),
        );
        server.exhaust_remote_input_budget(&peer.public);
        let request = Request::Input {
            name: "session".into(),
            instance_id: "instance".into(),
            client_id: "00000000000000000000000000000001".into(),
            seq: 1,
            bytes: b"x".to_vec(),
        };
        let sealed = sealed_payload_request(&peer, &server, &serde_json::to_vec(&request).unwrap());
        let (status, response) = server.exchange(sealed);
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_slice::<Response>(&response).unwrap(),
            Response::RateLimited
        );
        assert_eq!(dispatched.load(Ordering::SeqCst), 0);
    }

    #[cfg(unix)]
    #[test]
    fn socket_setting_off_returns_disabled_without_dispatch() {
        let settings = TestControlSettings::new(br#"{"allow_remote_control":false}"#);
        let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let peer = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let dispatched = Arc::new(AtomicUsize::new(0));
        let seen = dispatched.clone();
        let server = SocketTestServer::start_production_with_control_source(
            responder.private,
            responder.public,
            Arc::new(|_| Ok(())),
            Arc::new(move |_, _| {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::to_vec(&Response::Ok).unwrap())
            }),
            settings.source(),
        );
        let request = Request::Input {
            name: "session".into(),
            instance_id: "id".into(),
            client_id: "00000000000000000000000000000001".into(),
            seq: 1,
            bytes: b"x".to_vec(),
        };
        let sealed = sealed_payload_request(&peer, &server, &serde_json::to_vec(&request).unwrap());
        let (status, response) = server.exchange(sealed);
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_slice::<Response>(&response).unwrap(),
            Response::RemoteControlDisabled
        );
        assert_eq!(dispatched.load(Ordering::SeqCst), 0);
    }

    #[cfg(unix)]
    #[test]
    fn socket_malformed_setting_errors_without_dispatch() {
        let settings = TestControlSettings::new(br#"{"allow_remote_control":no}"#);
        let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let peer = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let dispatched = Arc::new(AtomicUsize::new(0));
        let seen = dispatched.clone();
        let server = SocketTestServer::start_production_with_control_source(
            responder.private,
            responder.public,
            Arc::new(|_| Ok(())),
            Arc::new(move |_, _| {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::to_vec(&Response::Ok).unwrap())
            }),
            settings.source(),
        );
        let request = Request::Input {
            name: "session".into(),
            instance_id: "id".into(),
            client_id: "00000000000000000000000000000001".into(),
            seq: 1,
            bytes: b"x".to_vec(),
        };
        let sealed = sealed_payload_request(&peer, &server, &serde_json::to_vec(&request).unwrap());
        let (status, response) = server.exchange(sealed);
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_slice::<Response>(&response).unwrap(),
            Response::Error("remote control setting unavailable; refusing Input".into())
        );
        assert_eq!(dispatched.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn unspecified_bind_requires_explicit_public_opt_in() {
        assert!(validate_bind_address("127.0.0.1:0".parse().unwrap(), false).is_ok());
        assert!(validate_bind_address("0.0.0.0:0".parse().unwrap(), false).is_err());
        assert!(validate_bind_address("[::]:0".parse().unwrap(), false).is_err());
        assert!(validate_bind_address("0.0.0.0:0".parse().unwrap(), true).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn remote_input_obeys_local_control_setting_and_close_stays_refused() {
        let settings = TestControlSettings::new(br#"{"allow_remote_control":true}"#);
        let source = settings.source();
        assert!(authorize_remote_request_with_source(&Request::List, &source).is_ok());
        let input = Request::Input {
            name: "session".into(),
            instance_id: "instance".into(),
            client_id: "00000000000000000000000000000001".into(),
            seq: 1,
            bytes: b"hello\r".to_vec(),
        };
        assert!(authorize_remote_request_with_source(&input, &source).is_ok());
        assert_eq!(
            authorize_remote_request_with_control(&input, false),
            Err(Response::RemoteControlDisabled)
        );
        let close = Request::Close {
            name: "session".into(),
            instance_id: None,
            confirm: None,
        };
        assert_eq!(
            authorize_remote_request_with_source(&close, &source),
            Err(Response::error("remote front refuses Close"))
        );
        assert_eq!(authorize_remote_request_with_control(&input, true), Ok(()));
        let wire = serde_json::to_vec(&Response::RemoteControlDisabled).unwrap();
        assert_eq!(
            serde_json::from_slice::<Response>(&wire).unwrap(),
            Response::RemoteControlDisabled
        );
    }

    #[test]
    fn cluster_registry_verbs_are_admitted_while_input_remains_control_gated() {
        let sync = Request::ClusterRegistrySync {
            digest: None,
            offset: 0,
        };
        let update = Request::ClusterRegistryUpdate {
            update_json: "{}".into(),
        };
        let input = Request::Input {
            name: "session".into(),
            instance_id: "instance".into(),
            client_id: "00000000000000000000000000000001".into(),
            seq: 1,
            bytes: b"hello\r".to_vec(),
        };

        assert_eq!(authorize_remote_request_with_control(&sync, false), Ok(()));
        assert_eq!(
            authorize_remote_request_with_control(&update, false),
            Ok(())
        );
        assert_eq!(
            authorize_remote_request_with_control(&input, false),
            Err(Response::RemoteControlDisabled)
        );
    }

    #[test]
    fn remote_input_rate_limit_aggregates_bytes_by_peer_and_resets_after_a_second() {
        let mut limiter = RemoteInputRateLimiter::default();
        let now = Instant::now();
        assert!(limiter.check("peer-a", REMOTE_INPUT_BYTES_PER_SECOND, now));
        assert!(!limiter.check("peer-a", 1, now));
        assert!(limiter.check("peer-b", 1, now));
        assert!(limiter.check(
            "peer-a",
            1,
            now + REMOTE_INPUT_WINDOW + Duration::from_millis(1)
        ));
    }

    #[test]
    fn remote_input_rate_limit_accepts_exact_budget_deterministically() {
        let mut limiter = RemoteInputRateLimiter::default();
        let now = Instant::now();
        for _ in 0..4 {
            assert!(limiter.check("peer", 64 * 1024, now));
        }
        assert!(!limiter.check("peer", 1, now));
    }

    #[test]
    fn remote_input_rate_limiter_bounds_peer_state_and_expires_old_windows() {
        let mut limiter = RemoteInputRateLimiter::default();
        let now = Instant::now();
        for index in 0..MAX_REMOTE_INPUT_PEERS {
            assert!(limiter.check(&format!("peer-{index}"), 1, now));
        }
        assert!(!limiter.check("peer-over-cap", 1, now));
        assert!(limiter.check(
            "peer-over-cap",
            1,
            now + REMOTE_INPUT_WINDOW + Duration::from_millis(1)
        ));
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
    fn production_serve_until_member_request_evicts_idle_preauth_connection() {
        let (server, peer) = production_socket_server();
        let mut idle = TcpStream::connect(server.address).unwrap();
        idle.set_read_timeout(Some(socket_test_timeout())).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let member = sealed_list_request(&peer, &server);
        assert_eq!(server.exchange(member).0, 200);

        let mut byte = [0; 1];
        assert_eq!(idle.read(&mut byte).unwrap(), 0);
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
                delivered_by: None,
                format_major: 1,
                format_minor: 0,
                optional_fields: std::collections::BTreeMap::new(),
                endpoint: None,
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

    #[test]
    fn request_limiter_caps_join_attempts_per_ip_for_a_monotonic_minute() {
        let limiter = RequestLimiter::default();
        let address = "192.0.2.44".parse().unwrap();
        let start = Instant::now();
        for attempt in 0..MAX_JOIN_ATTEMPTS_PER_IP {
            assert!(
                limiter.allow_join_attempt(address, start + Duration::from_secs(attempt as u64))
            );
        }
        assert!(!limiter.allow_join_attempt(address, start + Duration::from_secs(20)));
        assert!(limiter.allow_join_attempt(
            address,
            start + JOIN_ATTEMPT_WINDOW + Duration::from_secs(1)
        ));
    }

    #[test]
    fn registry_sync_leaves_reserved_capacity_for_revoke_push() {
        let limiter = RequestLimiter::default();
        for _ in 0..9 {
            assert!(limiter.acquire_registry_request("peer-a", false));
        }
        // Anti-entropy keeps polling and must not consume the reserved token.
        assert!(!limiter.acquire_registry_request("peer-a", false));
        assert!(limiter.acquire_registry_request("peer-a", true));
        assert!(!limiter.acquire_registry_request("peer-a", true));
        // Updates still share the normal per-peer cap when no sync traffic
        // competes with them.
        for _ in 0..10 {
            assert!(limiter.acquire_registry_request("peer-b", true));
        }
        assert!(!limiter.acquire_registry_request("peer-b", true));
    }

    #[cfg(unix)]
    #[test]
    fn socket_accepts_a_fresh_ik_request_and_refuses_its_replay() {
        let (server, peer, _) = socket_server(
            |payload, peer| {
                dispatch_payload(
                    payload,
                    peer,
                    Path::new("unused-daemon-path"),
                    &RequestLimiter::default(),
                )
            },
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
    fn socket_admits_unknown_join_once_and_passes_optional_endpoint() {
        let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let peer = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let admissions = Arc::new(Mutex::new(Vec::new()));
        let recorded = admissions.clone();
        let server = SocketTestServer::start_with_admitter(
            responder.private,
            responder.public,
            Arc::new(|_| Err(io::Error::new(io::ErrorKind::PermissionDenied, "unknown"))),
            Arc::new(|_, _| panic!("Join must not reach daemon dispatch")),
            ConnectionLimits {
                outer_hold: socket_test_timeout(),
                post_dispatch_hold: socket_test_timeout(),
                idle_read: socket_test_timeout(),
                total_read: socket_test_timeout(),
            },
            Arc::new(move |key, endpoint| {
                recorded.lock().unwrap().push((
                    cluster::encoding::fingerprint(key),
                    endpoint.map(str::to_owned),
                ));
                Ok(())
            }),
        );
        let token_store =
            JoinTokenStore::open_at(&server.state_dir, Arc::new(crate::SystemWallClock::new()))
                .unwrap();
        let minted = token_store.mint().unwrap();
        let make_payload = || {
            serde_json::to_vec(&serde_json::json!({
                "join": {
                    "token": minted.token.as_str(),
                    "endpoint": "198.51.100.20:9443"
                }
            }))
            .unwrap()
        };
        let first = sealed_payload_request(&peer, &server, &make_payload());
        let (status, response) = server.exchange(first);
        assert_eq!(status, 200);
        assert_eq!(response, b"{\"joined\":true}");
        let reused = sealed_payload_request(&peer, &server, &make_payload());
        let (status, response) = server.exchange(reused);
        assert_eq!(status, 200);
        assert_eq!(response, b"{\"joined\":false}");
        assert_eq!(
            admissions.lock().unwrap().as_slice(),
            &[(
                cluster::encoding::fingerprint(&peer.public),
                Some("198.51.100.20:9443".into())
            )]
        );
    }

    #[cfg(unix)]
    #[test]
    fn socket_rejects_low_order_unknown_and_mismatched_static_keys_before_dispatch() {
        let dispatched = Arc::new(AtomicUsize::new(0));
        let dispatched_for_worker = dispatched.clone();
        let (server, peer, registry) = socket_server(
            move |_, _| {
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
        let (server, peer, _) = socket_server(
            |payload, peer| {
                dispatch_payload(
                    payload,
                    peer,
                    Path::new("unused-daemon-path"),
                    &RequestLimiter::default(),
                )
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
    }

    #[cfg(unix)]
    #[test]
    fn bad_join_tokens_are_rate_limited_without_writing_token_state() {
        use std::os::unix::fs::MetadataExt;

        let responder = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let peer = snow::Builder::new("Noise_IK_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        let server = SocketTestServer::start_production(
            responder.private,
            responder.public,
            Arc::new(|_| Err(permission_denied())),
            Arc::new(|_, _| Ok(serde_json::to_vec(&Response::Ok).unwrap())),
        );
        let token_path = server.state_dir.join("join_tokens.json");
        let deadline = Instant::now() + Duration::from_secs(1);
        while !token_path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let initial_bytes =
            std::fs::read(&token_path).expect("listener startup observes token state");
        let initial_inode = std::fs::metadata(&token_path).unwrap().ino();
        let request = serde_json::json!({
            "join": { "token": crate::cluster::encoding::encode_base64(&[9; 32]) }
        });
        let payload = serde_json::to_vec(&request).unwrap();
        for _ in 0..MAX_JOIN_ATTEMPTS_PER_IP {
            let sealed = sealed_payload_request(&peer, &server, &payload);
            let (status, response) = server.exchange(sealed);
            assert_eq!(status, 200);
            assert_eq!(response, b"{\"joined\":false}");
        }
        let too_many = sealed_payload_request(&peer, &server, &payload);
        assert_eq!(server.exchange(too_many).0, 429);
        assert_eq!(std::fs::read(&token_path).unwrap(), initial_bytes);
        assert_eq!(
            std::fs::metadata(&token_path).unwrap().ino(),
            initial_inode,
            "bad tokens must not replace token state"
        );
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
        let settings = TestControlSettings::new(br#"{"allow_remote_control":false}"#);
        let dispatched = Arc::new(AtomicUsize::new(0));
        let seen = dispatched.clone();
        let server = SocketTestServer::start_production_with_control_source(
            responder.private,
            responder.public,
            Arc::new(|_| Ok(())),
            Arc::new(move |_, _| {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::to_vec(&Response::Ack { duplicate: false }).unwrap())
            }),
            settings.source(),
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
        assert_eq!(response, Response::RemoteControlDisabled);
        assert_eq!(dispatched.load(Ordering::SeqCst), 0);
    }

    #[cfg(unix)]
    #[test]
    fn socket_rechecks_membership_before_releasing_a_held_response() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let worker_release_rx = release_rx.clone();
        let (server, peer, registry) = socket_server(
            move |_, _| {
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
            |_, _| {
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
            |_, _| {
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
            |_, _| Ok(serde_json::to_vec(&Response::Ok).unwrap()),
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
            |_, _| Ok(serde_json::to_vec(&Response::Ok).unwrap()),
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
            |_, _| Ok(serde_json::to_vec(&Response::Ok).unwrap()),
            socket_test_timeout(),
            socket_test_timeout(),
            Duration::from_secs(10),
            Duration::from_secs(1),
        );
        let started = Instant::now();
        let mut stream = TcpStream::connect(server.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(12)))
            .unwrap();
        let request = b"POST /cluster HTTP/1.1\r\nHost: test\r\nContent-Length: 4\r\n\r\ntest";
        for byte in request {
            if stream.write_all(&[*byte]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        let mut tail = Vec::new();
        let _ = stream.read_to_end(&mut tail);
        assert!(started.elapsed() < Duration::from_secs(5));
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
            delivered_by: None,
            format_major: 1,
            format_minor: 0,
            optional_fields: std::collections::BTreeMap::new(),
            endpoint: None,
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
                delivered_by: None,
                format_major: 1,
                format_minor: 0,
                optional_fields: std::collections::BTreeMap::new(),
                endpoint: None,
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
