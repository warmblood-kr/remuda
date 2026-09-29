//! Restricted request front for later remote transports.

use crate::ipc::TryClone as _;
use interprocess::local_socket::traits::ListenerExt as _;
use remuda_core::input::validate_batch;
use remuda_core::protocol::Request;
use std::io::{BufRead, Read, Write};
use std::path::Path;

/// Maximum single JSON frame accepted by this front (512 KiB).
pub const MAX_FRAME_BYTES: usize = 512 * 1024;
/// Maximum bytes accepted in one atomic input batch (64 KiB).
pub const MAX_INPUT_BYTES: usize = remuda_core::input::MAX_INPUT_BYTES;
/// Maximum bytes accepted in one remote Input batch (12 KiB).
pub const MAX_REMOTE_INPUT_BATCH_BYTES: usize = 12 * 1024;
/// Maximum simultaneous local front connections.
pub const MAX_CONNECTIONS: usize = 8;
/// A connection is closed if one request takes longer than this.
pub const CONNECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Decode and authorize exactly one JSON frame. This is the front's trust
/// boundary: callers must send the returned Request to the local daemon, never
/// forward the original bytes.
pub fn decode_frame(frame: &[u8]) -> Result<Request, String> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(format!("remote frame exceeds {MAX_FRAME_BYTES} bytes"));
    }
    let request: Request =
        serde_json::from_slice(frame).map_err(|_| "invalid remote request".to_owned())?;
    authorize(&request)?;
    Ok(request)
}

/// Forward an authorized frame to the existing local daemon and encode its
/// typed response afresh. Request bytes are never copied to the daemon socket.
pub fn forward_frame(path: &std::path::Path, frame: &[u8]) -> Result<Vec<u8>, String> {
    forward_frame_with_timeout(path, frame, std::time::Duration::from_secs(30))
}

/// Forward a frame with an explicit bound on the local daemon response wait.
pub fn forward_frame_with_timeout(
    path: &std::path::Path,
    frame: &[u8],
    timeout: std::time::Duration,
) -> Result<Vec<u8>, String> {
    let request = decode_frame(frame)?;
    let response = crate::client::request_with_timeout(path, &request, timeout)
        .map_err(|error| error.to_string())?;
    serde_json::to_vec(&response).map_err(|error| error.to_string())
}

/// Serve one request per connection on a local socket. This listener is not
/// network facing; a later authenticated transport can call `forward_frame`.
pub fn serve_local(path: &Path, daemon_path: &Path) -> std::io::Result<()> {
    let listener = listen_front(path)?;
    let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    loop {
        accept_front_connection(&listener, daemon_path, &active)?;
    }
}

fn listen_front(path: &Path) -> std::io::Result<crate::ipc::Listener> {
    let listener = crate::ipc::listen(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(listener)
}

fn accept_front_connection(
    listener: &crate::ipc::Listener,
    daemon_path: &Path,
    active: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> std::io::Result<()> {
    let stream = listener
        .incoming()
        .next()
        .ok_or_else(|| std::io::Error::other("local front listener closed"))??;
    dispatch_connection(stream, daemon_path, active)
}

fn dispatch_connection(
    mut stream: crate::ipc::Stream,
    daemon_path: &Path,
    active: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> std::io::Result<()> {
    let Some(slot) = ConnectionSlot::acquire(active) else {
        let mut response = serde_json::to_vec(&remuda_core::protocol::Response::error(
            "remote front is at connection capacity",
        ))
        .expect("response serializes");
        response.push(b'\n');
        stream.write_all(&response)?;
        return stream.flush();
    };
    let daemon_path = daemon_path.to_path_buf();
    std::thread::spawn(move || {
        let _slot = slot;
        let _ = serve_connection(stream, &daemon_path);
    });
    Ok(())
}

struct ConnectionSlot(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl ConnectionSlot {
    fn acquire(active: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Option<Self> {
        active
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |count| (count < MAX_CONNECTIONS).then_some(count + 1),
            )
            .ok()
            .map(|_| Self(std::sync::Arc::clone(active)))
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn serve_connection(mut stream: crate::ipc::Stream, daemon_path: &Path) -> std::io::Result<()> {
    let wake_stream = stream.try_clone()?;
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let timer = std::thread::spawn(move || {
        if done_rx.recv_timeout(CONNECTION_TIMEOUT).is_err() {
            crate::ipc::wake(&wake_stream);
        }
    });
    let result = (|| {
        let mut frame = Vec::new();
        let limited = (&mut stream).take((MAX_FRAME_BYTES + 1) as u64);
        let mut reader = std::io::BufReader::new(limited);
        reader.read_until(b'\n', &mut frame)?;
        if frame.last() == Some(&b'\n') {
            frame.pop();
        }
        let reply = match forward_frame(daemon_path, &frame) {
            Ok(response) => response,
            Err(reason) => serde_json::to_vec(&remuda_core::protocol::Response::error(reason))
                .expect("response serializes"),
        };
        stream.write_all(&reply)?;
        stream.write_all(b"\n")?;
        stream.flush()
    })();
    let _ = done_tx.send(());
    let _ = timer.join();
    result
}

/// Validate the deliberately small request surface. The explicit deny arms
/// make the compiler require a policy decision for every new Request variant.
/// Exhaustive on purpose: no wildcard arm, a new Request variant must be classified here.
pub fn authorize(request: &Request) -> Result<(), String> {
    match request {
        Request::List | Request::CaptureStyled { .. } => Ok(()),
        Request::Input {
            client_id,
            seq,
            bytes,
            ..
        } => {
            if bytes.len() > MAX_REMOTE_INPUT_BATCH_BYTES {
                return Err(format!(
                    "remote Input batch exceeds {MAX_REMOTE_INPUT_BATCH_BYTES} bytes"
                ));
            }
            validate_batch(client_id, *seq, bytes).map(|_| ())
        }
        Request::New { .. } => Err(refusal("New")),
        Request::SendLine { .. } => Err(refusal("SendLine")),
        Request::Send { .. } => Err(refusal("Send")),
        Request::Feed { .. } => Err(refusal("Feed")),
        Request::Resize { .. } => Err(refusal("Resize")),
        Request::Capture { .. } => Err(refusal("Capture")),
        Request::MouseState { .. } => Err(refusal("MouseState")),
        Request::Attach { .. } => Err(refusal("Attach")),
        Request::AttachTracked { .. } => Err(refusal("AttachTracked")),
        Request::AttachStatus { .. } => Err(refusal("AttachStatus")),
        Request::Close { .. } => Err(refusal("Close")),
        Request::ListDir { .. } => Err(refusal("ListDir")),
        Request::Mkdir { .. } => Err(refusal("Mkdir")),
        Request::RemoveDirAll { .. } => Err(refusal("RemoveDirAll")),
        Request::Version => Err(refusal("Version")),
        Request::Shutdown { .. } => Err(refusal("Shutdown")),
        Request::Eval { .. } => Err(refusal("Eval")),
    }
}

fn refusal(variant: &str) -> String {
    format!("remote front refuses {variant}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_core::protocol::Request;

    #[test]
    fn permits_only_the_read_and_batched_input_surface() {
        assert!(authorize(&Request::List).is_ok());
        assert!(authorize(&Request::CaptureStyled {
            name: "dev".into(),
            scrollback: 0
        })
        .is_ok());
        assert!(authorize(&Request::Input {
            name: "dev".into(),
            instance_id: "instance".into(),
            client_id: "00000000000000000000000000000001".into(),
            seq: 1,
            bytes: b"hello\r".to_vec()
        })
        .is_ok());
        assert!(authorize(&Request::Eval {
            code: "return 1".into(),
            name: None
        })
        .is_err());
        assert!(authorize(&Request::Send {
            name: "dev".into(),
            bytes: vec![0]
        })
        .is_err());
        assert!(authorize(&Request::Feed {
            name: "dev".into(),
            steps: vec![]
        })
        .is_err());
    }

    #[test]
    fn refuses_every_other_request_variant() {
        let dangerous = [
            Request::New {
                name: None,
                command: vec![],
                size: Default::default(),
                cwd: None,
                env: None,
            },
            Request::SendLine {
                name: "dev".into(),
                text: "hello".into(),
            },
            Request::Resize {
                name: "dev".into(),
                size: Default::default(),
            },
            Request::Capture { name: "dev".into() },
            Request::MouseState { name: "dev".into() },
            Request::Attach { name: "dev".into() },
            Request::AttachTracked { name: "dev".into() },
            Request::AttachStatus {
                name: "dev".into(),
                generation: 0,
            },
            Request::Close { name: "dev".into() },
            Request::ListDir { path: "/".into() },
            Request::Mkdir { path: "/".into() },
            Request::RemoveDirAll { path: "/".into() },
            Request::Version,
            Request::Shutdown {
                requester_daemon_id: None,
                requester_session_id: None,
                requester_session_name: None,
                override_hosted: false,
            },
        ];
        for request in dangerous {
            let encoded = serde_json::to_vec(&request).unwrap();
            assert!(
                decode_frame(&encoded).is_err(),
                "unexpectedly authorized {request:?}"
            );
        }
    }

    #[test]
    fn malformed_request_errors_do_not_echo_payload_fragments() {
        let error = decode_frame(br#"{"Eval":{"code":"SECRET_PAYLOAD_XYZ""#).unwrap_err();
        assert_eq!(error, "invalid remote request");
        assert!(!error.contains("SECRET_PAYLOAD_XYZ"));
    }

    #[test]
    fn refusal_names_the_variant_without_echoing_its_contents() {
        let error = authorize(&Request::Send {
            name: "private-session-name".into(),
            bytes: b"secret-input-payload".to_vec(),
        })
        .unwrap_err();
        assert_eq!(error, "remote front refuses Send");
    }

    #[test]
    fn rejects_oversize_frames_and_remote_batches() {
        assert!(decode_frame(&vec![b' '; MAX_FRAME_BYTES + 1])
            .unwrap_err()
            .contains("exceeds"));
        let oversized = Request::Input {
            name: "dev".into(),
            instance_id: "instance".into(),
            client_id: "00000000000000000000000000000001".into(),
            seq: 1,
            bytes: vec![0; MAX_INPUT_BYTES + 1],
        };
        assert!(decode_frame(&serde_json::to_vec(&oversized).unwrap())
            .unwrap_err()
            .contains("remote Input batch exceeds"));
        let empty = Request::Input {
            name: "dev".into(),
            instance_id: "instance".into(),
            client_id: "00000000000000000000000000000001".into(),
            seq: 1,
            bytes: Vec::new(),
        };
        assert!(decode_frame(&serde_json::to_vec(&empty).unwrap())
            .unwrap_err()
            .contains("input"));
    }

    #[test]
    fn remote_input_batch_limit_accepts_12288_and_refuses_12289() {
        let request = |length| Request::Input {
            name: "dev".into(),
            instance_id: "instance".into(),
            client_id: "00000000000000000000000000000001".into(),
            seq: 1,
            bytes: vec![u8::MAX; length],
        };
        assert!(
            decode_frame(&serde_json::to_vec(&request(MAX_REMOTE_INPUT_BATCH_BYTES)).unwrap())
                .is_ok()
        );
        assert_eq!(
            decode_frame(&serde_json::to_vec(&request(MAX_REMOTE_INPUT_BATCH_BYTES + 1)).unwrap())
                .unwrap_err(),
            format!("remote Input batch exceeds {MAX_REMOTE_INPUT_BATCH_BYTES} bytes")
        );
    }

    #[test]
    fn input_validation_errors_never_echo_batch_bytes() {
        let request = Request::Input {
            name: "dev".into(),
            instance_id: "instance".into(),
            client_id: "malformed".into(),
            seq: 0,
            bytes: b"secret-input-payload".to_vec(),
        };
        let error = authorize(&request).unwrap_err();
        assert!(!error.contains("secret-input-payload"));
        assert!(error.contains("client_id"));
    }

    fn test_socket_path() -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("rf-{}-{stamp}", std::process::id()));
        crate::daemon::socket_path_in(&base, "front")
    }

    fn cleanup_test_socket(path: &Path) {
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(path);
            if let Some(parent) = path.parent() {
                let _ = std::fs::remove_dir(parent);
                if let Some(base) = parent.parent() {
                    let _ = std::fs::remove_dir(base);
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_front_socket_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let path = test_socket_path();
        let listener = listen_front(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        drop(listener);
        cleanup_test_socket(&path);
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn stalled_partial_frame_closes_at_connection_timeout() {
        use std::io::{Read, Write};
        let path = test_socket_path();
        let listener = listen_front(&path).unwrap();
        let socket = path.clone();
        let worker = std::thread::spawn(move || {
            let stream = listener.incoming().next().unwrap().unwrap();
            let _ = serve_connection(stream, Path::new("unused-daemon-socket"));
        });
        let mut client = crate::ipc::connect(&socket).unwrap();
        client.write_all(b"{\"type\":").unwrap();
        let start = std::time::Instant::now();
        let mut byte = [0];
        let result = client.read(&mut byte);
        let elapsed = start.elapsed();
        let _ = worker.join();
        cleanup_test_socket(&socket);
        assert!(
            matches!(result, Err(_) | Ok(0)),
            "stalled connection stayed open"
        );
        assert!(elapsed <= CONNECTION_TIMEOUT + std::time::Duration::from_secs(2));
    }

    #[test]
    fn connection_after_the_limit_is_refused_while_slots_are_occupied() {
        use std::io::{Read, Write};
        let path = test_socket_path();
        let listener = listen_front(&path).unwrap();
        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_active = std::sync::Arc::clone(&active);
        let socket = path.clone();
        let worker = std::thread::spawn(move || {
            for _ in 0..=MAX_CONNECTIONS {
                accept_front_connection(
                    &listener,
                    Path::new("unused-daemon-socket"),
                    &worker_active,
                )
                .unwrap();
            }
        });
        let mut clients = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            let mut client = crate::ipc::connect(&socket).unwrap();
            client.write_all(b"{").unwrap();
            clients.push(client);
        }
        let mut overflow = crate::ipc::connect(&socket).unwrap();
        let _ = worker.join();
        let mut reply = Vec::new();
        overflow.read_to_end(&mut reply).unwrap();
        let response: remuda_core::protocol::Response = serde_json::from_slice(&reply).unwrap();
        assert!(matches!(
            response,
            remuda_core::protocol::Response::Error(_)
        ));
        drop(clients);
        cleanup_test_socket(&socket);
    }
}
