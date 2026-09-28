//! Bounded HTTP/1.1 client transport. All socket operations live in this module.

use base64::Engine as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

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
}

type ResponseHeaders = BTreeMap<String, Vec<Vec<u8>>>;

pub const MAX_REQUEST_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_HEADER_LINE: usize = 8 * 1024;
const SOCKET_POLL: Duration = Duration::from_millis(50);

#[derive(Clone, Debug)]
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: ResponseHeaders,
    pub body: Vec<u8>,
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
        .host_str()
        .ok_or_else(|| "HTTP URL has no host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "HTTP URL has no port".to_string())?;
    let tls = if url.scheme() == "https" {
        let config = tls_config(&req)?;
        let server_name = ServerName::try_from(host.to_owned())
            .map_err(|_| "invalid TLS server name".to_string())?;
        Some((config, server_name))
    } else {
        None
    };
    let address = if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
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
    let addresses = loop {
        check(deadline, cancelled)?;
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(SOCKET_POLL);
        match resolved_rx.recv_timeout(wait) {
            Ok(Ok(addresses)) => break addresses,
            Ok(Err(_)) => return Err("HTTP DNS resolution failed".into()),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("HTTP DNS resolution failed".into())
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
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
        execute_http(&mut wire, &req, &url)
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
    if let Some(pin) = &req.pin {
        decode_pin(pin)?;
    }
    if (req.ca_file.is_some() || req.pin.is_some()) && url.scheme() != "https" {
        return Err("TLS options require HTTPS".into());
    }
    Ok(())
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn connect(
    addresses: impl Iterator<Item = SocketAddr>,
    deadline: Instant,
    cancelled: Option<&AtomicBool>,
) -> Result<TcpStream, String> {
    let mut last = None;
    for address in addresses {
        check(deadline, cancelled)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let attempt = remaining.min(Duration::from_millis(250));
        match TcpStream::connect_timeout(&address, attempt) {
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
    let host = url.host_str().unwrap_or_default();
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
        format!("HTTP transport error: {}", error)
    }
}
fn tls_error(error: TlsError) -> String {
    format!("TLS request failed: {error}")
}

fn tls_config(req: &HttpRequest) -> Result<Arc<ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier: Arc<dyn ServerCertVerifier> = if let Some(path) = &req.ca_file {
        let pem = read_ca_file(path)?;
        let certs = parse_pem_certs(&pem)?;
        if certs.is_empty() {
            return Err("custom CA file contains no certificates".into());
        }
        let mut roots = rustls::RootCertStore::empty();
        for cert in certs {
            roots.add(cert).map_err(tls_error)?;
        }
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .map_err(|error| error.to_string())?
    } else {
        Arc::new(rustls_platform_verifier::Verifier::new(provider.clone()).map_err(tls_error)?)
    };
    let verifier: Arc<dyn ServerCertVerifier> = match &req.pin {
        Some(pin) => Arc::new(PinnedVerifier {
            inner: verifier,
            pin: decode_pin(pin)?,
        }),
        None => verifier,
    };
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(tls_error)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth()
        .pipe(Arc::new)
        .pipe(Ok)
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
        self.inner
            .verify_server_cert(cert, intermediates, name, ocsp, now)?;
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

pub fn install(
    lua: &mlua::Lua,
    remuda: &mlua::Table,
    image: crate::image::Image,
) -> mlua::Result<()> {
    let http = lua.create_table()?;
    let request_image = image;
    http.set(
        "request",
        lua.create_function(move |lua, options: mlua::Table| {
            let method: String = options.get("method")?;
            let url: String = options.get("url")?;
            let timeout = duration_option(&options, "timeout", None)?
                .ok_or_else(|| mlua::Error::runtime("http.request requires timeout"))?;
            let connect_timeout = duration_option(
                &options,
                "connect_timeout",
                Some(timeout.min(Duration::from_secs(10))),
            )?
            .unwrap();
            let max_bytes = options
                .get::<Option<usize>>("max_bytes")?
                .unwrap_or(1024 * 1024);
            let body = options
                .get::<Option<mlua::LuaString>>("body")?
                .map(|value| value.as_bytes().to_vec())
                .unwrap_or_default();
            let ca_file = options.get::<Option<String>>("ca_file")?;
            let pin = options.get::<Option<String>>("pin")?;
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
) -> mlua::Result<Option<Duration>> {
    let seconds = options.get::<Option<f64>>(name)?;
    let Some(seconds) = seconds else {
        return Ok(default);
    };
    if !seconds.is_finite() || seconds <= 0.0 || seconds > 3600.0 {
        return Err(mlua::Error::runtime(format!(
            "http.request {name} must be between 0 and 3600 seconds"
        )));
    }
    Ok(Some(Duration::from_secs_f64(seconds)))
}

#[cfg(test)]
mod tests {
    use super::{perform, HttpRequest};
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

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
        }
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
        use rustls::pki_types::PrivateKeyDer;
        use std::sync::mpsc;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let certs = super::parse_pem_certs(include_str!("testdata/test-leaf.pem")).unwrap();
        let key = pem_key(include_str!("testdata/test-leaf-key.pem"));
        let config = rustls::ServerConfig::builder()
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
        (
            format!("https://localhost:{}/test", address.port()),
            seen_rx,
        )
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
