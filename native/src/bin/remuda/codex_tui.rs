use serde_json::{json, Value};
#[cfg(unix)]
use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

pub fn run(args: &[&str]) -> ExitCode {
    let Some((&"--status", status)) = args.first().zip(args.get(1)) else {
        eprintln!("remuda: Codex TUI needs --status PATH");
        return ExitCode::FAILURE;
    };
    let model = match args.get(2..) {
        Some(["--model", model]) => Some(*model),
        Some([]) | None => None,
        _ => {
            eprintln!("remuda: Codex TUI takes --status PATH [--model M]");
            return ExitCode::FAILURE;
        }
    };
    // The selected model is useful status even before the first prompt. The
    // app-server does not announce a thread (or its model) until a thread is
    // started, so waiting for `thread/started` leaves the caller showing its
    // previous/default model while the new TUI is idle.
    seed_status(status, model);
    let socket = std::env::temp_dir().join(format!("remuda-codex-{}.sock", std::process::id()));
    let address = format!("unix://{}", socket.display());
    let (server_args, client_args) = codex_args(&address, model);
    let mut server = match Command::new("codex")
        .args(&server_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return fail(error),
    };
    if !wait_for_socket(&socket) {
        let _ = server.kill();
        return fail("Codex app-server did not bind its local socket");
    }
    let monitor_socket = socket.clone();
    let monitor_status = status.to_string();
    std::thread::spawn(move || monitor(&monitor_socket, &monitor_status));
    let result = Command::new("codex").args(&client_args).status();
    let _ = server.kill();
    let _ = std::fs::remove_file(socket);
    match result {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => fail(error),
    }
}

fn seed_status(status: &str, model: Option<&str>) {
    if let Some(model) = model {
        write_status(status, model, "?", "?");
    }
}

/// Argv for the app-server and the TUI client.  A chosen model goes to both:
/// the server's config default and the client's thread override.
fn codex_args(address: &str, model: Option<&str>) -> (Vec<String>, Vec<String>) {
    let mut server = vec!["app-server".into(), "--listen".into(), address.into()];
    let mut client = vec!["--remote".into(), address.into(), "--approve-for-me".into()];
    if let Some(model) = model {
        server.extend(["-c".into(), format!("model={model:?}")]);
        client.extend(["-m".into(), model.into()]);
    }
    (server, client)
}

#[cfg(unix)]
fn wait_for_socket(socket: &std::path::Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if UnixStream::connect(socket).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

/// Codex integration needs a Unix domain socket; there's no Windows
/// equivalent wired up here, so `run` fails fast via this always-false
/// stub instead of hanging.
#[cfg(not(unix))]
fn wait_for_socket(_socket: &std::path::Path) -> bool {
    false
}

#[cfg(unix)]
fn monitor(socket: &std::path::Path, status: &str) {
    monitor_with_key(socket, status, None);
}

#[cfg(unix)]
fn monitor_with_key(socket: &std::path::Path, status: &str, key: Option<&str>) {
    let Ok(stream) = UnixStream::connect(socket) else {
        return;
    };
    let Ok(mut stream) = websocket_handshake(stream, key) else {
        return;
    };
    let initialize = json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"remuda-butler","version":env!("CARGO_PKG_VERSION")}}});
    if websocket_send_text(&mut stream, &initialize.to_string()).is_err() {
        return;
    }
    let mut model = "?".to_string();
    let mut next_request_id = 2_u64;
    let mut persistent_threads = HashSet::new();
    let mut subscribed_threads = HashSet::new();
    let mut resume_requests = HashMap::new();
    while let Ok(Some(message)) = websocket_read_text(&mut stream) {
        let Ok(event) = serde_json::from_str::<Value>(&message) else {
            continue;
        };
        if event.get("id").and_then(Value::as_u64) == Some(1)
            && websocket_send_text(
                &mut stream,
                &json!({"method":"initialized","params":{}}).to_string(),
            )
            .is_err()
        {
            return;
        }
        if let Some(id) = event.get("id").and_then(Value::as_u64) {
            if let Some(thread_id) = resume_requests.remove(&id) {
                if event.get("result").is_some() {
                    subscribed_threads.insert(thread_id);
                }
            }
        }
        if event.get("method").and_then(Value::as_str) == Some("thread/started")
            && event
                .pointer("/params/thread/ephemeral")
                .and_then(Value::as_bool)
                != Some(true)
        {
            if let Some(thread_id) = event.pointer("/params/thread/id").and_then(Value::as_str) {
                persistent_threads.insert(thread_id.to_string());
            }
        }
        if event.get("method").and_then(Value::as_str) == Some("thread/status/changed") {
            if let (Some(thread_id), Some(status)) = (
                event.pointer("/params/threadId").and_then(Value::as_str),
                event.pointer("/params/status/type").and_then(Value::as_str),
            ) {
                let should_resume = matches!(status, "active" | "idle")
                    && persistent_threads.contains(thread_id)
                    && !subscribed_threads.contains(thread_id)
                    && !resume_requests.values().any(|id| id == thread_id);
                if should_resume {
                    let request_id = next_request_id;
                    next_request_id += 1;
                    let resume = json!({
                        "id": request_id,
                        "method": "thread/resume",
                        "params": {"threadId": thread_id, "excludeTurns": true}
                    });
                    resume_requests.insert(request_id, thread_id.to_string());
                    if websocket_send_text(&mut stream, &resume.to_string()).is_err() {
                        return;
                    }
                }
            }
        }
        update_status(&event, &mut model, status);
    }
}

#[cfg(unix)]
/// ponytail: this small client supports local UDS handshakes and unfragmented
/// text frames only; it negotiates no extensions and does not reassemble fragments.
fn websocket_handshake(
    mut stream: UnixStream,
    supplied_key: Option<&str>,
) -> std::io::Result<UnixStream> {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let key = match supplied_key {
        Some(key) => key.to_string(),
        None => {
            let mut nonce = [0; 16];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut nonce)?;
            base64(&nonce)
        }
    };
    let request = format!(
        "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;
    let response = read_http_headers(&mut stream)?;
    let mut lines = response.split("\r\n");
    let status = lines.next().unwrap_or_default();
    if !(status.starts_with("HTTP/1.1 101 ") || status.starts_with("HTTP/1.0 101 ")) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("WebSocket upgrade refused: {status}"),
        ));
    }
    let headers: Vec<_> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    if !header("upgrade").is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        || !header("connection").is_some_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
        })
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid WebSocket upgrade response headers",
        ));
    }
    let mut accept_input = key;
    accept_input.push_str(GUID);
    let expected = base64(&sha1(accept_input.as_bytes()));
    if header("sec-websocket-accept") != Some(expected.as_str()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid Sec-WebSocket-Accept value",
        ));
    }
    Ok(stream)
}

#[cfg(unix)]
fn read_http_headers(stream: &mut UnixStream) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
        if bytes.len() > 8192 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "oversized WebSocket handshake headers",
            ));
        }
    }
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

#[cfg(unix)]
fn websocket_send_text(stream: &mut UnixStream, text: &str) -> std::io::Result<()> {
    let payload = text.as_bytes();
    let mut header = vec![0x81];
    match payload.len() {
        len @ 0..=125 => header.push(0x80 | len as u8),
        len @ 126..=65535 => {
            header.push(0x80 | 126);
            header.extend_from_slice(&(len as u16).to_be_bytes());
        }
        len => {
            header.push(0x80 | 127);
            header.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }
    let mut mask = [0; 4];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut mask)?;
    stream.write_all(&header)?;
    stream.write_all(&mask)?;
    for (index, byte) in payload.iter().enumerate() {
        stream.write_all(&[*byte ^ mask[index % 4]])?;
    }
    Ok(())
}

#[cfg(unix)]
fn websocket_read_text(stream: &mut UnixStream) -> std::io::Result<Option<String>> {
    const MAX_MESSAGE: u64 = 4 * 1024 * 1024;
    loop {
        let mut header = [0; 2];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error),
        }
        let fin = header[0] & 0x80 != 0;
        let opcode = header[0] & 0x0f;
        if header[0] & 0x70 != 0 || !fin {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "WebSocket extensions and fragmented frames are unsupported",
            ));
        }
        if header[1] & 0x80 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "server WebSocket frames must not be masked",
            ));
        }
        let mut length = (header[1] & 0x7f) as u64;
        match length {
            126 => {
                let mut extended = [0; 2];
                stream.read_exact(&mut extended)?;
                length = u16::from_be_bytes(extended) as u64;
            }
            127 => {
                let mut extended = [0; 8];
                stream.read_exact(&mut extended)?;
                length = u64::from_be_bytes(extended);
            }
            _ => {}
        }
        if length > MAX_MESSAGE || (opcode >= 8 && length > 125) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "oversized WebSocket frame",
            ));
        }
        let mut payload = vec![0; length as usize];
        stream.read_exact(&mut payload)?;
        match opcode {
            0x1 => {
                return String::from_utf8(payload)
                    .map(Some)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error));
            }
            0x8 => return Ok(None),
            0x9 => websocket_send_control(stream, 0xA, &payload)?,
            0xA => {}
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "unsupported WebSocket opcode",
                ));
            }
        }
    }
}

#[cfg(unix)]
fn websocket_send_control(
    stream: &mut UnixStream,
    opcode: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    if payload.len() > 125 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "oversized WebSocket control frame",
        ));
    }
    let mut mask = [0; 4];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut mask)?;
    stream.write_all(&[0x80 | opcode, 0x80 | payload.len() as u8])?;
    stream.write_all(&mask)?;
    for (index, byte) in payload.iter().enumerate() {
        stream.write_all(&[*byte ^ mask[index % 4]])?;
    }
    Ok(())
}

#[cfg(unix)]
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(a >> 2) as usize] as char);
        out.push(ALPHABET[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(unix)]
fn sha1(input: &[u8]) -> [u8; 20] {
    let mut data = input.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());
    let mut h = [
        0x67452301u32,
        0xefcdab89,
        0x98badcfe,
        0x10325476,
        0xc3d2e1f0,
    ];
    for block in data.as_chunks::<64>().0 {
        let mut words = [0u32; 80];
        for (index, bytes) in block.as_chunks::<4>().0.iter().enumerate() {
            words[index] = u32::from_be_bytes(*bytes);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (index, word) in words.into_iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5a827999),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        for (value, add) in h.iter_mut().zip([a, b, c, d, e]) {
            *value = value.wrapping_add(add);
        }
    }
    let mut digest = [0; 20];
    for (chunk, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *chunk = word.to_be_bytes();
    }
    digest
}

#[cfg(not(unix))]
fn monitor(_socket: &std::path::Path, _status: &str) {}

/// Publish a complete record as soon as Codex announces its thread.  Context
/// capacity is intentionally unknown until its first token-usage event: the
/// App Server does not include it in `thread/started`.
fn update_status(event: &Value, model: &mut String, status: &str) {
    match event.get("method").and_then(Value::as_str) {
        Some("thread/started") => {
            if event
                .pointer("/params/thread/ephemeral")
                .and_then(Value::as_bool)
                == Some(true)
            {
                return;
            }
            if let Some(value) = event
                .pointer("/params/thread/model")
                .and_then(Value::as_str)
            {
                *model = value.into();
            }
            write_status(status, model, "?", "?");
        }
        Some("thread/tokenUsage/updated") => {
            let usage = &event["params"]["tokenUsage"];
            let Some(window) = usage["modelContextWindow"].as_u64() else {
                return;
            };
            let last = &usage["last"];
            let used = ["inputTokens", "cachedInputTokens", "cacheWriteInputTokens"]
                .iter()
                .map(|key| last[*key].as_u64().unwrap_or(0))
                .sum::<u64>();
            write_status(status, model, &used.to_string(), &window.to_string());
        }
        _ => {}
    }
}

fn write_status(status: &str, model: &str, used: &str, window: &str) {
    let percent = match (used.parse::<u64>(), window.parse::<u64>()) {
        (Ok(used), Ok(window)) if window > 0 => (used * 100 / window).to_string(),
        _ => "?".into(),
    };
    let _ = std::fs::write(
        status,
        format!("MODEL:{model} CTX:{used} CTXWIN:{window} CTXPCT:{percent}\n"),
    );
}

fn fail(error: impl std::fmt::Display) -> ExitCode {
    eprintln!("remuda: Codex TUI: {error}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::io::Read;
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;

    fn status_path(test: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("remuda-codex-{test}-{}", std::process::id()))
    }

    #[test]
    fn model_reaches_both_the_app_server_and_the_client() {
        assert_eq!(
            codex_args("unix://s", None),
            (
                vec!["app-server".into(), "--listen".into(), "unix://s".into()],
                vec![
                    "--remote".into(),
                    "unix://s".into(),
                    "--approve-for-me".into()
                ],
            )
        );
        let (server, client) = codex_args("unix://s", Some("gpt-5.5"));
        assert_eq!(server[3..], ["-c", "model=\"gpt-5.5\""]);
        assert_eq!(client[3..], ["-m", "gpt-5.5"]);
    }

    #[test]
    fn thread_start_publishes_the_model_before_any_usage_event() {
        let path = status_path("thread-start");
        let _ = std::fs::remove_file(&path);
        let mut model = "?".to_string();
        update_status(
            &json!({"method":"thread/started","params":{"thread":{"model":"gpt-5.4"}}}),
            &mut model,
            &path.to_string_lossy(),
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "MODEL:gpt-5.4 CTX:? CTXWIN:? CTXPCT:?\n"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ephemeral_thread_does_not_replace_the_selected_model() {
        let path = status_path("ephemeral-thread");
        let _ = std::fs::remove_file(&path);
        let mut model = "gpt-5.6-sol".to_string();
        update_status(
            &json!({"method":"thread/started","params":{"thread":{"model":"gpt-5.6-luna","ephemeral":true}}}),
            &mut model,
            &path.to_string_lossy(),
        );
        assert_eq!(model, "gpt-5.6-sol");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn token_usage_replaces_unknown_context_with_real_capacity() {
        let path = status_path("token-usage");
        let _ = std::fs::remove_file(&path);
        let mut model = "gpt-5.4".to_string();
        update_status(
            &json!({"method":"thread/tokenUsage/updated","params":{"tokenUsage":{
                "modelContextWindow":200000,
                "last":{"inputTokens":12000,"cachedInputTokens":3000,"cacheWriteInputTokens":0}
            }}}),
            &mut model,
            &path.to_string_lossy(),
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "MODEL:gpt-5.4 CTX:15000 CTXWIN:200000 CTXPCT:7\n"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn selected_model_is_published_before_any_thread_starts() {
        let path = status_path("selected-model-before-thread");
        let _ = std::fs::remove_file(&path);

        seed_status(&path.to_string_lossy(), Some("gpt-5.6-sol"));

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "MODEL:gpt-5.6-sol CTX:? CTXWIN:? CTXPCT:?\n"
        );
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn websocket_monitor_receives_thread_and_usage_notifications() {
        const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
        const ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
        // Captured from Codex 0.156.0 after one prompt; retain the exact usage
        // payload shape so protocol drift is visible in this end-to-end test.
        const LIVE_USAGE_FRAME: &str = r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"01a0e074-2cc9-7800-b5cc-44c2d192d539","turnId":"01a0e074-545b-7f40-a9d7-23c60713d59f","tokenUsage":{"total":{"totalTokens":14250,"inputTokens":14245,"cachedInputTokens":11008,"cacheWriteInputTokens":0,"outputTokens":5,"reasoningOutputTokens":0},"last":{"totalTokens":14250,"inputTokens":14245,"cachedInputTokens":11008,"cacheWriteInputTokens":0,"outputTokens":5,"reasoningOutputTokens":0},"modelContextWindow":258400}},"emittedAtMs":1790472116912}"#;
        let socket_path = status_path("fake-websocket").with_extension("sock");
        let status_path = status_path("fake-websocket-status");
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_file(&status_path);
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let request = test_read_http_headers(&mut stream);
            assert!(
                request.starts_with("GET / HTTP/1.1\r\n"),
                "not a WebSocket handshake: {request:?}"
            );
            assert!(request.contains(&format!("Sec-WebSocket-Key: {KEY}\r\n")));
            stream
                .write_all(format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {ACCEPT}\r\n\r\n"
                ).as_bytes())
                .unwrap();

            let initialize: Value =
                serde_json::from_str(&test_read_client_text(&mut stream)).unwrap();
            assert_eq!(initialize["id"], 1);
            assert_eq!(initialize["method"], "initialize");
            test_write_server_text(&mut stream, r#"{"id":1,"result":{}}"#);

            let initialized: Value =
                serde_json::from_str(&test_read_client_text(&mut stream)).unwrap();
            assert_eq!(initialized["method"], "initialized");
            test_write_server_text(
                &mut stream,
                r#"{"method":"thread/started","params":{"thread":{"id":"01a0e074-2cc9-7800-b5cc-44c2d192d539","model":"gpt-test-X","ephemeral":false}}}"#,
            );
            test_write_server_text(
                &mut stream,
                r#"{"method":"thread/status/changed","params":{"threadId":"01a0e074-2cc9-7800-b5cc-44c2d192d539","status":{"type":"active","activeFlags":[]}}}"#,
            );
            let resume: Value = serde_json::from_str(&test_read_client_text(&mut stream)).unwrap();
            assert_eq!(resume["id"], 2);
            assert_eq!(resume["method"], "thread/resume");
            assert_eq!(
                resume["params"]["threadId"],
                "01a0e074-2cc9-7800-b5cc-44c2d192d539"
            );
            assert_eq!(resume["params"]["excludeTurns"], true);
            test_write_server_text(
                &mut stream,
                r#"{"id":2,"result":{"thread":{"id":"01a0e074-2cc9-7800-b5cc-44c2d192d539","model":"gpt-test-X"}}}"#,
            );
            test_write_server_text(
                &mut stream,
                r#"{"method":"thread/status/changed","params":{"threadId":"01a0e074-2cc9-7800-b5cc-44c2d192d539","status":{"type":"idle"}}}"#,
            );
            test_write_server_text(&mut stream, LIVE_USAGE_FRAME);
        });

        monitor_with_key(&socket_path, &status_path.to_string_lossy(), Some(KEY));
        server.join().unwrap();
        assert_eq!(
            std::fs::read_to_string(&status_path).unwrap(),
            "MODEL:gpt-test-X CTX:25253 CTXWIN:258400 CTXPCT:9\n"
        );
        let _ = std::fs::remove_file(socket_path);
        let _ = std::fs::remove_file(status_path);
    }

    #[cfg(unix)]
    fn test_read_http_headers(stream: &mut UnixStream) -> String {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
            assert!(bytes.len() <= 8192, "oversized fake handshake");
        }
        String::from_utf8(bytes).unwrap()
    }

    #[cfg(unix)]
    fn test_read_client_text(stream: &mut UnixStream) -> String {
        let mut header = [0; 2];
        stream.read_exact(&mut header).unwrap();
        assert_eq!(header[0], 0x81, "expected a final text frame");
        assert_ne!(header[1] & 0x80, 0, "client frame must be masked");
        let mut len = (header[1] & 0x7f) as usize;
        if len == 126 {
            let mut extended = [0; 2];
            stream.read_exact(&mut extended).unwrap();
            len = u16::from_be_bytes(extended) as usize;
        }
        assert!(len < 65536, "fake message is unexpectedly large");
        let mut mask = [0; 4];
        stream.read_exact(&mut mask).unwrap();
        let mut payload = vec![0; len];
        stream.read_exact(&mut payload).unwrap();
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
        String::from_utf8(payload).unwrap()
    }

    #[cfg(unix)]
    fn test_write_server_text(stream: &mut UnixStream, text: &str) {
        let payload = text.as_bytes();
        if payload.len() < 126 {
            stream.write_all(&[0x81, payload.len() as u8]).unwrap();
        } else {
            stream.write_all(&[0x81, 126]).unwrap();
            stream
                .write_all(&(payload.len() as u16).to_be_bytes())
                .unwrap();
        }
        stream.write_all(payload).unwrap();
    }
}
