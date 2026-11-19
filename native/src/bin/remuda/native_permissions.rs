//! Read-only OpenCode permission list for Butler's adopted sessions.
//!
//! Reads only the private binding manifests, asks each local OpenCode server
//! for pending permission requests, and prints metadata. It never replies,
//! creates, or writes. Credentials, nonces and endpoints are never printed
//! or logged.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use remuda_native::net::http_client::{perform, HttpRequest};

const PROTOCOLS: [&str; 2] = ["opencode-http-v1", "opencode-http-adopt-v1"];
const MAX_TEXT: usize = 120;
const MAX_MANIFESTS: usize = 256;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

#[derive(Debug, Deserialize)]
struct Manifest {
    schema: u64,
    protocol: String,
    nonce: String,
    session: String,
    server_pid: u32,
    server_version: String,
    generation: String,
    endpoint: String,
    conversation: String,
    auth: Auth,
}

#[derive(Debug, Deserialize)]
struct Auth {
    scheme: String,
    user: String,
    password: String,
}

#[derive(Deserialize)]
struct Permission {
    id: String,
    #[serde(rename = "sessionID")]
    session_id: String,
    action: String,
    #[serde(default)]
    resources: Vec<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct PermissionList {
    #[serde(default)]
    data: Vec<Permission>,
}

#[derive(Debug, Serialize)]
struct Row {
    id: String,
    session: String,
    opencode_session: String,
    action: String,
    resources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    status: &'static str,
    decision: &'static str,
}

fn default_root() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("REMUDA_BUTLER_BINDINGS_DIR") {
        return Some(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(path).join("remuda/butler/bindings"));
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local/share/remuda/butler/bindings"))
}

fn safe_word(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.')
}

fn valid_endpoint(endpoint: &str) -> bool {
    let Some(rest) = endpoint.strip_prefix("http://127.0.0.1:") else {
        return false;
    };
    if rest.is_empty() || !rest.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    matches!(rest.parse::<u16>(), Ok(port) if port > 0)
}

fn valid_conversation(value: &str) -> bool {
    value.starts_with("ses_")
        && value.len() <= 128
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn pid_alive(pid: u32) -> bool {
    if pid <= 1 || pid > i32::MAX as u32 {
        return false;
    }
    #[cfg(unix)]
    {
        // Signal 0 performs the permission/existence check and sends nothing.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn validate(manifest: &Manifest) -> Result<(), &'static str> {
    if manifest.schema != 1 {
        return Err("schema");
    }
    if !PROTOCOLS.contains(&manifest.protocol.as_str()) {
        return Err("protocol");
    }
    if !safe_word(&manifest.nonce, 128) || !safe_word(&manifest.generation, 128) {
        return Err("binding_identity");
    }
    if manifest.session.is_empty()
        || manifest.session.len() > 128
        || manifest.session.chars().any(|ch| ch.is_control())
    {
        return Err("session");
    }
    if manifest.server_version.is_empty() || manifest.server_version.len() > 64 {
        return Err("server_version");
    }
    if !pid_alive(manifest.server_pid) {
        return Err("server_pid");
    }
    if !valid_endpoint(&manifest.endpoint) {
        return Err("endpoint");
    }
    if !valid_conversation(&manifest.conversation) {
        return Err("conversation");
    }
    if !manifest.auth.scheme.eq_ignore_ascii_case("basic")
        || manifest.auth.user.is_empty()
        || manifest.auth.password.is_empty()
    {
        return Err("auth");
    }
    Ok(())
}

fn parse_manifest(bytes: &[u8]) -> Result<Manifest, &'static str> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| "manifest_json")?;
    let protocol = value
        .get("protocol")
        .and_then(Value::as_str)
        .ok_or("manifest_json")?;
    if !PROTOCOLS.contains(&protocol) {
        // Codex bindings and future protocols are not malformed; this list
        // only reads OpenCode.
        return Err("unsupported_protocol");
    }
    serde_json::from_value(value).map_err(|_| "manifest_json")
}

fn read_manifest(path: &Path) -> Result<Manifest, &'static str> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "manifest_io")?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_MANIFEST_BYTES {
        return Err("manifest_shape");
    }
    let bytes = fs::read(path).map_err(|_| "manifest_io")?;
    parse_manifest(&bytes)
}

fn manifests(root: &Path) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    let Ok(sessions) = fs::read_dir(root) else {
        return found;
    };
    for session in sessions.flatten().take(MAX_MANIFESTS) {
        if !session
            .file_type()
            .map(|kind| kind.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let session_name = session.file_name().to_string_lossy().into_owned();
        let Ok(nonces) = fs::read_dir(session.path()) else {
            continue;
        };
        for nonce in nonces.flatten().take(MAX_MANIFESTS) {
            if !nonce.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            let path = nonce.path().join("manifest.json");
            if fs::symlink_metadata(&path)
                .map(|meta| meta.file_type().is_file())
                .unwrap_or(false)
            {
                found.push((session_name.clone(), path));
            }
        }
    }
    found.sort();
    found.truncate(MAX_MANIFESTS);
    found
}

fn sanitize(value: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut space = false;
    for ch in value.chars() {
        if ch.is_control() || ch.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(ch);
        if out.chars().count() >= limit {
            break;
        }
    }
    out
}

fn auth_header(auth: &Auth) -> String {
    let encoded = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", auth.user, auth.password));
    format!("Basic {encoded}")
}

fn pending(manifest: &Manifest) -> Result<Vec<Row>, &'static str> {
    let url = format!(
        "{}/api/session/{}/permission",
        manifest.endpoint, manifest.conversation
    );
    let request = HttpRequest {
        method: "GET".into(),
        url,
        headers: vec![
            (
                "Authorization".into(),
                auth_header(&manifest.auth).into_bytes(),
            ),
            ("Accept".into(), b"application/json".to_vec()),
        ],
        body: Vec::new(),
        timeout: Duration::from_secs(3),
        connect_timeout: Duration::from_secs(2),
        max_bytes: 256 * 1024,
        ca_file: None,
        pin: None,
        pin_only: false,
    };
    let response = perform(request, None).map_err(|_| "transport")?;
    if response.status != 200 {
        return Err("http_status");
    }
    let body: PermissionList = serde_json::from_slice(&response.body).map_err(|_| "malformed")?;
    let mut rows = Vec::new();
    for item in body.data {
        if !item.id.starts_with("per_") || item.session_id != manifest.conversation {
            return Err("session_mismatch");
        }
        rows.push(Row {
            id: sanitize(&item.id, 64),
            session: manifest.session.clone(),
            opencode_session: manifest.conversation.clone(),
            action: sanitize(&item.action, 64),
            resources: item
                .resources
                .iter()
                .take(4)
                .map(|value| sanitize(value, 80))
                .collect(),
            message: item
                .message
                .as_deref()
                .map(|value| sanitize(value, MAX_TEXT))
                .filter(|value| !value.is_empty()),
            status: "pending",
            decision: "once | reject",
        });
    }
    rows.sort_by(|a, b| {
        (a.session.as_str(), a.id.as_str()).cmp(&(b.session.as_str(), b.id.as_str()))
    });
    Ok(rows)
}

fn help() -> ExitCode {
    println!(
        "remuda _native_permissions [--json] [--root DIR]\n\
         Read-only list of pending OpenCode permission requests from Butler bindings.\n\
         It never replies or writes; credentials and nonces are not printed."
    );
    ExitCode::SUCCESS
}

pub fn run(args: &[&str]) -> ExitCode {
    let mut json_out = false;
    let mut root = None;
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "--help" | "-h" => return help(),
            "--json" => json_out = true,
            "--root" => {
                index += 1;
                match args.get(index) {
                    Some(path) if !path.is_empty() => root = Some(PathBuf::from(path)),
                    _ => {
                        eprintln!("remuda: _native_permissions: --root needs a directory");
                        return ExitCode::FAILURE;
                    }
                }
            }
            other => {
                eprintln!("remuda: _native_permissions: unknown argument {other}");
                return ExitCode::FAILURE;
            }
        }
        index += 1;
    }
    let root = match root.or_else(default_root) {
        Some(root) => root,
        None => {
            eprintln!("remuda: _native_permissions: no bindings directory");
            return ExitCode::FAILURE;
        }
    };
    if !root.is_dir() {
        eprintln!(
            "remuda: _native_permissions: no bindings directory at {}",
            root.display()
        );
        return ExitCode::FAILURE;
    }

    let mut bindings = Vec::new();
    let mut rows = Vec::new();
    for (session, path) in manifests(&root) {
        match read_manifest(&path).and_then(|manifest| {
            validate(&manifest)?;
            let rows = pending(&manifest)?;
            Ok((manifest, rows))
        }) {
            Ok((_manifest, mut found)) => {
                rows.append(&mut found);
                bindings.push(json!({"session": session, "status": "ok"}));
            }
            Err(reason) => {
                bindings
                    .push(json!({"session": session, "status": "unavailable", "reason": reason}));
            }
        }
    }

    if json_out {
        let report = json!({
            "schema": 1,
            "authority": "butler-local",
            "root": root.display().to_string(),
            "bindings": bindings,
            "rows": rows,
        });
        println!("{report}");
        return ExitCode::SUCCESS;
    }

    for binding in &bindings {
        if binding["status"].as_str() == Some("unavailable") {
            println!(
                "{}: unavailable ({})",
                binding["session"].as_str().unwrap_or("?"),
                binding["reason"].as_str().unwrap_or("?")
            );
        }
    }
    if rows.is_empty() {
        println!("No pending permission requests in reachable bindings.");
        return ExitCode::SUCCESS;
    }
    println!("SESSION  SOURCE  ACTION  RESOURCE  STATUS  DECISION");
    for row in rows {
        println!(
            "{}  {}  {}  {}  {}  {}",
            row.session,
            row.opencode_session,
            row.action,
            if row.resources.is_empty() {
                "-".to_string()
            } else {
                row.resources.join("; ")
            },
            row.status,
            row.decision
        );
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn manifest() -> Manifest {
        Manifest {
            schema: 1,
            protocol: "opencode-http-v1".into(),
            nonce: "a".repeat(32),
            session: "synthetic".into(),
            server_pid: std::process::id(),
            server_version: "2.2.5".into(),
            generation: "g".repeat(16),
            endpoint: "http://127.0.0.1:41234".into(),
            conversation: "ses_abc123".into(),
            auth: Auth {
                scheme: "basic".into(),
                user: "opencode".into(),
                password: "secret".into(),
            },
        }
    }

    fn serve(status: &str, body: &str) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(response.as_bytes());
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    #[test]
    fn validates_a_binding() {
        assert!(validate(&manifest()).is_ok());
    }

    #[test]
    fn rejects_bad_bindings() {
        let mut cases = Vec::new();
        let mut bad = manifest();
        bad.protocol = "other".into();
        cases.push(bad);
        let mut bad = manifest();
        bad.endpoint = "http://127.0.0.1:41234/path".into();
        cases.push(bad);
        let mut bad = manifest();
        bad.endpoint = "http://192.168.1.2:41234".into();
        cases.push(bad);
        let mut bad = manifest();
        bad.conversation = "ses_/bad".into();
        cases.push(bad);
        let mut bad = manifest();
        bad.server_pid = u32::MAX;
        cases.push(bad);
        let mut bad = manifest();
        bad.auth.scheme = "bearer".into();
        cases.push(bad);
        for case in cases {
            assert!(validate(&case).is_err());
        }
    }

    #[test]
    fn codex_binding_is_unsupported_not_malformed() {
        let codex = br#"{"schema":1,"protocol":"codex-app-server-v1","nonce":"n","session":"s","mode":"m","generation":"g","socket":"/tmp/x","thread":"t"}"#;
        assert_eq!(parse_manifest(codex).unwrap_err(), "unsupported_protocol");
        assert_eq!(parse_manifest(b"not json").unwrap_err(), "manifest_json");
    }

    #[test]
    fn sanitizes_and_limits() {
        let value = sanitize("a\nb\tc  d", 10);
        assert_eq!(value, "a b c d");
        assert_eq!(sanitize("0123456789abcdef", 4), "0123");
    }

    #[test]
    fn reads_pending_rows() {
        let body = r#"{"data":[{"id":"per_1","sessionID":"ses_abc123","action":"bash","resources":["echo hi"],"message":"run it"}]}"#;
        let (endpoint, handle) = serve("200 OK", body);
        let mut valid = manifest();
        valid.endpoint = endpoint;
        let rows = pending(&valid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "bash");
        assert_eq!(rows[0].resources, vec!["echo hi"]);
        handle.join().unwrap();
    }

    #[test]
    fn rejects_mismatched_session() {
        let body =
            r#"{"data":[{"id":"per_1","sessionID":"ses_other","action":"bash","resources":[]}]}"#;
        let (endpoint, handle) = serve("200 OK", body);
        let mut valid = manifest();
        valid.endpoint = endpoint;
        assert_eq!(pending(&valid).unwrap_err(), "session_mismatch");
        handle.join().unwrap();
    }

    #[test]
    fn http_errors_are_not_empty() {
        let (endpoint, handle) = serve("401 Unauthorized", "{}");
        let mut valid = manifest();
        valid.endpoint = endpoint;
        assert_eq!(pending(&valid).unwrap_err(), "http_status");
        handle.join().unwrap();
    }

    #[test]
    fn malformed_is_not_empty() {
        let (endpoint, handle) = serve("200 OK", "not json");
        let mut valid = manifest();
        valid.endpoint = endpoint;
        assert_eq!(pending(&valid).unwrap_err(), "malformed");
        handle.join().unwrap();
    }

    #[test]
    fn json_does_not_print_the_password() {
        let report = json!({
            "schema": 1,
            "authority": "butler-local",
            "bindings": [{"session": "synthetic", "status": "ok"}],
            "rows": [],
        });
        let encoded = report.to_string();
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("password"));
        let value: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["authority"], "butler-local");
    }
}
