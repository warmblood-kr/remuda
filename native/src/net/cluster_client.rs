//! One-shot HTTP/1.1 + Noise IK client for an authenticated cluster peer.
#![allow(clippy::disallowed_types)]

use super::frame;
use remuda_core::clock::WallClock;
use remuda_core::protocol::{Request, Response};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_HEADER_COUNT: usize = 64;
const MAX_BODY_BYTES: usize = 65_535;
const MAX_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Failures returned by the cluster client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientError {
    /// The peer could not be reached.
    Unreachable,
    /// A connect, read, or total deadline elapsed.
    Timeout,
    /// The peer refused the request with this HTTP status.
    Refused(u16),
    /// The peer returned malformed or oversized HTTP/JSON data.
    BadResponse,
    /// Noise sealing or response authentication failed. HTTP 400 from the
    /// listener also maps here: the generated msg1 was rejected before open.
    Crypto,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable => formatter.write_str("cluster peer is unreachable"),
            Self::Timeout => formatter.write_str("cluster request timed out"),
            Self::Refused(status) => write!(formatter, "cluster peer refused request ({status})"),
            Self::BadResponse => formatter.write_str("cluster peer returned a bad response"),
            Self::Crypto => formatter.write_str("cluster Noise authentication failed"),
        }
    }
}

impl std::error::Error for ClientError {}

/// Per-operation and end-to-end timeouts. `total` is always capped at 30s.
#[derive(Clone, Copy, Debug)]
pub struct ClientTimeouts {
    pub connect: Duration,
    pub read: Duration,
    pub total: Duration,
}

impl Default for ClientTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            read: Duration::from_secs(5),
            total: MAX_TOTAL_TIMEOUT,
        }
    }
}

impl ClientTimeouts {
    fn bounded(self) -> Self {
        let total = self.total.min(MAX_TOTAL_TIMEOUT);
        Self {
            connect: self.connect.min(total),
            read: self.read.min(total),
            total,
        }
    }
}

/// Sends one request per connection. Retry and Input sequence policy belong
/// to the caller; this unit never logs request or response payloads.
pub struct ClusterClient {
    clock: Arc<dyn WallClock>,
    timeouts: ClientTimeouts,
}

impl ClusterClient {
    /// Construct a client with an injectable Unix wall clock.
    pub fn new(clock: Arc<dyn WallClock>) -> Self {
        Self::with_timeouts(clock, ClientTimeouts::default())
    }

    /// Construct a client with testable operation and total timeouts.
    pub fn with_timeouts(clock: Arc<dyn WallClock>, timeouts: ClientTimeouts) -> Self {
        Self {
            clock,
            timeouts: timeouts.bounded(),
        }
    }

    /// Construct a client using the host wall clock.
    pub fn system() -> Self {
        Self::new(Arc::new(crate::SystemWallClock::new()))
    }

    /// Seal and send one allowlisted request to a pinned cluster peer.
    pub fn request(
        &self,
        peer: SocketAddr,
        pinned_peer_static: &[u8],
        our_static_private: &[u8],
        request: &Request,
    ) -> Result<Response, ClientError> {
        let deadline = Instant::now() + self.timeouts.total;
        let payload = serde_json::to_vec(request).map_err(|_| ClientError::BadResponse)?;
        let timestamp = self.clock.unix_seconds().min(i64::MAX as u64) as i64;
        let sealed =
            frame::seal_request(our_static_private, pinned_peer_static, timestamp, &payload)
                .map_err(|_| ClientError::Crypto)?;
        let body = sealed.message.clone();
        let connect_timeout = remaining(deadline)?
            .min(self.timeouts.connect)
            .max(Duration::from_millis(1));
        let mut stream = TcpStream::connect_timeout(&peer, connect_timeout).map_err(map_io)?;
        let remaining_total = remaining(deadline)?;
        self.exchange(&mut stream, sealed, &body, remaining_total)
    }

    fn exchange(
        &self,
        stream: &mut TcpStream,
        sealed: frame::SealedRequest,
        body: &[u8],
        watchdog_after: Duration,
    ) -> Result<Response, ClientError> {
        let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let timer_stream = stream.try_clone().map_err(map_io)?;
        let timer_flag = timed_out.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let watchdog = std::thread::spawn(move || {
            if done_rx.recv_timeout(watchdog_after).is_err() {
                timer_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = timer_stream.shutdown(Shutdown::Both);
            }
        });
        let result = self.exchange_inner(stream, sealed, body);
        let _ = done_tx.send(());
        let _ = watchdog.join();
        if timed_out.load(std::sync::atomic::Ordering::SeqCst) {
            Err(ClientError::Timeout)
        } else {
            result
        }
    }

    fn exchange_inner(
        &self,
        stream: &mut TcpStream,
        sealed: frame::SealedRequest,
        body: &[u8],
    ) -> Result<Response, ClientError> {
        stream
            .set_write_timeout(Some(self.timeouts.read))
            .map_err(map_io)?;
        stream
            .set_read_timeout(Some(self.timeouts.read))
            .map_err(map_io)?;
        write!(stream, "POST /cluster HTTP/1.1\r\nHost: peer\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
            .map_err(map_io)?;
        stream.write_all(body).map_err(map_io)?;
        stream.shutdown(Shutdown::Write).map_err(map_io)?;

        let response = read_http_response(stream)?;
        if response.status == 400 {
            return Err(ClientError::Crypto);
        }
        if response.status != 200 {
            return Err(ClientError::Refused(response.status));
        }
        let plaintext =
            frame::open_response(sealed, &response.body).map_err(|_| ClientError::Crypto)?;
        serde_json::from_slice(&plaintext).map_err(|_| ClientError::BadResponse)
    }
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn read_http_response(stream: &TcpStream) -> Result<HttpResponse, ClientError> {
    let mut reader = BufReader::new(stream.try_clone().map_err(map_io)?);
    let status_line = read_line(&mut reader)?;
    let status_text = std::str::from_utf8(&status_line).map_err(|_| ClientError::BadResponse)?;
    let mut status_parts = status_text.split_ascii_whitespace();
    if status_parts.next() != Some("HTTP/1.1") {
        return Err(ClientError::BadResponse);
    }
    let status = status_parts
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or(ClientError::BadResponse)?;
    if !(100..600).contains(&status) {
        return Err(ClientError::BadResponse);
    }
    let mut header_bytes = status_line.len() + 2;
    let mut header_count = 0;
    let mut content_length = None;
    loop {
        let line = read_line(&mut reader)?;
        header_bytes = header_bytes.saturating_add(line.len() + 2);
        if header_bytes > MAX_HEADER_BYTES {
            return Err(ClientError::BadResponse);
        }
        if line.is_empty() {
            break;
        }
        header_count += 1;
        if header_count > MAX_HEADER_COUNT {
            return Err(ClientError::BadResponse);
        }
        let text = std::str::from_utf8(&line).map_err(|_| ClientError::BadResponse)?;
        let (name, value) = text.split_once(':').ok_or(ClientError::BadResponse)?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(ClientError::BadResponse);
        }
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some()
                || value.trim().is_empty()
                || !value.trim().bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(ClientError::BadResponse);
            }
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| ClientError::BadResponse)?,
            );
        }
    }
    let length = content_length.ok_or(ClientError::BadResponse)?;
    if length > MAX_BODY_BYTES {
        return Err(ClientError::BadResponse);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(map_io)?;
    Ok(HttpResponse { status, body })
}

fn read_line(reader: &mut BufReader<TcpStream>) -> Result<Vec<u8>, ClientError> {
    let mut line = Vec::new();
    let count = (&mut *reader)
        .take((MAX_REQUEST_LINE_BYTES + 1) as u64)
        .read_until(b'\n', &mut line)
        .map_err(map_io)?;
    if count == 0 || line.len() > MAX_REQUEST_LINE_BYTES || line.last() != Some(&b'\n') {
        return Err(ClientError::BadResponse);
    }
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(line)
}

fn remaining(deadline: Instant) -> Result<Duration, ClientError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(ClientError::Timeout)
}

fn map_io(error: io::Error) -> ClientError {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ClientError::Timeout,
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => ClientError::BadResponse,
        _ => ClientError::Unreachable,
    }
}
