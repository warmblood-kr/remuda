use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use super::agent_notice::{basic, http, is_token, read_stdin};
use super::opencode_tui::{loopback_endpoint, process_identity, ProcessIdentity};
use super::private_dir::{CreatedFile, PrivateDir};

pub(crate) const PROTOCOL: &str = "opencode-http-adopt-v1";
pub(crate) const SCHEMA: u64 = 1;
const MAX_CREDENTIAL: usize = 1024;

/// The owner's adoption declaration: a same-node loopback endpoint, its Basic
/// credential and the exact existing session A. The credential arrives only on
/// stdin and never in argv, status or a journal.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdoptRequest {
    version: u64,
    binding: String,
    nonce: String,
    session: String,
    endpoint: String,
    conversation: String,
    auth: AdoptAuth,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdoptAuth {
    pub(crate) scheme: String,
    pub(crate) user: String,
    pub(crate) password: String,
}

/// A separately tagged binding for an already-running session: owner-declared
/// endpoint/A, local server start corroboration only, and no wrapper pid,
/// ancestry or instance.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdoptedManifest {
    pub(crate) schema: u64,
    pub(crate) protocol: String,
    pub(crate) nonce: String,
    pub(crate) session: String,
    pub(crate) server_pid: u32,
    pub(crate) server_version: String,
    pub(crate) generation: String,
    pub(crate) endpoint: String,
    pub(crate) conversation: String,
    pub(crate) auth: AdoptAuth,
}

#[derive(Debug, Deserialize)]
struct ServerInfo {
    pid: u32,
    version: String,
    urls: Vec<String>,
}

pub fn run(args: &[&str]) -> ExitCode {
    if args.iter().any(|arg| *arg == "--help" || *arg == "-h") {
        println!(
            "remuda _opencode_adopt\nReads one frozen adoption request from stdin; returns typed JSON."
        );
        return ExitCode::SUCCESS;
    }
    if !args.is_empty() {
        return emit_unavailable("invalid_args");
    }
    if !super::opencode_tui::platform_supported() {
        return emit_unavailable("unsupported_platform");
    }
    let text = match read_stdin() {
        Ok(text) => text,
        Err(reason) => return emit_unavailable(reason),
    };
    let request: AdoptRequest = match serde_json::from_str(&text) {
        Ok(request) => request,
        Err(_) => return emit_unavailable("invalid_request"),
    };
    if let Err(reason) = validate_request(&request) {
        return emit_unavailable(reason);
    }
    match adopt(&request) {
        Ok(generation) => emit(json!({"outcome": "adopted", "generation": generation})),
        Err(reason) => emit_unavailable(reason),
    }
}

fn emit_unavailable(reason: &str) -> ExitCode {
    emit(json!({"outcome": "unavailable", "reason": reason}))
}

fn emit(value: Value) -> ExitCode {
    println!("{value}");
    ExitCode::SUCCESS
}

/// Only `http://127.0.0.1:PORT` is an adoptable node. The owned wrapper may
/// also accept `[::1]`, but an owner-declared existing node is exactly IPv4
/// loopback, so a hostname, scheme, path or remote address is refused.
pub(crate) fn adopted_endpoint(value: &str) -> bool {
    let Some(port) = value.strip_prefix("http://127.0.0.1:") else {
        return false;
    };
    let valid_port = !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|port| port > 0);
    valid_port && loopback_endpoint(value)
}

/// Accept the bounded URL-safe `ses_` subset used by real default and explicit
/// ids; target proof comes from the exact credentialed GET of A, not a narrower
/// invented upstream rule.
pub(crate) fn adoptable_conversation(value: &str) -> bool {
    value.len() <= 128
        && value.strip_prefix("ses_").is_some_and(|id| {
            !id.is_empty()
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
}

/// The local server's process start in microseconds is the adopted generation.
/// A recycled pid or a restarted server no longer matches.
pub(crate) fn adopted_generation(started_us: u64) -> String {
    format!("{started_us:x}")
}

pub(crate) fn adopted_generation_matches(value: &str) -> bool {
    !value.is_empty() && value.len() <= 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn validate_request(request: &AdoptRequest) -> Result<(), &'static str> {
    if request.version != 1
        || !Path::new(&request.binding).is_absolute()
        || !is_token(&request.nonce)
        || !is_token(&request.session)
        || !adopted_endpoint(&request.endpoint)
        || !adoptable_conversation(&request.conversation)
        || !valid_auth(&request.auth)
    {
        return Err("invalid_request");
    }
    Ok(())
}

pub(crate) fn valid_auth(auth: &AdoptAuth) -> bool {
    auth.scheme == "basic"
        && auth.user == "opencode"
        && !auth.password.is_empty()
        && auth.password.len() <= MAX_CREDENTIAL
}

/// The manifest must describe the same binding the notice request names. The
/// endpoint and conversation come only from this private manifest, never from
/// the request or argv.
pub(crate) fn validate_manifest(
    nonce: &str,
    session: &str,
    manifest: &AdoptedManifest,
) -> Result<(), &'static str> {
    if manifest.schema != SCHEMA
        || manifest.protocol != PROTOCOL
        || manifest.nonce != nonce
        || manifest.session != session
        || manifest.server_pid <= 1
        || manifest.server_version.is_empty()
        || !adopted_generation_matches(&manifest.generation)
        || !adopted_endpoint(&manifest.endpoint)
        || !adoptable_conversation(&manifest.conversation)
        || !valid_auth(&manifest.auth)
    {
        return Err("invalid_binding");
    }
    Ok(())
}

/// An adopted helper has no wrapper to ask. It corroborates the declared local
/// server pid's start time instead; there is no parent or ancestry claim.
pub(crate) fn check_process<F>(manifest: &AdoptedManifest, identity: F) -> Result<(), &'static str>
where
    F: Fn(u32) -> Option<ProcessIdentity>,
{
    let server = identity(manifest.server_pid).ok_or("server_gone")?;
    if manifest.generation != adopted_generation(server.started_us) {
        return Err("stale_binding");
    }
    Ok(())
}

fn adopt(request: &AdoptRequest) -> Result<String, &'static str> {
    adopt_with(request, http, process_identity)
}

fn adopt_with<F, I>(
    request: &AdoptRequest,
    mut http: F,
    identity: I,
) -> Result<String, &'static str>
where
    F: FnMut(&str, String, &str, String) -> Result<remuda_native::net::HttpResponse, String>,
    I: Fn(u32) -> Option<ProcessIdentity>,
{
    binding_absent(&request.binding)?;
    let info = fetch_info(&mut http, request)?;
    let server = identity(info.pid).ok_or("node_unverified")?;
    let generation = adopted_generation(server.started_us);
    fetch_target(&mut http, request)?;
    let after = identity(info.pid).ok_or("server_gone")?;
    if adopted_generation(after.started_us) != generation {
        return Err("stale_binding");
    }
    publish(request, &info, &generation)?;
    Ok(generation)
}

fn binding_absent(binding: &str) -> Result<(), &'static str> {
    match std::fs::symlink_metadata(binding) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err("already_adopted"),
    }
}

fn fetch_info<F>(http: &mut F, request: &AdoptRequest) -> Result<ServerInfo, &'static str>
where
    F: FnMut(&str, String, &str, String) -> Result<remuda_native::net::HttpResponse, String>,
{
    let url = request.endpoint.clone() + "/api/info";
    let response = http("GET", url, &basic(&request.auth.password), String::new())
        .map_err(|_| "endpoint_unavailable")?;
    verify_info(&response, &request.endpoint)
}

fn verify_info(
    response: &remuda_native::net::HttpResponse,
    endpoint: &str,
) -> Result<ServerInfo, &'static str> {
    match response.status {
        200 => {}
        401 | 403 => return Err("auth"),
        _ => return Err("node_unverified"),
    }
    let info: ServerInfo = serde_json::from_slice(&response.body).map_err(|_| "node_unverified")?;
    if info.pid <= 1 || info.version.is_empty() || !info.urls.iter().any(|url| url == endpoint) {
        return Err("node_unverified");
    }
    Ok(info)
}

fn fetch_target<F>(http: &mut F, request: &AdoptRequest) -> Result<(), &'static str>
where
    F: FnMut(&str, String, &str, String) -> Result<remuda_native::net::HttpResponse, String>,
{
    let url = format!("{}/api/session/{}", request.endpoint, request.conversation);
    let response = http("GET", url, &basic(&request.auth.password), String::new())
        .map_err(|_| "target_unavailable")?;
    verify_target(&response, &request.conversation)
}

fn verify_target(
    response: &remuda_native::net::HttpResponse,
    conversation: &str,
) -> Result<(), &'static str> {
    match response.status {
        200 => {}
        401 | 403 => return Err("auth"),
        404 => return Err("target_missing"),
        _ => return Err("target_unverified"),
    }
    let value: Value = serde_json::from_slice(&response.body).map_err(|_| "target_unverified")?;
    if value.pointer("/data/id").and_then(Value::as_str) == Some(conversation) {
        return Ok(());
    }
    Err("target_unverified")
}

fn publish(
    request: &AdoptRequest,
    info: &ServerInfo,
    generation: &str,
) -> Result<(), &'static str> {
    let manifest = AdoptedManifest {
        schema: SCHEMA,
        protocol: PROTOCOL.into(),
        nonce: request.nonce.clone(),
        session: request.session.clone(),
        server_pid: info.pid,
        server_version: info.version.clone(),
        generation: generation.into(),
        endpoint: request.endpoint.clone(),
        conversation: request.conversation.clone(),
        auth: AdoptAuth {
            scheme: "basic".into(),
            user: "opencode".into(),
            password: request.auth.password.clone(),
        },
    };
    let text = serde_json::to_vec(&manifest).map_err(|_| "write_failed")?;
    publish_with(&request.binding, |file| {
        file.write_all(&text).and_then(|()| file.sync_all())
    })
}

fn publish_with<F>(binding: &str, write: F) -> Result<(), &'static str>
where
    F: FnOnce(&mut std::fs::File) -> std::io::Result<()>,
{
    let path = Path::new(binding);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or("write_failed")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("write_failed")?;
    let directory = PrivateDir::open(parent).map_err(|_| "write_failed")?;
    let (mut file, id) = directory.create_new(name).map_err(|_| "write_failed")?;
    let created = CreatedFile::new(directory, name.to_string(), id);
    if write(&mut file).is_err() {
        drop(file);
        created.remove();
        return Err("write_failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_native::net::HttpResponse;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    const CONVERSATION: &str = "ses_0123456789abcdefghijklmnop";
    const ENDPOINT: &str = "http://127.0.0.1:4096";

    fn request() -> AdoptRequest {
        AdoptRequest {
            version: 1,
            binding: "/private/tmp/adopt-test/manifest.json".into(),
            nonce: "nonce".into(),
            session: "session".into(),
            endpoint: ENDPOINT.into(),
            conversation: CONVERSATION.into(),
            auth: AdoptAuth {
                scheme: "basic".into(),
                user: "opencode".into(),
                password: "secret".into(),
            },
        }
    }

    fn response(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: Default::default(),
            body: body.as_bytes().to_vec(),
            peer_certificate: None,
        }
    }

    fn info_ok() -> HttpResponse {
        response(
            200,
            &format!(
                r#"{{"pid":22,"version":"2.2.5","urls":["{ENDPOINT}"],"paths":{{"tmp":"/tmp"}}}}"#
            ),
        )
    }

    fn target_ok() -> HttpResponse {
        response(200, &format!(r#"{{"data":{{"id":"{CONVERSATION}"}}}}"#))
    }

    #[cfg(unix)]
    fn scratch() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("remuda-adopt-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("0700");
        dir
    }

    #[test]
    fn endpoint_is_exactly_ipv4_loopback() {
        assert!(adopted_endpoint(ENDPOINT));
        for bad in [
            "http://[::1]:4096",
            "https://127.0.0.1:4096",
            "http://localhost:4096",
            "http://127.0.0.1:4096/",
            "http://127.0.0.1:4096/path",
            "http://127.0.0.1:4096?query",
            "http://user@127.0.0.1:4096",
            "http://127.0.0.1",
            "http://example.com:4096",
        ] {
            assert!(!adopted_endpoint(bad), "{bad}");
        }
    }

    #[test]
    fn adopted_conversation_is_bounded_and_url_safe() {
        assert!(adoptable_conversation(CONVERSATION));
        // Actual 2.2.5 default fixture from independent QA.
        assert!(adoptable_conversation("ses_e54f46499ffeglY6MpLaInY8Tt"));
        // The owned wrapper's explicit 32-hex session is also a real session.
        assert!(adoptable_conversation(
            "ses_0123456789abcdef0123456789abcdef"
        ));
        assert!(adoptable_conversation("ses_A_b-9"));
        for bad in [
            "ses_",
            "ses",
            "ses_0123456789abcdefghijklmno?",
            "ses_0123456789abcdefghijklmno/",
            "ses_../../etc/passwd",
            "ses_0123456789abcdefghijklmno%2f",
            "SES_0123456789abcdefghijklmno",
        ] {
            assert!(!adoptable_conversation(bad), "{bad}");
        }
        assert!(!adoptable_conversation(&format!("ses_{}", "a".repeat(125))));
    }

    #[test]
    fn default_2_2_5_generator_fixture_matches() {
        let fixture = "ses_e54f46499ffeglY6MpLaInY8Tt";
        let suffix = fixture.strip_prefix("ses_").expect("prefix");
        assert_eq!(suffix.len(), 26);
        assert!(suffix[..12].bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(suffix[..12].bytes().all(|byte| !byte.is_ascii_uppercase()));
        assert!(suffix[12..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric()));
    }

    #[test]
    fn generation_is_the_server_start() {
        assert_eq!(adopted_generation(0x10), "10");
        assert!(adopted_generation_matches(&adopted_generation(1)));
        assert!(!adopted_generation_matches(""));
        assert!(!adopted_generation_matches("xyz"));
        assert!(!adopted_generation_matches("0123456789abcdef0"));
    }

    #[test]
    fn request_rejects_unsafe_fields() {
        assert!(validate_request(&request()).is_ok());
        let mut relative = request();
        relative.binding = "manifest.json".into();
        assert_eq!(validate_request(&relative), Err("invalid_request"));
        let mut endpoint = request();
        endpoint.endpoint = "http://[::1]:4096".into();
        assert_eq!(validate_request(&endpoint), Err("invalid_request"));
        let mut conversation = request();
        conversation.conversation = "ses_../../etc/passwd".into();
        assert_eq!(validate_request(&conversation), Err("invalid_request"));
        let mut auth = request();
        auth.auth.user = "root".into();
        assert_eq!(validate_request(&auth), Err("invalid_request"));
        let mut empty = request();
        empty.auth.password.clear();
        assert_eq!(validate_request(&empty), Err("invalid_request"));
    }

    fn manifest() -> AdoptedManifest {
        AdoptedManifest {
            schema: SCHEMA,
            protocol: PROTOCOL.into(),
            nonce: "nonce".into(),
            session: "session".into(),
            server_pid: 22,
            server_version: "2.2.5".into(),
            generation: adopted_generation(200),
            endpoint: ENDPOINT.into(),
            conversation: CONVERSATION.into(),
            auth: AdoptAuth {
                scheme: "basic".into(),
                user: "opencode".into(),
                password: "secret".into(),
            },
        }
    }

    #[test]
    fn manifest_is_strict_and_bound_to_the_request() {
        assert!(validate_manifest("nonce", "session", &manifest()).is_ok());
        let mut wrong = manifest();
        wrong.nonce = "other".into();
        assert_eq!(
            validate_manifest("nonce", "session", &wrong),
            Err("invalid_binding")
        );
        let mut wrapper = manifest();
        wrapper.protocol = "opencode-http-v1".into();
        assert_eq!(
            validate_manifest("nonce", "session", &wrapper),
            Err("invalid_binding")
        );
        let mut pid = manifest();
        pid.server_pid = 1;
        assert_eq!(
            validate_manifest("nonce", "session", &pid),
            Err("invalid_binding")
        );
        let mut target = manifest();
        target.conversation = "ses_../../etc/passwd".into();
        assert_eq!(
            validate_manifest("nonce", "session", &target),
            Err("invalid_binding")
        );
        let mut remote = manifest();
        remote.endpoint = "http://example.com:4096".into();
        assert_eq!(
            validate_manifest("nonce", "session", &remote),
            Err("invalid_binding")
        );
    }

    #[test]
    fn process_check_uses_only_local_start_corroboration() {
        let manifest = manifest();
        let live = ProcessIdentity {
            parent: 999,
            started_us: 200,
        };
        assert_eq!(check_process(&manifest, |_| Some(live)), Ok(()));
        assert_eq!(check_process(&manifest, |_| None), Err("server_gone"));
        let recycled = ProcessIdentity {
            parent: 999,
            started_us: 201,
        };
        assert_eq!(
            check_process(&manifest, |_| Some(recycled)),
            Err("stale_binding")
        );
    }

    #[cfg(unix)]
    fn scripted(
        responses: Vec<HttpResponse>,
    ) -> impl FnMut(&str, String, &str, String) -> Result<HttpResponse, String> {
        let mut responses = responses;
        move |_method, _url, _auth, _body| Ok(responses.remove(0))
    }

    #[cfg(unix)]
    fn live_identity(pid: u32) -> Option<ProcessIdentity> {
        (pid == 22).then_some(ProcessIdentity {
            parent: 999,
            started_us: 200,
        })
    }

    #[cfg(unix)]
    #[test]
    fn adoption_publishes_one_private_manifest_after_exact_proof() {
        let dir = scratch();
        let mut request = request();
        request.binding = dir.join("manifest.json").to_string_lossy().into_owned();
        let generation = adopt_with(
            &request,
            scripted(vec![info_ok(), target_ok()]),
            live_identity,
        )
        .expect("adopt");
        assert_eq!(generation, adopted_generation(200));
        let path = dir.join("manifest.json");
        let metadata = std::fs::metadata(&path).expect("manifest");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("wrapper_pid"));
        assert!(!text.contains("instance"));
        let stored: AdoptedManifest = serde_json::from_str(&text).expect("parse");
        assert_eq!(stored.protocol, PROTOCOL);
        assert_eq!(stored.conversation, CONVERSATION);
        assert_eq!(stored.server_pid, 22);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn wrong_auth_node_or_target_publishes_nothing() {
        let cases = [
            (vec![response(401, "{}")], "auth"),
            (
                vec![response(
                    200,
                    r#"{"pid":22,"version":"2.2.5","urls":["http://127.0.0.1:1"]}"#,
                )],
                "node_unverified",
            ),
            (vec![info_ok(), response(404, "{}")], "target_missing"),
            (
                vec![info_ok(), response(200, r#"{"data":{"id":"ses_other"}}"#)],
                "target_unverified",
            ),
        ];
        for (responses, reason) in cases {
            let dir = scratch();
            let mut request = request();
            request.binding = dir.join("manifest.json").to_string_lossy().into_owned();
            assert_eq!(
                adopt_with(&request, scripted(responses), live_identity),
                Err(reason)
            );
            assert!(!dir.join("manifest.json").exists());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[cfg(unix)]
    #[test]
    fn adoption_never_posts_and_never_retries() {
        let dir = scratch();
        let mut request = request();
        request.binding = dir.join("manifest.json").to_string_lossy().into_owned();
        let mut calls = Vec::new();
        let mut responses = vec![info_ok(), target_ok()];
        let result = adopt_with(
            &request,
            |method, url, _auth, _body| {
                calls.push((method.to_string(), url));
                Ok(responses.remove(0))
            },
            live_identity,
        );
        assert!(result.is_ok());
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|(method, _)| method == "GET"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn preexisting_binding_is_refused_before_any_request() {
        let dir = scratch();
        let path = dir.join("manifest.json");
        std::fs::write(&path, b"preexisting").expect("write");
        let mut request = request();
        request.binding = path.to_string_lossy().into_owned();
        let mut calls = 0;
        assert_eq!(
            adopt_with(
                &request,
                |_, _, _, _| {
                    calls += 1;
                    Ok(info_ok())
                },
                live_identity
            ),
            Err("already_adopted")
        );
        assert_eq!(calls, 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"preexisting");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn failed_write_removes_only_the_partial_manifest() {
        let dir = scratch();
        let path = dir.join("manifest.json");
        assert_eq!(
            publish_with(&path.to_string_lossy(), |file| {
                file.write_all(b"{\"partial\":")?;
                Err(std::io::Error::other("injected failure"))
            }),
            Err("write_failed")
        );
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn request_deserialization_denies_unknown_and_widened_auth_fields() {
        let value = serde_json::to_value(adoption_request_value()).expect("json");
        assert!(serde_json::from_value::<AdoptRequest>(value.clone()).is_ok());
        let mut extra = value.clone();
        extra["token"] = json!("widened");
        assert!(serde_json::from_value::<AdoptRequest>(extra).is_err());
        let mut auth_extra = value;
        auth_extra["auth"]["scheme_extra"] = json!("widened");
        assert!(serde_json::from_value::<AdoptRequest>(auth_extra).is_err());
    }

    fn adoption_request_value() -> serde_json::Value {
        json!({
            "version": 1,
            "binding": "/private/tmp/adopt-test/manifest.json",
            "nonce": "nonce",
            "session": "session",
            "endpoint": ENDPOINT,
            "conversation": CONVERSATION,
            "auth": {"scheme": "basic", "user": "opencode", "password": "secret"},
        })
    }

    #[cfg(unix)]
    #[test]
    fn stale_between_adoption_gets_publishes_nothing() {
        let dir = scratch();
        let mut request = request();
        request.binding = dir.join("manifest.json").to_string_lossy().into_owned();
        let identity_calls = std::cell::Cell::new(0_u32);
        let identity = |_pid: u32| {
            let call = identity_calls.get();
            identity_calls.set(call + 1);
            Some(ProcessIdentity {
                parent: 999,
                started_us: if call == 0 { 200 } else { 201 },
            })
        };
        assert_eq!(
            adopt_with(&request, scripted(vec![info_ok(), target_ok()]), identity),
            Err("stale_binding")
        );
        assert!(!dir.join("manifest.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_never_claims_an_instance_or_wrapper() {
        let value = serde_json::to_value(manifest()).expect("json");
        assert!(value.get("instance").is_none());
        assert!(value.get("wrapper_pid").is_none());
        assert_eq!(value["protocol"], PROTOCOL);
    }

    #[test]
    fn startup_response_must_name_the_exact_endpoint() {
        let info = verify_info(&info_ok(), ENDPOINT).expect("info");
        assert_eq!(info.pid, 22);
        let wrong = response(
            200,
            r#"{"pid":22,"version":"2.2.5","urls":["http://127.0.0.1:1"]}"#,
        );
        assert_eq!(verify_info(&wrong, ENDPOINT).err(), Some("node_unverified"));
        assert_eq!(
            verify_info(&response(401, "{}"), ENDPOINT).err(),
            Some("auth")
        );
    }
}
