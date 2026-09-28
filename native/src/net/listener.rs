//! Bounded, VPN-address-only server for one-shot Noise IK requests.
#![allow(clippy::disallowed_types)]

use super::{frame, replay};
use crate::cluster::{self, identity, join_token::JoinTokenStore, NodeState};
use remuda_core::protocol::{Request, Response};
use remuda_core::WallClock;
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
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
pub const MAX_HELD_REQUEST: Duration = Duration::from_secs(30);
const SOCKET_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_READ_TIME: Duration = Duration::from_secs(5);
const OBSERVE_INTERVAL: Duration = Duration::from_secs(5);
const ACCEPT_POLL: Duration = Duration::from_millis(20);
const REPLAY_CAPACITY: usize = 65_536;

/// Explicit listener address and opt-in for wildcard binding.
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
}

struct ListenerState {
    responder_private: Zeroizing<Vec<u8>>,
    daemon_path: PathBuf,
    replay: Mutex<replay::ReplayWindow>,
    limiter: Arc<RequestLimiter>,
    join_tokens: JoinTokenStore,
}

#[derive(Default)]
struct RequestLimiter {
    active_global: AtomicUsize,
    active_peers: Mutex<HashMap<String, usize>>,
}

struct GlobalPermit(Arc<RequestLimiter>);

struct PeerPermit {
    limiter: Arc<RequestLimiter>,
    fingerprint: String,
}

struct InboundRequest {
    body: Vec<u8>,
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
    let join_tokens = JoinTokenStore::open(Arc::new(crate::SystemWallClock::new()))?;
    let socket = TcpListener::bind(config.bind_addr)?;
    socket.set_nonblocking(true)?;
    Ok(Listener {
        socket,
        state: Arc::new(ListenerState {
            responder_private,
            daemon_path: daemon_path.to_path_buf(),
            replay: Mutex::new(replay::ReplayWindow::new(REPLAY_CAPACITY)),
            limiter: Arc::new(RequestLimiter::default()),
            join_tokens,
        }),
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
        self.state.join_tokens.observe()?;
        let mut last_observe = Instant::now();
        while !stop.load(Ordering::Acquire) {
            if last_observe.elapsed() >= OBSERVE_INTERVAL {
                self.state.join_tokens.observe()?;
                last_observe = Instant::now();
            }
            match self.socket.accept() {
                Ok((stream, remote_addr)) => self.accept(stream, remote_addr)?,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub fn serve(&self) -> io::Result<()> {
        self.serve_until(&AtomicBool::new(false))
    }

    fn accept(&self, stream: TcpStream, _remote_addr: SocketAddr) -> io::Result<()> {
        let Some(global_permit) = self.state.limiter.acquire_global() else {
            return write_http_response(stream, 503, b"request capacity reached");
        };
        let state = self.state.clone();
        std::thread::Builder::new()
            .name("remuda-cluster-listener".into())
            .spawn(move || handle_connection(stream, state, global_permit))?;
        Ok(())
    }
}

impl RequestLimiter {
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

fn parse_socket_request(stream: &TcpStream) -> io::Result<InboundRequest> {
    let wake_stream = stream.try_clone()?;
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let timer = std::thread::Builder::new()
        .name("remuda-cluster-read-deadline".into())
        .spawn(move || {
            if done_rx.recv_timeout(MAX_REQUEST_READ_TIME).is_err() {
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
    crate::remote_front::authorize(request)
        .map_err(|reason| io::Error::new(io::ErrorKind::PermissionDenied, reason))?;
    if matches!(request, Request::Input { .. }) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "remote input is disabled until PTY writes have a timeout",
        ));
    }
    Ok(())
}

fn authorize_member(peer_static: &[u8]) -> io::Result<()> {
    let Some((_, registry)) = cluster::nodes()? else {
        return Err(permission_denied());
    };
    authorize_key_in_registry(&registry, peer_static)
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
    dispatch: D,
) -> io::Result<Vec<u8>>
where
    A: FnMut(&[u8]) -> io::Result<()>,
    D: FnOnce() -> io::Result<Vec<u8>>,
{
    authorize(peer_static)?;
    let started = Instant::now();
    let response = dispatch()?;
    if started.elapsed() > effective_hold_timeout(MAX_HELD_REQUEST) {
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
    crate::remote_front::forward_frame(daemon_path, payload)
        .map_err(|reason| io::Error::other(format!("local request failed: {reason}")))
}

fn handle_connection(
    stream: TcpStream,
    state: Arc<ListenerState>,
    global_permit: Arc<GlobalPermit>,
) {
    let _ = stream.set_read_timeout(Some(SOCKET_IDLE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_IDLE_TIMEOUT));
    let inbound = match parse_socket_request(&stream) {
        Ok(request) => request,
        Err(_) => return ignore_response_error(write_http_response(stream, 400, b"bad request")),
    };
    let opened = match frame::open_request(&state.responder_private, &inbound.body) {
        Ok(request) => request,
        Err(_) => return ignore_response_error(write_http_response(stream, 400, b"bad frame")),
    };
    let peer_fp = cluster::encoding::fingerprint(&opened.peer_static);
    if authorize_member(&opened.peer_static).is_err() {
        return ignore_response_error(write_http_response(stream, 403, b"not admitted"));
    }
    let now = crate::SystemWallClock::new().unix_seconds() as i64;
    let replay = state
        .replay
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .check_and_insert(opened.ephemeral, opened.timestamp_seconds, now);
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
    let daemon_path = state.daemon_path.clone();
    let worker_global = global_permit.clone();
    let worker_peer = peer_permit.clone();
    let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("remuda-cluster-dispatch".into())
        .spawn(move || {
            let _global = worker_global;
            let _peer = worker_peer;
            let result = dispatch_and_reauthorize(&peer_static, authorize_member, || {
                dispatch_payload(&payload, &daemon_path)
            });
            let _ = reply_tx.send(result);
        });
    if worker.is_err() {
        let response = encode_error("unable to start request worker");
        return send_encrypted_response(stream, opened, &response);
    }
    let response = match reply_rx.recv_timeout(effective_hold_timeout(MAX_HELD_REQUEST)) {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => encode_error(&error.to_string()),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => encode_error("request timed out"),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            encode_error("request worker failed")
        }
    };
    if authorize_member(&opened.peer_static).is_err() {
        let refused = encode_error("remote node was revoked while the request was held");
        return send_encrypted_response(stream, opened, &refused);
    }
    send_encrypted_response(stream, opened, &response)
}

fn encode_error(reason: &str) -> Vec<u8> {
    serde_json::to_vec(&Response::error(reason)).unwrap_or_else(|_| b"null".to_vec())
}

fn send_encrypted_response(stream: TcpStream, opened: frame::OpenedRequest, payload: &[u8]) {
    let result = frame::seal_response(opened, payload)
        .and_then(|body| write_http_response(stream, 200, &body));
    ignore_response_error(result);
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

    #[test]
    fn unspecified_bind_requires_explicit_public_opt_in() {
        assert!(validate_bind_address("127.0.0.1:0".parse().unwrap(), false).is_ok());
        assert!(validate_bind_address("0.0.0.0:0".parse().unwrap(), false).is_err());
        assert!(validate_bind_address("[::]:0".parse().unwrap(), false).is_err());
        assert!(validate_bind_address("0.0.0.0:0".parse().unwrap(), true).is_ok());
    }

    #[test]
    fn remote_input_is_disabled_until_the_pty_write_timeout_lands() {
        assert!(authorize_remote_request(&Request::List).is_ok());
        let error = authorize_remote_request(&Request::Input {
            name: "session".into(),
            bytes: b"hello\r".to_vec(),
        })
        .unwrap_err();
        assert!(error.to_string().contains("disabled"));
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
}
