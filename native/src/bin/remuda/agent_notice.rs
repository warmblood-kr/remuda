use base64::Engine as _;
use serde::Deserialize;
use serde_json::json;
use std::io::Read;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use super::opencode_adopt::{self, AdoptedManifest};
use super::opencode_tui::{
    fixed_conversation, generation_for, generation_matches, loopback_endpoint, process_identity,
    ProcessIdentity,
};
use super::private_dir::PrivateDir;
use remuda_native::net::http_client::{perform, HttpRequest};

const MAX_STDIN: u64 = 64 * 1024;
const MAX_NOTICE: usize = 1024;
const INBOX_HINT: &str = "MCP butler_inbox (or remuda butler inbox)";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u64,
    binding: String,
    nonce: String,
    session: String,
    instance: String,
    request_id: String,
    text: String,
    message_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u64,
    protocol: String,
    nonce: String,
    session: String,
    wrapper_pid: u32,
    server_pid: u32,
    server_version: String,
    generation: String,
    endpoint: String,
    conversation: String,
    auth: Auth,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Auth {
    scheme: String,
    user: String,
    password: String,
}

pub fn run(args: &[&str]) -> ExitCode {
    if args.iter().any(|arg| *arg == "--help" || *arg == "-h") {
        println!(
            "remuda _agent_notice\nReads one frozen notice request from stdin; returns typed JSON."
        );
        return ExitCode::SUCCESS;
    }
    if !args.is_empty() {
        return emit("definitely_not_written", "invalid_args");
    }
    if !platform_supported() {
        return emit("definitely_not_written", "unsupported_platform");
    }
    let text = match read_stdin() {
        Ok(text) => text,
        Err(reason) => return emit("definitely_not_written", reason),
    };
    let request: Request = match serde_json::from_str(&text) {
        Ok(request) => request,
        Err(_) => return emit("definitely_not_written", "invalid_request"),
    };
    if let Err(reason) = validate_request(&request) {
        return emit("definitely_not_written", reason);
    }
    // No publication leaf exists under a valid private parent: a proven
    // pre-write state. Butler decides from its own provenance whether this is
    // startup-pending or a vanished binding; the filesystem cannot.
    if binding_missing(Path::new(&request.binding)) {
        return emit("definitely_not_written", "binding_missing");
    }
    let binding = match load_binding(&request.binding) {
        Ok(binding) => binding,
        Err(reason) => return emit("definitely_not_written", reason),
    };
    let (outcome, reason) = deliver_binding(&request, binding);
    emit(outcome, reason)
}

enum Binding {
    Owned(Manifest),
    Adopted(AdoptedManifest),
}

/// The manifest protocol selects one complete strict schema. Reading the tag
/// first does not weaken either parser: the selected struct still denies
/// unknown fields and validates every owned/adopted field.
fn load_binding(path: &str) -> Result<Binding, &'static str> {
    let data = read_binding(path)?;
    match protocol_of(&data).as_deref() {
        Some("opencode-http-v1") => serde_json::from_slice(&data)
            .map(Binding::Owned)
            .map_err(|_| "invalid_binding"),
        Some(opencode_adopt::PROTOCOL) => serde_json::from_slice(&data)
            .map(Binding::Adopted)
            .map_err(|_| "invalid_binding"),
        _ => Err("invalid_binding"),
    }
}

#[derive(Deserialize)]
struct ProtocolTag {
    protocol: String,
}

fn protocol_of(data: &[u8]) -> Option<String> {
    serde_json::from_slice::<ProtocolTag>(data)
        .ok()
        .map(|tag| tag.protocol)
}

fn deliver_binding(request: &Request, binding: Binding) -> (&'static str, &'static str) {
    match binding {
        Binding::Owned(manifest) => deliver_owned(request, &manifest),
        Binding::Adopted(manifest) => deliver_adopted_checked(request, &manifest),
    }
}

fn deliver_owned(request: &Request, manifest: &Manifest) -> (&'static str, &'static str) {
    if let Err(reason) = validate_manifest(request, manifest) {
        return ("definitely_not_written", reason);
    }
    if let Err(reason) = check_processes(manifest, process_identity) {
        return ("definitely_not_written", reason);
    }
    match deliver(request, manifest) {
        Ok(outcome) => outcome,
        Err(reason) => ("uncertain", reason),
    }
}

fn deliver_adopted_checked(
    request: &Request,
    manifest: &AdoptedManifest,
) -> (&'static str, &'static str) {
    if let Err(reason) =
        opencode_adopt::validate_manifest(&request.nonce, &request.session, manifest)
    {
        return ("definitely_not_written", reason);
    }
    if let Err(reason) = opencode_adopt::check_process(manifest, process_identity) {
        return ("definitely_not_written", reason);
    }
    match deliver_adopted(request, manifest) {
        Ok(outcome) => outcome,
        Err(reason) => ("uncertain", reason),
    }
}

fn platform_supported() -> bool {
    cfg!(unix)
}

/// Missing only when a valid private parent opens and `openat` says ENOENT
/// for the leaf. A missing/invalid parent, dangling symlink or wrong-owner
/// leaf is not missing; prior publication is Butler provenance, not proof.
pub(crate) fn binding_missing(path: &Path) -> bool {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return false;
    };
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Ok(directory) = PrivateDir::open(parent) else {
        return false;
    };
    matches!(directory.leaf_absent(name), Ok(true))
}

pub(crate) fn canonical_notice(ids: &[String]) -> String {
    if ids.len() == 1 {
        format!("Butler message {} arrived. Read it: {INBOX_HINT}", ids[0])
    } else {
        format!(
            "{} new Butler messages arrived. Read them: {INBOX_HINT}",
            ids.len()
        )
    }
}

pub(crate) fn read_stdin() -> Result<String, &'static str> {
    let mut data = Vec::new();
    std::io::stdin()
        .take(MAX_STDIN + 1)
        .read_to_end(&mut data)
        .map_err(|_| "invalid_request")?;
    if data.len() as u64 > MAX_STDIN {
        return Err("invalid_request");
    }
    String::from_utf8(data).map_err(|_| "invalid_request")
}

fn validate_request(request: &Request) -> Result<(), &'static str> {
    if request.version != 1
        || request.binding.is_empty()
        || request.nonce.is_empty()
        || request.session.is_empty()
        || request.instance.is_empty()
        || request.message_ids.is_empty()
    {
        return Err("invalid_request");
    }
    if request.text.len() > MAX_NOTICE
        || request.text.contains('\r')
        || request.text.contains('\n')
        || request.text != canonical_notice(&request.message_ids)
    {
        return Err("invalid_request");
    }
    if request.message_ids.iter().any(|id| !is_token(id))
        || !request.request_id.starts_with("msg_")
        || request.request_id.len() > 128
        || !request
            .request_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return Err("invalid_request");
    }
    Ok(())
}

pub(crate) fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// Read the manifest through a pinned parent descriptor: owner/mode are checked
/// once, then the leaf opens relative to it with `O_NOFOLLOW|O_NONBLOCK`, so a
/// swapped parent, symlink leaf or FIFO cannot redirect or block this read.
fn read_binding(path: &str) -> Result<Vec<u8>, &'static str> {
    let path = Path::new(path);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or("invalid_binding")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid_binding")?;
    let directory = PrivateDir::open(parent).map_err(|_| "invalid_binding")?;
    directory
        .read_limited(name, MAX_STDIN)
        .map_err(|_| "invalid_binding")
}

#[cfg(test)]
fn load_manifest(path: &str) -> Result<Manifest, &'static str> {
    match load_binding(path)? {
        Binding::Owned(manifest) => Ok(manifest),
        Binding::Adopted(_) => Err("invalid_binding"),
    }
}

fn validate_manifest(request: &Request, manifest: &Manifest) -> Result<(), &'static str> {
    if manifest.schema != 1
        || manifest.protocol != "opencode-http-v1"
        || manifest.nonce != request.nonce
        || manifest.session != request.session
        || manifest.wrapper_pid == 0
        || manifest.server_pid == 0
        || manifest.wrapper_pid == manifest.server_pid
        || manifest.server_version.is_empty()
        || !generation_matches(&manifest.generation)
        || !fixed_conversation(&manifest.conversation)
        || !manifest.endpoint.starts_with("http://127.0.0.1:")
        || !loopback_endpoint(&manifest.endpoint)
        || manifest.auth.scheme != "basic"
        || manifest.auth.user != "opencode"
        || manifest.auth.password.is_empty()
    {
        return Err("invalid_binding");
    }
    Ok(())
}

/// A stale manifest can outlive its wrapper and point at a recycled pid, so
/// liveness alone is not enough: the server must be a child of the live
/// wrapper and the launch generation must match both process start times.
fn check_processes<F>(manifest: &Manifest, identity: F) -> Result<(), &'static str>
where
    F: Fn(u32) -> Option<ProcessIdentity>,
{
    let wrapper = identity(manifest.wrapper_pid).ok_or("wrapper_gone")?;
    let server = identity(manifest.server_pid).ok_or("server_gone")?;
    if server.parent != manifest.wrapper_pid {
        return Err("server_not_owned");
    }
    if manifest.generation != generation_for(wrapper.started_us, server.started_us) {
        return Err("stale_binding");
    }
    Ok(())
}

struct Target<'a> {
    endpoint: &'a str,
    conversation: &'a str,
    server_pid: u32,
    server_version: &'a str,
    password: &'a str,
}

fn owned_target(manifest: &Manifest) -> Target<'_> {
    Target {
        endpoint: &manifest.endpoint,
        conversation: &manifest.conversation,
        server_pid: manifest.server_pid,
        server_version: &manifest.server_version,
        password: &manifest.auth.password,
    }
}

fn adopted_target(manifest: &AdoptedManifest) -> Target<'_> {
    Target {
        endpoint: &manifest.endpoint,
        conversation: &manifest.conversation,
        server_pid: manifest.server_pid,
        server_version: &manifest.server_version,
        password: &manifest.auth.password,
    }
}

fn deliver(
    request: &Request,
    manifest: &Manifest,
) -> Result<(&'static str, &'static str), &'static str> {
    deliver_with(request, manifest, |method, url, auth, body| {
        http(method, url, auth, body)
    })
}

fn deliver_with<F>(
    request: &Request,
    manifest: &Manifest,
    http: F,
) -> Result<(&'static str, &'static str), &'static str>
where
    F: FnMut(&str, String, &str, String) -> Result<remuda_native::net::HttpResponse, String>,
{
    deliver_target_with(request, &owned_target(manifest), http)
}

fn deliver_adopted(
    request: &Request,
    manifest: &AdoptedManifest,
) -> Result<(&'static str, &'static str), &'static str> {
    deliver_adopted_with(request, manifest, |method, url, auth, body| {
        http(method, url, auth, body)
    })
}

fn deliver_adopted_with<F>(
    request: &Request,
    manifest: &AdoptedManifest,
    http: F,
) -> Result<(&'static str, &'static str), &'static str>
where
    F: FnMut(&str, String, &str, String) -> Result<remuda_native::net::HttpResponse, String>,
{
    deliver_target_with(request, &adopted_target(manifest), http)
}

fn deliver_target_with<F>(
    request: &Request,
    target: &Target<'_>,
    mut http: F,
) -> Result<(&'static str, &'static str), &'static str>
where
    F: FnMut(&str, String, &str, String) -> Result<remuda_native::net::HttpResponse, String>,
{
    let auth = basic(target.password);
    let info = http(
        "GET",
        target.endpoint.to_string() + "/api/info",
        &auth,
        String::new(),
    )
    .map_err(|_| "server_unverified")?;
    if info.status != 200 || !server_identity(&info.body, target) {
        return Ok(("definitely_not_written", "server_identity"));
    }
    let url = format!("{}/api/session/{}", target.endpoint, target.conversation);
    let response =
        http("GET", url.clone(), &auth, String::new()).map_err(|_| "target_unverified")?;
    if !target_matches(&response, target.conversation) {
        return match response.status {
            401 | 403 => Ok(("definitely_not_written", "auth")),
            404 => Ok(("definitely_not_written", "target_missing")),
            _ => Err("target_unverified"),
        };
    }
    let body = json!({
        "id": request.request_id,
        "text": request.text,
        "delivery": "queue",
        "resume": true,
    })
    .to_string();
    let response = http("POST", url + "/prompt", &auth, body).map_err(|_| "transport")?;
    Ok(classify_response(&response, request, target.conversation))
}

fn server_identity(body: &[u8], target: &Target<'_>) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    value.get("pid").and_then(serde_json::Value::as_u64) == Some(target.server_pid as u64)
        && value.get("version").and_then(serde_json::Value::as_str) == Some(target.server_version)
        && value
            .get("urls")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|urls| urls.iter().any(|url| url.as_str() == Some(target.endpoint)))
}

fn target_matches(response: &remuda_native::net::HttpResponse, conversation: &str) -> bool {
    if response.status != 200 {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(&response.body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/data/id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some(conversation)
}

fn classify_response(
    response: &remuda_native::net::HttpResponse,
    request: &Request,
    conversation: &str,
) -> (&'static str, &'static str) {
    match response.status {
        200 if prompt_receipt(&response.body, request, conversation) => ("accepted_queue", ""),
        200 => ("uncertain", "receipt_mismatch"),
        400 => ("uncertain", "http_400"),
        401 | 403 => ("definitely_not_written", "auth"),
        404 => ("definitely_not_written", "target_missing"),
        409 => ("uncertain", "http_409"),
        _ => ("uncertain", "http_5xx"),
    }
}

fn prompt_receipt(body: &[u8], request: &Request, conversation: &str) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    value
        .pointer("/data/id")
        .and_then(serde_json::Value::as_str)
        == Some(request.request_id.as_str())
        && value
            .pointer("/data/sessionID")
            .and_then(serde_json::Value::as_str)
            == Some(conversation)
        && value
            .pointer("/data/payload/text")
            .and_then(serde_json::Value::as_str)
            == Some(request.text.as_str())
}

pub(crate) fn basic(password: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"));
    format!("Basic {encoded}")
}

pub(crate) fn http(
    method: &str,
    url: String,
    auth: &str,
    body: String,
) -> Result<remuda_native::net::HttpResponse, String> {
    perform(
        HttpRequest {
            method: method.into(),
            url,
            headers: vec![
                ("Authorization".into(), auth.as_bytes().to_vec()),
                ("Content-Type".into(), b"application/json".to_vec()),
            ],
            body: body.into_bytes(),
            timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(2),
            max_bytes: 64 * 1024,
            ca_file: None,
            pin: None,
            pin_only: false,
        },
        None,
    )
    .map_err(|error| error.to_string())
}

fn emit(outcome: &str, reason: &str) -> ExitCode {
    let value = if reason.is_empty() {
        json!({"outcome": outcome})
    } else {
        json!({"outcome": outcome, "reason": reason})
    };
    println!("{value}");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_native::net::HttpResponse;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    const CONVERSATION: &str = "ses_00000000000000000000000000000000";
    const INFO_OK: &str = r#"{"pid":22,"version":"2.2.5","urls":["http://127.0.0.1:1"]}"#;

    fn request() -> Request {
        Request {
            version: 1,
            binding: "/tmp/binding".into(),
            nonce: "n".into(),
            session: "s".into(),
            instance: "i".into(),
            request_id: "msg_01ABC".into(),
            text: canonical_notice(&["01ABC".into()]),
            message_ids: vec!["01ABC".into()],
        }
    }

    fn sample_manifest() -> Manifest {
        Manifest {
            schema: 1,
            protocol: "opencode-http-v1".into(),
            nonce: "n".into(),
            session: "s".into(),
            wrapper_pid: 11,
            server_pid: 22,
            server_version: "2.2.5".into(),
            generation: generation_for(100, 200),
            endpoint: "http://127.0.0.1:1".into(),
            conversation: CONVERSATION.into(),
            auth: Auth {
                scheme: "basic".into(),
                user: "opencode".into(),
                password: "p".into(),
            },
        }
    }

    #[cfg(unix)]
    #[test]
    fn binding_missing_needs_a_valid_private_parent() {
        use std::os::unix::fs::PermissionsExt;
        let base =
            std::env::temp_dir().join(format!("remuda-binding-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir(&base).expect("base");
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).expect("0700");
        let good = base.join("good");
        std::fs::create_dir(&good).expect("good");
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o700)).expect("0700");
        let bad_parent = base.join("bad");
        std::fs::create_dir(&bad_parent).expect("bad");
        std::fs::set_permissions(&bad_parent, std::fs::Permissions::from_mode(0o755))
            .expect("0755");

        let present = good.join("manifest.json");
        std::fs::write(&present, b"{}").expect("write");
        std::fs::set_permissions(&present, std::fs::Permissions::from_mode(0o600)).expect("0600");
        assert!(!binding_missing(&present));

        // Absent leaf under a valid 0700 parent is the only missing case.
        assert!(binding_missing(&good.join("absent.json")));

        // A missing or wrong-mode parent is not publication-missing.
        assert!(!binding_missing(&base.join("no-such-dir/manifest.json")));
        assert!(!binding_missing(&bad_parent.join("absent.json")));

        // A dangling symlink leaf is not "absent"; load_manifest must refuse it.
        std::os::unix::fs::symlink(good.join("nowhere"), good.join("dangling.json"))
            .expect("symlink");
        assert!(!binding_missing(&good.join("dangling.json")));

        let _ = std::fs::remove_dir_all(&base);
    }

    fn manifest_text() -> Vec<u8> {
        json!({
            "schema": 1,
            "protocol": "opencode-http-v1",
            "nonce": "n",
            "session": "s",
            "wrapper_pid": 11,
            "server_pid": 22,
            "server_version": "2.2.5",
            "generation": generation_for(100, 200),
            "endpoint": "http://127.0.0.1:1",
            "conversation": CONVERSATION,
            "auth": {"scheme": "basic", "user": "opencode", "password": "p"},
        })
        .to_string()
        .into_bytes()
    }

    fn response(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: Default::default(),
            body: body.as_bytes().to_vec(),
            peer_certificate: None,
        }
    }

    fn target_ok() -> HttpResponse {
        response(200, &format!(r#"{{"data":{{"id":"{CONVERSATION}"}}}}"#))
    }

    fn receipt(status: u16) -> HttpResponse {
        response(
            status,
            &format!(
                r#"{{"data":{{"id":"msg_01ABC","sessionID":"{CONVERSATION}","payload":{{"text":"{text}"}}}}}}"#,
                text = canonical_notice(&["01ABC".into()])
            ),
        )
    }

    #[cfg(unix)]
    fn temp_path() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "remuda-agent-notice-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).expect("temp dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("private temp dir");
        dir.join("manifest.json")
    }

    #[test]
    fn platform_gate_matches_the_compile_target() {
        assert_eq!(platform_supported(), cfg!(unix));
    }

    #[test]
    fn canonical_pointer_is_exact_and_counted() {
        assert_eq!(
            canonical_notice(&["A".into()]),
            "Butler message A arrived. Read it: MCP butler_inbox (or remuda butler inbox)"
        );
        assert_eq!(
            canonical_notice(&["A".into(), "B".into()]),
            "2 new Butler messages arrived. Read them: MCP butler_inbox (or remuda butler inbox)"
        );
        let mut changed = request();
        changed.text = "Butler message 01ABC arrived.".into();
        assert_eq!(validate_request(&changed), Err("invalid_request"));
        let mut targeted = request();
        targeted.text = "Butler message 01ABC arrived. Read it: MCP butler_inbox".into();
        assert_eq!(validate_request(&targeted), Err("invalid_request"));
        let mut counted = request();
        counted.message_ids.push("01DEF".into());
        assert_eq!(validate_request(&counted), Err("invalid_request"));
        let mut injected = request();
        injected.text = canonical_notice(&["01ABC".into()]) + "\n";
        assert_eq!(validate_request(&injected), Err("invalid_request"));
    }

    #[test]
    fn request_rejects_unsafe_ids() {
        let mut traversal = request();
        traversal.message_ids = vec!["01ABC\n".into()];
        traversal.text =
            "Butler message 01ABC\n arrived. Read it: MCP butler_inbox (or remuda butler inbox)"
                .into();
        assert_eq!(validate_request(&traversal), Err("invalid_request"));
        let mut slash = request();
        slash.message_ids = vec!["../../x".into()];
        assert_eq!(validate_request(&slash), Err("invalid_request"));
        let mut long = request();
        long.message_ids = vec!["a".repeat(129)];
        assert_eq!(validate_request(&long), Err("invalid_request"));
        let mut id = request();
        id.request_id = "no-prefix".into();
        assert_eq!(validate_request(&id), Err("invalid_request"));
    }

    #[test]
    fn manifest_requires_exact_loopback_and_identity_fields() {
        let request = request();
        let manifest = sample_manifest();
        assert!(validate_manifest(&request, &manifest).is_ok());

        let mut remote = sample_manifest();
        remote.endpoint = "http://example.com:1".into();
        assert_eq!(validate_manifest(&request, &remote), Err("invalid_binding"));

        let mut traversal = sample_manifest();
        traversal.conversation = "ses_../../etc/passwd".into();
        assert_eq!(
            validate_manifest(&request, &traversal),
            Err("invalid_binding")
        );

        let mut query = sample_manifest();
        query.conversation = format!("{CONVERSATION}?x");
        assert_eq!(validate_manifest(&request, &query), Err("invalid_binding"));

        let mut uppercase = sample_manifest();
        uppercase.conversation = "ses_0000000000000000000000000000000A".into();
        assert_eq!(
            validate_manifest(&request, &uppercase),
            Err("invalid_binding")
        );

        let mut same_pid = sample_manifest();
        same_pid.server_pid = same_pid.wrapper_pid;
        assert_eq!(
            validate_manifest(&request, &same_pid),
            Err("invalid_binding")
        );

        let mut random_generation = sample_manifest();
        random_generation.generation = "not-a-generation".into();
        assert_eq!(
            validate_manifest(&request, &random_generation),
            Err("invalid_binding")
        );

        let mut nonce = sample_manifest();
        nonce.nonce = "other".into();
        assert_eq!(validate_manifest(&request, &nonce), Err("invalid_binding"));

        let mut session = sample_manifest();
        session.session = "other".into();
        assert_eq!(
            validate_manifest(&request, &session),
            Err("invalid_binding")
        );

        let mut user = sample_manifest();
        user.auth.user = "root".into();
        assert_eq!(validate_manifest(&request, &user), Err("invalid_binding"));
    }

    #[test]
    #[cfg(unix)]
    fn manifest_file_requires_private_regular_non_symlink() {
        let path = temp_path();
        std::fs::write(&path, manifest_text()).expect("write manifest");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 0644");
        assert!(matches!(
            load_manifest(path.to_str().expect("utf8")),
            Err("invalid_binding")
        ));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 0600");
        assert!(load_manifest(path.to_str().expect("utf8")).is_ok());

        let link = path.with_extension("link");
        std::os::unix::fs::symlink(&path, &link).expect("symlink");
        assert!(matches!(
            load_manifest(link.to_str().expect("utf8")),
            Err("invalid_binding")
        ));

        let missing = path.with_extension("missing");
        assert!(matches!(
            load_manifest(missing.to_str().expect("utf8")),
            Err("invalid_binding")
        ));

        let shared_dir = path.parent().expect("parent").with_extension("shared");
        std::fs::create_dir(&shared_dir).expect("shared dir");
        std::fs::set_permissions(&shared_dir, std::fs::Permissions::from_mode(0o755))
            .expect("chmod 0755");
        let shared = shared_dir.join("manifest.json");
        std::fs::write(&shared, manifest_text()).expect("shared manifest");
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 0600");
        assert!(matches!(
            load_manifest(shared.to_str().expect("utf8")),
            Err("invalid_binding")
        ));

        let oversized = path.with_extension("big");
        std::fs::write(&oversized, vec![b' '; (MAX_STDIN + 1) as usize]).expect("big manifest");
        std::fs::set_permissions(&oversized, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 0600");
        assert!(matches!(
            load_manifest(oversized.to_str().expect("utf8")),
            Err("invalid_binding")
        ));
    }

    fn adopted_manifest() -> AdoptedManifest {
        AdoptedManifest {
            schema: opencode_adopt::SCHEMA,
            protocol: opencode_adopt::PROTOCOL.into(),
            nonce: "n".into(),
            session: "s".into(),
            server_pid: 22,
            server_version: "2.2.5".into(),
            generation: opencode_adopt::adopted_generation(200),
            endpoint: "http://127.0.0.1:1".into(),
            conversation: CONVERSATION.into(),
            auth: opencode_adopt::AdoptAuth {
                scheme: "basic".into(),
                user: "opencode".into(),
                password: "p".into(),
            },
        }
    }

    #[test]
    #[cfg(unix)]
    fn protocol_selects_one_strict_schema() {
        let path = temp_path();
        let adopted = serde_json::to_vec(&adopted_manifest()).expect("adopted json");
        std::fs::write(&path, &adopted).expect("write adopted");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("0600");
        assert!(matches!(
            load_binding(path.to_str().expect("utf8")),
            Ok(Binding::Adopted(_))
        ));

        let owned = path.with_extension("owned");
        std::fs::write(&owned, manifest_text()).expect("write owned");
        std::fs::set_permissions(&owned, std::fs::Permissions::from_mode(0o600)).expect("0600");
        assert!(matches!(
            load_binding(owned.to_str().expect("utf8")),
            Ok(Binding::Owned(_))
        ));

        // The adopted parser must not silently accept owned wrapper fields.
        let mut widened = adopted_manifest();
        let mut value = serde_json::to_value(&widened).expect("json");
        value["wrapper_pid"] = json!(11);
        let extra = path.with_extension("extra");
        std::fs::write(&extra, value.to_string()).expect("write extra");
        std::fs::set_permissions(&extra, std::fs::Permissions::from_mode(0o600)).expect("0600");
        assert!(matches!(
            load_binding(extra.to_str().expect("utf8")),
            Err("invalid_binding")
        ));

        // A tag-only edit cannot turn an owned manifest into an adopted one.
        let mut mixed_value: serde_json::Value =
            serde_json::from_slice(&manifest_text()).expect("owned json");
        mixed_value["protocol"] = json!("opencode-http-adopt-v1");
        let mixed = path.with_extension("mixed");
        std::fs::write(&mixed, mixed_value.to_string()).expect("write mixed");
        std::fs::set_permissions(&mixed, std::fs::Permissions::from_mode(0o600)).expect("0600");
        assert!(matches!(
            load_binding(mixed.to_str().expect("utf8")),
            Err("invalid_binding")
        ));

        let unknown = path.with_extension("unknown");
        std::fs::write(&unknown, br#"{"protocol":"other"}"#).expect("write unknown");
        std::fs::set_permissions(&unknown, std::fs::Permissions::from_mode(0o600)).expect("0600");
        assert!(matches!(
            load_binding(unknown.to_str().expect("utf8")),
            Err("invalid_binding")
        ));
    }

    #[test]
    fn adopted_delivery_reuses_the_exact_receipt_and_target_rules() {
        let request = request();
        let manifest = adopted_manifest();
        let mut responses = vec![response(200, INFO_OK), target_ok(), receipt(200)];
        let outcome =
            deliver_adopted_with(&request, &manifest, |_, _, _, _| Ok(responses.remove(0)));
        assert_eq!(outcome, Ok(("accepted_queue", "")));

        let mut responses = vec![response(200, INFO_OK), response(404, "{}")];
        let outcome =
            deliver_adopted_with(&request, &manifest, |_, _, _, _| Ok(responses.remove(0)));
        assert_eq!(outcome, Ok(("definitely_not_written", "target_missing")));

        let mut responses = vec![response(401, "{}")];
        let outcome =
            deliver_adopted_with(&request, &manifest, |_, _, _, _| Ok(responses.remove(0)));
        assert_eq!(outcome, Ok(("definitely_not_written", "server_identity")));
    }

    #[test]
    fn adopted_binding_is_checked_before_any_request() {
        let request = request();
        let mut manifest = adopted_manifest();
        manifest.nonce = "other".into();
        let outcome = deliver_adopted_checked(&request, &manifest);
        assert_eq!(outcome, ("definitely_not_written", "invalid_binding"));
    }

    #[test]
    fn process_identity_must_be_live_owned_and_matching() {
        let manifest = sample_manifest();
        let wrapper = ProcessIdentity {
            parent: 1,
            started_us: 100,
        };
        let server = ProcessIdentity {
            parent: 11,
            started_us: 200,
        };
        let lookup = |wrapper_pid: u32| {
            move |pid: u32| {
                if pid == wrapper_pid {
                    Some(wrapper)
                } else {
                    Some(server)
                }
            }
        };
        assert_eq!(check_processes(&manifest, lookup(11)), Ok(()));
        assert_eq!(check_processes(&manifest, |_| None), Err("wrapper_gone"));

        let orphan = ProcessIdentity {
            parent: 999,
            started_us: 200,
        };
        assert_eq!(
            check_processes(&manifest, |pid| Some(if pid == 11 {
                wrapper
            } else {
                orphan
            })),
            Err("server_not_owned")
        );

        let reused = ProcessIdentity {
            parent: 11,
            started_us: 999,
        };
        assert_eq!(
            check_processes(&manifest, |pid| Some(if pid == 11 {
                wrapper
            } else {
                reused
            })),
            Err("stale_binding")
        );
    }

    #[test]
    fn accepted_requires_matching_receipt() {
        let request = request();
        let manifest = sample_manifest();
        let mut responses = vec![response(200, INFO_OK), target_ok(), receipt(200)];
        responses[2] = response(
            200,
            &format!(
                r#"{{"data":{{"id":"msg_01ABC","sessionID":"{CONVERSATION}","payload":{{"text":"BUTLER"}}}}}}"#
            ),
        );
        let outcome = deliver_with(&request, &manifest, |_, _, _, _| Ok(responses.remove(0)));
        assert_eq!(outcome, Ok(("uncertain", "receipt_mismatch")));

        let mut responses = vec![response(200, INFO_OK), target_ok(), receipt(200)];
        responses.truncate(3);
        let outcome = deliver_with(&request, &manifest, |_, _, _, _| Ok(responses.remove(0)));
        assert_eq!(outcome, Ok(("accepted_queue", "")));
    }

    #[test]
    fn malformed_receipt_is_uncertain() {
        let request = request();
        let manifest = sample_manifest();
        let mut responses = vec![
            response(200, INFO_OK),
            target_ok(),
            response(200, r#"{"data":"opaque"}"#),
        ];
        let outcome = deliver_with(&request, &manifest, |_, _, _, _| Ok(responses.remove(0)));
        assert_eq!(outcome, Ok(("uncertain", "receipt_mismatch")));
    }

    #[test]
    fn target_404_is_definitely_not_written() {
        let request = request();
        let manifest = sample_manifest();
        let mut responses = vec![response(200, INFO_OK), response(404, "{}")];
        let outcome = deliver_with(&request, &manifest, |_, _, _, _| Ok(responses.remove(0)));
        assert_eq!(outcome, Ok(("definitely_not_written", "target_missing")));
    }

    #[test]
    fn target_401_is_auth_refusal() {
        let request = request();
        let manifest = sample_manifest();
        let mut responses = vec![response(200, INFO_OK), response(401, "{}")];
        let outcome = deliver_with(&request, &manifest, |_, _, _, _| Ok(responses.remove(0)));
        assert_eq!(outcome, Ok(("definitely_not_written", "auth")));
    }

    #[test]
    fn unverified_server_is_definitely_not_written() {
        let request = request();
        let manifest = sample_manifest();
        for status in [401_u16, 503] {
            let mut responses = vec![response(status, "{}")];
            let outcome = deliver_with(&request, &manifest, |_, _, _, _| Ok(responses.remove(0)));
            assert_eq!(outcome, Ok(("definitely_not_written", "server_identity")));
        }
        let outcome = deliver_with(&request, &manifest, |_, _, _, _| Err("timeout".into()));
        assert_eq!(outcome, Err("server_unverified"));
    }

    #[test]
    fn generic_400_is_uncertain() {
        let request = request();
        let manifest = sample_manifest();
        let mut responses = vec![response(200, INFO_OK), target_ok(), response(400, "{}")];
        let outcome = deliver_with(&request, &manifest, |_, _, _, _| Ok(responses.remove(0)));
        assert_eq!(outcome, Ok(("uncertain", "http_400")));
    }

    #[test]
    fn conflict_is_uncertain() {
        let request = request();
        let manifest = sample_manifest();
        let mut responses = vec![response(200, INFO_OK), target_ok(), response(409, "{}")];
        let outcome = deliver_with(&request, &manifest, |_, _, _, _| Ok(responses.remove(0)));
        assert_eq!(outcome, Ok(("uncertain", "http_409")));
    }

    #[test]
    fn post_transport_failure_keeps_uncertain() {
        let request = request();
        let manifest = sample_manifest();
        let mut calls = 0;
        let outcome = deliver_with(&request, &manifest, |_, _, _, _| {
            calls += 1;
            match calls {
                1 => Ok(response(200, INFO_OK)),
                2 => Ok(target_ok()),
                _ => Err("timeout".into()),
            }
        });
        assert_eq!(outcome, Err("transport"));
        assert_eq!(calls, 3);
    }

    #[test]
    fn unverified_target_never_posts() {
        let request = request();
        let manifest = sample_manifest();
        let mut calls = 0;
        let outcome = deliver_with(&request, &manifest, |method, _, _, _| {
            assert_ne!(method, "POST");
            calls += 1;
            if calls == 1 {
                Ok(response(200, INFO_OK))
            } else {
                Ok(response(200, r#"{"data":{"id":"ses_other"}}"#))
            }
        });
        assert_eq!(outcome, Err("target_unverified"));
        assert_eq!(calls, 2);
    }
}
