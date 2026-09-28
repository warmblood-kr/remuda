//! Restricted request front for later remote transports.

use crate::ipc::TryClone as _;
use interprocess::local_socket::traits::ListenerExt as _;
use remuda_core::protocol::Request;
use std::io::{BufRead, Read, Write};
use std::path::Path;

/// Maximum single JSON frame accepted by this front (64 KiB).
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Maximum number of actions in a future batch input request.
pub const MAX_BATCH_ITEMS: usize = 64;
/// A connection is closed if one request takes longer than this.
pub const CONNECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Decode and authorize exactly one JSON frame. This is the front's trust
/// boundary: callers must send the returned Request to the local daemon, never
/// forward the original bytes.
pub fn decode_frame(frame: &[u8]) -> Result<Request, String> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(format!("remote frame exceeds {MAX_FRAME_BYTES} bytes"));
    }
    let request: Request = serde_json::from_slice(frame)
        .map_err(|error| format!("invalid remote request: {error}"))?;
    authorize(&request)?;
    Ok(request)
}

/// Forward an authorized frame to the existing local daemon and encode its
/// typed response afresh. Request bytes are never copied to the daemon socket.
pub fn forward_frame(path: &std::path::Path, frame: &[u8]) -> Result<Vec<u8>, String> {
    let request = decode_frame(frame)?;
    let response = crate::client::request(path, &request).map_err(|error| error.to_string())?;
    serde_json::to_vec(&response).map_err(|error| error.to_string())
}

/// Serve one request per connection on a local socket. This listener is not
/// network facing; a later authenticated transport can call `forward_frame`.
pub fn serve_local(path: &Path, daemon_path: &Path) -> std::io::Result<()> {
    let listener = crate::ipc::listen(path)?;
    for accepted in listener.incoming() {
        let stream = match accepted {
            Ok(stream) => stream,
            Err(_) => continue,
        };
        let daemon_path = daemon_path.to_path_buf();
        std::thread::spawn(move || {
            let _ = serve_connection(stream, &daemon_path);
        });
    }
    Ok(())
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
pub fn authorize(request: &Request) -> Result<(), String> {
    match request {
        Request::List | Request::CaptureStyled { .. } => Ok(()),
        Request::Input { lines, .. } if !lines.is_empty() && lines.len() <= MAX_BATCH_ITEMS => {
            Ok(())
        }
        Request::Input { .. } => Err(format!(
            "remote input batch must contain 1 to {MAX_BATCH_ITEMS} lines"
        )),
        Request::New { .. }
        | Request::SendLine { .. }
        | Request::Send { .. }
        | Request::Feed { .. }
        | Request::Resize { .. }
        | Request::Capture { .. }
        | Request::MouseState { .. }
        | Request::Attach { .. }
        | Request::AttachTracked { .. }
        | Request::AttachStatus { .. }
        | Request::Close { .. }
        | Request::ListDir { .. }
        | Request::Mkdir { .. }
        | Request::RemoveDirAll { .. }
        | Request::Version
        | Request::Shutdown
        | Request::Eval { .. } => Err(format!("remote front refuses {request:?}")),
    }
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
            lines: vec!["hello".into()]
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
            Request::Shutdown,
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
    fn rejects_oversize_frames_and_batches() {
        assert!(decode_frame(&vec![b' '; MAX_FRAME_BYTES + 1])
            .unwrap_err()
            .contains("exceeds"));
        let oversized = Request::Input {
            name: "dev".into(),
            lines: vec![String::new(); MAX_BATCH_ITEMS + 1],
        };
        assert!(decode_frame(&serde_json::to_vec(&oversized).unwrap())
            .unwrap_err()
            .contains("batch"));
    }
}
