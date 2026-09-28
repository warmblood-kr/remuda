//! A local scripted HTTP server for isolated transport and Matrix tests.
//!
//! Enable the `http-test-support` feature to use this helper from integration or
//! downstream test crates. It binds loopback only and owns its listener thread.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const MAX_TEST_REQUEST_BYTES: usize = 20 * 1024 * 1024;
const MAX_TEST_HEADER_BYTES: usize = 64 * 1024;
type TestHeaders = BTreeMap<String, Vec<Vec<u8>>>;
type ParsedRequest = (String, String, TestHeaders);

#[derive(Clone, Debug)]
pub struct ScriptedResponse {
    pub status: u16,
    pub headers: BTreeMap<String, Vec<u8>>,
    pub body: Vec<u8>,
    pub delay: Duration,
}
impl ScriptedResponse {
    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: BTreeMap::from([("content-type".into(), b"application/json".to_vec())]),
            body: body.into(),
            delay: Duration::ZERO,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: TestHeaders,
    pub body: Vec<u8>,
}

pub struct ScriptedHttpServer {
    address: SocketAddr,
    routes: Arc<Mutex<HashMap<String, VecDeque<ScriptedResponse>>>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl ScriptedHttpServer {
    pub fn new() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let routes = Arc::new(Mutex::new(
            HashMap::<String, VecDeque<ScriptedResponse>>::new(),
        ));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let server_routes = routes.clone();
        let server_requests = requests.clone();
        let server_stop = stop.clone();
        let thread = thread::spawn(move || {
            while !server_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => serve_one(stream, &server_routes, &server_requests),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            address,
            routes,
            requests,
            stop,
            thread: Some(thread),
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Queue one response for this exact request path. Repeated calls queue a
    /// deterministic response sequence for polling endpoints such as /sync.
    pub fn route(&self, path: impl Into<String>, response: ScriptedResponse) {
        self.routes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .entry(path.into())
            .or_default()
            .push_back(response);
    }

    pub fn route_json(&self, path: impl Into<String>, status: u16, body: impl Into<Vec<u8>>) {
        self.route(path, ScriptedResponse::json(status, body));
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn take_requests(&self) -> Vec<RecordedRequest> {
        std::mem::take(
            &mut *self
                .requests
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        )
    }
}
impl Drop for ScriptedHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_one(
    mut stream: TcpStream,
    routes: &Arc<Mutex<HashMap<String, VecDeque<ScriptedResponse>>>>,
    requests: &Arc<Mutex<Vec<RecordedRequest>>>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let Ok(head) = read_head(&mut stream) else {
        return;
    };
    let Some((method, path, headers)) = parse_head(&head) else {
        return;
    };
    let length = headers
        .get("content-length")
        .and_then(|items| items.first())
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if length > MAX_TEST_REQUEST_BYTES {
        return;
    }
    let mut body = vec![0; length];
    if stream.read_exact(&mut body).is_err() {
        return;
    }
    requests
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(RecordedRequest {
            method,
            path: path.clone(),
            headers,
            body,
        });
    let response = routes
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get_mut(&path)
        .and_then(VecDeque::pop_front)
        .unwrap_or_else(|| {
            ScriptedResponse::json(404, b"{\"error\":\"no scripted response\"}".to_vec())
        });
    if !response.delay.is_zero() {
        thread::sleep(response.delay);
    }
    let reason = reason(response.status);
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nConnection: close\r\nContent-Length: {}\r\n",
        response.status,
        reason,
        response.body.len()
    )
    .into_bytes();
    for (name, value) in response.headers {
        head.extend_from_slice(name.as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(&value);
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    if head.len() <= MAX_TEST_HEADER_BYTES {
        let _ = stream.write_all(&head);
        let _ = stream.write_all(&response.body);
    }
}

fn read_head(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() == MAX_TEST_HEADER_BYTES {
            return Err(std::io::Error::other("test request headers too large"));
        }
        stream.read_exact(&mut byte)?;
        head.push(byte[0]);
    }
    Ok(head)
}

fn parse_head(head: &[u8]) -> Option<ParsedRequest> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let mut request = lines.next()?.split_ascii_whitespace();
    let method = request.next()?.to_owned();
    let path = request.next()?.to_owned();
    let mut headers = BTreeMap::<String, Vec<Vec<u8>>>::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        headers
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(value.trim().as_bytes().to_vec());
    }
    Some((method, path, headers))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Test Response",
    }
}
