//! `remuda vfs serve`: a read-only HTTP bridge from a browser to the cluster
//! file store (#742). One page, one prefix, loopback by default.
//!
//! The server runs in the CLI process, not the daemon: it blocks for as long as
//! a viewer is open, and the one-writer rule says only the daemon serving the
//! socket touches the index. Every request becomes one `Request::Vfs` on a
//! fresh connection (`read --json`, `ls --json`), so this file never opens
//! `VfsStore` and never holds the cluster lock.
//!
//! Design: `docs/design-vfs-serve.md`. The defaults treat stored HTML as
//! untrusted: no script, an opaque origin, and one path prefix.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use remuda_core::protocol::{Request, Response as DaemonResponse};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Largest head we read before giving up: one request line and its headers.
pub const MAX_HEAD_BYTES: usize = 8 * 1024;
/// Most header lines in one head.
pub const MAX_HEADERS: usize = 64;
/// Longest request target we accept.
pub const MAX_TARGET_BYTES: usize = 2048;
/// Largest file the bridge puts in memory. A stored file may be 64 MiB; a page
/// that big is not a page, and eight concurrent reads of that size is more
/// memory than a viewer node should lend out.
pub const MAX_BRIDGE_BYTES: u64 = 32 * 1024 * 1024;
/// Requests served at once. A browser opens about six connections per page.
pub const MAX_CONNECTIONS: usize = 8;
/// Read and write timeout for one connection.
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// What a store failure means to the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    NotFound,
    TooLarge,
    NoCluster,
    Unreachable,
    Corrupt,
    WrongCluster,
    BadUri,
    Io,
}

impl Kind {
    fn from_code(code: &str) -> Self {
        match code {
            "not_found" => Kind::NotFound,
            "too_large" => Kind::TooLarge,
            "no_cluster" => Kind::NoCluster,
            "unreachable" | "not_local" => Kind::Unreachable,
            "corrupt" => Kind::Corrupt,
            "wrong_cluster" => Kind::WrongCluster,
            "bad_uri" => Kind::BadUri,
            _ => Kind::Io,
        }
    }

    fn status(self) -> (u16, &'static str) {
        match self {
            Kind::NotFound => (404, "Not Found"),
            Kind::TooLarge => (413, "Content Too Large"),
            Kind::WrongCluster | Kind::BadUri => (400, "Bad Request"),
            Kind::NoCluster | Kind::Unreachable => (503, "Service Unavailable"),
            Kind::Corrupt => (502, "Bad Gateway"),
            Kind::Io => (500, "Internal Server Error"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StoreError {
    pub kind: Kind,
    pub detail: String,
}

impl StoreError {
    pub fn new(kind: Kind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

/// One file's metadata, as the index knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub path: String,
    pub version: u64,
    pub sha256: String,
    pub size: u64,
}

/// A stored file's bytes and the digest the store verified them against.
#[derive(Debug, Clone)]
pub struct Content {
    pub meta: Meta,
    pub bytes: Vec<u8>,
}

/// The bridge's whole view of the store. `read` answers `NotFound` rather than
/// an error when no file is at the path, because a missing file may still be a
/// directory.
pub trait Source {
    fn read(&self, path: &str, max_bytes: u64) -> Result<Result<Content, Kind>, StoreError>;
    fn list(&self, prefix: &str) -> Result<Vec<Meta>, StoreError>;
}

/// The daemon-backed source: every call is one `Request::Vfs` on its own
/// connection, so a daemon reload is retried rather than remembered.
pub struct DaemonSource {
    socket: PathBuf,
}

impl DaemonSource {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    fn command(&self, args: &[String]) -> Result<Vec<u8>, StoreError> {
        let request = Request::Vfs {
            args: args.to_vec(),
            input_base64: None,
        };
        let reply = crate::client::request(&self.socket, &request).map_err(|error| {
            StoreError::new(Kind::Io, format!("daemon request failed: {error}"))
        })?;
        match reply {
            DaemonResponse::CommandResult {
                exit_code,
                stdout_base64,
                stderr_base64,
            } => {
                let stdout = BASE64.decode(stdout_base64).unwrap_or_default();
                if exit_code == 0 {
                    Ok(stdout)
                } else {
                    let stderr = BASE64.decode(stderr_base64).unwrap_or_default();
                    Err(StoreError::new(
                        Kind::Io,
                        String::from_utf8_lossy(&stderr).trim().to_owned(),
                    ))
                }
            }
            DaemonResponse::Error(error) => Err(StoreError::new(Kind::Io, error)),
            other => Err(StoreError::new(
                Kind::Io,
                format!("unexpected daemon answer: {other:?}"),
            )),
        }
    }

    /// Refuse to start against a daemon that does not have `vfs read`: every
    /// request would answer 500 with a confusing message. One probe at startup
    /// turns that into one clear line.
    /// `--max-bytes 0` means an existing file answers `too_large` from the
    /// index, so the probe never moves a blob, whatever the name happens to be.
    pub fn probe(&self) -> Result<(), String> {
        let args = [
            "read".to_owned(),
            "remuda:///__remuda_serve_probe__".to_owned(),
            "--max-bytes".to_owned(),
            "0".to_owned(),
        ];
        match self.envelope(&args) {
            Ok(_) => Ok(()),
            // The daemon's refusal may carry its usage text; one line is enough.
            Err(error) => Err(error
                .detail
                .lines()
                .next()
                .unwrap_or("no answer")
                .to_owned()),
        }
    }

    /// `read --json` always answers exit 0 with an envelope, so a store failure
    /// arrives as a stable code instead of stderr text.
    fn envelope(&self, args: &[String]) -> Result<Value, StoreError> {
        let stdout = self.command(args)?;
        serde_json::from_slice(&stdout)
            .map_err(|error| StoreError::new(Kind::Io, format!("unreadable vfs answer: {error}")))
    }
}

impl Source for DaemonSource {
    fn read(&self, path: &str, max_bytes: u64) -> Result<Result<Content, Kind>, StoreError> {
        let args = vec![
            "read".to_owned(),
            format!("remuda:///{path}"),
            "--max-bytes".to_owned(),
            max_bytes.to_string(),
        ];
        let value = self.envelope(&args)?;
        if !value["ok"].as_bool().unwrap_or(false) {
            return Ok(Err(Kind::from_code(value["code"].as_str().unwrap_or(""))));
        }
        let meta = meta_from(&value);
        let bytes = value["content"]
            .as_str()
            .and_then(|text| BASE64.decode(text).ok())
            .ok_or_else(|| StoreError::new(Kind::Corrupt, "vfs read carried no content"))?;
        Ok(Ok(Content { meta, bytes }))
    }

    fn list(&self, prefix: &str) -> Result<Vec<Meta>, StoreError> {
        let args = vec![
            "ls".to_owned(),
            format!("remuda:///{prefix}"),
            "--json".to_owned(),
        ];
        let stdout = self.command(&args)?;
        let rows: Value = serde_json::from_slice(&stdout).map_err(|error| {
            StoreError::new(Kind::Io, format!("unreadable vfs listing: {error}"))
        })?;
        let rows = rows
            .as_array()
            .ok_or_else(|| StoreError::new(Kind::Io, "vfs listing was not an array"))?;
        Ok(rows.iter().map(meta_from).collect())
    }
}

fn meta_from(value: &Value) -> Meta {
    Meta {
        path: value["path"].as_str().unwrap_or_default().to_owned(),
        version: value["version"].as_u64().unwrap_or(0),
        sha256: value["sha256"].as_str().unwrap_or_default().to_owned(),
        size: value["size"].as_u64().unwrap_or(0),
    }
}

/// How one `serve` run is configured. `root` is a cluster path with no leading
/// or trailing slash; empty means the whole store.
#[derive(Debug, Clone)]
pub struct Config {
    pub root: String,
    pub allow_scripts: bool,
}

impl Config {
    pub fn new(root: &str, allow_scripts: bool) -> Result<Self, String> {
        let root = root.trim_matches('/');
        if !root.is_empty() {
            remuda_core::vfs::validate_path(root)
                .map_err(|error| format!("--root {root:?} is not a cluster path: {error}"))?;
        }
        Ok(Self {
            root: root.to_owned(),
            allow_scripts,
        })
    }

    /// A cluster path as the browser sees it: the configured root removed on a
    /// component boundary, so a request for `/` under `--root design` still
    /// links to `/index.html`, not to `/design/index.html`.
    fn http_path(&self, cluster_path: &str) -> String {
        if self.root.is_empty() {
            return cluster_path.to_owned();
        }
        match cluster_path.strip_prefix(&self.root) {
            Some(rest) => rest.trim_start_matches('/').to_owned(),
            None => cluster_path.to_owned(),
        }
    }

    /// The listing prefix for a request path. A root always ends in `/` so a
    /// root named `design` can never list `design-private`.
    fn prefix_for(&self, request_path: &str) -> String {
        if request_path.is_empty() {
            if self.root.is_empty() {
                String::new()
            } else {
                format!("{}/", self.root)
            }
        } else {
            format!("{request_path}/")
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
}

/// A parsed request head. Everything is owned, so a worker thread can keep it
/// after the read buffer is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub method: Method,
    /// Origin-form only: `/path?query`.
    pub target: String,
    pub host: Option<String>,
    pub if_none_match: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadRequest {
    pub status: u16,
    pub reason: &'static str,
    pub detail: String,
}

impl BadRequest {
    fn new(status: u16, reason: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            detail: detail.into(),
        }
    }
}

fn is_token_byte(byte: u8) -> bool {
    matches!(
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
    ) || byte.is_ascii_alphanumeric()
}

/// Parse one request head. Strict on purpose: an unusual head is a bug or an
/// attack, and one request per connection means we never have to survive a
/// second one.
pub fn parse_head(bytes: &[u8]) -> Result<Head, BadRequest> {
    if bytes.len() > MAX_HEAD_BYTES {
        return Err(too_large("head too large"));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| BadRequest::new(400, "Bad Request", "head is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| BadRequest::new(400, "Bad Request", "empty request"))?;
    let (method, target, version) = parse_request_line(request_line)?;
    let headers = parse_headers(lines)?;
    if version == "HTTP/1.1" && headers.host.is_none() {
        return Err(BadRequest::new(
            400,
            "Bad Request",
            "HTTP/1.1 needs a Host header",
        ));
    }
    if headers.has_body() {
        return Err(BadRequest::new(
            400,
            "Bad Request",
            "a request body is never read here",
        ));
    }
    Ok(Head {
        method,
        target: target.to_owned(),
        host: headers.host,
        if_none_match: headers.if_none_match,
    })
}

fn too_large(detail: &str) -> BadRequest {
    BadRequest::new(431, "Request Header Fields Too Large", detail)
}

/// `METHOD SP /path SP HTTP/1.x`, and nothing else.
fn parse_request_line(line: &str) -> Result<(Method, &str, &str), BadRequest> {
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(BadRequest::new(
            400,
            "Bad Request",
            "malformed request line",
        ));
    };
    let method = match method {
        "GET" => Method::Get,
        "HEAD" => Method::Head,
        _ => {
            return Err(BadRequest::new(
                405,
                "Method Not Allowed",
                "only GET and HEAD are served",
            ))
        }
    };
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(BadRequest::new(
            505,
            "HTTP Version Not Supported",
            "use HTTP/1.1",
        ));
    }
    if !target.starts_with('/') {
        // Absolute-form proxies, authority-form and `*` are never ours.
        return Err(BadRequest::new(400, "Bad Request", "target must be a path"));
    }
    if target.len() > MAX_TARGET_BYTES {
        return Err(BadRequest::new(414, "URI Too Long", "target too long"));
    }
    if target
        .bytes()
        .any(|byte| byte < 0x20 || byte == 0x7f || byte == b'#')
    {
        return Err(BadRequest::new(
            400,
            "Bad Request",
            "bad character in target",
        ));
    }
    Ok((method, target, version))
}

/// The three headers the bridge acts on, and the two it refuses.
#[derive(Default)]
struct RawHeaders {
    host: Option<String>,
    if_none_match: Option<String>,
    content_length: Option<u64>,
    chunked: bool,
}

impl RawHeaders {
    fn has_body(&self) -> bool {
        // `Some(0)` is no body; anything else, and any chunked body, is refused.
        self.content_length.is_some_and(|length| length != 0) || self.chunked
    }
}

fn parse_headers<'a>(lines: impl Iterator<Item = &'a str>) -> Result<RawHeaders, BadRequest> {
    let mut raw = RawHeaders::default();
    let mut count = 0;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        count += 1;
        if count > MAX_HEADERS {
            return Err(too_large("too many headers"));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| BadRequest::new(400, "Bad Request", "header without a colon"))?;
        if name.is_empty() || !name.bytes().all(is_token_byte) {
            return Err(BadRequest::new(400, "Bad Request", "bad header name"));
        }
        let value = value.trim_matches(' ');
        if value
            .bytes()
            .any(|byte| (byte < 0x20 && byte != b'\t') || byte == 0x7f)
        {
            return Err(BadRequest::new(400, "Bad Request", "bad header value"));
        }
        match name.to_ascii_lowercase().as_str() {
            "host" => {
                if raw.host.is_some() {
                    return Err(BadRequest::new(400, "Bad Request", "two Host headers"));
                }
                if value.is_empty() {
                    return Err(BadRequest::new(400, "Bad Request", "empty Host"));
                }
                raw.host = Some(value.to_owned());
            }
            "if-none-match" => {
                if raw.if_none_match.is_some() {
                    return Err(BadRequest::new(
                        400,
                        "Bad Request",
                        "two If-None-Match headers",
                    ));
                }
                raw.if_none_match = Some(value.to_owned());
            }
            "content-length" => {
                if raw.content_length.is_some() {
                    return Err(BadRequest::new(
                        400,
                        "Bad Request",
                        "two Content-Length headers",
                    ));
                }
                // HTTP's grammar is digits only: `+0` and ` 0` are not lengths.
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(BadRequest::new(400, "Bad Request", "bad Content-Length"));
                }
                raw.content_length = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| BadRequest::new(400, "Bad Request", "bad Content-Length"))?,
                );
            }
            "transfer-encoding" => raw.chunked = true,
            _ => {}
        }
    }
    Ok(raw)
}

/// One request path, decoded and checked against the cluster path rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Cluster path with the root applied. Stored paths carry no leading
    /// slash, so neither does this one.
    pub path: String,
    pub trailing_slash: bool,
    pub query: String,
}

pub fn resolve_target(target: &str, root: &str) -> Result<Target, BadRequest> {
    let (path_part, query) = target.split_once('?').unwrap_or((target, ""));
    if query.len() > MAX_TARGET_BYTES {
        return Err(BadRequest::new(414, "URI Too Long", "query too long"));
    }
    let decoded = percent_decode(path_part)?;
    let trailing_slash = decoded.ends_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in decoded.split('/') {
        if segment.is_empty() {
            continue;
        }
        if segment == "." || segment == ".." {
            return Err(BadRequest::new(404, "Not Found", "no such path"));
        }
        if segment.contains('\\') || segment.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
            return Err(BadRequest::new(400, "Bad Request", "bad character in path"));
        }
        segments.push(segment);
    }
    let request = segments.join("/");
    let root = root.trim_matches('/');
    let joined = if request.is_empty() {
        root.to_owned()
    } else if root.is_empty() {
        request
    } else {
        format!("{root}/{request}")
    };
    // The same rules `remuda vfs put` applies: no empty, `.` or `..` component,
    // no `\`, NUL or control character, at most 1024 bytes. Segments above were
    // already split, so this is the second, authoritative check. An empty path
    // is the whole store, which has no path of its own.
    if !joined.is_empty() {
        remuda_core::vfs::validate_path(&joined)
            .map_err(|_| BadRequest::new(404, "Not Found", "no such path"))?;
    }
    Ok(Target {
        path: joined,
        trailing_slash,
        query: query.to_owned(),
    })
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Decode exactly one layer of percent escapes. A malformed escape, a decoded
/// NUL or a byte that breaks UTF-8 is refused, never passed on.
fn percent_decode(text: &str) -> Result<String, BadRequest> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let (Some(high), Some(low)) = (
                bytes.get(index + 1).copied().and_then(hex_value),
                bytes.get(index + 2).copied().and_then(hex_value),
            ) else {
                return Err(BadRequest::new(400, "Bad Request", "bad percent escape"));
            };
            let byte = high * 16 + low;
            if byte == 0 {
                return Err(BadRequest::new(400, "Bad Request", "NUL in path"));
            }
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| BadRequest::new(400, "Bad Request", "path is not UTF-8"))
}

pub fn content_type(path: &str) -> &'static str {
    let extension = path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("txt" | "md" | "rst" | "csv" | "log") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml; charset=utf-8",
        Some("svg") => "image/svg+xml; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("pdf") => "application/pdf",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("wasm") => "application/wasm",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        _ => "application/octet-stream",
    }
}

fn is_active(content_type: &str) -> bool {
    content_type.starts_with("text/html") || content_type.starts_with("image/svg+xml")
}

fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Percent-encode one path for an href: everything but unreserved characters
/// and `/` is escaped, so a name containing `?`, `#`, `%` or a space survives
/// the round trip.
fn href_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn bytes_text(size: u64) -> String {
    match size {
        size if size < 1024 => format!("{size} B"),
        size if size < 1024 * 1024 => format!("{:.1} KiB", size as f64 / 1024.0),
        size => format!("{:.1} MiB", size as f64 / (1024.0 * 1024.0)),
    }
}

pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    fn new(status: u16, reason: &'static str, content_type: &str) -> Self {
        Self {
            status,
            reason,
            headers: vec![("Content-Type".to_owned(), content_type.to_owned())],
            body: Vec::new(),
        }
    }

    fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    fn text(status: u16, reason: &'static str, message: &str) -> Self {
        let body = format!(
            "<!doctype html>\n<meta charset=\"utf-8\">\n<title>{}</title>\n<p>{}</p>\n",
            html_escape(reason),
            html_escape(message)
        );
        Response::new(status, reason, "text/html; charset=utf-8").with_body(body.into_bytes())
    }

    /// The wire bytes. `HEAD` sends the same head and no body; a 304 has no
    /// body and no `Content-Length`, which RFC 9110 asks for.
    pub fn to_bytes(&self, method: Method) -> Vec<u8> {
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, self.reason);
        head.push_str("Connection: close\r\n");
        head.push_str("Server: remuda-vfs-serve\r\n");
        if self.status != 304 {
            head.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        }
        for (name, value) in &self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        if method == Method::Get && self.status != 304 {
            bytes.extend_from_slice(&self.body);
        }
        bytes
    }
}

/// The security headers every answer carries. An HTML or SVG answer also gets a
/// policy built for the exact origin this viewer listens on.
/// The sources a page may load from: the bound origin, plus the `localhost`
/// name the browser may have used instead. Without both, a page opened at
/// `localhost:PORT` would have its own stylesheet blocked by the policy.
fn sources(origin: &str) -> String {
    let authority = origin.trim_start_matches("http://");
    match authority.rsplit_once(':') {
        Some((host, port)) if host == "127.0.0.1" || host == "[::1]" || host == "::1" => {
            format!("{origin} http://localhost:{port}")
        }
        _ => origin.to_owned(),
    }
}

fn harden(response: Response, origin: &str, allow_scripts: bool) -> Response {
    let active = response
        .headers
        .iter()
        .any(|(name, value)| name == "Content-Type" && is_active(value));
    let response = response
        .with_header("X-Content-Type-Options", "nosniff")
        .with_header("X-Frame-Options", "DENY")
        .with_header("Referrer-Policy", "no-referrer")
        // An opaque-origin page still loads its own stylesheet and images;
        // `same-origin` here would block them.
        .with_header("Cross-Origin-Resource-Policy", "cross-origin")
        .with_header("Cross-Origin-Opener-Policy", "same-origin")
        .with_header("Cache-Control", "no-cache");
    if !active {
        return response;
    }
    let sources = sources(origin);
    let policy = if allow_scripts {
        format!(
            "default-src {sources}; img-src {sources} data:; style-src {sources} 'unsafe-inline'; \
             font-src {sources}; script-src {sources}; connect-src 'none'; object-src 'none'; \
             base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
        )
    } else {
        format!(
            "sandbox; default-src {sources}; img-src {sources} data:; \
             style-src {sources} 'unsafe-inline'; font-src {sources}; connect-src 'none'; \
             object-src 'none'; base-uri 'none'; form-action 'none'; \
             frame-ancestors 'none'; script-src 'none'"
        )
    };
    response.with_header("Content-Security-Policy", policy)
}

fn etag_matches(header: &str, etag: &str) -> bool {
    header.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate == etag || candidate.strip_prefix("W/") == Some(etag)
    })
}

fn store_failure(kind: Kind) -> Response {
    let (status, reason) = kind.status();
    Response::text(status, reason, reason)
}

/// Answer one request. Pure except for calls into `Source`, so a test can drive
/// every branch with a fake store. The answer is always hardened.
pub fn handle<S: Source + ?Sized>(
    source: &S,
    config: &Config,
    origin: &str,
    head: &Head,
) -> Response {
    let response = route(source, config, head);
    harden(response, origin, config.allow_scripts)
}

fn route<S: Source + ?Sized>(source: &S, config: &Config, head: &Head) -> Response {
    let target = match resolve_target(&head.target, &config.root) {
        Ok(target) => target,
        Err(bad) => return Response::text(bad.status, bad.reason, &bad.detail),
    };
    if !target.trailing_slash && !target.path.is_empty() {
        // A file wins over a prefix of the same name; only a bare directory
        // redirects, and the redirect never carries the query through.
        match source.read(&target.path, MAX_BRIDGE_BYTES) {
            Ok(Ok(content)) => return serve_content(content, head),
            Ok(Err(Kind::NotFound)) => {}
            Ok(Err(kind)) => return store_failure(kind),
            Err(error) => return store_failure(error.kind),
        }
        let prefix = config.prefix_for(&target.path);
        match source.list(&prefix) {
            Ok(entries) if entries.is_empty() => {
                return Response::text(404, "Not Found", "no file at that path")
            }
            Ok(_) => {
                let relative = config.http_path(&target.path);
                return Response::new(301, "Moved Permanently", "text/html; charset=utf-8")
                    .with_header("Location", format!("/{}/", href_path(&relative)));
            }
            Err(error) => return store_failure(error.kind),
        }
    }
    let prefix = config.prefix_for(&target.path);
    let relative = config.http_path(&target.path);
    match source.list(&prefix) {
        Ok(entries) => Response::new(200, "OK", "text/html; charset=utf-8")
            .with_body(render_listing(&relative, config, &prefix, &entries).into_bytes()),
        Err(error) => store_failure(error.kind),
    }
}

fn serve_content(content: Content, head: &Head) -> Response {
    // The store checked the index size before reading; this is the size of what
    // actually arrived, so a concurrent overwrite cannot slip past the cap.
    if content.bytes.len() as u64 > MAX_BRIDGE_BYTES {
        return store_failure(Kind::TooLarge);
    }
    let etag = format!("\"{}\"", content.meta.sha256);
    if let Some(condition) = &head.if_none_match {
        if etag_matches(condition, &etag) {
            return Response::new(304, "Not Modified", content_type(&content.meta.path))
                .with_header("ETag", etag);
        }
    }
    let version = content.meta.version.to_string();
    let path = content.meta.path.clone();
    Response::new(200, "OK", content_type(&content.meta.path))
        .with_header("ETag", etag)
        .with_header("X-Remuda-Path", path)
        .with_header("X-Remuda-Version", version)
        .with_body(content.bytes)
}

/// Render one directory: subdirectories first, then files, each with a link.
fn render_listing(request_path: &str, config: &Config, prefix: &str, entries: &[Meta]) -> String {
    let mut directories: Vec<String> = Vec::new();
    let mut files: Vec<&Meta> = Vec::new();
    for entry in entries {
        let Some(rest) = entry.path.strip_prefix(prefix) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        match rest.split_once('/') {
            Some((directory, _)) => {
                if !directories.iter().any(|known| known == directory) {
                    directories.push(directory.to_owned());
                }
            }
            None => files.push(entry),
        }
    }
    directories.sort();
    files.sort_by(|left, right| left.path.cmp(&right.path));

    let base = if request_path.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}/", href_path(request_path))
    };
    let title = if request_path.is_empty() {
        format!("remuda vfs /{}", config.root)
    } else {
        format!("remuda vfs /{request_path}")
    };
    let mut body = String::new();
    body.push_str("<!doctype html>\n<meta charset=\"utf-8\">\n");
    body.push_str(&format!("<title>{}</title>\n", html_escape(&title)));
    body.push_str("<style>body{font:14px/1.5 system-ui,sans-serif;max-width:60rem;margin:2rem auto;padding:0 1rem}td{padding:.15rem .8rem .15rem 0}a{text-decoration:none}code{opacity:.6}</style>\n");
    body.push_str(&format!("<h1>{}</h1>\n", html_escape(&title)));
    body.push_str(&format!(
        "<p><code>{}</code> &middot; {} {}</p>\n",
        html_escape(prefix),
        entries.len(),
        if entries.len() == 1 {
            "file"
        } else {
            "entries"
        }
    ));
    if !request_path.is_empty() {
        body.push_str(&format!(
            "<p><a href=\"{}\">..</a></p>\n",
            parent_href(request_path)
        ));
    }
    body.push_str("<table>\n");
    for directory in &directories {
        body.push_str(&format!(
            "<tr><td><a href=\"{}{}/\">{}/</a></td><td></td></tr>\n",
            base,
            href_path(directory),
            html_escape(directory)
        ));
    }
    for file in &files {
        let Some(name) = file.path.strip_prefix(prefix) else {
            continue;
        };
        body.push_str(&format!(
            "<tr><td><a href=\"{}{}\">{}</a></td><td><code>{}</code></td><td><code>v{}</code></td></tr>\n",
            base,
            href_path(name),
            html_escape(name),
            bytes_text(file.size),
            file.version
        ));
    }
    body.push_str("</table>\n");
    body
}

fn parent_href(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => format!("/{}/", href_path(parent)),
        _ => "/".to_owned(),
    }
}

/// Serve until `stop` is set or the process ends. `origin` is the
/// `http://host:port` the CSP names; it comes from the bound address, never
/// from a request.
pub fn serve(
    listener: TcpListener,
    config: Arc<Config>,
    source: Arc<dyn Source + Send + Sync>,
    origin: String,
    stop: Arc<AtomicBool>,
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    let live = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if live.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                    live.fetch_sub(1, Ordering::SeqCst);
                    let mut stream = stream;
                    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                    let response = harden(
                        Response::text(503, "Service Unavailable", "too many requests"),
                        &origin,
                        false,
                    );
                    let _ = stream.write_all(&response.to_bytes(Method::Get));
                    continue;
                }
                let config = Arc::clone(&config);
                let source = Arc::clone(&source);
                let origin = origin.clone();
                let live = Arc::clone(&live);
                std::thread::spawn(move || {
                    let _ = serve_one(stream, &*source, &config, &origin);
                    live.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn serve_one(
    mut stream: TcpStream,
    source: &dyn Source,
    config: &Config,
    origin: &str,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let (method, response) = match read_head(&mut stream) {
        Err(refused) => (
            refused.method,
            harden(
                Response::text(refused.bad.status, refused.bad.reason, &refused.bad.detail),
                origin,
                false,
            ),
        ),
        Ok(head) => {
            // Defeat DNS rebinding: the browser must have reached us by the
            // name we listen on, not by an attacker's name that resolves here.
            let refused = head
                .host
                .as_deref()
                .is_some_and(|host| !host_allowed(host, origin));
            if refused {
                (
                    head.method,
                    harden(
                        Response::text(
                            421,
                            "Misdirected Request",
                            "this bridge answers its own address only",
                        ),
                        origin,
                        false,
                    ),
                )
            } else {
                (head.method, handle(source, config, origin, &head))
            }
        }
    };
    stream.write_all(&response.to_bytes(method))?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

fn host_allowed(host: &str, origin: &str) -> bool {
    let authority = origin.trim_start_matches("http://");
    if host.eq_ignore_ascii_case(authority) {
        return true;
    }
    // A loopback listener is also reached as `localhost:PORT`.
    let Some((_, port)) = authority.rsplit_once(':') else {
        return false;
    };
    let loopback = authority.starts_with("127.0.0.1:") || authority.starts_with("[::1]:");
    loopback && host.eq_ignore_ascii_case(&format!("localhost:{port}"))
}

/// A refused head, and the method it claimed, so a `HEAD` error still answers
/// without a body.
struct Refused {
    method: Method,
    bad: BadRequest,
}

/// `HEAD` is visible before the rest of the line parses; trust it only for
/// suppressing a body, never for choosing what to run.
fn claimed_method(buffer: &[u8]) -> Method {
    if buffer.starts_with(b"HEAD ") {
        Method::Head
    } else {
        Method::Get
    }
}

/// Read up to the blank line. One request per connection, so anything after it
/// is ignored and the connection closes.
fn read_head(stream: &mut TcpStream) -> Result<Head, Refused> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    loop {
        let read = stream.read(&mut chunk).map_err(|_| Refused {
            method: claimed_method(&buffer),
            bad: BadRequest::new(408, "Request Timeout", "request head not read"),
        })?;
        if read == 0 {
            return Err(Refused {
                method: claimed_method(&buffer),
                bad: BadRequest::new(400, "Bad Request", "connection closed before a request"),
            });
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_HEAD_BYTES {
            return Err(Refused {
                method: claimed_method(&buffer),
                bad: too_large("head too large"),
            });
        }
        if let Some(end) = find_head_end(&buffer) {
            return parse_head(&buffer[..end]).map_err(|bad| Refused {
                method: claimed_method(&buffer),
                bad,
            });
        }
    }
}

fn find_head_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

/// The URL a viewer should open, from the address actually bound.
pub fn origin_of(addr: SocketAddr) -> String {
    format!("http://{addr}")
}

/// `remuda vfs serve` from the CLI. Returns the process exit code.
pub fn run(
    socket: &Path,
    addr: &str,
    root: &str,
    allow_scripts: bool,
    allow_remote: bool,
) -> std::process::ExitCode {
    use std::process::ExitCode;
    let config = match Config::new(root, allow_scripts) {
        Ok(config) => Arc::new(config),
        Err(error) => {
            eprintln!("remuda: vfs serve: {error}");
            return ExitCode::FAILURE;
        }
    };
    let requested: SocketAddr = match addr.parse() {
        Ok(addr) => addr,
        Err(error) => {
            eprintln!("remuda: vfs serve: --addr {addr:?} is not ADDRESS:PORT ({error})");
            return ExitCode::FAILURE;
        }
    };
    if !requested.ip().is_loopback() && !allow_remote {
        eprintln!(
            "remuda: vfs serve: {requested} is not a loopback address; add --allow-remote if you really mean to publish cluster files to the network"
        );
        return ExitCode::FAILURE;
    }
    let source = DaemonSource::new(socket.to_path_buf());
    if let Err(detail) = source.probe() {
        eprintln!("remuda: vfs serve: the daemon did not answer `remuda vfs read`: {detail}");
        eprintln!(
            "Next: run `remuda reload` so the daemon is this build, or check that it is running."
        );
        return ExitCode::FAILURE;
    }
    let listener = match TcpListener::bind(requested) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("remuda: vfs serve: cannot listen on {requested}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let bound = match listener.local_addr() {
        Ok(bound) => bound,
        Err(error) => {
            eprintln!("remuda: vfs serve: cannot read the bound address: {error}");
            return ExitCode::FAILURE;
        }
    };
    let origin = origin_of(bound);
    let root_text = if config.root.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", config.root)
    };
    println!(
        "Serving remuda://{root_text} at {origin}/ (read-only; scripts {})",
        if allow_scripts { "allowed" } else { "blocked" }
    );
    println!("Press Ctrl-C to stop.");
    let source: Arc<dyn Source + Send + Sync> = Arc::new(source);
    let stop = Arc::new(AtomicBool::new(false));
    match serve(listener, config, source, origin, stop) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("remuda: vfs serve: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        files: Vec<(Meta, Vec<u8>)>,
        fail: Option<Kind>,
    }

    impl Fake {
        fn new(files: &[(&str, u64, &[u8])]) -> Self {
            Self {
                files: files
                    .iter()
                    .map(|(path, version, bytes)| {
                        (
                            Meta {
                                path: (*path).to_owned(),
                                version: *version,
                                sha256: "a".repeat(64),
                                size: bytes.len() as u64,
                            },
                            bytes.to_vec(),
                        )
                    })
                    .collect(),
                fail: None,
            }
        }

        fn failing(kind: Kind) -> Self {
            Self {
                files: Vec::new(),
                fail: Some(kind),
            }
        }
    }

    impl Source for Fake {
        fn read(&self, path: &str, max_bytes: u64) -> Result<Result<Content, Kind>, StoreError> {
            if let Some(kind) = self.fail {
                return Ok(Err(kind));
            }
            match self.files.iter().find(|(meta, _)| meta.path == path) {
                Some((meta, _)) if meta.size > max_bytes => Ok(Err(Kind::TooLarge)),
                Some((meta, bytes)) => Ok(Ok(Content {
                    meta: meta.clone(),
                    bytes: bytes.clone(),
                })),
                None => Ok(Err(Kind::NotFound)),
            }
        }

        fn list(&self, prefix: &str) -> Result<Vec<Meta>, StoreError> {
            if let Some(kind) = self.fail {
                return Err(StoreError::new(kind, "failed"));
            }
            Ok(self
                .files
                .iter()
                .filter(|(meta, _)| meta.path.starts_with(prefix))
                .map(|(meta, _)| meta.clone())
                .collect())
        }
    }

    const ORIGIN: &str = "http://127.0.0.1:9";

    fn get(target: &str) -> Head {
        Head {
            method: Method::Get,
            target: target.to_owned(),
            host: Some("127.0.0.1:9".to_owned()),
            if_none_match: None,
        }
    }

    fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
        response
            .headers
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn a_head_is_parsed_strictly() {
        let head = parse_head(b"GET /a?b HTTP/1.1\r\nHost: h\r\n\r\n").unwrap();
        assert_eq!(head.method, Method::Get);
        assert_eq!(head.target, "/a?b");
        assert_eq!(head.host.as_deref(), Some("h"));
        for bad in [
            &b"GET /a HTTP/2\r\nHost: h\r\n\r\n"[..],
            b"GET /a\r\nHost: h\r\n\r\n",
            b"GET a HTTP/1.1\r\nHost: h\r\n\r\n",
            b"POST /a HTTP/1.1\r\nHost: h\r\n\r\n",
            b"GET /a HTTP/1.1\r\n\r\n",
            b"GET /a HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"GET /a HTTP/1.1\r\nHost: h\r\nContent-Length: 3\r\n\r\n",
            b"GET /a HTTP/1.1\r\nHost: h\r\nHost: i\r\n\r\n",
            b"GET /a HTTP/1.1\r\nHost: h\r\nBad Name: x\r\n\r\n",
            b"GET http://evil/a HTTP/1.1\r\nHost: h\r\n\r\n",
            b"GET * HTTP/1.1\r\nHost: h\r\n\r\n",
        ] {
            assert!(
                parse_head(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn traversal_is_refused_after_decoding() {
        for bad in [
            "/../etc",
            "/a/../../b",
            "/%2e%2e/x",
            "/a/%2E%2E",
            "/a%00b",
            "/a%zz",
            "/a%",
        ] {
            assert!(resolve_target(bad, "").is_err(), "{bad} should not resolve");
        }
        let ok = resolve_target("/design/x.html?q=1", "").unwrap();
        assert_eq!(ok.path, "design/x.html");
        assert_eq!(ok.query, "q=1");
        assert!(!ok.trailing_slash);
    }

    #[test]
    fn the_root_scopes_every_request() {
        let target = resolve_target("/x.html", "design").unwrap();
        assert_eq!(target.path, "design/x.html");
        assert_eq!(resolve_target("/", "design").unwrap().path, "design");
        assert!(Config::new("../etc", false).is_err());
        let config = Config::new("design", false).unwrap();
        assert_eq!(config.prefix_for(""), "design/");
    }

    #[test]
    fn a_file_is_served_with_an_etag_and_can_be_revalidated() {
        let fake = Fake::new(&[("a.html", 3, b"<h1>hi</h1>")]);
        let config = Config::new("", false).unwrap();
        let response = handle(&fake, &config, ORIGIN, &get("/a.html"));
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"<h1>hi</h1>");
        let etag = format!("\"{}\"", "a".repeat(64));
        assert_eq!(header(&response, "ETag"), Some(etag.as_str()));

        let mut conditional = get("/a.html");
        conditional.if_none_match = Some(format!("W/\"nope\", {etag}"));
        let response = handle(&fake, &config, ORIGIN, &conditional);
        assert_eq!(response.status, 304);
        assert!(response.body.is_empty());
        assert!(
            !String::from_utf8_lossy(&response.to_bytes(Method::Get)).contains("Content-Length")
        );
    }

    #[test]
    fn html_is_sandboxed_and_assets_are_not() {
        let fake = Fake::new(&[
            ("a.html", 1, b"<script>alert(1)</script>"),
            ("a.css", 1, b"body{}"),
        ]);
        let config = Config::new("", false).unwrap();
        let html = handle(&fake, &config, ORIGIN, &get("/a.html"));
        let policy = header(&html, "Content-Security-Policy").expect("html carries a policy");
        assert!(policy.starts_with("sandbox;"));
        assert!(policy.contains("connect-src 'none'"));
        assert!(policy.contains("script-src 'none'"));

        let css = handle(&fake, &config, ORIGIN, &get("/a.css"));
        assert_eq!(header(&css, "Content-Security-Policy"), None);
        assert_eq!(header(&css, "X-Content-Type-Options"), Some("nosniff"));
        assert_eq!(header(&css, "Cache-Control"), Some("no-cache"));
        assert_eq!(header(&css, "X-Frame-Options"), Some("DENY"));
    }

    #[test]
    fn svg_is_treated_as_active_content() {
        let fake = Fake::new(&[("a.svg", 1, b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>")]);
        let config = Config::new("", false).unwrap();
        let response = handle(&fake, &config, ORIGIN, &get("/a.svg"));
        assert!(header(&response, "Content-Security-Policy")
            .unwrap_or_default()
            .starts_with("sandbox;"));
    }

    #[test]
    fn trusted_mode_allows_same_origin_script_but_no_network() {
        let config = Config::new("", true).unwrap();
        let fake = Fake::new(&[("/a.html", 1, b"hi")]);
        let response = handle(&fake, &config, ORIGIN, &get("/a.html"));
        let policy = header(&response, "Content-Security-Policy").unwrap();
        assert!(!policy.starts_with("sandbox"));
        assert!(policy.contains("script-src http://127.0.0.1:9"));
        assert!(policy.contains("connect-src 'none'"));
    }

    #[test]
    fn a_directory_lists_and_a_bare_prefix_redirects() {
        let fake = Fake::new(&[
            ("design/a.html", 1, b"a"),
            ("design/sub/b.css", 1, b"b"),
            ("design-private/secret.txt", 1, b"no"),
        ]);
        let config = Config::new("", false).unwrap();
        let listing = handle(&fake, &config, ORIGIN, &get("/design/"));
        assert_eq!(listing.status, 200);
        let page = String::from_utf8(listing.body).unwrap();
        assert!(page.contains("/design/a.html"), "{page}");
        assert!(page.contains("/design/sub/"), "{page}");
        assert!(!page.contains("design-private"), "{page}");

        let redirect = handle(&fake, &config, ORIGIN, &get("/design"));
        assert_eq!(redirect.status, 301);
        assert_eq!(header(&redirect, "Location"), Some("/design/"));
    }

    #[test]
    fn a_root_listing_cannot_see_a_sibling_prefix() {
        let fake = Fake::new(&[
            ("design/a.html", 1, b"a"),
            ("design-private/secret.txt", 1, b"no"),
        ]);
        let config = Config::new("design", false).unwrap();
        let page = String::from_utf8(handle(&fake, &config, ORIGIN, &get("/")).body).unwrap();
        assert!(page.contains("a.html"), "{page}");
        assert!(!page.contains("secret"), "{page}");
        assert_eq!(
            handle(
                &fake,
                &config,
                ORIGIN,
                &get("/../design-private/secret.txt")
            )
            .status,
            404
        );
    }

    #[test]
    fn a_listing_escapes_names_it_did_not_write() {
        let fake = Fake::new(&[("<script>.txt", 1, b"no")]);
        let config = Config::new("", false).unwrap();
        let page = String::from_utf8(handle(&fake, &config, ORIGIN, &get("/")).body).unwrap();
        assert!(!page.contains("<script>"), "{page}");
        assert!(page.contains("&lt;script&gt;"), "{page}");
    }

    #[test]
    fn store_failures_map_to_statuses() {
        let config = Config::new("", false).unwrap();
        for (kind, status) in [
            (Kind::NoCluster, 503),
            (Kind::Unreachable, 503),
            (Kind::Corrupt, 502),
            (Kind::Io, 500),
            (Kind::WrongCluster, 400),
            (Kind::TooLarge, 413),
        ] {
            assert_eq!(
                handle(&Fake::failing(kind), &config, ORIGIN, &get("/a.html")).status,
                status,
                "{kind:?}"
            );
        }
        let fake = Fake::new(&[("present.html", 1, b"x")]);
        assert_eq!(
            handle(&fake, &config, ORIGIN, &get("/missing.html")).status,
            404
        );
    }

    #[test]
    fn head_sends_the_same_head_without_a_body() {
        let fake = Fake::new(&[("a.txt", 1, b"hello")]);
        let config = Config::new("", false).unwrap();
        let mut head = get("/a.txt");
        head.method = Method::Head;
        let response = handle(&fake, &config, ORIGIN, &head);
        let wire = String::from_utf8_lossy(&response.to_bytes(Method::Head)).to_string();
        assert!(wire.contains("Content-Length: 5"), "{wire}");
        assert!(wire.ends_with("\r\n\r\n"), "no body after the head: {wire}");
    }

    #[test]
    fn content_length_is_digits_only_and_appears_once() {
        assert!(parse_head(b"GET / HTTP/1.1\r\nHost: h\r\nContent-Length: 0\r\n\r\n").is_ok());
        for bad in [
            &b"GET / HTTP/1.1\r\nHost: h\r\nContent-Length: +0\r\n\r\n"[..],
            b"GET / HTTP/1.1\r\nHost: h\r\nContent-Length: 0\r\nContent-Length: 5\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: h\r\nContent-Length: 5\r\nContent-Length: 0\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: h\r\nContent-Length: \r\n\r\n",
        ] {
            assert!(
                parse_head(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn links_stay_inside_the_mounted_root() {
        let fake = Fake::new(&[
            ("design/index.html", 1, b"<h1>q3</h1>"),
            ("design/sub/a.css", 1, b"a{}"),
        ]);
        let config = Config::new("design", false).unwrap();
        let page = String::from_utf8(handle(&fake, &config, ORIGIN, &get("/")).body).unwrap();
        assert!(page.contains("href=\"/index.html\""), "{page}");
        assert!(page.contains("href=\"/sub/\""), "{page}");
        assert!(
            !page.contains("/design/"),
            "cluster path leaked into a link: {page}"
        );

        let redirect = handle(&fake, &config, ORIGIN, &get("/sub"));
        assert_eq!(redirect.status, 301);
        assert_eq!(header(&redirect, "Location"), Some("/sub/"));
    }

    #[test]
    fn localhost_pages_may_load_their_own_assets() {
        let loopback = sources("http://127.0.0.1:9");
        assert!(loopback.contains("http://127.0.0.1:9"));
        assert!(loopback.contains("http://localhost:9"));
        assert_eq!(sources("http://10.0.0.2:9"), "http://10.0.0.2:9");
    }

    #[test]
    fn a_foreign_host_is_refused() {
        assert!(host_allowed("127.0.0.1:9", ORIGIN));
        assert!(host_allowed("localhost:9", ORIGIN));
        assert!(!host_allowed("evil.example:9", ORIGIN));
        assert!(!host_allowed("127.0.0.1:10", ORIGIN));
    }

    #[test]
    fn hrefs_survive_names_with_query_characters() {
        assert_eq!(href_path("a b?c#d%e"), "a%20b%3Fc%23d%25e");
        assert_eq!(parent_href("a/b/c"), "/a/b/");
        assert_eq!(parent_href("a"), "/");
    }

    #[test]
    fn content_types_cover_what_a_page_needs() {
        assert_eq!(content_type("/a/b.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("/a.css"), "text/css; charset=utf-8");
        assert_eq!(content_type("/A.PNG"), "image/png");
        assert_eq!(content_type("/no-extension"), "application/octet-stream");
    }

    #[test]
    fn the_server_answers_one_request_and_closes() {
        let fake: Arc<dyn Source + Send + Sync> =
            Arc::new(Fake::new(&[("a.html", 1, b"<h1>ok</h1>")]));
        let config = Arc::new(Config::new("", false).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let origin = origin_of(addr);
        let stop = Arc::new(AtomicBool::new(false));
        let running = {
            let stop = Arc::clone(&stop);
            let origin = origin.clone();
            std::thread::spawn(move || serve(listener, config, fake, origin, stop).unwrap())
        };
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .write_all(
                format!(
                    "GET /a.html HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
                    addr
                )
                .as_bytes(),
            )
            .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"), "{reply}");
        assert!(reply.contains("Connection: close\r\n"), "{reply}");
        assert!(reply.ends_with("<h1>ok</h1>"), "{reply}");
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .write_all(
                format!("HEAD /a.html HTTP/1.1\r\nHost: {addr}\r\nBad Name: x\r\n\r\n").as_bytes(),
            )
            .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 400"), "{reply}");
        assert!(
            reply.ends_with("\r\n\r\n"),
            "a refused HEAD has no body: {reply}"
        );

        stop.store(true, Ordering::Relaxed);
        running.join().unwrap();
    }
}
