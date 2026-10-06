//! Bounded HTTP/1.1 client transport. All socket operations live in this module.

use base64::Engine as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::{Host, Url};

#[derive(Clone, Default)]
pub struct HttpClient {
    inner: Arc<ClientState>,
}
#[derive(Default)]
struct ClientState {
    next_id: std::sync::atomic::AtomicU64,
    active: std::sync::atomic::AtomicUsize,
    dns_active: Arc<std::sync::atomic::AtomicUsize>,
}
#[derive(Clone)]
pub struct HttpTask {
    pub id: u64,
    cancelled: Arc<AtomicBool>,
}
impl HttpTask {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}
impl HttpClient {
    pub fn start(
        &self,
        req: HttpRequest,
        complete: impl FnOnce(u64, Result<HttpResponse, String>) + Send + 'static,
    ) -> HttpTask {
        const MAX_IN_FLIGHT: usize = 32;
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        let task = HttpTask {
            id,
            cancelled: cancelled.clone(),
        };
        let admitted = self
            .inner
            .active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_IN_FLIGHT).then_some(n + 1)
            })
            .is_ok();
        if !admitted {
            complete(id, Err("maximum concurrent HTTP requests reached".into()));
            return task;
        }
        let active = self.inner.clone();
        std::thread::spawn(move || {
            let result = perform_with_dns_limit(req, Some(&cancelled), active.dns_active.clone());
            active.active.fetch_sub(1, Ordering::AcqRel);
            complete(id, result);
        });
        task
    }

    pub fn start_peer_certificate(
        &self,
        req: HttpRequest,
        complete: impl FnOnce(u64, Result<PeerCertificate, String>) + Send + 'static,
    ) -> HttpTask {
        const MAX_IN_FLIGHT: usize = 32;
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        let task = HttpTask {
            id,
            cancelled: cancelled.clone(),
        };
        let admitted = self
            .inner
            .active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_IN_FLIGHT).then_some(n + 1)
            })
            .is_ok();
        if !admitted {
            complete(id, Err("maximum concurrent HTTP requests reached".into()));
            return task;
        }
        let active = self.inner.clone();
        std::thread::spawn(move || {
            let result = inspect_peer_certificate(req, Some(&cancelled), active.dns_active.clone());
            active.active.fetch_sub(1, Ordering::AcqRel);
            complete(id, result);
        });
        task
    }
}

type ResponseHeaders = BTreeMap<String, Vec<Vec<u8>>>;

pub const MAX_REQUEST_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_HEADER_LINE: usize = 8 * 1024;
const SOCKET_POLL: Duration = Duration::from_millis(50);

#[derive(Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
    pub timeout: Duration,
    pub connect_timeout: Duration,
    pub max_bytes: usize,
    pub ca_file: Option<String>,
    /// `sha256/<base64 SPKI SHA-256>`.
    pub pin: Option<String>,
    /// A matching `pin` replaces chain validation (#382); requires `pin`.
    pub pin_only: bool,
}

impl std::fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let safe_url = Url::parse(&self.url)
            .map(|mut url| {
                let _ = url.set_username("");
                let _ = url.set_password(None);
                url.set_query(None);
                url.set_fragment(None);
                url.to_string()
            })
            .unwrap_or_else(|_| "<invalid URL>".into());
        let header_names: Vec<&str> = self
            .headers
            .iter()
            .map(|(name, _)| name)
            .map(String::as_str)
            .collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &safe_url)
            .field("header_names", &header_names)
            .field("body", &format_args!("<{} bytes>", self.body.len()))
            .field("timeout", &self.timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("max_bytes", &self.max_bytes)
            .field("ca_file", &self.ca_file)
            .field("pin", &self.pin)
            .field("pin_only", &self.pin_only)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: ResponseHeaders,
    pub body: Vec<u8>,
    pub peer_certificate: Option<PeerCertificate>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerCertificate {
    pub sha256: String,
    pub spki_sha256: String,
    pub not_before: String,
    pub not_after: String,
    pub trusted: bool,
    pub reason: Option<String>,
}

pub fn perform(req: HttpRequest, cancelled: Option<&AtomicBool>) -> Result<HttpResponse, String> {
    perform_with_dns_limit(
        req,
        cancelled,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    )
}

fn perform_with_dns_limit(
    req: HttpRequest,
    cancelled: Option<&AtomicBool>,
    dns_active: Arc<std::sync::atomic::AtomicUsize>,
) -> Result<HttpResponse, String> {
    validate(&req)?;
    let deadline = Instant::now() + req.timeout;
    let url = Url::parse(&req.url).map_err(|_| "invalid HTTP URL".to_string())?;
    let host = url
        .host()
        .ok_or_else(|| "HTTP URL has no host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "HTTP URL has no port".to_string())?;
    let tls = if url.scheme() == "https" {
        let config = tls_config(&req)?;
        let server_name = match host {
            Host::Domain(domain) => ServerName::try_from(domain.to_owned())
                .map_err(|_| "invalid TLS server name".to_string())?,
            Host::Ipv4(address) => ServerName::IpAddress(address.into()),
            Host::Ipv6(address) => ServerName::IpAddress(address.into()),
        };
        Some((config, server_name))
    } else {
        None
    };
    let address = match host {
        Host::Domain(domain) => format!("{domain}:{port}"),
        Host::Ipv4(address) => format!("{address}:{port}"),
        Host::Ipv6(address) => format!("[{address}]:{port}"),
    };
    let addresses = resolve_addresses(address, deadline, cancelled, dns_active)?;
    let connect_deadline = (Instant::now() + req.connect_timeout).min(deadline);
    let stream = connect(addresses.into_iter(), connect_deadline, cancelled)?;
    stream
        .set_read_timeout(Some(SOCKET_POLL))
        .map_err(io_error)?;
    stream
        .set_write_timeout(Some(SOCKET_POLL))
        .map_err(io_error)?;

    if let Some((tls, server_name)) = tls {
        let conn = rustls::ClientConnection::new(tls, server_name).map_err(tls_error)?;
        let mut wire = Wire::new(rustls::StreamOwned::new(conn, stream), deadline, cancelled);
        wire.flush()?;
        let mut response = execute_http(&mut wire, &req, &url)?;
        response.peer_certificate = Some(peer_certificate_from_der(
            wire.stream
                .conn
                .peer_certificates()
                .and_then(|certs| certs.first())
                .ok_or_else(|| "TLS handshake returned no peer certificate".to_string())?,
            true,
            None,
        )?);
        Ok(response)
    } else {
        let mut wire = Wire::new(stream, deadline, cancelled);
        execute_http(&mut wire, &req, &url)
    }
}

fn validate(req: &HttpRequest) -> Result<(), String> {
    let url = Url::parse(&req.url).map_err(|_| "invalid HTTP URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("HTTP URL scheme must be http or https".into());
    }
    if req.method.is_empty() || !req.method.bytes().all(is_token) {
        return Err("invalid HTTP method".into());
    }
    if req.timeout.is_zero() || req.connect_timeout.is_zero() || req.connect_timeout > req.timeout {
        return Err("invalid HTTP timeout bounds".into());
    }
    if req.max_bytes > MAX_REQUEST_BYTES {
        return Err("max_bytes exceeds 20 MiB".into());
    }
    if req.body.len() > MAX_REQUEST_BYTES {
        return Err("request body exceeds 20 MiB".into());
    }
    if req.headers.iter().any(|(name, value)| {
        !name.bytes().all(is_token) || value.iter().any(|b| *b == b'\r' || *b == b'\n')
    }) {
        return Err("invalid HTTP header".into());
    }
    let total: usize = req
        .headers
        .iter()
        .map(|(name, value)| name.len() + value.len() + 4)
        .sum();
    if total > MAX_HEADER_BYTES {
        return Err("request headers exceed 64 KiB".into());
    }
    validate_pin_only(req, "http.request")?;
    if let Some(pin) = &req.pin {
        if !req.pin_only {
            decode_pin(pin)?;
        }
    }
    if (req.ca_file.is_some() || req.pin.is_some() || req.pin_only) && url.scheme() != "https" {
        return Err("TLS options require HTTPS".into());
    }
    Ok(())
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn resolve_addresses(
    address: String,
    deadline: Instant,
    cancelled: Option<&AtomicBool>,
    dns_active: Arc<std::sync::atomic::AtomicUsize>,
) -> Result<Vec<SocketAddr>, String> {
    if dns_active
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < 32).then_some(count + 1)
        })
        .is_err()
    {
        return Err("maximum concurrent DNS lookups reached".into());
    }
    let (resolved_tx, resolved_rx) = std::sync::mpsc::channel();
    let resolver_count = dns_active.clone();
    std::thread::spawn(move || {
        let result = address
            .to_socket_addrs()
            .map(|items| items.collect::<Vec<_>>());
        resolver_count.fetch_sub(1, Ordering::AcqRel);
        let _ = resolved_tx.send(result);
    });
    loop {
        check(deadline, cancelled)?;
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(SOCKET_POLL);
        match resolved_rx.recv_timeout(wait) {
            Ok(Ok(addresses)) => return Ok(addresses),
            Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("HTTP DNS resolution failed".into())
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn connect(
    addresses: impl Iterator<Item = SocketAddr>,
    deadline: Instant,
    cancelled: Option<&AtomicBool>,
) -> Result<TcpStream, String> {
    connect_with(
        addresses.collect(),
        deadline,
        cancelled,
        TcpStream::connect_timeout,
    )
}

fn connect_with(
    addresses: Vec<SocketAddr>,
    deadline: Instant,
    cancelled: Option<&AtomicBool>,
    mut connect_one: impl FnMut(&SocketAddr, Duration) -> std::io::Result<TcpStream>,
) -> Result<TcpStream, String> {
    let mut last = None;
    let mut addresses_left = addresses.len();
    for address in addresses {
        check(deadline, cancelled)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let attempt = (remaining / addresses_left as u32)
            .max(Duration::from_millis(250))
            .min(remaining);
        addresses_left -= 1;
        match connect_one(&address, attempt) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    if let Some(error) = last {
        Err(io_error(error))
    } else {
        Err("timeout while connecting".into())
    }
}

fn execute_http<S: Read + Write>(
    wire: &mut Wire<'_, S>,
    req: &HttpRequest,
    url: &Url,
) -> Result<HttpResponse, String> {
    let host = match url.host() {
        Some(Host::Domain(domain)) => domain.to_owned(),
        Some(Host::Ipv4(address)) => address.to_string(),
        Some(Host::Ipv6(address)) => format!("[{address}]"),
        None => String::new(),
    };
    let host_header = if let Some(port) = url.port() {
        format!("{host}:{port}")
    } else {
        host.to_string()
    };
    let path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    let target = match url.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_string(),
    };
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
        req.method,
        target,
        host_header,
        req.body.len()
    )
    .into_bytes();
    for (name, value) in &req.headers {
        if name.eq_ignore_ascii_case("host")
            || name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("transfer-encoding")
        {
            continue;
        }
        head.extend_from_slice(name.as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value);
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    if head.len() > MAX_HEADER_BYTES {
        return Err("request headers exceed 64 KiB".into());
    }
    wire.write_all(&head)?;
    wire.write_all(&req.body)?;
    wire.flush()?;

    let mut header_bytes = Vec::new();
    while !header_bytes.ends_with(b"\r\n\r\n") {
        if header_bytes.len() >= MAX_HEADER_BYTES {
            return Err("response headers exceed 64 KiB".into());
        }
        let byte = wire.read_byte()?;
        header_bytes.push(byte);
    }
    let (status, headers) = parse_headers(&header_bytes)?;
    let no_body = req.method.eq_ignore_ascii_case("HEAD")
        || (100..200).contains(&status)
        || status == 204
        || status == 304;
    let chunked = header_value(&headers, "transfer-encoding")
        .and_then(|value| std::str::from_utf8(value).ok())
        .is_some_and(|value| {
            value
                .split(',')
                .next_back()
                .is_some_and(|coding| coding.trim().eq_ignore_ascii_case("chunked"))
        });
    let body = if no_body {
        Vec::new()
    } else if chunked {
        read_chunked(wire, req.max_bytes)?
    } else if let Some(length) = header_value(&headers, "content-length") {
        let length = std::str::from_utf8(length)
            .map_err(|_| "invalid Content-Length".to_string())?
            .trim()
            .parse::<usize>()
            .map_err(|_| "invalid Content-Length".to_string())?;
        if length > req.max_bytes {
            return Err("response exceeds max_bytes".into());
        }
        wire.read_exact_vec(length)?
    } else {
        wire.read_to_end_limited(req.max_bytes)?
    };
    Ok(HttpResponse {
        status,
        headers,
        body,
        peer_certificate: None,
    })
}

fn parse_headers(bytes: &[u8]) -> Result<(u16, ResponseHeaders), String> {
    let lines: Vec<&[u8]> = bytes
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .collect();
    let status_line = lines
        .first()
        .ok_or_else(|| "missing HTTP status".to_string())?;
    let status_line =
        std::str::from_utf8(status_line).map_err(|_| "invalid HTTP status line".to_string())?;
    let mut words = status_line.split_ascii_whitespace();
    if !words
        .next()
        .is_some_and(|version| version.starts_with("HTTP/1."))
    {
        return Err("invalid HTTP status line".into());
    }
    let status = words
        .next()
        .ok_or_else(|| "missing HTTP status code".to_string())?
        .parse::<u16>()
        .map_err(|_| "invalid HTTP status code".to_string())?;
    let mut headers = ResponseHeaders::new();
    for line in lines.into_iter().skip(1) {
        if line.is_empty() {
            break;
        }
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or_else(|| "invalid response header".to_string())?;
        let (name, rest) = line.split_at(colon);
        let value = &rest[1..];
        let name =
            std::str::from_utf8(name).map_err(|_| "invalid response header name".to_string())?;
        if name.is_empty() || !name.bytes().all(is_token) {
            return Err("invalid response header name".into());
        }
        let value = trim_ows(value).to_vec();
        headers
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(value);
    }
    for (name, values) in &mut headers {
        if name != "set-cookie" && values.len() > 1 {
            let joined = values
                .iter()
                .enumerate()
                .fold(Vec::new(), |mut out, (index, value)| {
                    if index > 0 {
                        out.extend_from_slice(b", ");
                    }
                    out.extend_from_slice(value);
                    out
                });
            *values = vec![joined];
        }
    }
    Ok((status, headers))
}

fn trim_ows(mut value: &[u8]) -> &[u8] {
    while value
        .first()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        value = &value[1..];
    }
    while value
        .last()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        value = &value[..value.len() - 1];
    }
    value
}

fn header_value<'a>(headers: &'a ResponseHeaders, name: &str) -> Option<&'a [u8]> {
    headers.get(name)?.first().map(Vec::as_slice)
}

fn read_chunked<S: Read + Write>(wire: &mut Wire<'_, S>, max: usize) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let line = wire.read_line(MAX_HEADER_LINE)?;
        let size = std::str::from_utf8(&line)
            .map_err(|_| "invalid chunk size".to_string())?
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let size = usize::from_str_radix(size, 16).map_err(|_| "invalid chunk size".to_string())?;
        if size == 0 {
            let mut trailer_bytes = 0usize;
            loop {
                let trailer = wire.read_line(MAX_HEADER_LINE)?;
                trailer_bytes = trailer_bytes.saturating_add(trailer.len() + 2);
                if trailer_bytes > MAX_HEADER_BYTES {
                    return Err("response headers exceed 64 KiB".into());
                }
                if trailer.is_empty() {
                    break;
                }
            }
            break;
        }
        if size > max.saturating_sub(body.len()) {
            return Err("response exceeds max_bytes".into());
        }
        body.extend(wire.read_exact_vec(size)?);
        if wire.read_exact_vec(2)? != b"\r\n" {
            return Err("invalid chunk terminator".into());
        }
    }
    Ok(body)
}

struct Wire<'a, S> {
    stream: S,
    deadline: Instant,
    cancelled: Option<&'a AtomicBool>,
}

impl<'a, S: Read + Write> Wire<'a, S> {
    fn new(stream: S, deadline: Instant, cancelled: Option<&'a AtomicBool>) -> Self {
        Self {
            stream,
            deadline,
            cancelled,
        }
    }
    fn check(&self) -> Result<(), String> {
        check(self.deadline, self.cancelled)
    }
    fn write_all(&mut self, mut bytes: &[u8]) -> Result<(), String> {
        while !bytes.is_empty() {
            self.check()?;
            match self.stream.write(bytes) {
                Ok(0) => return Err("HTTP socket closed while writing".into()),
                Ok(n) => bytes = &bytes[n..],
                Err(e) if is_timeout(&e) => continue,
                Err(e) => return Err(io_error(e)),
            }
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<(), String> {
        loop {
            self.check()?;
            match self.stream.flush() {
                Ok(()) => return Ok(()),
                Err(e) if is_timeout(&e) => continue,
                Err(e) => return Err(io_error(e)),
            }
        }
    }
    fn read_byte(&mut self) -> Result<u8, String> {
        let mut byte = [0];
        self.read_exact(&mut byte)?;
        Ok(byte[0])
    }
    fn read_exact(&mut self, mut out: &mut [u8]) -> Result<(), String> {
        while !out.is_empty() {
            self.check()?;
            match self.stream.read(out) {
                Ok(0) => return Err("unexpected end of HTTP response".into()),
                Ok(n) => {
                    let (_, rest) = out.split_at_mut(n);
                    out = rest;
                }
                Err(e) if is_timeout(&e) => continue,
                Err(e) => return Err(io_error(e)),
            }
        }
        Ok(())
    }
    fn read_exact_vec(&mut self, len: usize) -> Result<Vec<u8>, String> {
        let mut out = vec![0; len];
        self.read_exact(&mut out)?;
        Ok(out)
    }
    fn read_line(&mut self, max: usize) -> Result<Vec<u8>, String> {
        let mut line = Vec::new();
        loop {
            let byte = self.read_byte()?;
            if byte == b'\n' {
                if line.last() != Some(&b'\r') {
                    return Err("invalid HTTP line ending".into());
                }
                line.pop();
                return Ok(line);
            }
            if line.len() >= max {
                return Err("HTTP line exceeds limit".into());
            }
            line.push(byte);
        }
    }
    fn read_to_end_limited(&mut self, max: usize) -> Result<Vec<u8>, String> {
        let mut body = Vec::new();
        let mut buf = [0; 8192];
        loop {
            self.check()?;
            match self.stream.read(&mut buf) {
                Ok(0) => return Ok(body),
                Ok(n) => {
                    if n > max.saturating_sub(body.len()) {
                        return Err("response exceeds max_bytes".into());
                    }
                    body.extend_from_slice(&buf[..n]);
                }
                Err(e) if is_timeout(&e) => continue,
                Err(e) => return Err(io_error(e)),
            }
        }
    }
}

fn check(deadline: Instant, cancelled: Option<&AtomicBool>) -> Result<(), String> {
    if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Err("request cancelled".into());
    }
    if Instant::now() >= deadline {
        return Err("request timeout".into());
    }
    Ok(())
}
fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}
fn io_error(error: std::io::Error) -> String {
    if is_timeout(&error) {
        "request timeout".into()
    } else {
        let message = error.to_string();
        let lower = message.to_ascii_lowercase();
        if let Some(reason) = tls_failure_reason(&message) {
            format!("TLS request failed: {reason}")
        } else if error.kind() == std::io::ErrorKind::InvalidData || lower.contains("osstatus") {
            "TLS request failed: certificate or protocol validation failed".into()
        } else {
            format!("HTTP transport error: {error}")
        }
    }
}
fn tls_error(error: TlsError) -> String {
    match error {
        TlsError::InvalidCertificate(reason) => {
            format!(
                "TLS request failed: {}",
                certificate_failure_reason(&reason)
            )
        }
        TlsError::NoCertificatesPresented => {
            "TLS request failed: server presented no certificate".into()
        }
        TlsError::General(message) => {
            let reason = tls_failure_reason(&message).unwrap_or("handshake validation failed");
            format!("TLS request failed: {reason}")
        }
        _ => "TLS request failed: TLS negotiation failed".into(),
    }
}

fn certificate_failure_reason(error: &CertificateError) -> &'static str {
    match error {
        CertificateError::Expired | CertificateError::ExpiredContext { .. } => {
            "server certificate expired"
        }
        CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => {
            "server certificate not yet valid"
        }
        CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
            "server hostname mismatch"
        }
        CertificateError::UnknownIssuer => "server certificate issuer not trusted",
        CertificateError::Revoked => "server certificate revoked",
        CertificateError::InvalidPurpose | CertificateError::InvalidPurposeContext { .. } => {
            "server certificate not valid for TLS server authentication"
        }
        CertificateError::UnhandledCriticalExtension => {
            "server certificate has an unsupported critical extension"
        }
        _ => "server certificate validation failed",
    }
}

fn tls_failure_reason(message: &str) -> Option<&'static str> {
    let lower = message.to_ascii_lowercase();
    if lower.contains("spki pin mismatch") {
        Some("SPKI pin mismatch")
    } else if lower.contains("expired") {
        Some("server certificate expired")
    } else if lower.contains("not valid yet") || lower.contains("notvalidyet") {
        Some("server certificate not yet valid")
    } else if lower.contains("not valid for name") || lower.contains("notvalidforname") {
        Some("server hostname mismatch")
    } else if lower.contains("unknown issuer") || lower.contains("unknownissuer") {
        Some("server certificate issuer not trusted")
    } else if lower.contains("invalid purpose") || lower.contains("invalidpurpose") {
        Some("server certificate not valid for TLS server authentication")
    } else {
        None
    }
}

fn tls_config(req: &HttpRequest) -> Result<Arc<ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = tls_verifier(req, provider.clone())?;
    client_config(provider, verifier)
}

fn inspecting_tls_config(
    req: &HttpRequest,
    result: Arc<std::sync::Mutex<Option<Result<(), String>>>>,
) -> Result<Arc<ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let inner = tls_verifier(req, provider.clone())?;
    let verifier: Arc<dyn ServerCertVerifier> = Arc::new(InspectingVerifier { inner, result });
    client_config(provider, verifier)
}

fn tls_verifier(
    req: &HttpRequest,
    provider: Arc<rustls::crypto::CryptoProvider>,
) -> Result<Arc<dyn ServerCertVerifier>, String> {
    validate_pin_only(req, "http.request")?;
    let verifier: Arc<dyn ServerCertVerifier> = if req.pin_only {
        Arc::new(rustls_platform_verifier::Verifier::new(provider.clone()).map_err(tls_error)?)
    } else if let Some(path) = &req.ca_file {
        let pem = read_ca_file(path)?;
        let certs = parse_pem_certs(&pem)?;
        if certs.is_empty() {
            return Err("custom CA file contains no certificates".into());
        }
        let mut roots = rustls::RootCertStore::empty();
        for cert in &certs {
            roots.add(cert.clone()).map_err(tls_error)?;
        }
        let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .map_err(|error| error.to_string())?;
        Arc::new(CaFileVerifier {
            inner,
            trusted_end_entities: certs
                .into_iter()
                .map(|cert| cert.as_ref().to_vec())
                .collect(),
        })
    } else {
        Arc::new(rustls_platform_verifier::Verifier::new(provider.clone()).map_err(tls_error)?)
    };
    match &req.pin {
        Some(pin) => Ok(Arc::new(PinnedVerifier {
            inner: verifier,
            pin: decode_pin(pin)?,
            pin_only: req.pin_only,
        })),
        None => Ok(verifier),
    }
}

fn client_config(
    provider: Arc<rustls::crypto::CryptoProvider>,
    verifier: Arc<dyn ServerCertVerifier>,
) -> Result<Arc<ClientConfig>, String> {
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(tls_error)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth()
        .pipe(Arc::new)
        .pipe(Ok)
}

#[derive(Debug)]
struct InspectingVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    result: Arc<std::sync::Mutex<Option<Result<(), String>>>>,
}

impl ServerCertVerifier for InspectingVerifier {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let result = self
            .inner
            .verify_server_cert(cert, intermediates, name, ocsp, now);
        let trust = result.as_ref().map(|_| ()).map_err(peer_validation_reason);
        if let Ok(mut state) = self.result.lock() {
            *state = Some(trust);
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn peer_validation_reason(error: &TlsError) -> String {
    match error {
        TlsError::InvalidCertificate(reason) => certificate_failure_reason(reason).to_string(),
        TlsError::NoCertificatesPresented => "server presented no certificate".into(),
        TlsError::General(message) => tls_failure_reason(message)
            .unwrap_or("server certificate validation failed")
            .into(),
        _ => "server certificate validation failed".into(),
    }
}

#[cfg(test)]
fn peer_certificate(
    req: &HttpRequest,
    cancelled: Option<&AtomicBool>,
) -> Result<PeerCertificate, String> {
    inspect_peer_certificate(
        req.clone(),
        cancelled,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    )
}

fn inspect_peer_certificate(
    req: HttpRequest,
    cancelled: Option<&AtomicBool>,
    dns_active: Arc<std::sync::atomic::AtomicUsize>,
) -> Result<PeerCertificate, String> {
    let url = Url::parse(&req.url).map_err(|_| "invalid HTTPS URL".to_string())?;
    if url.scheme() != "https" {
        return Err("peer_certificate requires an https URL".into());
    }
    validate_pin_only(&req, "http.peer_certificate")?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("peer_certificate URL must not contain credentials".into());
    }
    if req.timeout.is_zero() || req.connect_timeout.is_zero() || req.connect_timeout > req.timeout {
        return Err("invalid HTTP timeout bounds".into());
    }
    let deadline = Instant::now() + req.timeout;
    let host = url
        .host()
        .ok_or_else(|| "HTTPS URL has no host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "HTTPS URL has no port".to_string())?;
    let server_name = match host {
        Host::Domain(domain) => ServerName::try_from(domain.to_owned())
            .map_err(|_| "invalid TLS server name".to_string())?,
        Host::Ipv4(address) => ServerName::IpAddress(address.into()),
        Host::Ipv6(address) => ServerName::IpAddress(address.into()),
    };
    let address = match host {
        Host::Domain(domain) => format!("{domain}:{port}"),
        Host::Ipv4(address) => format!("{address}:{port}"),
        Host::Ipv6(address) => format!("[{address}]:{port}"),
    };
    let addresses = resolve_addresses(address, deadline, cancelled, dns_active)?;
    let stream = connect(
        addresses.into_iter(),
        (Instant::now() + req.connect_timeout).min(deadline),
        cancelled,
    )?;
    stream
        .set_read_timeout(Some(SOCKET_POLL))
        .map_err(io_error)?;
    stream
        .set_write_timeout(Some(SOCKET_POLL))
        .map_err(io_error)?;
    let outcome = Arc::new(std::sync::Mutex::new(None));
    let config = inspecting_tls_config(&req, outcome.clone())?;
    let conn = rustls::ClientConnection::new(config, server_name).map_err(tls_error)?;
    let mut wire = Wire::new(rustls::StreamOwned::new(conn, stream), deadline, cancelled);
    wire.flush()?;
    let cert = wire
        .stream
        .conn
        .peer_certificates()
        .and_then(|certs| certs.first())
        .ok_or_else(|| "TLS handshake returned no peer certificate".to_string())?;
    let trust = outcome
        .lock()
        .map_err(|_| "TLS verifier state unavailable".to_string())?
        .clone()
        .ok_or_else(|| "TLS verifier returned no result".to_string())?;
    peer_certificate_from_der(cert, trust.is_ok(), trust.err())
}

fn peer_certificate_from_der(
    cert: &CertificateDer<'_>,
    trusted: bool,
    reason: Option<String>,
) -> Result<PeerCertificate, String> {
    let (not_before, not_after) = certificate_validity(cert.as_ref())?;
    let sha256 = Sha256::digest(cert.as_ref())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let spki_sha256 = base64::engine::general_purpose::STANDARD
        .encode(Sha256::digest(certificate_spki(cert.as_ref())?));
    Ok(PeerCertificate {
        sha256,
        spki_sha256,
        not_before: format_utc(not_before),
        not_after: format_utc(not_after),
        trusted,
        reason,
    })
}

fn format_utc(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let seconds = timestamp.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3_600,
        seconds % 3_600 / 60,
        seconds % 60
    )
}

#[derive(Debug)]
struct CaFileVerifier {
    inner: Arc<WebPkiServerVerifier>,
    trusted_end_entities: Vec<Vec<u8>>,
}

impl ServerCertVerifier for CaFileVerifier {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        match self
            .inner
            .verify_server_cert(cert, intermediates, name, ocsp, now)
        {
            Ok(verified) => Ok(verified),
            Err(_error)
                if self
                    .trusted_end_entities
                    .iter()
                    .any(|trusted| trusted == cert.as_ref()) =>
            {
                verify_trusted_end_entity(cert, name, now)?;
                Ok(ServerCertVerified::assertion())
            }
            Err(error) => Err(error),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn certificate_validity(cert: &[u8]) -> Result<(i64, i64), String> {
    let (_, outer, _) = der_value(cert, 0)?;
    let (tag, tbs, _) = der_value(outer, 0)?;
    if tag != 0x30 {
        return Err("malformed certificate".into());
    }
    let mut at = if tbs.first() == Some(&0xa0) {
        der_value(tbs, 0)?.2
    } else {
        0
    };
    for _ in 0..3 {
        at = der_value(tbs, at)?.2;
    }
    let (tag, validity, _) = der_value(tbs, at)?;
    if tag != 0x30 {
        return Err("malformed certificate validity".into());
    }
    let (before_tag, before, next) = der_value(validity, 0)?;
    let (after_tag, after, _) = der_value(validity, next)?;
    let before = asn1_time(before_tag, before)
        .ok_or_else(|| "unsupported certificate validity time".to_string())?;
    let after = asn1_time(after_tag, after)
        .ok_or_else(|| "unsupported certificate validity time".to_string())?;
    Ok((before, after))
}

fn verify_trusted_end_entity(
    cert: &CertificateDer<'_>,
    name: &ServerName<'_>,
    now: UnixTime,
) -> Result<(), TlsError> {
    let parsed = rustls::server::ParsedCertificate::try_from(cert)?;
    rustls::client::verify_server_name(&parsed, name)?;
    let (before, after) = certificate_validity(cert.as_ref()).map_err(TlsError::General)?;
    let now = now.as_secs() as i64;
    if now < before {
        return Err(TlsError::General(
            "server certificate is not yet valid".into(),
        ));
    }
    if now > after {
        return Err(TlsError::General("server certificate has expired".into()));
    }
    Ok(())
}

fn asn1_time(tag: u8, bytes: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(bytes).ok()?;
    if !text.is_ascii()
        || !text.ends_with('Z')
        || !text[..text.len() - 1]
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let (year, rest) = match tag {
        0x17 if text.ends_with('Z') && text.len() == 13 => {
            let short = text[0..2].parse::<i64>().ok()?;
            (
                if short >= 50 {
                    1900 + short
                } else {
                    2000 + short
                },
                &text[2..12],
            )
        }
        0x18 if text.ends_with('Z') && text.len() == 15 => {
            (text[0..4].parse::<i64>().ok()?, &text[4..14])
        }
        _ => return None,
    };
    let month = rest[0..2].parse::<i64>().ok()?;
    let day = rest[2..4].parse::<i64>().ok()?;
    let hour = rest[4..6].parse::<i64>().ok()?;
    let minute = rest[6..8].parse::<i64>().ok()?;
    let second = rest[8..10].parse::<i64>().ok()?;
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=max_day).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146097 + day_of_era - 719468;
    Some(days * 86400 + hour * 3600 + minute * 60 + second)
}

const MAX_CA_FILE_BYTES: u64 = 1024 * 1024;
fn read_ca_file(path: &str) -> Result<String, String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).map_err(|_| "cannot read custom CA file".to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_CA_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read custom CA file".to_string())?;
    if bytes.len() as u64 > MAX_CA_FILE_BYTES {
        return Err("custom CA file exceeds 1 MiB".into());
    }
    String::from_utf8(bytes).map_err(|_| "malformed PEM CA file".into())
}

fn parse_pem_certs(pem: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let mut certs = Vec::new();
    let mut rest = pem;
    while let Some(start) = rest.find("-----BEGIN CERTIFICATE-----") {
        rest = &rest[start + "-----BEGIN CERTIFICATE-----".len()..];
        let end = rest
            .find("-----END CERTIFICATE-----")
            .ok_or_else(|| "malformed PEM CA certificate".to_string())?;
        let encoded: String = rest[..end]
            .chars()
            .filter(|ch| !ch.is_ascii_whitespace())
            .collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "malformed PEM CA certificate".to_string())?;
        certs.push(CertificateDer::from(der));
        rest = &rest[end + "-----END CERTIFICATE-----".len()..];
    }
    Ok(certs)
}

fn decode_pin(pin: &str) -> Result<[u8; 32], String> {
    let encoded = pin
        .strip_prefix("sha256/")
        .ok_or_else(|| "pin must use sha256/<base64 SPKI SHA-256>".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "invalid SPKI pin".to_string())?;
    bytes
        .try_into()
        .map_err(|_| "SPKI pin must contain a SHA-256 digest".into())
}

#[derive(Debug)]
struct PinnedVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    pin: [u8; 32],
    pin_only: bool,
}
impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        if self.pin_only {
            verify_trusted_end_entity(cert, name, now)?;
        } else {
            self.inner
                .verify_server_cert(cert, intermediates, name, ocsp, now)?;
        }
        let spki = certificate_spki(cert.as_ref()).map_err(TlsError::General)?;
        let actual = Sha256::digest(spki);
        if actual.as_slice() != self.pin {
            return Err(TlsError::General("SPKI pin mismatch".into()));
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn certificate_spki(cert: &[u8]) -> Result<&[u8], String> {
    let (outer_tag, outer, _) = der_value(cert, 0)?;
    if outer_tag != 0x30 {
        return Err("invalid leaf certificate".into());
    }
    let (tbs_tag, tbs, _) = der_value(outer, 0)?;
    if tbs_tag != 0x30 {
        return Err("invalid leaf certificate TBS".into());
    }
    let mut at = 0;
    if tbs.get(at) == Some(&0xa0) {
        at = der_value(tbs, at)?.2;
    }
    for _ in 0..5 {
        at = der_value(tbs, at)?.2;
    }
    let (tag, _, spki_end) = der_value(tbs, at)?;
    if tag != 0x30 {
        return Err("invalid certificate SPKI".into());
    }
    Ok(&tbs[at..spki_end])
}
fn der_value(input: &[u8], at: usize) -> Result<(u8, &[u8], usize), String> {
    let tag = *input.get(at).ok_or_else(|| "truncated DER".to_string())?;
    let first = *input
        .get(at + 1)
        .ok_or_else(|| "truncated DER".to_string())?;
    let (len, len_bytes) = if first & 0x80 == 0 {
        (usize::from(first), 1)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > std::mem::size_of::<usize>() {
            return Err("invalid DER length".into());
        }
        let mut len = 0usize;
        for byte in input
            .get(at + 2..at + 2 + count)
            .ok_or_else(|| "truncated DER length".to_string())?
        {
            len = len
                .checked_mul(256)
                .and_then(|n| n.checked_add(usize::from(*byte)))
                .ok_or_else(|| "DER length overflow".to_string())?;
        }
        (len, 1 + count)
    };
    let content_at = at + 1 + len_bytes;
    let end = content_at
        .checked_add(len)
        .filter(|end| *end <= input.len())
        .ok_or_else(|| "truncated DER value".to_string())?;
    Ok((tag, &input[content_at..end], end))
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

fn pin_only_error(api: &str) -> String {
    format!(
        "{api} pin_only requires a valid pin.\nNext: set pin to sha256/<base64 SPKI SHA-256> or omit pin_only"
    )
}

fn validate_pin_only(req: &HttpRequest, api: &str) -> Result<(), String> {
    if !req.pin_only {
        return Ok(());
    }
    let Some(pin) = req.pin.as_deref() else {
        return Err(pin_only_error(api));
    };
    decode_pin(pin).map_err(|_| pin_only_error(api))?;
    Ok(())
}

fn tls_options(
    options: &mlua::Table,
    api: &str,
) -> mlua::Result<(Option<String>, Option<String>, bool)> {
    let ca_file = options.get::<Option<String>>("ca_file")?;
    let pin = options.get::<Option<String>>("pin")?;
    let pin_only = match options.get::<mlua::Value>("pin_only")? {
        mlua::Value::Nil => false,
        mlua::Value::Boolean(value) => value,
        _ => {
            return Err(mlua::Error::runtime(format!(
                "{api} pin_only must be a boolean.\nNext: set pin_only to true or false"
            )));
        }
    };
    if pin_only {
        let Some(value) = pin.as_deref() else {
            return Err(mlua::Error::runtime(pin_only_error(api)));
        };
        if decode_pin(value).is_err() {
            return Err(mlua::Error::runtime(pin_only_error(api)));
        }
    }
    Ok((ca_file, pin, pin_only))
}

pub fn install(
    lua: &mlua::Lua,
    remuda: &mlua::Table,
    image: crate::image::Image,
) -> mlua::Result<()> {
    let http = lua.create_table()?;
    let request_image = image.clone();
    http.set(
        "request",
        lua.create_function(move |lua, options: mlua::Table| {
            let method: String = options.get("method")?;
            let url: String = options.get("url")?;
            let timeout = duration_option(&options, "timeout", None, "http.request")?
                .ok_or_else(|| mlua::Error::runtime("http.request requires timeout"))?;
            let connect_timeout = duration_option(
                &options,
                "connect_timeout",
                Some(timeout.min(Duration::from_secs(10))),
                "http.request",
            )?
            .unwrap();
            let max_bytes = options
                .get::<Option<usize>>("max_bytes")?
                .unwrap_or(1024 * 1024);
            let body = options
                .get::<Option<mlua::LuaString>>("body")?
                .map(|value| value.as_bytes().to_vec())
                .unwrap_or_default();
            let (ca_file, pin, pin_only) = tls_options(&options, "http.request")?;
            let callback: mlua::Function = options.get("callback")?;
            let mut headers = Vec::new();
            if let Some(table) = options.get::<Option<mlua::Table>>("headers")? {
                for pair in table.pairs::<String, mlua::LuaString>() {
                    let (name, value) = pair?;
                    headers.push((name, value.as_bytes().to_vec()));
                }
            }
            let task = request_image.start_http(HttpRequest {
                method,
                url,
                headers,
                body,
                timeout,
                connect_timeout,
                max_bytes,
                ca_file,
                pin,
                pin_only,
            });
            let key = format!("remuda.http.callback.{}", task.id);
            lua.set_named_registry_value(&key, callback)?;
            let handle = lua.create_table()?;
            handle.set(
                "cancel",
                lua.create_function(move |_, _: mlua::MultiValue| {
                    task.cancel();
                    Ok(())
                })?,
            )?;
            Ok(handle)
        })?,
    )?;
    let peer_image = image.clone();
    http.set(
        "peer_certificate",
        lua.create_function(move |lua, options: mlua::Table| {
            let url: String = options.get("url")?;
            let timeout = duration_option(&options, "timeout", None, "http.peer_certificate")?
                .ok_or_else(|| mlua::Error::runtime("http.peer_certificate requires timeout"))?;
            let connect_timeout = duration_option(
                &options,
                "connect_timeout",
                Some(timeout.min(Duration::from_secs(10))),
                "http.peer_certificate",
            )?
            .unwrap();
            let (ca_file, pin, pin_only) = tls_options(&options, "http.peer_certificate")?;
            let callback: mlua::Function = options.get("callback")?;
            let task = peer_image.start_peer_certificate(HttpRequest {
                method: "GET".into(),
                url,
                headers: Vec::new(),
                body: Vec::new(),
                timeout,
                connect_timeout,
                max_bytes: 0,
                ca_file,
                pin,
                pin_only,
            });
            let key = format!("remuda.http.callback.{}", task.id);
            lua.set_named_registry_value(&key, callback)?;
            let handle = lua.create_table()?;
            handle.set(
                "cancel",
                lua.create_function(move |_, _: mlua::MultiValue| {
                    task.cancel();
                    Ok(())
                })?,
            )?;
            Ok(handle)
        })?,
    )?;
    remuda.set("http", http)
}

fn duration_option(
    options: &mlua::Table,
    name: &str,
    default: Option<Duration>,
    api: &str,
) -> mlua::Result<Option<Duration>> {
    let seconds = options.get::<Option<f64>>(name)?;
    let Some(seconds) = seconds else {
        return Ok(default);
    };
    if !seconds.is_finite() || seconds <= 0.0 || seconds > 3600.0 {
        return Err(mlua::Error::runtime(format!(
            "{api} {name} must be between 0 and 3600 seconds"
        )));
    }
    Ok(Some(Duration::from_secs_f64(seconds)))
}

#[cfg(test)]
mod tests {
    use super::{perform, CertificateError, HttpRequest, TlsError};
    use base64::Engine as _;
    use rustls::pki_types::ServerName;
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn invalid_timeout_error_names_peer_certificate_api() {
        let lua = mlua::Lua::new();
        let options = lua.create_table().unwrap();
        options.set("timeout", 3601.0).unwrap();
        let error = super::duration_option(&options, "timeout", None, "http.peer_certificate")
            .unwrap_err()
            .to_string();
        assert!(error.contains("http.peer_certificate timeout"), "{error}");
        let request_error = super::duration_option(&options, "timeout", None, "http.request")
            .unwrap_err()
            .to_string();
        assert!(
            request_error.contains("http.request timeout"),
            "{request_error}"
        );
    }

    fn stub(response: &'static [u8], delay: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request);
            if !delay.is_zero() {
                thread::sleep(delay);
            }
            let _ = socket.write_all(response);
        });
        format!("http://{address}/test")
    }

    fn request(url: String) -> HttpRequest {
        HttpRequest {
            method: "GET".into(),
            url,
            headers: vec![],
            body: vec![],
            timeout: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(1),
            max_bytes: 1024,
            ca_file: None,
            pin: None,
            pin_only: false,
        }
    }

    #[test]
    fn request_debug_redacts_credentials_query_header_values_and_body() {
        let mut req = request("https://user:pass@example.test/path?token=secret".into());
        req.headers
            .push(("Authorization".into(), b"Bearer secret".to_vec()));
        req.body = b"private body".to_vec();
        let debug = format!("{req:?}");
        assert!(debug.contains("https://example.test/path"));
        assert!(debug.contains("Authorization"));
        assert!(debug.contains("<12 bytes>"));
        for secret in [
            "user",
            "pass",
            "token=secret",
            "Bearer secret",
            "private body",
        ] {
            assert!(!debug.contains(secret), "Debug leaked {secret}");
        }
    }

    #[test]
    fn platform_certificate_errors_use_tls_class_without_raw_os_status() {
        let error = super::io_error(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "OSStatus -67843",
        ));
        assert!(error.starts_with("TLS request failed:"));
        assert!(!error.contains("-67843"));

        let expired = super::tls_error(TlsError::InvalidCertificate(CertificateError::Expired));
        assert_eq!(expired, "TLS request failed: server certificate expired");
        let hostname = super::tls_error(TlsError::InvalidCertificate(
            CertificateError::NotValidForName,
        ));
        assert_eq!(hostname, "TLS request failed: server hostname mismatch");
        let issuer = super::tls_error(TlsError::InvalidCertificate(
            CertificateError::UnknownIssuer,
        ));
        assert_eq!(
            issuer,
            "TLS request failed: server certificate issuer not trusted"
        );
        let via_io = super::io_error(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid peer certificate: certificate not valid for name; OSStatus -67843",
        ));
        assert_eq!(via_io, "TLS request failed: server hostname mismatch");
        assert!(!via_io.contains("-67843"));
    }

    #[test]
    fn connection_attempts_share_the_budget_and_allow_a_slow_first_address() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut first_budget = Duration::ZERO;
        let stream =
            super::connect_with(vec![address, address], deadline, None, |address, budget| {
                if first_budget.is_zero() {
                    first_budget = budget;
                    thread::sleep(Duration::from_millis(300));
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "delayed address",
                    ))
                } else {
                    TcpStream::connect_timeout(address, budget)
                }
            })
            .unwrap();
        assert!(first_budget >= Duration::from_millis(900));
        drop(stream);
        drop(listener);
    }

    #[test]
    fn reusable_matrix_stub_routes_json_and_records_requests() {
        let server = super::super::testing::ScriptedHttpServer::new().unwrap();
        server.route_json(
            "/sync?since=s0",
            200,
            br#"{"next_batch":"s1","events":[]}"#.to_vec(),
        );
        let mut req = request(format!("{}{}", server.base_url(), "/sync?since=s0"));
        req.headers
            .push(("Authorization".into(), b"Bearer local-test-token".to_vec()));
        req.headers
            .push(("Content-Type".into(), b"application/json".to_vec()));
        req.method = "POST".into();
        req.body = br#"{"timeout":30000}"#.to_vec();
        let response = perform(req, None).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(
            response.headers.get("content-type").unwrap()[0],
            b"application/json"
        );
        let recorded = server.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].path, "/sync?since=s0");
        assert_eq!(
            recorded[0].headers["authorization"][0],
            b"Bearer local-test-token"
        );
        assert_eq!(recorded[0].body, br#"{"timeout":30000}"#);
    }

    #[test]
    fn sends_binary_headers_and_a_20_mib_request_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            assert!(request
                .windows(b"X-Byte: ".len())
                .any(|window| window == b"X-Byte: "));
            let content_length = String::from_utf8_lossy(&request)
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .unwrap()
                .parse::<usize>()
                .unwrap();
            let mut body = vec![0; content_length];
            socket.read_exact(&mut body).unwrap();
            assert_eq!(content_length, 20 * 1024 * 1024);
            assert!(body.iter().all(|byte| *byte == 0xA5));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        });
        let mut req = request(format!("http://{address}/upload"));
        req.method = "PUT".into();
        req.headers.push(("X-Byte".into(), vec![0x80]));
        req.body = vec![0xA5; 20 * 1024 * 1024];
        assert_eq!(perform(req, None).unwrap().status, 200);
        server.join().unwrap();
    }

    #[test]
    fn accepts_a_20_mib_response_body_and_does_not_follow_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request);
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 20971520\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            socket.write_all(&vec![0x5a; 20 * 1024 * 1024]).unwrap();
        });
        let mut req = request(format!("http://{address}/download"));
        req.max_bytes = 20 * 1024 * 1024;
        let response = perform(req, None).unwrap();
        assert_eq!(response.body.len(), 20 * 1024 * 1024);
        assert!(response.body.iter().all(|byte| *byte == 0x5a));
        server.join().unwrap();

        let redirect = stub(b"HTTP/1.1 302 Found\r\nLocation: https://other.invalid/\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno", Duration::ZERO);
        let response = perform(request(redirect), None).unwrap();
        assert_eq!(response.status, 302);
        assert_eq!(response.body, b"no");
    }

    #[test]
    fn exposes_status_headers_and_body_bytes() {
        let url = stub(b"HTTP/1.1 201 Created\r\nX-Trace: abc\r\nX-Trace: def\r\nX-Byte: \x80\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok!", Duration::ZERO);
        let response = perform(request(url), None).unwrap();
        assert_eq!(response.status, 201);
        assert_eq!(response.headers.get("x-trace").unwrap()[0], b"abc, def");
        assert_eq!(response.headers.get("x-byte").unwrap()[0], &[0x80]);
        assert_eq!(response.body, b"ok!");
    }

    fn tls_stub() -> (String, std::sync::mpsc::Receiver<bool>) {
        tls_stub_with(
            include_str!("testdata/test-leaf.pem"),
            include_str!("testdata/test-leaf-key.pem"),
            false,
            "localhost",
        )
    }

    fn tls_stub_with(
        cert_pem: &'static str,
        key_pem: &'static str,
        ipv6: bool,
        host: &str,
    ) -> (String, std::sync::mpsc::Receiver<bool>) {
        use rustls::pki_types::PrivateKeyDer;
        use std::sync::mpsc;
        let listener = if ipv6 {
            TcpListener::bind("[::1]:0").unwrap()
        } else {
            TcpListener::bind("127.0.0.1:0").unwrap()
        };
        let address = listener.local_addr().unwrap();
        let certs = super::parse_pem_certs(cert_pem).unwrap();
        let key = pem_key(key_pem);
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, PrivateKeyDer::try_from(key).unwrap())
            .unwrap();
        let (seen_tx, seen_rx) = mpsc::channel();
        thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            let connection = rustls::ServerConnection::new(std::sync::Arc::new(config)).unwrap();
            let mut tls = rustls::StreamOwned::new(connection, socket);
            let mut request = [0; 4096];
            let reached = tls.read(&mut request).is_ok_and(|n| n > 0);
            let _ = seen_tx.send(reached);
            if reached {
                let _ = tls.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
                let _ = tls.flush();
            }
        });
        let authority = if ipv6 {
            format!("[{host}]:{}", address.port())
        } else {
            format!("{host}:{}", address.port())
        };
        (format!("https://{authority}/test"), seen_rx)
    }

    fn pem_key(pem: &str) -> Vec<u8> {
        let text = pem
            .split("-----BEGIN PRIVATE KEY-----")
            .nth(1)
            .unwrap()
            .split("-----END PRIVATE KEY-----")
            .next()
            .unwrap();
        base64::engine::general_purpose::STANDARD
            .decode(text.split_whitespace().collect::<String>())
            .unwrap()
    }

    fn test_pin() -> String {
        let cert = super::parse_pem_certs(include_str!("testdata/test-leaf.pem")).unwrap();
        let spki = super::certificate_spki(cert[0].as_ref()).unwrap();
        format!(
            "sha256/{}",
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(spki))
        )
    }

    #[test]
    fn peer_certificate_lua_binding_delivers_untrusted_metadata_asynchronously() {
        let (url, seen) = tls_stub();
        let socket =
            std::env::temp_dir().join(format!("unused-http-peer-image-{}", std::process::id()));
        let image = crate::image::Image::spawn(
            &socket,
            std::sync::Arc::new(remuda_core::Registry::new()),
            std::sync::Arc::new(crate::tick::Counters::default()),
        );
        let code = format!("remuda.http.peer_certificate{{url='{url}', timeout=2, callback=function(r) remuda._peer_done=true; remuda._peer_trusted=r.trusted; remuda._peer_sha=r.sha256; remuda._peer_spki_sha256=r.spki_sha256; remuda._peer_reason=r.reason end}}; return 'started'");
        assert_eq!(image.eval(&code, None).unwrap(), "started");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if image
                .eval("return remuda._peer_done or false", None)
                .unwrap()
                == "true"
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "peer certificate callback did not run"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            image.eval("return remuda._peer_trusted", None).unwrap(),
            "false"
        );
        assert_eq!(image.eval("return #remuda._peer_sha", None).unwrap(), "64");
        assert_eq!(
            image.eval("return remuda._peer_spki_sha256", None).unwrap(),
            test_pin().strip_prefix("sha256/").unwrap()
        );
        let reason = image.eval("return remuda._peer_reason", None).unwrap();
        assert!(!reason.is_empty() && reason != "nil");
        assert!(!seen.recv_timeout(Duration::from_secs(1)).unwrap());
        image.stop_for_test();
    }

    #[test]
    fn peer_certificate_refuses_non_https_urls() {
        assert!(
            super::peer_certificate(&request("http://127.0.0.1/".into()), None)
                .unwrap_err()
                .contains("requires an https URL")
        );
    }

    #[test]
    fn peer_certificate_reports_trust_and_never_sends_http_bytes() {
        let ca_file = concat!(env!("CARGO_MANIFEST_DIR"), "/src/net/testdata/test-ca.pem");
        let (url, seen) = tls_stub();
        let mut req = request(url);
        req.ca_file = Some(ca_file.into());
        let peer = super::peer_certificate(&req, None).unwrap();
        let leaf = super::parse_pem_certs(include_str!("testdata/test-leaf.pem")).unwrap();
        assert!(peer.trusted);
        assert_eq!(
            peer.spki_sha256,
            test_pin().strip_prefix("sha256/").unwrap()
        );
        assert_eq!(
            peer.sha256,
            format!("{:x}", Sha256::digest(leaf[0].as_ref()))
        );
        assert!(!peer.not_before.is_empty());
        assert!(!peer.not_after.is_empty());
        assert!(!seen.recv_timeout(Duration::from_secs(1)).unwrap());

        let (url, seen) = tls_stub();
        let peer = super::peer_certificate(&request(url), None).unwrap();
        assert!(!peer.trusted);
        assert!(peer.reason.is_some());
        assert_eq!(
            peer.spki_sha256,
            test_pin().strip_prefix("sha256/").unwrap()
        );
        assert_eq!(
            peer.sha256,
            format!("{:x}", Sha256::digest(leaf[0].as_ref()))
        );
        assert!(!seen.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn peer_certificate_spki_sha256_is_accepted_as_request_pin() {
        let ca_file = concat!(env!("CARGO_MANIFEST_DIR"), "/src/net/testdata/test-ca.pem");
        let (url, seen) = tls_stub();
        let mut req = request(url);
        req.ca_file = Some(ca_file.into());
        let peer = super::peer_certificate(&req, None).unwrap();
        assert!(!seen.recv_timeout(Duration::from_secs(1)).unwrap());

        let (url, seen) = tls_stub();
        let mut req = request(url);
        req.ca_file = Some(ca_file.into());
        req.pin = Some(format!("sha256/{}", peer.spki_sha256));
        assert_eq!(perform(req, None).unwrap().status, 200);
        assert!(seen.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn invalid_custom_ca_fails_closed() {
        let mut req = request("https://localhost:443/".into());
        req.ca_file = Some("/this/path/does/not/exist/remuda-test-ca.pem".into());
        assert!(perform(req, None)
            .unwrap_err()
            .contains("cannot read custom CA file"));
        let mut req = request("https://localhost:443/".into());
        req.ca_file = Some(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/net/testdata/invalid-ca.pem"
            )
            .into(),
        );
        assert!(perform(req, None)
            .unwrap_err()
            .contains("custom CA file contains no certificates"));
    }

    #[test]
    fn custom_ca_and_spki_pin_pass_and_mismatch_fails_before_http_bytes() {
        let (url, seen) = tls_stub();
        let mut req = request(url);
        req.ca_file =
            Some(concat!(env!("CARGO_MANIFEST_DIR"), "/src/net/testdata/test-ca.pem").into());
        req.pin = Some(test_pin());
        let response = perform(req, None).unwrap();
        assert_eq!(response.status, 200);
        let peer = response
            .peer_certificate
            .expect("validated HTTPS peer metadata");
        assert!(peer.trusted);
        assert_eq!(
            peer.spki_sha256,
            test_pin().strip_prefix("sha256/").unwrap()
        );
        assert_eq!(peer.sha256.len(), 64);
        assert!(!peer.not_before.is_empty());
        assert!(!peer.not_after.is_empty());
        assert!(seen.recv_timeout(Duration::from_secs(1)).unwrap());

        let (url, seen) = tls_stub();
        let mut req = request(url);
        req.ca_file =
            Some(concat!(env!("CARGO_MANIFEST_DIR"), "/src/net/testdata/test-ca.pem").into());
        req.pin = Some(format!(
            "sha256/{}",
            base64::engine::general_purpose::STANDARD.encode([0u8; 32])
        ));
        assert!(perform(req, None)
            .unwrap_err()
            .contains("SPKI pin mismatch"));
        assert!(!seen.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn self_signed_ca_true_certificate_can_be_the_exact_server_certificate() {
        let cert = include_str!("testdata/selfsigned-ca.pem");
        let key = include_str!("testdata/selfsigned-ca-key.pem");
        let ca_file = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/net/testdata/selfsigned-ca.pem"
        );

        let (url, seen) = tls_stub_with(cert, key, false, "localhost");
        let mut req = request(url);
        req.ca_file = Some(ca_file.into());
        assert_eq!(perform(req, None).unwrap().status, 200);
        assert!(seen.recv_timeout(Duration::from_secs(1)).unwrap());

        let (url, seen) = tls_stub_with(cert, key, false, "localhost");
        let mut req = request(url.replace("localhost", "127.0.0.1"));
        req.ca_file = Some(ca_file.into());
        assert!(perform(req, None)
            .unwrap_err()
            .contains("TLS request failed"));
        assert!(!seen.recv_timeout(Duration::from_secs(1)).unwrap());

        let (url, seen) = tls_stub_with(cert, key, false, "localhost");
        let mut req = request(url);
        req.ca_file = Some(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/net/testdata/selfsigned-other.pem"
            )
            .into(),
        );
        assert!(perform(req, None)
            .unwrap_err()
            .contains("TLS request failed"));
        assert!(!seen.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    fn pin_of(cert_pem: &str) -> String {
        let cert = super::parse_pem_certs(cert_pem).unwrap();
        let spki = super::certificate_spki(cert[0].as_ref()).unwrap();
        format!(
            "sha256/{}",
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(spki))
        )
    }

    fn selfsigned_stub(host: &str) -> (String, std::sync::mpsc::Receiver<bool>) {
        tls_stub_with(
            include_str!("testdata/selfsigned-ca.pem"),
            include_str!("testdata/selfsigned-ca-key.pem"),
            false,
            host,
        )
    }

    fn pin_only_request(url: String, pin: Option<String>) -> HttpRequest {
        let mut req = request(url);
        req.pin = pin;
        req.pin_only = true;
        req
    }

    #[test]
    fn pin_only_accepts_self_signed_certificate_with_matching_pin() {
        let (url, seen) = selfsigned_stub("localhost");
        let req = pin_only_request(
            url,
            Some(pin_of(include_str!("testdata/selfsigned-ca.pem"))),
        );
        assert_eq!(perform(req, None).unwrap().status, 200);
        assert!(seen.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn plain_pin_stays_additive_for_self_signed_certificate() {
        let (url, seen) = selfsigned_stub("localhost");
        let mut req = request(url);
        req.pin = Some(pin_of(include_str!("testdata/selfsigned-ca.pem")));
        assert!(perform(req, None)
            .unwrap_err()
            .contains("TLS request failed"));
        assert!(!seen.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn pin_only_with_wrong_pin_fails_before_http_bytes() {
        let (url, seen) = selfsigned_stub("localhost");
        let req = pin_only_request(url, Some(test_pin()));
        assert!(perform(req, None)
            .unwrap_err()
            .contains("SPKI pin mismatch"));
        assert!(!seen.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn pin_only_with_matching_pin_still_checks_hostname() {
        let (url, seen) = selfsigned_stub("localhost");
        let req = pin_only_request(
            url.replace("localhost", "127.0.0.1"),
            Some(pin_of(include_str!("testdata/selfsigned-ca.pem"))),
        );
        let error = perform(req, None).unwrap_err();
        assert!(
            error.contains("TLS request failed: server hostname mismatch"),
            "{error}"
        );
        assert!(!seen.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn pin_only_with_matching_pin_still_checks_validity_dates() {
        let pem = include_str!("testdata/selfsigned-ca.pem");
        let cert = super::parse_pem_certs(pem).unwrap();
        let req = pin_only_request("https://localhost/".into(), Some(pin_of(pem)));
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let verifier = super::tls_verifier(&req, provider).unwrap();
        let name = ServerName::try_from("localhost".to_owned()).unwrap();
        let now = rustls::pki_types::UnixTime::now();
        verifier
            .verify_server_cert(&cert[0], &[], &name, &[], now)
            .expect("pin-only accepts the pinned certificate while it is valid");
        let too_late =
            rustls::pki_types::UnixTime::since_unix_epoch(Duration::from_secs(4_102_444_800));
        assert!(verifier
            .verify_server_cert(&cert[0], &[], &name, &[], too_late)
            .unwrap_err()
            .to_string()
            .contains("expired"));
        let too_early = rustls::pki_types::UnixTime::since_unix_epoch(Duration::from_secs(1));
        assert!(verifier
            .verify_server_cert(&cert[0], &[], &name, &[], too_early)
            .unwrap_err()
            .to_string()
            .contains("not yet valid"));
    }

    #[test]
    fn pin_only_over_plain_http_is_refused_before_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "http://127.0.0.1:{}/test",
            listener.local_addr().unwrap().port()
        );
        let pin = pin_of(include_str!("testdata/selfsigned-ca.pem"));
        let error = perform(pin_only_request(url, Some(pin)), None).unwrap_err();
        assert!(error.contains("TLS options require HTTPS"), "{error}");
        assert!(
            listener.accept().is_err(),
            "pin_only over http must not connect"
        );
    }

    #[test]
    fn pin_only_with_malformed_pin_is_refused_before_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "https://localhost:{}/test",
            listener.local_addr().unwrap().port()
        );
        let digest = pin_of(include_str!("testdata/selfsigned-ca.pem"));
        let base64 = digest.strip_prefix("sha256/").unwrap();
        for pin in [
            base64.to_string(),
            format!("sha1/{base64}"),
            "sha256/not base64!".to_string(),
            format!(
                "sha256/{}",
                base64::engine::general_purpose::STANDARD.encode([0u8; 16])
            ),
        ] {
            let error =
                perform(pin_only_request(url.clone(), Some(pin.clone())), None).unwrap_err();
            assert!(
                error.contains("http.request pin_only requires a valid pin")
                    && error.contains("Next:"),
                "{pin}: {error}"
            );
        }
        assert!(
            listener.accept().is_err(),
            "pin_only with a malformed pin must not connect"
        );
    }

    #[test]
    fn pin_only_ignores_ca_file() {
        let pin = pin_of(include_str!("testdata/selfsigned-ca.pem"));
        let test_ca = concat!(env!("CARGO_MANIFEST_DIR"), "/src/net/testdata/test-ca.pem");
        for ca_file in [test_ca, "/this/path/does/not/exist/remuda-test-ca.pem"] {
            let (url, seen) = selfsigned_stub("localhost");
            let mut req = pin_only_request(url, Some(pin.clone()));
            req.ca_file = Some(ca_file.into());
            assert_eq!(perform(req, None).unwrap().status, 200, "{ca_file}");
            assert!(seen.recv_timeout(Duration::from_secs(1)).unwrap());
        }

        // A CA that does trust the server adds no trust when the pin differs.
        let (url, seen) = tls_stub();
        let mut req = pin_only_request(url, Some(pin));
        req.ca_file = Some(test_ca.into());
        assert!(perform(req, None)
            .unwrap_err()
            .contains("SPKI pin mismatch"));
        assert!(!seen.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn pin_only_without_pin_is_refused_before_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "https://localhost:{}/test",
            listener.local_addr().unwrap().port()
        );
        let error = perform(pin_only_request(url, None), None).unwrap_err();
        assert!(
            error.contains("pin_only") && error.contains("pin"),
            "{error}"
        );
        assert!(
            listener.accept().is_err(),
            "pin_only without pin must not connect"
        );
    }

    #[test]
    fn pin_only_lua_option_requires_boolean_for_both_apis() {
        let socket = std::env::temp_dir().join(format!(
            "unused-http-pin-only-type-image-{}",
            std::process::id()
        ));
        let image = crate::image::Image::spawn(
            &socket,
            std::sync::Arc::new(remuda_core::Registry::new()),
            std::sync::Arc::new(crate::tick::Counters::default()),
        );
        let pin = pin_of(include_str!("testdata/selfsigned-ca.pem"));
        for api in ["request", "peer_certificate"] {
            let name = format!("http.{api}");
            for value in ["'false'", "0"] {
                let code = if api == "request" {
                    format!(
                        "local ok, err = pcall(function() remuda.http.request{{method='GET', url='https://127.0.0.1:1/', timeout=1, pin='{pin}', pin_only={value}, callback=function() end}} end); assert(not ok, 'http.request accepted non-boolean pin_only'); assert(tostring(err):find('{name}', 1, true), tostring(err)); assert(tostring(err):find('must be a boolean', 1, true), tostring(err)); assert(tostring(err):find('Next:', 1, true), tostring(err)); return true"
                    )
                } else {
                    format!(
                        "local ok, err = pcall(function() remuda.http.peer_certificate{{url='https://127.0.0.1:1/', timeout=1, pin='{pin}', pin_only={value}, callback=function() end}} end); assert(not ok, 'http.peer_certificate accepted non-boolean pin_only'); assert(tostring(err):find('{name}', 1, true), tostring(err)); assert(tostring(err):find('must be a boolean', 1, true), tostring(err)); assert(tostring(err):find('Next:', 1, true), tostring(err)); return true"
                    )
                };
                assert_eq!(image.eval(&code, None).unwrap(), "true");
            }
        }
        image.stop_for_test();
    }

    #[test]
    fn pin_only_lua_option_reaches_peer_certificate() {
        let (url, seen) = selfsigned_stub("localhost");
        let socket = std::env::temp_dir().join(format!(
            "unused-http-pin-only-peer-image-{}",
            std::process::id()
        ));
        let image = crate::image::Image::spawn(
            &socket,
            std::sync::Arc::new(remuda_core::Registry::new()),
            std::sync::Arc::new(crate::tick::Counters::default()),
        );
        let pin = pin_of(include_str!("testdata/selfsigned-ca.pem"));
        let code = format!("remuda.http.peer_certificate{{url='{url}', timeout=2, pin='{pin}', pin_only=true, callback=function(r) remuda._pin_only_peer_trusted=r.trusted; remuda._pin_only_peer_reason=r.reason; remuda._pin_only_peer_done=true end}}; return 'started'");
        assert_eq!(image.eval(&code, None).unwrap(), "started");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while image
            .eval("return remuda._pin_only_peer_done or false", None)
            .unwrap()
            != "true"
        {
            assert!(
                std::time::Instant::now() < deadline,
                "peer certificate pin-only callback did not run"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let trusted = image
            .eval("return remuda._pin_only_peer_trusted", None)
            .unwrap();
        let reason = image
            .eval("return remuda._pin_only_peer_reason", None)
            .unwrap();
        image.stop_for_test();
        assert_eq!(
            trusted, "true",
            "matching pin-only peer certificate: {reason}"
        );
        assert!(!seen.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn peer_certificate_pin_only_without_pin_is_refused_before_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "https://localhost:{}/test",
            listener.local_addr().unwrap().port()
        );
        let socket = std::env::temp_dir().join(format!(
            "unused-http-pin-only-missing-peer-{}",
            std::process::id()
        ));
        let image = crate::image::Image::spawn(
            &socket,
            std::sync::Arc::new(remuda_core::Registry::new()),
            std::sync::Arc::new(crate::tick::Counters::default()),
        );
        let code = format!("remuda.http.peer_certificate{{url='{url}', timeout=2, pin_only=true, callback=function() end}}");
        let result = image.eval(&code, None);
        let mut accepted = false;
        let deadline = std::time::Instant::now() + Duration::from_millis(250);
        while std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok(_) => {
                    accepted = true;
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("listener accept failed: {error}"),
            }
        }
        image.stop_for_test();
        assert!(
            result.is_err(),
            "missing peer_certificate pin should be rejected"
        );
        assert!(
            !accepted,
            "missing peer_certificate pin connected to the server"
        );
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("http.peer_certificate") && error.contains("pin"),
            "{error}"
        );
        assert!(error.contains("Next:"), "{error}");
    }

    #[test]
    fn pin_only_lua_option_reaches_the_request() {
        let (url, seen) = selfsigned_stub("localhost");
        let socket =
            std::env::temp_dir().join(format!("unused-http-pin-only-image-{}", std::process::id()));
        let image = crate::image::Image::spawn(
            &socket,
            std::sync::Arc::new(remuda_core::Registry::new()),
            std::sync::Arc::new(crate::tick::Counters::default()),
        );
        let pin = pin_of(include_str!("testdata/selfsigned-ca.pem"));
        let code = format!("remuda.http.request{{method='GET', url='{url}', timeout=2, pin='{pin}', pin_only=true, callback=function(r) remuda._pin_only_status=r.status; remuda._pin_only_error=r.error; remuda._pin_only_done=true end}}; return 'started'");
        assert_eq!(image.eval(&code, None).unwrap(), "started");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while image
            .eval("return remuda._pin_only_done or false", None)
            .unwrap()
            != "true"
        {
            assert!(
                std::time::Instant::now() < deadline,
                "pin_only request callback did not run"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let status = image.eval("return remuda._pin_only_status", None).unwrap();
        let error = image.eval("return remuda._pin_only_error", None).unwrap();
        image.stop_for_test();
        assert_eq!(
            status, "200",
            "pin_only=true must reach the request: {error}"
        );
        assert!(seen.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn exact_ca_certificate_still_obeys_validity_dates() {
        let cert = super::parse_pem_certs(include_str!("testdata/selfsigned-ca.pem")).unwrap();
        let name = ServerName::try_from("localhost".to_owned()).unwrap();
        let too_early = rustls::pki_types::UnixTime::since_unix_epoch(Duration::from_secs(1));
        assert!(super::verify_trusted_end_entity(&cert[0], &name, too_early)
            .unwrap_err()
            .to_string()
            .contains("not yet valid"));
        let too_late =
            rustls::pki_types::UnixTime::since_unix_epoch(Duration::from_secs(4_102_444_800));
        assert!(super::verify_trusted_end_entity(&cert[0], &name, too_late)
            .unwrap_err()
            .to_string()
            .contains("expired"));
    }

    #[test]
    fn malformed_non_ascii_asn1_time_is_rejected_without_panicking() {
        let time = [
            b'1', 0xc3, 0xa9, b'0', b'1', b'0', b'1', b'0', b'0', b'0', b'0', b'0', b'Z',
        ];
        assert_eq!(std::str::from_utf8(&time).unwrap().len(), 13);
        assert_eq!(super::asn1_time(0x17, &time), None);
    }

    #[test]
    fn signed_asn1_time_fields_are_rejected() {
        for time in [
            b"-10101000000Z".as_slice(),
            b"+10101000000Z",
            b"24+10100000Z",
            b"2401+1000000Z",
            b"240101-10000Z",
            b"24010100-100Z",
            b"2401010000-1Z",
        ] {
            assert_eq!(super::asn1_time(0x17, time), None, "{time:?}");
        }
        assert_eq!(super::asn1_time(0x18, b"-0010101000000Z"), None);
    }

    #[test]
    fn https_ipv6_literal_is_verified_as_an_ip_address() {
        let (url, seen) = tls_stub_with(
            include_str!("testdata/selfsigned-ca.pem"),
            include_str!("testdata/selfsigned-ca-key.pem"),
            true,
            "::1",
        );
        let mut req = request(url);
        req.ca_file = Some(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/net/testdata/selfsigned-ca.pem"
            )
            .into(),
        );
        assert_eq!(perform(req, None).unwrap().status, 200);
        assert!(seen.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn cancellation_and_concurrency_limit_complete_asynchronously() {
        use std::sync::mpsc;
        let url = stub(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            Duration::from_millis(400),
        );
        let (tx, rx) = mpsc::channel();
        let client = super::HttpClient::default();
        let task = client.start(request(url), move |_, result| {
            let _ = tx.send(result);
        });
        thread::sleep(Duration::from_millis(40));
        task.cancel();
        assert!(rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err()
            .contains("cancelled"));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for _ in 0..32 {
                let Ok((mut socket, _)) = listener.accept() else {
                    break;
                };
                thread::spawn(move || {
                    let mut request = [0; 2048];
                    let _ = socket.read(&mut request);
                    thread::sleep(Duration::from_millis(200));
                    let _ = socket.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    );
                });
            }
        });
        let (tx, rx) = mpsc::channel();
        for _ in 0..33 {
            let tx = tx.clone();
            client.start(request(format!("http://{address}/")), move |_, result| {
                let _ = tx.send(result);
            });
        }
        drop(tx);
        let outcomes: Vec<_> = (0..33)
            .map(|_| rx.recv_timeout(Duration::from_secs(3)).unwrap())
            .collect();
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| result
                    .as_ref()
                    .is_err_and(|error| error.contains("maximum concurrent")))
                .count(),
            1
        );
        server.join().unwrap();
    }

    #[test]
    fn cancelling_after_completion_keeps_the_completed_result_once() {
        use std::sync::mpsc;
        let url = stub(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            Duration::ZERO,
        );
        let (tx, rx) = mpsc::channel();
        let task = super::HttpClient::default().start(request(url), move |_, result| {
            let _ = tx.send(result);
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap()
                .status,
            200
        );
        task.cancel();
        thread::sleep(Duration::from_millis(60));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn slow_request_does_not_block_the_lua_image() {
        let url = stub(
            b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Duration::from_millis(800),
        );
        let socket = std::env::temp_dir().join(format!("unused-http-image-{}", std::process::id()));
        let image = crate::image::Image::spawn(
            &socket,
            std::sync::Arc::new(remuda_core::Registry::new()),
            std::sync::Arc::new(crate::tick::Counters::default()),
        );
        let code = format!("remuda.http.request{{method='GET', url='{url}', timeout=2, callback=function(r) remuda._http_done=r.status or r.error end}}; return 'started'");
        assert_eq!(image.eval(&code, None).unwrap(), "started");
        let then = std::time::Instant::now();
        assert_eq!(image.eval("return 7", None).unwrap(), "7");
        assert!(then.elapsed() < Duration::from_millis(300));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let status = image.eval("return remuda._http_done or 0", None).unwrap();
            if status == "201" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "async HTTP callback was not delivered; result={status}"
            );
            thread::sleep(Duration::from_millis(20));
        }
        image.stop_for_test();
    }

    #[test]
    fn callback_error_is_logged_and_does_not_stop_the_lua_image() {
        let url = stub(
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Duration::ZERO,
        );
        let socket =
            std::env::temp_dir().join(format!("unused-http-error-image-{}", std::process::id()));
        let image = crate::image::Image::spawn(
            &socket,
            std::sync::Arc::new(remuda_core::Registry::new()),
            std::sync::Arc::new(crate::tick::Counters::default()),
        );
        let code = format!("remuda.http.request{{method='GET', url='{url}', timeout=2, callback=function() remuda._callback_started=true; error('callback exploded') end}}; return 'started'");
        assert_eq!(image.eval(&code, None).unwrap(), "started");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if image
                .eval("return remuda._callback_started or false", None)
                .unwrap()
                == "true"
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "HTTP callback did not run"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(image.eval("return 7", None).unwrap(), "7");
        image.stop_for_test();
    }

    #[test]
    fn enforces_total_timeout_and_response_byte_limit() {
        let slow = stub(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            Duration::from_millis(250),
        );
        let mut req = request(slow);
        req.timeout = Duration::from_millis(30);
        assert!(perform(req, None).unwrap_err().contains("timeout"));
        let large = stub(
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n1234",
            Duration::ZERO,
        );
        let mut req = request(large);
        req.max_bytes = 3;
        assert!(perform(req, None).unwrap_err().contains("max_bytes"));
    }
}
