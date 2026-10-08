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
/// Largest legal v2 body: total plaintext + per-record (2 len + 16 tag) +
/// msg2 (2 len + 32 ephemeral + 16 tag + 5 header, rounded up to 8 spare).
const V2_BODY_BOUND: usize = frame::MAX_RESPONSE_TOTAL + frame::MAX_RECORDS * 18 + 2 + 32 + 16 + 8;
const MAX_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Failures returned by the cluster client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientError {
    /// The peer could not be reached.
    Unreachable,
    /// The peer address refused a TCP connection.
    PeerNotListening,
    /// The peer closed an established TCP connection.
    PeerClosedConnection,
    /// The operating system denied opening the connection.
    OsBlockedConnection,
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
            Self::PeerNotListening => formatter.write_str("peer is not listening"),
            Self::PeerClosedConnection => formatter.write_str("peer closed the connection"),
            Self::OsBlockedConnection => formatter.write_str("the OS blocked the connection"),
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
        frame::reject_low_order_dh(our_static_private, pinned_peer_static)
            .map_err(|_| ClientError::Crypto)?;
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
        write!(stream, "POST /cluster HTTP/1.1\r\nHost: peer\r\nContent-Length: {}\r\nX-Remuda-Chunked: 1\r\nConnection: close\r\n\r\n", body.len())
            .map_err(map_io)?;
        stream.write_all(body).map_err(map_io)?;
        stream.shutdown(Shutdown::Write).map_err(map_io)?;

        let mut reader = BufReader::new(stream.try_clone().map_err(map_io)?);
        let plaintext = read_response(&mut reader, sealed)?;
        serde_json::from_slice(&plaintext).map_err(|_| ClientError::BadResponse)
    }
}

/// Read the HTTP reply and open it. Bodies up to `MAX_BODY_BYTES` are v1/v2
/// buffered; larger ones can only be v2 and are streamed through the record
/// opener, which authenticates msg2 before anything else is consumed.
fn read_response<R: Read>(
    reader: &mut BufReader<R>,
    sealed: frame::SealedRequest,
) -> Result<Vec<u8>, ClientError> {
    let (status, length) = read_http_head(reader)?;
    if length > V2_BODY_BOUND {
        return Err(ClientError::BadResponse);
    }
    if status == 400 {
        return Err(ClientError::Crypto);
    }
    if status != 200 {
        return Err(ClientError::Refused(status));
    }
    let plaintext = {
        let mut body = reader.take(length as u64);
        let opened = if length <= MAX_BODY_BYTES {
            let mut bytes = Vec::new();
            body.read_to_end(&mut bytes).map_err(map_io)?;
            frame::open_response_any(sealed, &bytes)
        } else {
            frame::open_response_chunked_from(sealed, &mut body)
        };
        // The peer ended early, or sent more than it declared: HTTP framing is broken.
        let short = body.limit() > 0;
        match opened {
            Ok(_) if short => return Err(ClientError::BadResponse),
            Ok(plaintext) => plaintext,
            Err(error) if is_transport_io(&error) => return Err(map_io(error)),
            Err(_) if short => return Err(ClientError::BadResponse),
            Err(error) => return Err(map_frame_error(&error)),
        }
    };
    let mut trailing = [0; 1];
    match reader.read(&mut trailing).map_err(map_io)? {
        0 => Ok(plaintext),
        _ => Err(ClientError::BadResponse),
    }
}

fn map_frame_error(error: &io::Error) -> ClientError {
    match error.kind() {
        io::ErrorKind::Unsupported => ClientError::BadResponse,
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => ClientError::Crypto,
        _ => map_io(io::Error::from(error.kind())),
    }
}

fn is_transport_io(error: &io::Error) -> bool {
    !matches!(
        error.kind(),
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof | io::ErrorKind::Unsupported
    )
}

fn read_http_head<R: Read>(reader: &mut BufReader<R>) -> Result<(u16, usize), ClientError> {
    let status_line = read_line(reader)?;
    let first_space = status_line
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or(ClientError::BadResponse)?;
    let rest = &status_line[first_space + 1..];
    let second_space = rest
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or(ClientError::BadResponse)?;
    let version = &status_line[..first_space];
    let code = &rest[..second_space];
    let reason = &rest[second_space + 1..];
    if version != b"HTTP/1.1"
        || code.len() != 3
        || !code.iter().all(u8::is_ascii_digit)
        || !valid_field_value(reason)
    {
        return Err(ClientError::BadResponse);
    }
    let status = std::str::from_utf8(code)
        .map_err(|_| ClientError::BadResponse)?
        .parse::<u16>()
        .map_err(|_| ClientError::BadResponse)?;
    if !(100..600).contains(&status) {
        return Err(ClientError::BadResponse);
    }
    let mut header_bytes = status_line.len() + 2;
    let mut header_count = 0;
    let mut content_length = None;
    loop {
        let line = read_line(reader)?;
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
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or(ClientError::BadResponse)?;
        let name = &line[..colon];
        let value = &line[colon + 1..];
        if !valid_field_name(name) || !valid_field_value(value) {
            return Err(ClientError::BadResponse);
        }
        if name.eq_ignore_ascii_case(b"transfer-encoding") {
            return Err(ClientError::BadResponse);
        }
        if name.eq_ignore_ascii_case(b"content-length") {
            let value = trim_ows(value);
            if content_length.is_some() || value.is_empty() || !value.iter().all(u8::is_ascii_digit)
            {
                return Err(ClientError::BadResponse);
            }
            content_length = Some(
                std::str::from_utf8(value)
                    .map_err(|_| ClientError::BadResponse)?
                    .parse::<usize>()
                    .map_err(|_| ClientError::BadResponse)?,
            );
        }
    }
    let length = content_length.ok_or(ClientError::BadResponse)?;
    Ok((status, length))
}

fn read_line<R: Read>(reader: &mut BufReader<R>) -> Result<Vec<u8>, ClientError> {
    let mut line = Vec::new();
    let count = (&mut *reader)
        .take((MAX_REQUEST_LINE_BYTES + 1) as u64)
        .read_until(b'\n', &mut line)
        .map_err(map_io)?;
    if count == 0 || line.len() > MAX_REQUEST_LINE_BYTES || !line.ends_with(b"\r\n") {
        return Err(ClientError::BadResponse);
    }
    line.truncate(line.len() - 2);
    Ok(line)
}

fn valid_field_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.iter().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn valid_field_value(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| *byte == b'\t' || (b' '..=b'~').contains(byte) || *byte >= 0x80)
}

fn trim_ows(mut value: &[u8]) -> &[u8] {
    while matches!(value.first(), Some(b' ' | b'\t')) {
        value = &value[1..];
    }
    while matches!(value.last(), Some(b' ' | b'\t')) {
        value = &value[..value.len() - 1];
    }
    value
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
        io::ErrorKind::ConnectionRefused => ClientError::PeerNotListening,
        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted => {
            ClientError::PeerClosedConnection
        }
        io::ErrorKind::PermissionDenied => ClientError::OsBlockedConnection,
        _ => ClientError::Unreachable,
    }
}

#[cfg(test)]
mod tests {
    use super::{map_io, read_http_head, read_response, ClientError, V2_BODY_BOUND};
    use crate::net::frame;
    use std::io::{BufReader, Cursor, Read};

    struct CountedReader {
        source: Cursor<Vec<u8>>,
        bytes_read: usize,
    }

    impl Read for CountedReader {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let count = self.source.read(bytes)?;
            self.bytes_read += count;
            Ok(count)
        }
    }

    const PATTERN: &str = "Noise_IK_25519_ChaChaPoly_SHA256";

    fn pair() -> (frame::SealedRequest, frame::OpenedRequest) {
        let initiator = snow::Builder::new(PATTERN.parse().unwrap())
            .generate_keypair()
            .unwrap();
        let responder = snow::Builder::new(PATTERN.parse().unwrap())
            .generate_keypair()
            .unwrap();
        let sealed =
            frame::seal_request(&initiator.private, &responder.public, 1_800_000_000, b"{}")
                .unwrap();
        let opened = frame::open_request(&responder.private, &sealed.message).unwrap();
        (sealed, opened)
    }

    fn http(body: &[u8], declared: usize) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\n\r\n").into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Decode `raw` HTTP bytes; returns the result and how many bytes were consumed.
    fn run(sealed: frame::SealedRequest, raw: Vec<u8>) -> (Result<Vec<u8>, ClientError>, usize) {
        let mut reader = BufReader::new(CountedReader {
            source: Cursor::new(raw),
            bytes_read: 0,
        });
        let result = read_response(&mut reader, sealed);
        (result, reader.get_ref().bytes_read)
    }

    fn v2(len: usize) -> (frame::SealedRequest, Vec<u8>, Vec<u8>) {
        let (sealed, opened) = pair();
        let data = payload(len);
        let body = frame::seal_response_chunked(opened, &data).unwrap();
        (sealed, data, body)
    }

    #[test]
    fn v1_reply_from_an_old_server_still_decodes() {
        let (sealed, opened) = pair();
        let body = frame::seal_response(opened, b"{\"ok\":1}").unwrap();
        let (result, _) = run(sealed, http(&body, body.len()));
        assert_eq!(result.unwrap(), b"{\"ok\":1}");
    }

    #[test]
    fn v2_replies_roundtrip_small_1mib_and_4mib() {
        for len in [10, 70_000, 1 << 20, frame::MAX_RESPONSE_TOTAL] {
            let (sealed, data, body) = v2(len);
            let (result, _) = run(sealed, http(&body, body.len()));
            assert_eq!(result.unwrap(), data, "len {len}");
        }
    }

    #[test]
    fn v2_max_body_fits_under_the_bound() {
        let (_, _, body) = v2(frame::MAX_RESPONSE_TOTAL);
        assert!(
            body.len() <= V2_BODY_BOUND,
            "{} > {V2_BODY_BOUND}",
            body.len()
        );
    }

    #[test]
    fn oversized_content_length_is_refused_without_reading_the_body() {
        let declared = V2_BODY_BOUND + 1;
        let mut raw = http(&[], declared);
        raw.extend(std::iter::repeat_n(b'x', declared));
        let total = raw.len();
        let (sealed, _) = pair();
        let (result, read) = run(sealed, raw);
        assert_eq!(result, Err(ClientError::BadResponse));
        assert!(read <= 16 * 1024 && read < total, "read {read} bytes");
    }

    #[test]
    fn big_content_length_from_an_old_server_stops_after_the_first_frame() {
        // Old server cannot send v2; garbage > 65_535 must be rejected without reading it all.
        let mut raw = http(&[], 3 << 20);
        raw.extend(std::iter::repeat_n(0x55, 3 << 20));
        let (sealed, _) = pair();
        let (result, read) = run(sealed, raw);
        assert!(result.is_err());
        assert!(read <= 70_000, "read {read} bytes");
    }

    #[test]
    fn content_length_bigger_than_actual_body_is_bad_response() {
        let (sealed, _, body) = v2(200_000);
        let (result, _) = run(sealed, http(&body, body.len() + 10));
        assert_eq!(result, Err(ClientError::BadResponse));
    }

    #[test]
    fn content_length_smaller_than_actual_body_is_rejected() {
        let (sealed, _, body) = v2(200_000);
        let (result, _) = run(sealed, http(&body, body.len() - 10));
        assert!(result.is_err());
    }

    #[test]
    fn complete_response_followed_by_bytes_outside_content_length_is_rejected() {
        let mut rejected = Vec::new();
        for version in [1, 2] {
            let (sealed, body) = if version == 1 {
                let (sealed, opened) = pair();
                (sealed, frame::seal_response(opened, b"{}").unwrap())
            } else {
                let (sealed, _, body) = v2(200_000);
                (sealed, body)
            };
            let mut raw = http(&body, body.len());
            raw.extend_from_slice(b"tail");
            let (result, _) = run(sealed, raw);
            rejected.push((version, result == Err(ClientError::BadResponse)));
        }
        assert_eq!(rejected, [(1, true), (2, true)]);
    }

    #[test]
    fn v2_idle_read_timeout_after_msg2_remains_a_timeout() {
        struct TimeoutAfter(Vec<u8>);

        impl Read for TimeoutAfter {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if self.0.is_empty() {
                    return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
                }
                let count = output.len().min(self.0.len());
                output[..count].copy_from_slice(&self.0[..count]);
                self.0.drain(..count);
                Ok(count)
            }
        }

        let (sealed, _, body) = v2(200_000);
        let msg2_len = u16::from_be_bytes([body[0], body[1]]) as usize;
        let msg2_end = 2 + msg2_len;
        let mut raw = http(&body[..msg2_end], body.len());
        let body_start = raw.len() - msg2_end;
        raw.truncate(body_start + msg2_end);
        let mut reader = BufReader::new(TimeoutAfter(raw));
        assert_eq!(
            read_response(&mut reader, sealed),
            Err(ClientError::Timeout)
        );
    }

    #[test]
    fn response_headers_reject_invalid_names_folding_and_control_bytes() {
        for head in [
            b"HTTP/1.1 200 OK\r\nBad Name: value\r\nContent-Length: 0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding : chunked\r\nContent-Length: 0\r\n\r\n"
                .as_slice(),
            b"HTTP/1.1 200 OK\r\n Content-Length: 0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nX-Test: bad\x01value\r\nContent-Length: 0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 bad\x01reason\r\nContent-Length: 0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nX-Test: bare\nlf\r\nContent-Length: 0\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nX-Test: bare\rcr\r\nContent-Length: 0\r\n\r\n".as_slice(),
        ] {
            assert_eq!(
                read_http_head(&mut BufReader::new(Cursor::new(head))),
                Err(ClientError::BadResponse),
                "{head:?}"
            );
        }
    }

    #[test]
    fn authenticated_v1_response_accepts_obs_text_in_reason_and_values() {
        let (sealed, opened) = pair();
        let body = frame::seal_response(opened, b"{\"ok\":1}").unwrap();
        let mut raw =
            b"HTTP/1.1 200 Succ\xc3\xa8s\r\nX-Label: caf\xc3\xa9\r\nContent-Length: ".to_vec();
        raw.extend_from_slice(body.len().to_string().as_bytes());
        raw.extend_from_slice(b"\r\n\r\n");
        raw.extend_from_slice(&body);
        let (result, _) = run(sealed, raw);
        assert_eq!(result.unwrap(), b"{\"ok\":1}");
    }

    #[test]
    fn truncated_v2_bodies_are_rejected() {
        let (_, _, body) = v2(200_000);
        let boundary = 2 + u16::from_be_bytes([body[0], body[1]]) as usize;
        for cut in [boundary, boundary + 100, body.len() - 1, 1] {
            let (sealed, _, body) = v2(200_000);
            let cut_body = &body[..cut.min(body.len())];
            let (result, _) = run(sealed, http(cut_body, cut_body.len()));
            assert!(result.is_err(), "cut {cut}");
        }
    }

    #[test]
    fn trailing_bytes_after_the_last_record_are_rejected() {
        let (sealed, _, mut body) = v2(200_000);
        body.extend_from_slice(&[0, 3, 1, 2, 3]);
        let (result, _) = run(sealed, http(&body, body.len()));
        assert!(result.is_err());
    }

    #[test]
    fn newer_format_version_is_bad_response() {
        let initiator = snow::Builder::new(PATTERN.parse().unwrap())
            .generate_keypair()
            .unwrap();
        let responder = snow::Builder::new(PATTERN.parse().unwrap())
            .generate_keypair()
            .unwrap();
        let sealed =
            frame::seal_request(&initiator.private, &responder.public, 1_800_000_000, b"{}")
                .unwrap();
        let mut server = snow::Builder::new(PATTERN.parse().unwrap())
            .prologue(b"remuda-cluster-v1")
            .unwrap()
            .local_private_key(&responder.private)
            .unwrap()
            .build_responder()
            .unwrap();
        let mut scratch = [0; 65_535];
        server.read_message(&sealed.message, &mut scratch).unwrap();
        let n = server
            .write_message(&[2, 0, 0, 0, 1, 9], &mut scratch)
            .unwrap();
        let mut body = (n as u16).to_be_bytes().to_vec();
        body.extend_from_slice(&scratch[..n]);
        let (result, _) = run(sealed, http(&body, body.len()));
        assert_eq!(result, Err(ClientError::BadResponse));
    }

    #[test]
    fn io_connection_errors_have_distinct_diagnostics() {
        let refused = map_io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        assert_eq!(refused, ClientError::PeerNotListening);
        assert_eq!(refused.to_string(), "peer is not listening");

        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
        ] {
            let closed = map_io(std::io::Error::from(kind));
            assert_eq!(closed, ClientError::PeerClosedConnection);
            assert_eq!(closed.to_string(), "peer closed the connection");
        }

        let blocked = map_io(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert_eq!(blocked, ClientError::OsBlockedConnection);
        assert_eq!(blocked.to_string(), "the OS blocked the connection");
    }
}
