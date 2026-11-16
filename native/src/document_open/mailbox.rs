//! One bounded worker for one opt-in viewer; no new daemon protocol or listener.
use super::{Open, Receipt};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use remuda_core::protocol::{Request, Response};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io,
    path::Path,
    sync::{mpsc, Arc},
    time::Duration,
};

pub(super) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |v| v.as_secs())
}

pub(super) fn start(
    socket: &Path,
    target: &str,
    instance: &str,
    node: &str,
    guard: Arc<std::fs::File>,
) -> io::Result<(mpsc::Receiver<Open>, mpsc::SyncSender<Receipt>)> {
    let (send, incoming) = mpsc::sync_channel(1);
    let (outgoing, results) = mpsc::sync_channel::<Receipt>(8);
    let socket = socket.to_owned();
    let base = format!(
        "remuda:///ade-open/{:x}/{target}",
        Sha256::digest(node.as_bytes())
    );
    let node = node.to_owned();
    let instance = instance.to_owned();
    std::thread::Builder::new().name("ade-document-inbox".into()).spawn(move || {
        // Retain ownership through final in-flight IPC during UI teardown.
        let _guard = guard;
        let mut lease_at = 0;
        let mut receipt = None;
        loop {
            match results.recv_timeout(Duration::from_millis(300)) {
                Ok(result) => receipt = Some(serde_json::to_vec(&result).expect("receipt serializes")),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if let Some(bytes) = &receipt {
                if put(&socket, &format!("{base}/result.json"), bytes).is_ok() {
                    receipt = None;
                }
            }
            if now() >= lease_at {
                let ready = json!({"node":node,"base":base,"instance":instance,"expires":now()+45,"mode":"direct","renderer":"tui-text-html-v1"});
                if put(&socket, &format!("{base}/ready.json"), ready.to_string().as_bytes()).is_ok() {
                    lease_at = now()+15;
                }
            }
            if let Ok(request) = read(&socket, &format!("{base}/request.json")) {
                match send.try_send(request) {
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                    Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                }
            }
        }
    })?;
    Ok((incoming, outgoing))
}

fn call(socket: &Path, args: Vec<String>, bytes: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let response = crate::client::request_with_deadline(
        socket,
        &Request::Vfs {
            args,
            input_base64: bytes.map(|b| BASE64.encode(b)),
        },
        Duration::from_secs(3),
    )
    .map_err(|e| e.to_string())?;
    let Response::CommandResult {
        exit_code: 0,
        stdout_base64,
        ..
    } = response
    else {
        return Err("mailbox VFS command failed".into());
    };
    BASE64.decode(stdout_base64).map_err(|e| e.to_string())
}

fn put(socket: &Path, uri: &str, bytes: &[u8]) -> Result<(), String> {
    call(
        socket,
        vec!["put".into(), "-".into(), uri.into()],
        Some(bytes),
    )
    .map(|_| ())
}

fn read(socket: &Path, uri: &str) -> Result<Open, String> {
    let bytes = call(
        socket,
        vec![
            "read".into(),
            uri.into(),
            "--max-bytes".into(),
            "4096".into(),
        ],
        None,
    )?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if value["ok"] != true || value["path"] != uri.trim_start_matches("remuda:///") {
        return Err("mailbox read identity mismatch".into());
    }
    let content = value["content"].as_str().ok_or("missing mailbox content")?;
    if content.len() > 4096usize.div_ceil(3) * 4 {
        return Err("oversized mailbox".into());
    }
    let bytes = BASE64.decode(content).map_err(|e| e.to_string())?;
    if bytes.len() > 4096 || value["size"].as_u64() != Some(bytes.len() as u64) {
        return Err("invalid mailbox size".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}
