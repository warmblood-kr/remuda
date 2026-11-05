//! Raw owning UDS tunnel for the Codex TUI.
//!
//! The TUI connects to our downstream socket; we connect to the app-server and
//! forward every byte unchanged. A separate bounded observer inspects the first
//! `thread/start` request and its single exact response, so only the launch's own
//! initial thread can be pinned. Raw forwarding continues through any parser
//! failure, but that failure freezes publication in both directions.

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
    use std::time::{Duration, Instant};

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
        capture: Arc<Mutex<Capture>>,
    }

    impl Tunnel {
        pub(crate) fn downstream(&self) -> &Path {
            &self.downstream
        }

        /// Close publication first, then remove the binding and the downstream
        /// socket, so nothing can be published after cleanup returns.
        pub(crate) fn cleanup(&self) {
            if let Ok(mut state) = self.state.lock() {
                state.closing = true;
            }
            // An adopt that passed the fence publishes inline on its own
            // thread. Wait for it so the binding cannot reappear after we
            // remove it; the wait is bounded because adopt itself is bounded.
            let deadline = Instant::now() + Duration::from_secs(6);
            loop {
                let publishing = self
                    .capture
                    .lock()
                    .map(|capture| capture.publishing)
                    .unwrap_or(false);
                if !publishing || Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let taken = self
                .state
                .lock()
                .ok()
                .and_then(|mut state| state.published.take());
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

        /// Consume bytes for inspection only. It never reads the network or
        /// waits on a peer, but publication runs inline when a complete
        /// response arrives.
        fn feed(&mut self, bytes: &[u8]) {
            if self.disabled {
                return;
            }
            if !self.headers_done {
                self.buffer.extend_from_slice(bytes);
                let Some(end) = header_end(&self.buffer) else {
                    if self.buffer.len() > MAX_HEADERS {
                        self.fail();
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
                        self.fail();
                        return;
                    }
                }
            }
        }

        /// A parse failure ends authority, not just observation: bytes already
        /// seen in the other direction could otherwise publish a request we can
        /// no longer correlate.
        fn fail(&mut self) {
            self.disabled = true;
            if let Ok(mut capture) = self.capture.lock() {
                capture.failed = true;
            }
        }
    }

    fn random_token() -> std::io::Result<String> {
        let mut bytes = [0u8; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    /// A short, fresh, owner-only runtime directory, independent of TMPDIR.
    pub(crate) fn runtime_dir() -> std::io::Result<PathBuf> {
        let dir = Path::new("/tmp").join(format!(
            "remuda-codex-{}-{}",
            std::process::id(),
            random_token()?
        ));
        std::fs::create_dir(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let metadata = std::fs::symlink_metadata(&dir)?;
        // SAFETY: geteuid has no preconditions and cannot fail.
        if !metadata.file_type().is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            let _ = std::fs::remove_dir(&dir);
            return Err(std::io::Error::other(
                "runtime directory is not private to this user",
            ));
        }
        Ok(dir)
    }

    /// Bind the downstream listener and start forwarding in the background.
    pub(crate) fn start(
        upstream: &Path,
        runtime_dir: &Path,
        publish: Publish,
    ) -> std::io::Result<Tunnel> {
        let metadata = std::fs::symlink_metadata(runtime_dir)?;
        // SAFETY: geteuid has no preconditions and cannot fail.
        if !metadata.file_type().is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(std::io::Error::other(
                "runtime directory is not private to this user",
            ));
        }
        let downstream = runtime_dir.join("tui.sock");
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
            capture,
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
        settle_publication(
            capture,
            &state,
            codex_native::adopt_guarded(&request, || publication_permitted(capture, &state)),
        );
    }

    /// Current authority: no observed failure and no cleanup fence.
    fn publication_permitted(shared: &Arc<Mutex<Capture>>, state: &Arc<Mutex<State>>) -> bool {
        let capture = shared.lock().ok();
        let state = state.lock().ok();
        matches!((capture, state), (Some(capture), Some(state)) if !capture.failed && !state.closing)
    }

    /// Release the in-flight marker on both outcomes; a success re-checks the
    /// authority flag under the observer lock so a raced failure removes the
    /// created files instead of retaining a binding.
    fn settle_publication(
        shared: &Arc<Mutex<Capture>>,
        state: &Arc<Mutex<State>>,
        result: Result<codex_native::Published, &'static str>,
    ) {
        let Ok(mut capture) = shared.lock() else {
            if let Ok(published) = result {
                published.remove();
            }
            return;
        };
        capture.publishing = false;
        match result {
            Ok(published) => {
                if capture.failed {
                    drop(capture);
                    published.remove();
                    return;
                }
                match state.lock() {
                    Ok(mut state) if !state.closing => state.published = Some(published),
                    _ => {
                        drop(capture);
                        published.remove();
                    }
                }
            }
            Err(reason) => {
                eprintln!("remuda: Codex binding not published: {reason}");
                capture.failed = true;
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
        fn runtime_directories_are_fresh_and_private() {
            let first = runtime_dir().expect("first");
            let second = runtime_dir().expect("second");
            assert_ne!(first, second);
            for dir in [&first, &second] {
                let metadata = std::fs::symlink_metadata(dir).expect("stat");
                assert!(metadata.is_dir());
                assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
                assert_eq!(metadata.mode() & 0o077, 0);
            }
            let _ = std::fs::remove_dir(&first);
            let _ = std::fs::remove_dir(&second);
        }

        #[test]
        fn start_refuses_a_shared_runtime_directory() {
            let dir = std::env::temp_dir().join(format!("remuda-shared-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir(&dir).expect("dir");
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("0755");
            let publish = Publish {
                binding: PathBuf::from("/tmp/x/manifest.json"),
                nonce: "n".into(),
                session: "s".into(),
            };
            assert!(start(Path::new("/tmp/x.sock"), &dir, publish).is_err());
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn cleanup_without_an_inflight_adopt_returns_quickly() {
            let runtime = runtime_dir().expect("runtime");
            let tunnel = start(
                &runtime.join("app.sock"),
                &runtime,
                Publish {
                    binding: runtime.join("manifest.json"),
                    nonce: "n".into(),
                    session: "s".into(),
                },
            )
            .expect("tunnel");
            let started = Instant::now();
            tunnel.cleanup();
            assert!(started.elapsed() < Duration::from_secs(1));
            let _ = std::fs::remove_dir_all(&runtime);
        }

        #[test]
        fn a_failure_or_cleanup_fence_withdraws_authority() {
            let capture = capture();
            let state = capture.lock().expect("capture").state.clone();
            assert!(publication_permitted(&capture, &state));
            capture.lock().expect("capture").failed = true;
            assert!(!publication_permitted(&capture, &state));
            capture.lock().expect("capture").failed = false;
            state.lock().expect("state").closing = true;
            assert!(!publication_permitted(&capture, &state));
        }

        #[test]
        fn a_parse_failure_racing_adopt_removes_the_created_files() {
            let runtime = runtime_dir().expect("runtime");
            let capture = capture();
            let state = capture.lock().expect("capture").state.clone();
            capture.lock().expect("capture").publishing = true;
            let published = codex_native::fake_published(&runtime);
            assert!(runtime.join("manifest.json").exists());
            // The request observer latches failure while adopt is in flight.
            capture.lock().expect("capture").failed = true;
            settle_publication(&capture, &state, Ok(published));
            assert!(!runtime.join("manifest.json").exists());
            assert!(!runtime.join("pin.json").exists());
            assert!(state.lock().expect("state").published.is_none());
            assert!(!capture.lock().expect("capture").publishing);
            let _ = std::fs::remove_dir_all(&runtime);
        }

        #[test]
        fn a_completed_publication_is_stored_and_cleanup_is_immediate() {
            let runtime = runtime_dir().expect("runtime");
            let capture = capture();
            let state = capture.lock().expect("capture").state.clone();
            capture.lock().expect("capture").publishing = true;
            let published = codex_native::fake_published(&runtime);
            settle_publication(&capture, &state, Ok(published));
            assert!(state.lock().expect("state").published.is_some());
            assert!(!capture.lock().expect("capture").publishing);
            let tunnel = Tunnel {
                downstream: runtime.join("tui.sock"),
                downstream_id: (0, 0),
                state,
                capture,
            };
            let started = Instant::now();
            tunnel.cleanup();
            assert!(started.elapsed() < Duration::from_secs(1));
            assert!(!runtime.join("manifest.json").exists());
            assert!(!runtime.join("pin.json").exists());
            let _ = std::fs::remove_dir_all(&runtime);
        }

        #[test]
        fn a_reconnect_after_an_early_eof_cannot_publish() {
            let runtime = runtime_dir().expect("runtime");
            let upstream = runtime.join("app.sock");
            let upstream_listener = UnixListener::bind(&upstream).expect("upstream");
            let tunnel = start(
                &upstream,
                &runtime,
                Publish {
                    binding: runtime.join("manifest.json"),
                    nonce: "n".into(),
                    session: "s".into(),
                },
            )
            .expect("tunnel");
            {
                let mut first = UnixStream::connect(tunnel.downstream()).expect("first");
                let (_server, _) = upstream_listener.accept().expect("accept");
                first
                    .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
                    .expect("handshake");
            }
            // The first connection ended before any response, so the tunnel
            // must close its listener and refuse a replacement connection
            // instead of publishing from a later stream.
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && !tunnel.capture.lock().expect("capture").failed {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(tunnel.capture.lock().expect("capture").failed);
            loop {
                match UnixStream::connect(tunnel.downstream()) {
                    Err(_) => break,
                    Ok(_) if Instant::now() >= deadline => {
                        panic!("tunnel accepted a replacement connection")
                    }
                    Ok(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
            assert!(tunnel.state.lock().expect("state").published.is_none());
            tunnel.cleanup();
            let _ = std::fs::remove_dir_all(&runtime);
        }

        #[test]
        fn a_second_bootstrap_request_latches_unavailable() {
            let capture = capture();
            request_event(r#"{"id":"a","method":"thread/start"}"#, &capture);
            request_event(r#"{"id":"b","method":"thread/start"}"#, &capture);
            assert!(capture.lock().expect("capture").failed);
        }

        #[test]
        fn a_reused_bootstrap_id_latches_unavailable() {
            let capture = capture();
            request_event(r#"{"id":"a","method":"thread/start"}"#, &capture);
            request_event(r#"{"id":"a","method":"thread/start"}"#, &capture);
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

        fn text_frame(text: &str, masked: bool) -> Vec<u8> {
            let payload = text.as_bytes();
            let mask = [7u8, 11, 13, 17];
            let mut frame = vec![0x81];
            let mask_bit = if masked { 0x80 } else { 0 };
            if payload.len() < 126 {
                frame.push(mask_bit | payload.len() as u8);
            } else {
                assert!(payload.len() <= u16::MAX as usize);
                frame.push(mask_bit | 126);
                frame.extend((payload.len() as u16).to_be_bytes());
            }
            if masked {
                frame.extend(mask);
                frame.extend(
                    payload
                        .iter()
                        .enumerate()
                        .map(|(index, byte)| byte ^ mask[index % 4]),
                );
            } else {
                frame.extend(payload);
            }
            frame
        }

        fn max_plus_one_frame(masked: bool) -> Vec<u8> {
            let mut frame = vec![0x81, if masked { 0xFF } else { 0x7F }];
            frame.extend((super::MAX_FRAME + 1).to_be_bytes());
            frame.extend([1, 2, 3, 4]);
            frame
        }

        #[test]
        fn the_observer_forwards_through_a_parse_failure() {
            let capture = capture();
            let mut observer = Observer::new(Direction::Response, capture.clone());
            observer.feed(b"HTTP/1.1 101 Switching Protocols\r\n\r\n");
            // A fragmented/RSV frame freezes observation, not forwarding.
            observer.feed(&[0xF1, 0x80, 1, 2, 3, 4]);
            assert!(observer.disabled);
            assert!(capture.lock().expect("capture").failed);
            observer.feed(b"anything");
        }

        #[test]
        fn a_request_parse_failure_after_the_id_blocks_publication() {
            let capture = capture();
            let mut requests = Observer::new(Direction::Request, capture.clone());
            requests.feed(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
            requests.feed(&text_frame(
                r#"{"id":"startup-thread-start","method":"thread/start"}"#,
                true,
            ));
            assert_eq!(
                capture.lock().expect("capture").request_id,
                Some(json!("startup-thread-start"))
            );
            // An oversize request frame hides any later request, so the first
            // id can no longer be correlated with the response stream.
            requests.feed(&max_plus_one_frame(true));
            assert!(requests.disabled);
            assert!(capture.lock().expect("capture").failed);

            let mut responses = Observer::new(Direction::Response, capture.clone());
            responses.feed(b"HTTP/1.1 101 Switching Protocols\r\n\r\n");
            let ok = response(json!({
                "id": A, "sessionId": A, "ephemeral": false,
                "forkedFromId": null, "parentThreadId": null
            }));
            responses.feed(&text_frame(&ok.to_string(), false));
            let capture = capture.lock().expect("capture");
            assert!(!capture.publishing);
            assert!(capture.failed);
            assert!(capture.state.lock().expect("state").published.is_none());
        }

        #[test]
        fn a_header_overflow_latches_unavailable() {
            let capture = capture();
            let mut observer = Observer::new(Direction::Request, capture.clone());
            observer.feed(&vec![b'x'; super::MAX_HEADERS + 1]);
            assert!(observer.disabled);
            assert!(capture.lock().expect("capture").failed);
        }
    }
}

#[cfg(unix)]
pub(crate) use imp::{runtime_dir, start, Publish, Tunnel};

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

    pub(crate) fn runtime_dir() -> std::io::Result<PathBuf> {
        Err(std::io::Error::other("Codex native binding requires Unix"))
    }

    pub(crate) fn start(
        _upstream: &Path,
        _runtime_dir: &Path,
        _publish: Publish,
    ) -> std::io::Result<Tunnel> {
        Err(std::io::Error::other("Codex native binding requires Unix"))
    }
}

#[cfg(not(unix))]
pub(crate) use unsupported::{runtime_dir, start, Publish, Tunnel};
