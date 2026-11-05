//! Raw owning UDS tunnel for the Codex TUI.
//!
//! The TUI connects to our downstream socket; we connect to the app-server and
//! forward every byte unchanged. A bounded copy of the first `thread/start`
//! request and its single correlated response is inspected, so only the launch's
//! own initial thread can be pinned. Capture failure never interrupts forwarding;
//! it only leaves the binding unpublished.

#![allow(dead_code)]

#[cfg(unix)]
mod imp {
    use super::super::codex_native;
    use serde_json::Value;
    use std::io::{Read, Write};
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
        pub downstream: PathBuf,
        pub state: Arc<Mutex<State>>,
    }

    impl Tunnel {
        pub(crate) fn cleanup(&self) {
            let taken = self.state.lock().ok().and_then(|mut state| {
                state.closing = true;
                state.published.take()
            });
            if let Some(published) = taken {
                published.remove();
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

    /// Bind the downstream listener and start forwarding in the background.
    pub(crate) fn start(upstream: &Path, publish: Publish) -> std::io::Result<Tunnel> {
        let downstream = upstream.with_extension("tui.sock");
        let _ = std::fs::remove_file(&downstream);
        let listener = UnixListener::bind(&downstream)?;
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
            let state = state.clone();
            move || {
                if serve(listener, capture).is_err() {
                    // Forwarding stops only when the connection does; a capture
                    // failure is already latched, not an ownership proof.
                }
                drop(state);
            }
        });
        Ok(Tunnel { downstream, state })
    }

    fn serve(listener: UnixListener, capture: Arc<Mutex<Capture>>) -> std::io::Result<()> {
        let (client, _) = listener.accept()?;
        let upstream = UnixStream::connect(&capture.lock().expect("capture").upstream)?;
        let client_to_server = client.try_clone()?;
        let server_to_client = upstream.try_clone()?;
        let request_capture = capture.clone();
        let response_capture = capture.clone();
        let requests = std::thread::spawn(move || {
            relay(client_to_server, upstream, |text| {
                request_event(text, &request_capture)
            })
        });
        let responses = std::thread::spawn(move || {
            relay(server_to_client, client, |text| {
                response_event(text, &response_capture)
            })
        });
        let _ = requests.join();
        let _ = responses.join();
        Ok(())
    }

    /// Forward one direction byte-for-byte; inspect text frames only.
    fn relay(mut from: UnixStream, mut to: UnixStream, mut on_text: impl FnMut(&str)) {
        if copy_headers(&mut from, &mut to).is_err() {
            return;
        }
        loop {
            match read_frame(&mut from, &mut to) {
                Ok(Some(text)) => on_text(&text),
                Ok(None) => {}
                Err(_) => return,
            }
        }
    }

    fn copy_headers(from: &mut UnixStream, to: &mut UnixStream) -> std::io::Result<()> {
        let mut headers = Vec::new();
        let mut byte = [0; 1];
        while !headers.ends_with(b"\r\n\r\n") {
            from.read_exact(&mut byte)?;
            headers.push(byte[0]);
            if headers.len() > MAX_HEADERS {
                return Err(std::io::Error::other("oversized WebSocket headers"));
            }
        }
        to.write_all(&headers)
    }

    fn read_frame(from: &mut UnixStream, to: &mut UnixStream) -> std::io::Result<Option<String>> {
        let mut header = [0; 2];
        from.read_exact(&mut header)?;
        let opcode = header[0] & 0x0f;
        let masked = header[1] & 0x80 != 0;
        let mut length = (header[1] & 0x7f) as u64;
        let mut prefix = header.to_vec();
        if length == 126 {
            let mut extended = [0; 2];
            from.read_exact(&mut extended)?;
            prefix.extend_from_slice(&extended);
            length = u16::from_be_bytes(extended) as u64;
        } else if length == 127 {
            let mut extended = [0; 8];
            from.read_exact(&mut extended)?;
            prefix.extend_from_slice(&extended);
            length = u64::from_be_bytes(extended);
        }
        if length > MAX_FRAME || (opcode >= 8 && length > 125) {
            return Err(std::io::Error::other("oversized WebSocket frame"));
        }
        let mut mask = [0; 4];
        if masked {
            from.read_exact(&mut mask)?;
            prefix.extend_from_slice(&mask);
        }
        let mut payload = vec![0; length as usize];
        from.read_exact(&mut payload)?;
        to.write_all(&prefix)?;
        to.write_all(&payload)?;
        if opcode != 1 {
            return Ok(None);
        }
        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        String::from_utf8(payload)
            .map(Some)
            .map_err(|_| std::io::Error::other("non-utf8 text frame"))
    }

    fn request_event(text: &str, capture: &Arc<Mutex<Capture>>) {
        let Ok(message) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if message.get("method").and_then(Value::as_str) != Some("thread/start") {
            return;
        }
        let Some(id) = message.get("id").cloned() else {
            return;
        };
        let Ok(mut capture) = capture.lock() else {
            return;
        };
        if capture.failed || capture.publishing || published(&capture) {
            return;
        }
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
        pub(crate) fn cleanup(&self) {}
    }

    pub(crate) fn start(_upstream: &Path, _publish: Publish) -> std::io::Result<Tunnel> {
        Err(std::io::Error::other("Codex native binding requires Unix"))
    }
}

#[cfg(not(unix))]
pub(crate) use unsupported::{start, Publish};
