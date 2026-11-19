//! Read-only OpenCode permission list for Butler's adopted sessions.
//!
//! Reads only the private binding manifests, asks each local OpenCode server
//! for pending permission requests, and returns metadata. It never replies,
//! creates, or writes. Credentials, nonces and endpoints are never returned
//! or logged.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::net::http_client::{perform, HttpRequest};

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
    // No `default`: OpenCode always sends `data`, so a 200 `{}` is a
    // different service on a reused loopback port, not an empty list.
    data: Vec<Permission>,
}

#[derive(Debug, Serialize)]
pub struct BindingStatus {
    pub session: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct Row {
    pub id: String,
    pub session: String,
    pub opencode_session: String,
    pub action: String,
    pub resources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub status: &'static str,
    pub decision: &'static str,
}

/// One read-only observation. An empty `rows` list with an `ok` binding means
/// there are no pending requests; any `unavailable` binding is an error, not
/// an empty result.
#[derive(Debug)]
pub struct Scan {
    pub bindings: Vec<BindingStatus>,
    pub rows: Vec<Row>,
}

impl Scan {
    pub fn to_json(&self, root: &Path) -> Value {
        json!({
            "schema": 1,
            "authority": "butler-local",
            "root": root.display().to_string(),
            "bindings": &self.bindings,
            "rows": &self.rows,
        })
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for binding in &self.bindings {
            if binding.status == "unavailable" {
                out.push_str(&format!(
                    "{}: unavailable ({})\n",
                    binding.session,
                    binding.reason.unwrap_or("?")
                ));
            }
        }
        if self.rows.is_empty() {
            out.push_str("No pending permission requests in reachable bindings.\n");
            return out;
        }
        out.push_str("SESSION  SOURCE  ACTION  RESOURCE  STATUS  DECISION\n");
        for row in &self.rows {
            out.push_str(&format!(
                "{}  {}  {}  {}  {}  {}\n",
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
            ));
        }
        out
    }
}

pub fn default_root() -> Option<PathBuf> {
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

/// Manifest paths plus the problems found while walking the tree. A directory
/// that exists but cannot be read is an error, never an empty result.
fn manifests(root: &Path) -> (Vec<(String, PathBuf)>, Vec<BindingStatus>) {
    let mut found = Vec::new();
    let mut problems = Vec::new();
    let sessions = match fs::read_dir(root) {
        Ok(sessions) => sessions,
        Err(_) => {
            problems.push(BindingStatus {
                session: "(bindings)".into(),
                status: "unavailable",
                reason: Some("bindings_io"),
            });
            return (found, problems);
        }
    };
    let mut seen_sessions = 0usize;
    for session in sessions.flatten() {
        seen_sessions += 1;
        if seen_sessions > MAX_MANIFESTS {
            problems.push(BindingStatus {
                session: "(truncated)".into(),
                status: "unavailable",
                reason: Some("too_many_bindings"),
            });
            break;
        }
        let session_name = sanitize(&session.file_name().to_string_lossy(), 128);
        match session.file_type() {
            Ok(kind) if kind.is_dir() => {}
            Ok(_) => continue,
            Err(_) => {
                problems.push(BindingStatus {
                    session: session_name,
                    status: "unavailable",
                    reason: Some("bindings_io"),
                });
                continue;
            }
        }
        let nonces = match fs::read_dir(session.path()) {
            Ok(nonces) => nonces,
            Err(_) => {
                problems.push(BindingStatus {
                    session: session_name,
                    status: "unavailable",
                    reason: Some("bindings_io"),
                });
                continue;
            }
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
    if found.len() > MAX_MANIFESTS {
        found.truncate(MAX_MANIFESTS);
        problems.push(BindingStatus {
            session: "(truncated)".into(),
            status: "unavailable",
            reason: Some("too_many_bindings"),
        });
    }
    (found, problems)
}

/// Format controls (bidi overrides, zero-width joiners) are not covered by
/// `is_control`, and this text comes from the agent side of the wire.
fn is_format_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{00ad}'
            | '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{1d173}'..='\u{1d17a}'
    )
}

fn sanitize(value: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut space = false;
    for ch in value.chars() {
        // Format controls are invisible, so removing one must not leave a
        // visible gap where the wire had none.
        if is_format_control(ch) {
            continue;
        }
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

/// Scan one bindings root. The caller decides how often to run it; it is
/// bounded to at most 256 manifests and a 3 second timeout per request.
pub fn scan(root: &Path) -> Scan {
    scan_cancellable(root, None)
}

/// The same scan, stoppable between manifests. A closed overlay must not keep
/// sending loopback requests to every binding it can still reach.
pub fn scan_cancellable(root: &Path, cancel: Option<&AtomicBool>) -> Scan {
    let cancelled = || {
        cancel
            .map(|flag| flag.load(Ordering::Acquire))
            .unwrap_or(false)
    };
    if !root.is_dir() {
        return Scan {
            bindings: vec![BindingStatus {
                session: "(bindings)".into(),
                status: "unavailable",
                reason: Some("no_root"),
            }],
            rows: Vec::new(),
        };
    }
    let (found, problems) = manifests(root);
    let mut scan = Scan {
        bindings: problems,
        rows: Vec::new(),
    };
    for (session, path) in found {
        if cancelled() {
            break;
        }
        match read_manifest(&path).and_then(|manifest| {
            validate(&manifest)?;
            pending(&manifest)
        }) {
            Ok(mut rows) => {
                scan.rows.append(&mut rows);
                scan.bindings.push(BindingStatus {
                    session,
                    status: "ok",
                    reason: None,
                });
            }
            Err(reason) => scan.bindings.push(BindingStatus {
                session,
                status: "unavailable",
                reason: Some(reason),
            }),
        }
    }
    scan.rows.sort_by(|a, b| {
        (a.session.as_str(), a.id.as_str()).cmp(&(b.session.as_str(), b.id.as_str()))
    });
    scan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::testing::ScriptedHttpServer;

    const PERMISSION_PATH: &str = "/api/session/ses_abc123/permission";

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

    fn serve(status: u16, body: &str) -> ScriptedHttpServer {
        let server = ScriptedHttpServer::new().unwrap();
        server.route_json(PERMISSION_PATH, status, body);
        server
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
    fn strips_bidi_and_zero_width_controls() {
        // Agent-side text can carry a right-to-left override that reorders a
        // rendered line; it must not reach the terminal.
        assert_eq!(sanitize("a\u{202e}b\u{200b}c", 20), "abc");
        assert_eq!(sanitize("line\u{2028}two", 20), "line two");
    }

    #[test]
    fn reads_pending_rows() {
        let body = r#"{"data":[{"id":"per_1","sessionID":"ses_abc123","action":"bash","resources":["echo hi"],"message":"run it"}]}"#;
        let server = serve(200, body);
        let mut valid = manifest();
        valid.endpoint = server.base_url();
        let rows = pending(&valid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "bash");
        assert_eq!(rows[0].resources, vec!["echo hi"]);
    }

    #[test]
    fn rejects_mismatched_session() {
        let body =
            r#"{"data":[{"id":"per_1","sessionID":"ses_other","action":"bash","resources":[]}]}"#;
        let server = serve(200, body);
        let mut valid = manifest();
        valid.endpoint = server.base_url();
        assert_eq!(pending(&valid).unwrap_err(), "session_mismatch");
    }

    #[test]
    fn http_errors_are_not_empty() {
        let server = serve(401, "{}");
        let mut valid = manifest();
        valid.endpoint = server.base_url();
        assert_eq!(pending(&valid).unwrap_err(), "http_status");
    }

    #[test]
    fn malformed_is_not_empty() {
        let server = serve(200, "not json");
        let mut valid = manifest();
        valid.endpoint = server.base_url();
        assert_eq!(pending(&valid).unwrap_err(), "malformed");
    }

    #[test]
    fn an_empty_object_is_malformed_not_empty() {
        // A reused loopback port can answer 200 with an unrelated object.
        let server = serve(200, "{}");
        let mut valid = manifest();
        valid.endpoint = server.base_url();
        assert_eq!(pending(&valid).unwrap_err(), "malformed");
    }

    #[test]
    fn json_does_not_print_the_password() {
        let scan = Scan {
            bindings: vec![BindingStatus {
                session: "synthetic".into(),
                status: "ok",
                reason: None,
            }],
            rows: Vec::new(),
        };
        let encoded = scan.to_json(Path::new("/tmp/root")).to_string();
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("password"));
        assert!(encoded.contains("butler-local"));
    }

    #[test]
    fn no_root_is_unavailable_not_empty() {
        let scan = scan(Path::new("/nonexistent/bindings"));
        assert_eq!(scan.bindings.len(), 1);
        assert_eq!(scan.bindings[0].reason, Some("no_root"));
        assert!(scan.to_text().contains("unavailable"));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_session_is_unavailable_not_empty() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "remuda-perm-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let locked = root.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // A privileged runner ignores the mode; then this proves nothing.
        let readable = fs::read_dir(&locked).is_ok();
        let scan = if readable { None } else { Some(scan(&root)) };
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        let _ = fs::remove_dir_all(&root);
        let Some(scan) = scan else { return };
        assert!(scan
            .bindings
            .iter()
            .any(|binding| binding.reason == Some("bindings_io")));
        assert!(!scan.to_text().contains("No pending"));
    }
}
