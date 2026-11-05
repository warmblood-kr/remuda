//! Raw owning UDS tunnel for the Codex TUI.
//!
//! The TUI connects to our downstream socket; we connect to the app-server and
//! forward every byte unchanged. A separate bounded observer inspects the first
//! `thread/start` request and its single exact response, so only the launch's own
//! initial thread can be pinned. A parser failure disables the observer only;
//! forwarding and the TUI continue.

#![allow(dead_code)]

#[cfg(unix)]
mod imp {
    use super::super::codex_native;
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    const MAX_HEADERS: usize = 8 * 1024;
    const MAX_FRAME: u64 = 4 * 1024 * 1024;

    /// Parent-provided publication inputs. The binding is a manifest file path.
    pub(crate) struct Publish {
        pub binding: PathBuf,
        pub nonce: String,
        pub session: String,
    }

    #[derive(Default)]
    pub(crate) struct State {
        pub closing: bool,
        pub published: Option<codex_native::Published>,
    }

    pub(crate) struct Tunnel {
        downstream: PathBuf,
        downstream_id: (u64, u64),
        state: Arc<Mutex<State>>,
    }

    impl Tunnel {
        pub(crate) fn downstream(&self) -> &Path {
            &self.downstream
        }

        pub(crate) fn cleanup(&self) {
            let taken = self.state.lock().ok().and_then(|mut state| {
                state.closing = true;
                state.published.take()
            });
            if let Some(published) = taken {
                published.remove();
            }
            if let Ok(metadata) = std::fs::symlink_metadata(&self.downstream) {
                if (metadata.dev(), metadata.ino()) == self.downstream_id {
                    let _ = std::fs::remove_file(&self.downstream);
                }
            }
        }
    }

    struct Capture {
        request_id: Option<Value>,
        failed: bool,
        publishing: bool,
        publish: Publish,
        upstream: PathBuf,
        state: Arc<Mutex<State>>,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Direction {
        Request,
        Response,
    }

    struct Observer {
        direction: Direction,
        capture: Arc<Mutex<Capture>>,
        headers_done: bool,
        buffer: Vec<u8>,
        disabled: bool,
    }

    impl Observer {
        fn new(direction: Direction, capture: Arc<Mutex<Capture>>) -> Self {
            Self {
                direction,
                capture,
                headers_done: false,
                buffer: Vec::new(),
                disabled: false,
            }
        }

        /// Consume bytes for inspection only. Never blocks or affects forwarding.
        fn feed(&mut self, bytes: &[u8]) {
            if self.disabled {
                return;
            }
            if !self.headers_done {
                self.buffer.extend_from_slice(bytes);
                let Some(end) = header_end(&self.buffer) else {
                    if self.buffer.len() > MAX_HEADERS {
                        self.disabled = true;
                    }
                    return;
                };
                self.buffer.drain(..end);
                self.headers_done = true;
            } else {
                self.buffer.extend_from_slice(bytes);
            }
            loop {
                match next_frame(&self.buffer) {
                    Ok(Some((consumed, text))) => {
                        self.buffer.drain(..consumed);
                        if let Some(text) = text {
                            match self.direction {
                                Direction::Request => request_event(&text, &self.capture),
                                Direction::Response => response_event(&text, &self.capture),
                            }
                        }
                    }
                    Ok(None) => return,
                    Err(()) => {
                        self.disabled = true;
                        return;
                    }
                }
            }
        }
    }

    /// Bind the downstream listener and start forwarding in the background.
    pub(crate) fn start(upstream: &Path, publish: Publish) -> std::io::Result<Tunnel> {
        let parent = upstream
            .parent()
            .ok_or_else(|| std::io::Error::other("app-server socket has no parent"))?;
        let metadata = std::fs::metadata(parent)?;
        // SAFETY: geteuid has no preconditions and cannot fail.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(std::io::Error::other(
                "app-server socket directory is not private to this user",
            ));
        }
        let token = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let downstream = parent.join(format!(
            "remuda-codex-{}-{token}.tui.sock",
            std::process::id()
        ));
        let listener = UnixListener::bind(&downstream)?;
        std::fs::set_permissions(&downstream, std::fs::Permissions::from_mode(0o600))?;
        let bound = std::fs::symlink_metadata(&downstream)?;
        if !bound.file_type().is_socket() || bound.uid() != unsafe { libc::geteuid() } {
            let _ = std::fs::remove_file(&downstream);
            return Err(std::io::Error::other("downstream socket is not private"));
        }
        let downstream_id = (bound.dev(), bound.ino());
        let state = Arc::new(Mutex::new(State::default()));
        let capture = Arc::new(Mutex::new(Capture {
            request_id: None,
            failed: false,
            publishing: false,
            publish,
            upstream: upstream.to_path_buf(),
            state: state.clone(),
        }));
        std::thread::spawn({
            let capture = capture.clone();
            move || {
                let _ = serve(listener, capture);
            }
        });
        Ok(Tunnel {
            downstream,
            downstream_id,
            state,
        })
    }

    fn serve(listener: UnixListener, capture: Arc<Mutex<Capture>>) -> std::io::Result<()> {
        let (client, _) = listener.accept()?;
        let upstream = UnixStream::connect(&capture.lock().expect("capture").upstream)?;
        let client_read = client.try_clone()?;
        let upstream_read = upstream.try_clone()?;
        let requests = std::thread::spawn({
            let capture = capture.clone();
            move || {
                relay(
                    client_read,
                    upstream,
                    Observer::new(Direction::Request, capture),
                    true,
                )
            }
        });
        let responses = std::thread::spawn({
            let capture = capture.clone();
            move || {
                relay(
                    upstream_read,
                    client,
                    Observer::new(Direction::Response, capture),
                    false,
                )
            }
        });
        let _ = requests.join();
        let _ = responses.join();
        // A connection that ends before publication never becomes authority.
        if let Ok(mut capture) = capture.lock() {
            if !capture.publishing {
                capture.failed = true;
            }
        }
        Ok(())
    }

    /// Forward raw bytes in both directions. The request observer runs before
    /// the bytes reach the app-server, so a response cannot outrun its id.
    fn relay(
        mut from: UnixStream,
        mut to: UnixStream,
        mut observer: Observer,
        observe_before_write: bool,
    ) {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let read = match from.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            if observe_before_write {
                observer.feed(&chunk[..read]);
            }
            if to.write_all(&chunk[..read]).is_err() {
                return;
            }
            if !observe_before_write {
                observer.feed(&chunk[..read]);
            }
        }
    }

    fn header_end(buffer: &[u8]) -> Option<usize> {
        buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
    }

    /// One complete WebSocket frame, or `Err` when the observer must stop.
    fn next_frame(buffer: &[u8]) -> Result<Option<(usize, Option<String>)>, ()> {
        if buffer.len() < 2 {
            return Ok(None);
        }
        let opcode = buffer[0] & 0x0f;
        if buffer[0] & 0x70 != 0 || buffer[0] & 0x80 == 0 {
            return Err(());
        }
        let masked = buffer[1] & 0x80 != 0;
        let mut offset = 2;
        let mut length = (buffer[1] & 0x7f) as u64;
        if length == 126 {
            if buffer.len() < 4 {
                return Ok(None);
            }
            length = u16::from_be_bytes([buffer[2], buffer[3]]) as u64;
            offset = 4;
        } else if length == 127 {
            if buffer.len() < 10 {
                return Ok(None);
            }
            length = u64::from_be_bytes(buffer[2..10].try_into().map_err(|_| ())?);
            offset = 10;
        }
        if length > MAX_FRAME || (opcode >= 8 && length > 125) {
            return Err(());
        }
        let mask = if masked {
            if buffer.len() < offset + 4 {
                return Ok(None);
            }
            let mask = [
                buffer[offset],
                buffer[offset + 1],
                buffer[offset + 2],
                buffer[offset + 3],
            ];
            offset += 4;
            Some(mask)
        } else {
            None
        };
        let total = offset + length as usize;
        if buffer.len() < total {
            return Ok(None);
        }
        let text = if opcode == 1 {
            let mut payload = buffer[offset..total].to_vec();
            if let Some(mask) = mask {
                for (index, byte) in payload.iter_mut().enumerate() {
                    *byte ^= mask[index % 4];
                }
            }
            Some(String::from_utf8(payload).map_err(|_| ())?)
        } else {
            None
        };
        Ok(Some((total, text)))
    }

    fn request_event(text: &str, capture: &Arc<Mutex<Capture>>) {
        let Ok(message) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if message.get("method").and_then(Value::as_str) != Some("thread/start") {
            return;
        }
        let Ok(mut capture) = capture.lock() else {
            return;
        };
        if capture.failed || capture.publishing || published(&capture) {
            return;
        }
        let Some(id) = message.get("id").and_then(Value::as_str) else {
            capture.failed = true;
            return;
        };
        let id = Value::String(id.to_string());
        match &capture.request_id {
            None => capture.request_id = Some(id),
            Some(first) if *first == id => capture.failed = true,
            Some(_) => capture.failed = true,
        }
    }

    fn response_event(text: &str, capture: &Arc<Mutex<Capture>>) {
        let Ok(message) = serde_json::from_str::<Value>(text) else {
            return;
        };
        // A method-bearing envelope is a request/notification, not a response.
        if message.get("method").is_some() {
            return;
        }
        let Some(id) = message.get("id").cloned() else {
            return;
        };
        let thread = {
            let Ok(mut capture) = capture.lock() else {
                return;
            };
            if capture.request_id.as_ref() != Some(&id)
                || capture.failed
                || capture.publishing
                || published(&capture)
            {
                return;
            }
            if message.get("result").is_some() == message.get("error").is_some() {
                capture.failed = true;
                return;
            }
            if message.get("error").is_some() {
                capture.failed = true;
                return;
            }
            let Some(thread) = root_thread(&message) else {
                capture.failed = true;
                return;
            };
            capture.publishing = true;
            (
                thread,
                capture.publish.binding.clone(),
                capture.publish.nonce.clone(),
                capture.publish.session.clone(),
                capture.upstream.clone(),
                capture.state.clone(),
            )
        };
        let (thread, binding, nonce, session, upstream, state) = thread;
        let request = codex_native::AdoptRequest {
            version: 1,
            binding: binding.to_string_lossy().into_owned(),
            nonce,
            session,
            socket: upstream.to_string_lossy().into_owned(),
            thread,
        };
        match codex_native::adopt(&request) {
            Ok(published) => {
                if let Ok(mut state) = state.lock() {
                    if state.closing {
                        drop(state);
                        published.remove();
                    } else {
                        state.published = Some(published);
                    }
                }
            }
            Err(reason) => {
                eprintln!("remuda: Codex binding not published: {reason}");
                if let Ok(mut capture) = capture.lock() {
                    capture.publishing = false;
                    capture.failed = true;
                }
            }
        }
    }

    fn published(capture: &Capture) -> bool {
        capture
            .state
            .lock()
            .map(|state| state.published.is_some())
            .unwrap_or(false)
    }

    /// The first `thread/start` response must be a root, non-ephemeral thread.
    fn root_thread(response: &Value) -> Option<String> {
        response.get("result")?;
        let thread = response.pointer("/result/thread")?;
        let id = thread.get("id")?.as_str()?;
        if !codex_native::thread_uuid(id) {
            return None;
        }
        if let Some(session) = thread.get("sessionId").and_then(Value::as_str) {
            if session != id {
                return None;
            }
        }
        if thread.get("ephemeral").and_then(Value::as_bool) != Some(false) {
            return None;
        }
        if !thread.get("forkedFromId").is_none_or(Value::is_null)
            || !thread.get("parentThreadId").is_none_or(Value::is_null)
        {
            return None;
        }
        Some(id.to_string())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        const A: &str = "01a1abb6-d043-7d83-9bdc-31bb9e900278";

        fn response(thread: Value) -> Value {
            json!({"id": "startup-thread-start", "result": {"thread": thread}})
        }

        fn capture() -> Arc<Mutex<Capture>> {
            Arc::new(Mutex::new(Capture {
                request_id: None,
                failed: false,
                publishing: false,
                publish: Publish {
                    binding: PathBuf::from("/tmp/x/manifest.json"),
                    nonce: "n".into(),
                    session: "s".into(),
                },
                upstream: PathBuf::from("/tmp/x.sock"),
                state: Arc::new(Mutex::new(State::default())),
            }))
        }

        #[test]
        fn a_second_bootstrap_request_latches_unavailable() {
            let capture = capture();
            request_event(r#"{"id":"a","method":"thread/start"}"#, &capture);
            request_event(r#"{"id":"b","method":"thread/start"}"#, &capture);
            assert!(capture.lock().expect("capture").failed);
        }

        #[test]
        fn a_non_string_bootstrap_id_latches_unavailable() {
            let capture = capture();
            request_event(r#"{"id":7,"method":"thread/start"}"#, &capture);
            assert!(capture.lock().expect("capture").failed);
        }

        #[test]
        fn an_error_response_latches_unavailable() {
            let capture = capture();
            request_event(r#"{"id":"a","method":"thread/start"}"#, &capture);
            response_event(r#"{"id":"a","error":{"code":-1}}"#, &capture);
            assert!(capture.lock().expect("capture").failed);
        }

        #[test]
        fn a_non_root_response_latches_unavailable() {
            let capture = capture();
            request_event(
                r#"{"id":"startup-thread-start","method":"thread/start"}"#,
                &capture,
            );
            let response = response(json!({
                "id": A, "sessionId": A, "ephemeral": true
            }));
            response_event(&response.to_string(), &capture);
            assert!(capture.lock().expect("capture").failed);
        }

        #[test]
        fn a_method_bearing_matching_id_is_not_a_response() {
            let capture = capture();
            request_event(
                r#"{"id":"startup-thread-start","method":"thread/start"}"#,
                &capture,
            );
            response_event(
                r#"{"id":"startup-thread-start","method":"thread/start","result":{}}"#,
                &capture,
            );
            assert!(!capture.lock().expect("capture").failed);
        }

        #[test]
        fn root_thread_accepts_only_the_root_non_ephemeral_shape() {
            let ok = response(json!({
                "id": A, "sessionId": A, "ephemeral": false,
                "forkedFromId": null, "parentThreadId": null
            }));
            assert_eq!(root_thread(&ok), Some(A.to_string()));

            let mut ephemeral = ok.clone();
            ephemeral["result"]["thread"]["ephemeral"] = json!(true);
            assert_eq!(root_thread(&ephemeral), None);

            let mut forked = ok.clone();
            forked["result"]["thread"]["forkedFromId"] = json!("other");
            assert_eq!(root_thread(&forked), None);

            let mut session = ok.clone();
            session["result"]["thread"]["sessionId"] = json!("other");
            assert_eq!(root_thread(&session), None);

            let mut bad_id = ok.clone();
            bad_id["result"]["thread"]["id"] = json!("not-a-uuid");
            assert_eq!(root_thread(&bad_id), None);

            let error = json!({"id": "startup-thread-start", "error": {"code": -1}});
            assert_eq!(root_thread(&error), None);
        }

        #[test]
        fn the_observer_forwards_through_a_parse_failure() {
            let mut observer = Observer::new(Direction::Response, capture());
            observer.feed(b"HTTP/1.1 101 Switching Protocols\r\n\r\n");
            // A fragmented/RSV frame disables observation, not forwarding.
            observer.feed(&[0xF1, 0x80, 1, 2, 3, 4]);
            assert!(observer.disabled);
            observer.feed(b"anything");
        }
    }
}

#[cfg(unix)]
pub(crate) use imp::{start, Publish};

#[cfg(not(unix))]
mod unsupported {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    pub(crate) struct Publish {
        pub binding: PathBuf,
        pub nonce: String,
        pub session: String,
    }

    #[derive(Default)]
    pub(crate) struct State {
        pub closing: bool,
        pub published: Option<()>,
    }

    pub(crate) struct Tunnel {
        pub downstream: PathBuf,
        pub state: Arc<Mutex<State>>,
    }

    impl Tunnel {
        pub(crate) fn downstream(&self) -> &Path {
            &self.downstream
        }

        pub(crate) fn cleanup(&self) {}
    }

    pub(crate) fn start(_upstream: &Path, _publish: Publish) -> std::io::Result<Tunnel> {
        Err(std::io::Error::other("Codex native binding requires Unix"))
    }
}

#[cfg(not(unix))]
pub(crate) use unsupported::{start, Publish};
