//! `remuda vfs …`: the cluster virtual file store from the command line
//! (#742). The daemon owns the store (#778): the CLI parses, reads `put`'s
//! source and writes `get`'s destination, and the daemon runs the command
//! (`Request::Vfs`). With no daemon running, it fails with one line.

use crate::cli_parse::{self, ArgSpec, OptionSpec, RepeatPolicy, Spec, VerbSpec};
use crate::cluster::vfs::{VfsError, VfsErrorKind, VfsStore};
use crate::cluster::vfs_drain::DrainReport;
use crate::cluster::vfs_status::StatusReport;
use crate::cluster::vfs_sync;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use remuda_core::protocol::{Request, Response};
use remuda_core::vfs::{self as rules, VfsEntry, VfsUri};
use serde_json::{Map, Value};
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

/// `println!` into `out.stdout`.
macro_rules! say {
    ($out:expr, $($arg:tt)*) => {{ let _ = writeln!($out.stdout, $($arg)*); }};
}
/// `eprintln!` into `out.stderr`.
macro_rules! warn {
    ($out:expr, $($arg:tt)*) => {{ let _ = writeln!($out.stderr, $($arg)*); }};
}
use std::time::Duration;

const WAIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Under the CLI's 125 s wait for a vfs answer; drain can simply be rerun.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(100);
/// `put` stored the file but reached no peer: a temporary failure
/// (EX_TEMPFAIL from sysexits.h), so scripts can retry or wait.
const EXIT_TEMPFAIL: u8 = 75;

fn spec() -> Spec {
    let option = |long: &str, value: Option<&str>, help: &str| OptionSpec {
        long: long.into(),
        short: None,
        value: value.map(str::to_owned),
        help: help.into(),
        global: false,
        required: false,
        repeat_policy: RepeatPolicy::Reject,
    };
    let arg = |name: &str, help: &str, required: bool| ArgSpec {
        name: name.into(),
        help: help.into(),
        multiple: false,
        required,
    };
    let verb = |name: &str, about: &str, args, options, next: &str| VerbSpec {
        name: name.into(),
        about: about.into(),
        usage: next.strip_prefix("remuda vfs ").map(str::to_owned),
        args,
        options,
        next: next.into(),
    };
    Spec {
        name: "remuda vfs".into(),
        options: vec![],
        verbs: vec![
            verb("put", "Store a local file (or - for stdin) at a remuda:// URI. Example: remuda vfs put notes.md remuda:///notes.md",
                vec![arg("SOURCE", "Local file, or - for stdin", true), arg("URI", "remuda://CLUSTER/PATH or remuda:///PATH", true)],
                vec![option("replicas", Some("N"), "Copies to keep (1-16; default from vfs settings)"),
                     option("wait", None, "Wait until the copies exist on their nodes"),
                     option("min-copies", Some("K"), "Wait until K nodes hold it, this one included; exit 75 if not in 60 s")],
                "remuda vfs put SOURCE URI [--replicas N] [--wait | --min-copies K]"),
            verb("get", "Print a file, or write it to DEST. Example: remuda vfs get remuda:///notes.md notes.md",
                vec![arg("URI", "remuda:// URI", true), arg("DEST", "Local file; omit or - for stdout", false)],
                vec![option("local", None, "Do not ask other nodes")],
                "remuda vfs get URI [DEST] [--local]"),
            verb("resolve", "Keep a copy on this node and print its local path. Example: remuda vfs resolve remuda:///notes.md",
                vec![arg("URI", "remuda:// URI", true)], vec![option("local", None, "Do not ask other nodes")],
                "remuda vfs resolve URI [--local]"),
            verb("publish", "Upload a directory as one release and mark it complete last. Example: remuda vfs publish site remuda:///design --release 2026-11-04",
                vec![arg("DIR", "Local directory to upload", true), arg("PREFIX", "remuda:// URI prefix to publish into", true)],
                vec![OptionSpec { required: true, ..option("release", Some("NAME"), "Release name; one path component, never reused once complete") },
                     option("dry-run", None, "Show what would be uploaded, then stop"),
                     option("min-copies", Some("K"), "Wait for K copies of each file and of the completion marker"),
                     option("pointer", None, "After the marker, point PREFIX/CURRENT at this release")],
                "remuda vfs publish DIR PREFIX --release NAME [--dry-run] [--min-copies K] [--pointer]"),
            verb("stat", "Show one file's version, hash, size and copies. Example: remuda vfs stat remuda:///notes.md",
                vec![arg("URI", "remuda:// URI", true)], vec![option("json", None, "Print JSON")],
                "remuda vfs stat URI [--json]"),
            verb("ls", "List files under a prefix. Example: remuda vfs ls remuda:///butler/",
                vec![arg("PREFIX", "remuda:// URI prefix; omit for all", false)],
                vec![option("json", None, "Print JSON"),
                     option("conflicts", None, "Only paths with kept concurrent writes, deleted ones too")],
                "remuda vfs ls [PREFIX] [--json] [--conflicts]"),
            verb("resolve-conflict", "Settle a path's conflicts by writing one side again. Example: remuda vfs resolve-conflict remuda:///notes.md --keep winner",
                vec![arg("URI", "remuda:// URI", true)],
                vec![OptionSpec { required: true, ..option("keep", Some("WHICH"), "winner, or the VERSION of a conflict from `remuda vfs ls --conflicts`") }],
                "remuda vfs resolve-conflict URI --keep winner|VERSION"),
            verb("rm", "Delete a file everywhere. Example: remuda vfs rm remuda:///notes.md",
                vec![arg("URI", "remuda:// URI", true)], vec![], "remuda vfs rm URI"),
            verb("sync", "Pull indexes from peers and repair missing copies now. Example: remuda vfs sync",
                vec![], vec![], "remuda vfs sync"),
            verb("status", "Show files, conflicts, copies here and each peer at a glance. Example: remuda vfs status --copies",
                vec![], vec![option("copies", None, "Also ask the nodes which copies they hold; exit 75 if a file is short"),
                             option("json", None, "Print JSON")],
                "remuda vfs status [--copies] [--json]"),
            verb("drain", "Before this node leaves, have the other nodes take copies of what it holds. Example: remuda vfs drain",
                vec![], vec![option("timeout", Some("SECS"), "Give up after this long (1-110; default 100); run drain again to go on")],
                "remuda vfs drain [--timeout SECS]"),
            verb("config", "Show or set the cluster name and default replicas. Example: remuda vfs config --cluster home --replicas 2",
                vec![], vec![option("cluster", Some("NAME"), "Cluster name used in remuda://NAME/ URIs"),
                             option("replicas", Some("N"), "Default copies for new files (1-16)"),
                             option("tombstone-days", Some("DAYS"), "Forget a delete this many days after every node has it (1-3650; default 7)"),
                             option("stale-days", Some("DAYS"), "Stop waiting for a node that has not pulled for this long (1-3650; default 30)")],
                "remuda vfs config [--cluster NAME] [--replicas N] [--tombstone-days DAYS] [--stale-days DAYS]"),
            verb("export-index", "Write index.json for an older remuda, before a downgrade. Example: remuda vfs export-index",
                vec![], vec![], "remuda vfs export-index"),
            verb("index-downgrade", "Step index.db back to schema VERSION for an older remuda, after a backup. Example: remuda vfs index-downgrade 1 --yes",
                vec![arg("VERSION", "Schema version the older remuda knows", true)],
                vec![option("yes", None, "Run it; without this, only print what would be undone")],
                "remuda vfs index-downgrade VERSION [--yes]"),
            verb("help", "Print help for a vfs command", vec![arg("COMMAND", "vfs command name", false)], vec![], "remuda vfs help [COMMAND]"),
        ],
    }
}

pub fn usage(verb: &str) -> String {
    let spec = spec();
    if verb.is_empty() {
        let mut output = "Usage: remuda vfs <COMMAND>\n\nCluster file store: files any node can read, kept on several nodes.\n\nCommands:\n".to_owned();
        for command in &spec.verbs {
            output.push_str(&format!("  {:<8} {}\n", command.name, command.about));
        }
        return output;
    }
    cli_parse::parse(&spec, &[verb, "--help"]).text
}

fn value<'a>(values: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    values.get(key).and_then(Value::as_str)
}

fn flag(values: &Map<String, Value>, key: &str) -> bool {
    values.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn fail(out: &mut Out, verb: &str, error: impl std::fmt::Display) -> u8 {
    warn!(out, "remuda: vfs {verb}: {error}");
    1
}

fn fail_vfs(out: &mut Out, verb: &str, error: &VfsError) -> u8 {
    warn!(out, "remuda: vfs {verb}: {error}");
    let next = match error.kind {
        VfsErrorKind::NoCluster => Some("run `remuda cluster init` or `remuda cluster join` first"),
        VfsErrorKind::NotFound => Some("run `remuda vfs ls` to see stored files"),
        VfsErrorKind::Unreachable => {
            Some("start the nodes that hold it, or check `remuda cluster nodes`")
        }
        VfsErrorKind::WrongCluster => {
            Some("use remuda:///PATH for this cluster, or check `remuda vfs config`")
        }
        _ => None,
    };
    if let Some(next) = next {
        warn!(out, "Next: {next}.");
    }
    1
}

/// What a vfs command printed and its exit code. The daemon fills it; the
/// CLI copies it to its own stdout and stderr.
#[derive(Default)]
pub struct Out {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Parse `args`. `Err` carries the exit code once help or a parse error
/// has been written to `out`.
fn parse(args: &[&str], out: &mut Out) -> Result<(String, Map<String, Value>), u8> {
    let help_alias = ["help"];
    let args = match args.first() {
        None | Some(&"-h" | &"--help") => &help_alias[..],
        Some(_) => args,
    };
    let verb = args[0];
    let spec = spec();
    if !spec.verbs.iter().any(|known| known.name == verb) {
        warn!(out, "remuda: unknown vfs command '{}'", verb.escape_debug());
        let _ = write!(out.stderr, "{}", usage(""));
        return Err(2);
    }
    let report = cli_parse::parse(&spec, args);
    if verb == "help" {
        let topic = value(&report.values, "COMMAND").filter(|topic| *topic != "help");
        let _ = write!(out.stdout, "{}", usage(topic.unwrap_or("")));
        return Err(0);
    }
    if !report.ok {
        if report.kind.as_deref() == Some("help") {
            let _ = write!(out.stdout, "{}", usage(verb));
            return Err(0);
        }
        warn!(out, "{}", report.text.trim_end());
        return Err(2);
    }
    Ok((verb.to_owned(), report.values))
}

/// Run `remuda vfs ARGS…` from the CLI: parse here, run in the daemon of
/// `server` (socket `socket`), which owns the index. Never starts a daemon.
pub fn run(server: &str, socket: &Path, args: &[&str]) -> ExitCode {
    let mut out = Out::default();
    let (verb, values) = match parse(args, &mut out) {
        Ok(parsed) => parsed,
        Err(code) => return finish(out, code),
    };
    if crate::ipc::connect(socket).is_err() {
        eprintln!(
            "remuda: vfs {verb} needs the remuda daemon, and none is running for {server:?}; start it with `remuda run ...` or `remuda -e 1`, then retry"
        );
        return ExitCode::FAILURE;
    }
    if verb == "publish" {
        // One `put` per file, so the loop lives here and the daemon only sees
        // the request it already knows.
        return publish(socket, &values);
    }
    let mut input = None;
    if verb == "put" {
        let source = value(&values, "SOURCE").unwrap_or_default();
        match read_source(source) {
            Ok(bytes) => input = Some(BASE64.encode(bytes)),
            Err(error) => return finish_fail(&verb, format!("cannot read {source}: {error}")),
        }
    }
    let request = Request::Vfs {
        args: forward(&verb, &values),
        input_base64: input,
    };
    let (code, stdout, stderr) = match crate::client::request(socket, &request) {
        Ok(Response::CommandResult {
            exit_code,
            stdout_base64,
            stderr_base64,
        }) => (
            exit_code,
            BASE64.decode(stdout_base64).unwrap_or_default(),
            BASE64.decode(stderr_base64).unwrap_or_default(),
        ),
        Ok(Response::Error(error)) => return finish_fail(&verb, error),
        Ok(other) => return finish_fail(&verb, format!("unexpected daemon answer: {other:?}")),
        Err(error) => return finish_fail(&verb, format!("daemon request failed: {error}")),
    };
    let _ = std::io::stderr().lock().write_all(&stderr);
    if verb == "get" && code == 0 {
        if let Some(dest) = value(&values, "DEST").filter(|dest| *dest != "-") {
            return match std::fs::write(dest, &stdout) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => finish_fail(&verb, error),
            };
        }
    }
    let _ = std::io::stdout().lock().write_all(&stdout);
    ExitCode::from(code)
}

/// Run `remuda vfs drain` in the daemon at `socket` and print what it said;
/// `remuda cluster leave` uses it. Returns drain's exit code.
pub fn drain_in_daemon(socket: &Path) -> Result<u8, String> {
    let timeout = DRAIN_TIMEOUT.as_secs().to_string();
    let request = Request::Vfs {
        args: vec!["drain".into(), "--timeout".into(), timeout],
        input_base64: None,
    };
    match crate::client::request(socket, &request) {
        Ok(Response::CommandResult {
            exit_code,
            stdout_base64,
            stderr_base64,
        }) => {
            let _ = std::io::stdout().write_all(&BASE64.decode(stdout_base64).unwrap_or_default());
            let _ = std::io::stderr().write_all(&BASE64.decode(stderr_base64).unwrap_or_default());
            Ok(exit_code)
        }
        Ok(Response::Error(error)) => Err(error),
        Ok(other) => Err(format!("unexpected daemon answer: {other:?}")),
        Err(error) => Err(format!("daemon request failed: {error}")),
    }
}

/// `verb` and its parsed `values` as daemon arguments, in spec order. SOURCE
/// becomes `-` (the content travels as input) and DEST stays with the CLI.
fn forward(verb: &str, values: &Map<String, Value>) -> Vec<String> {
    let spec = spec();
    let Some(command) = spec.verbs.iter().find(|command| command.name == verb) else {
        return vec![verb.to_owned()];
    };
    let mut args = vec![verb.to_owned()];
    for arg in &command.args {
        match (arg.name.as_str(), value(values, &arg.name)) {
            ("SOURCE", _) => args.push("-".into()),
            ("DEST", _) | (_, None) => {}
            (_, Some(text)) => args.push(text.to_owned()),
        }
    }
    for option in &command.options {
        match values.get(&option.long) {
            Some(Value::Bool(true)) => args.push(format!("--{}", option.long)),
            Some(Value::String(text)) => args.extend([format!("--{}", option.long), text.clone()]),
            _ => {}
        }
    }
    args
}

fn read_source(source: &str) -> std::io::Result<Vec<u8>> {
    let limit = crate::cluster::vfs::MAX_BLOB_BYTES;
    let mut bytes = Vec::new();
    if source == "-" {
        std::io::stdin()
            .lock()
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
    } else {
        std::fs::File::open(source)?
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
    }
    Ok(bytes)
}

fn finish(out: Out, code: u8) -> ExitCode {
    let _ = std::io::stdout().lock().write_all(&out.stdout);
    let _ = std::io::stderr().lock().write_all(&out.stderr);
    ExitCode::from(code)
}

fn finish_fail(verb: &str, error: impl std::fmt::Display) -> ExitCode {
    let mut out = Out::default();
    let code = fail(&mut out, verb, error);
    finish(out, code)
}

/// The cluster path a `remuda://` prefix names. Empty is the whole store.
fn publish_prefix(text: &str) -> Result<String, String> {
    let rest = text
        .strip_prefix(rules::SCHEME)
        .ok_or_else(|| format!("PREFIX must be a remuda:// URI, not {text:?}"))?;
    let (cluster, path) = rest.split_once('/').unwrap_or((rest, ""));
    if !cluster.is_empty() {
        return Err(format!(
            "publish into this cluster with remuda:///PATH, not remuda://{cluster}/PATH"
        ));
    }
    let path = path.trim_matches('/');
    if !path.is_empty() {
        rules::validate_path(path).map_err(|error| format!("PREFIX {text:?}: {error}"))?;
    }
    Ok(path.to_owned())
}

/// One `Request::Vfs` on its own connection. Returns the command's exit code,
/// stdout and a one-line stderr.
fn daemon_call(
    socket: &Path,
    args: Vec<String>,
    input: Option<Vec<u8>>,
) -> Result<(u8, Vec<u8>, String), String> {
    let request = Request::Vfs {
        args,
        input_base64: input.map(|bytes| BASE64.encode(bytes)),
    };
    decode_answer(crate::client::request(socket, &request))
}

/// Turn the daemon's reply into (exit, stdout, one-line stderr). Pure, so the
/// base64 and unexpected-variant rules are tested without a socket.
fn decode_answer(reply: std::io::Result<Response>) -> Result<(u8, Vec<u8>, String), String> {
    match reply {
        Ok(Response::CommandResult {
            exit_code,
            stdout_base64,
            stderr_base64,
        }) => Ok((
            exit_code,
            BASE64.decode(stdout_base64).unwrap_or_default(),
            String::from_utf8_lossy(&BASE64.decode(stderr_base64).unwrap_or_default())
                .trim()
                .to_owned(),
        )),
        Ok(Response::Error(error)) => Err(error),
        Ok(other) => Err(format!("unexpected daemon answer: {other:?}")),
        Err(error) => Err(format!("daemon request failed: {error}")),
    }
}

/// Refuse to publish over a release that already has files. Cooperative only: a
/// partitioned index can hide one from this node.
fn destination_is_empty(socket: &Path, base: &str, release: &str) -> Result<(), String> {
    let rows = daemon_call(
        socket,
        vec![
            "ls".to_owned(),
            format!("remuda:///{base}/"),
            "--json".to_owned(),
        ],
        None,
    )?;
    match rows {
        (0, stdout, _) => destination_answer(&stdout, base, release),
        (_, _, stderr) => Err(format!("cannot check the destination: {stderr}")),
    }
}

/// Only an empty JSON array means the release name is free. Malformed output is
/// refused: treating it as empty would publish over an existing release.
fn destination_answer(stdout: &[u8], base: &str, release: &str) -> Result<(), String> {
    match serde_json::from_slice::<Value>(stdout)
        .ok()
        .as_ref()
        .and_then(Value::as_array)
    {
        Some(rows) if rows.is_empty() => Ok(()),
        Some(_) => Err(format!(
            "{release} already has files at remuda:///{base}/; publish a new --release name"
        )),
        None => Err(format!("cannot read the listing for remuda:///{base}/")),
    }
}

/// What one `put` means for a publish: written and replicated, written here
/// but short of the copies wanted, or not written at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Put {
    Done,
    Short,
    Failed,
}

fn put_result(code: u8) -> Put {
    if code == 0 {
        Put::Done
    } else if code == EXIT_TEMPFAIL {
        Put::Short
    } else {
        Put::Failed
    }
}

/// One file uploaded and how many bytes it took.
struct Uploaded {
    files: usize,
    bytes: u64,
    /// Files that are here but under-replicated; the release can still finish.
    short: usize,
    failure: Option<(String, u8)>,
}

/// Upload every planned file, stopping at the first hard failure or shortage so
/// the caller can tell a reader exactly what is and is not in the store.
/// The daemon arguments for one file of a release.
fn put_args(base: &str, relative: &str, min_copies: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "put".to_owned(),
        "-".to_owned(),
        format!("remuda:///{base}/{relative}"),
    ];
    if let Some(k) = min_copies {
        args.extend(["--min-copies".to_owned(), k.to_owned()]);
    }
    args
}

fn upload_release(
    socket: &Path,
    base: &str,
    plan: &crate::vfs_publish::Plan,
    min_copies: Option<&str>,
) -> Uploaded {
    upload_release_with(
        &|args, input| daemon_call(socket, args, input),
        &crate::vfs_publish::read_verified,
        base,
        plan,
        min_copies,
    )
}

/// One daemon call: args, optional input, and (exit, stdout, stderr).
type DaemonCall<'a> =
    dyn Fn(Vec<String>, Option<Vec<u8>>) -> Result<(u8, Vec<u8>, String), String> + 'a;

/// One planned file's verified bytes; injected so a read refusal is tested
/// without touching a disk.
type VerifiedRead<'a> = dyn Fn(&crate::vfs_publish::Planned) -> Result<Vec<u8>, String> + 'a;

/// The upload loop with its daemon call injected, so the accounting rules are
/// tested without a socket.
fn upload_release_with(
    call: &DaemonCall<'_>,
    verify: &VerifiedRead<'_>,
    base: &str,
    plan: &crate::vfs_publish::Plan,
    min_copies: Option<&str>,
) -> Uploaded {
    let mut uploaded = Uploaded {
        files: 0,
        bytes: 0,
        short: 0,
        failure: None,
    };
    for file in &plan.files {
        let bytes = match verify(file) {
            Ok(bytes) => bytes,
            Err(error) => {
                uploaded.failure = Some((error, 1));
                return uploaded;
            }
        };
        match call(put_args(base, &file.relative, min_copies), Some(bytes)) {
            Ok((code, _, stderr)) => match put_result(code) {
                Put::Done => {
                    uploaded.files += 1;
                    uploaded.bytes = uploaded.bytes.saturating_add(file.size);
                }
                Put::Short => {
                    uploaded.files += 1;
                    uploaded.bytes = uploaded.bytes.saturating_add(file.size);
                    uploaded.short += 1;
                }
                Put::Failed => {
                    uploaded.failure = Some((format!("{}: {stderr}", file.relative), 1));
                    return uploaded;
                }
            },
            Err(error) => {
                uploaded.failure = Some((format!("{}: {error}", file.relative), 1));
                return uploaded;
            }
        }
    }
    uploaded
}

/// Everything a finished upload needs to write its marker and pointer.
struct Finished<'a> {
    socket: &'a Path,
    prefix: &'a str,
    base: &'a str,
    release: &'a str,
    plan: &'a crate::vfs_publish::Plan,
    min_copies: Option<&'a str>,
}

/// Put one small marker. `Ok(Put::Short)` still means its bytes are in.
/// Why a marker is not in the store: refused (an answer) or unconfirmed (no
/// answer, so nobody can say what happened).
enum MarkerError {
    Refused(String),
    Unconfirmed(String),
}

/// How to word a marker that did not land.
fn marker_words(error: &MarkerError) -> (&'static str, &str) {
    match error {
        MarkerError::Refused(detail) => ("was not written", detail),
        MarkerError::Unconfirmed(detail) => ("could not be confirmed", detail),
    }
}

fn put_marker_with(
    call: &DaemonCall<'_>,
    uri: String,
    bytes: Vec<u8>,
    min_copies: Option<&str>,
) -> Result<Put, MarkerError> {
    let mut args = vec!["put".to_owned(), "-".to_owned(), uri];
    if let Some(k) = min_copies {
        args.extend(["--min-copies".to_owned(), k.to_owned()]);
    }
    match call(args, Some(bytes)) {
        Ok((code, _, stderr)) => match put_result(code) {
            Put::Failed => Err(MarkerError::Refused(stderr)),
            other => Ok(other),
        },
        Err(error) => Err(MarkerError::Unconfirmed(error)),
    }
}

/// Write `COMPLETE` last. `Err(exit)` means it is not known to be there; the
/// message says whether the daemon refused it or the answer was lost.
fn write_complete_with(
    call: &DaemonCall<'_>,
    finished: &Finished<'_>,
    out: &mut Out,
) -> Result<Put, u8> {
    let body = format!(
        "release {}\nfiles {}\nbytes {}\n",
        finished.release,
        finished.plan.files.len(),
        finished.plan.bytes
    );
    match put_marker_with(
        call,
        format!("remuda:///{}/COMPLETE", finished.base),
        body.into_bytes(),
        finished.min_copies,
    ) {
        Err(error) => {
            let (what, detail) = marker_words(&error);
            warn!(out, "remuda: publish incomplete: every file is in remuda:///{}/, but COMPLETE {what}: {detail}", finished.base);
            if matches!(error, MarkerError::Unconfirmed(_)) {
                warn!(out, "Next: check `remuda vfs stat remuda:///{}/COMPLETE`; if it is missing, publish a new --release name.", finished.base);
            }
            Err(1)
        }
        Ok(put) => Ok(put),
    }
}

/// Write `PREFIX/CURRENT`. `Err(exit)` means it is not known to be there; the
/// message says whether it was refused or unconfirmed.
fn write_pointer_with(
    call: &DaemonCall<'_>,
    finished: &Finished<'_>,
    out: &mut Out,
) -> Result<Put, u8> {
    let prefix = if finished.prefix.is_empty() {
        String::new()
    } else {
        format!("{}/", finished.prefix)
    };
    match put_marker_with(
        call,
        format!("remuda:///{prefix}CURRENT"),
        finished.release.as_bytes().to_vec(),
        finished.min_copies,
    ) {
        Err(error) => {
            let (what, detail) = marker_words(&error);
            warn!(
                out,
                "remuda: publish: the release is complete, but CURRENT {what}: {detail}"
            );
            if matches!(error, MarkerError::Unconfirmed(_)) {
                warn!(out, "Next: check `remuda vfs stat remuda:///{prefix}CURRENT`; if it is missing, publish a new --release name.");
            }
            Err(1)
        }
        Ok(put) => Ok(put),
    }
}

fn say_published(finished: &Finished<'_>, out: &mut Out) {
    say!(
        out,
        "Published {} files ({}) as {release} to remuda:///{base}/.",
        finished.plan.files.len(),
        crate::vfs_publish::bytes_text(finished.plan.bytes),
        release = finished.release,
        base = finished.base
    );
    for skipped in &finished.plan.skipped {
        say!(out, "  skipped (not a regular file): {skipped}");
    }
}

fn say_pointer_and_reader(finished: &Finished<'_>, out: &mut Out) {
    let prefix = if finished.prefix.is_empty() {
        String::new()
    } else {
        format!("{}/", finished.prefix)
    };
    say!(
        out,
        "  CURRENT names {}; `remuda vfs get remuda:///{prefix}CURRENT` shows it (the bridge does not resolve it).",
        finished.release
    );
    say!(
        out,
        "  Read it with `remuda vfs serve --root {}`.",
        if finished.prefix.is_empty() {
            finished.release
        } else {
            finished.prefix
        }
    );
}

fn finish_release(finished: Finished<'_>, pointer: bool, out: &mut Out) -> u8 {
    let socket = finished.socket;
    finish_with(
        &|args, input| daemon_call(socket, args, input),
        finished,
        pointer,
        out,
    )
}

/// The marker stage with its daemon call injected, so COMPLETE/CURRENT
/// sequencing and each outcome are tested without a socket.
fn finish_with(call: &DaemonCall<'_>, finished: Finished<'_>, pointer: bool, out: &mut Out) -> u8 {
    let marker = match write_complete_with(call, &finished, out) {
        Ok(put) => put,
        Err(code) => return code,
    };
    if marker == Put::Short {
        warn!(
            out,
            "remuda: publish: COMPLETE is written here but not yet on the copies wanted."
        );
        warn!(
            out,
            "Next: wait for the peers, or check `remuda vfs status --copies`."
        );
    }
    say_published(&finished, out);
    let mut code = if marker == Put::Short {
        EXIT_TEMPFAIL
    } else {
        0
    };
    if pointer {
        match write_pointer_with(call, &finished, out) {
            Ok(Put::Short) => {
                warn!(
                    out,
                    "remuda: publish: CURRENT is written here but not yet on the copies wanted."
                );
                code = EXIT_TEMPFAIL;
            }
            Ok(_) => {}
            Err(exit) => return exit,
        }
        say_pointer_and_reader(&finished, out);
    } else {
        say!(
            out,
            "  Read it with `remuda vfs serve --root {}`.",
            if finished.prefix.is_empty() {
                finished.release
            } else {
                finished.prefix
            }
        );
    }
    code
}

/// What publish learned before the first byte moves.
struct Prepared {
    prefix: String,
    base: String,
    release: String,
    plan: crate::vfs_publish::Plan,
}

/// Parse, plan, budget and destination checks. Nothing is uploaded here.
fn prepare(socket: &Path, values: &Map<String, Value>) -> Result<Prepared, String> {
    use crate::vfs_publish;
    let dir = Path::new(value(values, "DIR").unwrap_or_default());
    let prefix = publish_prefix(value(values, "PREFIX").unwrap_or_default())?;
    let release = value(values, "release").unwrap_or_default().to_owned();
    let plan = vfs_publish::plan(dir, &release).map_err(|error| error.detail)?;
    vfs_publish::check_budget(&plan, dir).map_err(|error| error.detail)?;
    let base = if prefix.is_empty() {
        release.clone()
    } else {
        format!("{prefix}/{release}")
    };
    destination_is_empty(socket, &base, &release)?;
    Ok(Prepared {
        prefix,
        base,
        release,
        plan,
    })
}

/// The dry-run report: what would go, and what is skipped.
fn say_plan(prepared: &Prepared, out: &mut Out) {
    use crate::vfs_publish;
    for file in prepared.plan.files.iter().take(20) {
        say!(
            out,
            "  {:<48} {}",
            file.relative,
            vfs_publish::bytes_text(file.size)
        );
    }
    if prepared.plan.files.len() > 20 {
        say!(out, "  ... and {} more", prepared.plan.files.len() - 20);
    }
    say!(
        out,
        "Would publish {} files ({}) as {} to remuda:///{}/.",
        prepared.plan.files.len(),
        vfs_publish::bytes_text(prepared.plan.bytes),
        prepared.release,
        prepared.base
    );
    for skipped in &prepared.plan.skipped {
        say!(out, "  skipped (not a regular file): {skipped}");
    }
}

/// Say exactly where an upload stopped, and that no marker exists.
fn say_upload_failure(prepared: &Prepared, uploaded: &Uploaded, detail: &str, out: &mut Out) {
    use crate::vfs_publish;
    let remaining = prepared.plan.files.len().saturating_sub(uploaded.files + 1);
    warn!(
        out,
        "remuda: publish incomplete: {} of {} files are in remuda:///{}/; 1 failed and {remaining} were not attempted.",
        uploaded.files,
        prepared.plan.files.len(),
        prepared.base
    );
    warn!(out, "  failed: {detail}");
    warn!(
        out,
        "  COMPLETE was not written, so this release is not finished; {} was uploaded.",
        vfs_publish::bytes_text(uploaded.bytes)
    );
    warn!(out, "Next: fix the source, then publish a new --release name; `remuda vfs ls remuda:///{}/` shows what is there.", prepared.base);
}

/// `remuda vfs publish`: one ordinary `put` per file, then a completion marker.
/// The loop lives in the CLI so the daemon contract does not change.
fn publish(socket: &Path, values: &Map<String, Value>) -> ExitCode {
    let prepared = match prepare(socket, values) {
        Ok(prepared) => prepared,
        Err(error) => return finish_fail("publish", error),
    };
    let mut out = Out::default();
    if flag(values, "dry-run") {
        say_plan(&prepared, &mut out);
        return finish(out, 0);
    }
    let min_copies = value(values, "min-copies");
    let uploaded = upload_release(socket, &prepared.base, &prepared.plan, min_copies);
    if let Some((detail, code)) = uploaded.failure.as_ref() {
        say_upload_failure(&prepared, &uploaded, detail, &mut out);
        return finish(out, *code);
    }
    let code = finish_release(
        Finished {
            socket,
            prefix: &prepared.prefix,
            base: &prepared.base,
            release: &prepared.release,
            plan: &prepared.plan,
            min_copies,
        },
        flag(values, "pointer"),
        &mut out,
    );
    if uploaded.short > 0 && code == 0 {
        warn!(
            out,
            "remuda: publish: {} of {} files are here but short of the copies wanted; COMPLETE is written.",
            uploaded.short,
            prepared.plan.files.len()
        );
        return finish(out, EXIT_TEMPFAIL);
    }
    finish(out, code)
}

/// The daemon's half of [`Request::Vfs`]: run the command against this
/// node's store and answer with what it printed.
pub fn serve_request(args: &[String], input_base64: Option<&str>) -> Response {
    let input = match input_base64.map(|text| BASE64.decode(text)).transpose() {
        Ok(input) => input.unwrap_or_default(),
        Err(error) => return Response::error(format!("vfs: bad input encoding: {error}")),
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut out = Out::default();
    let code = serve(&args, &input, &mut out);
    Response::CommandResult {
        exit_code: code,
        stdout_base64: BASE64.encode(&out.stdout),
        stderr_base64: BASE64.encode(&out.stderr),
    }
}

fn serve(args: &[&str], input: &[u8], out: &mut Out) -> u8 {
    let (verb, values) = match parse(args, out) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let values = &values;
    let store = match VfsStore::open_default() {
        Ok(store) => store,
        Err(error) => return fail_vfs(out, &verb, &error),
    };
    match verb.as_str() {
        "put" => put(&store, values, input, out),
        "get" => get(&store, values, out),
        "resolve" => resolve(&store, values, out),
        "stat" => stat(&store, values, out),
        "ls" => list(&store, values, out),
        "rm" => remove(&store, values, out),
        "resolve-conflict" => settle(&store, values, out),
        "sync" => {
            let report = vfs_sync::sync_once(true);
            say!(out, "{report}");
            0
        }
        "publish" => fail(
            out,
            "publish",
            "runs in the CLI process, not the daemon; run `remuda vfs publish` there",
        ),
        "drain" => drain(values, out),
        "status" => status(values, out),
        "config" => config(&store, values, out),
        "export-index" => match store.export_legacy_index() {
            Ok(count) => {
                say!(
                    out,
                    "Wrote {count} entries to {}.",
                    store.root().join("index.json").display()
                );
                0
            }
            Err(error) => fail_vfs(out, &verb, &error),
        },
        "index-downgrade" => {
            let Ok(to) = value(values, "VERSION").unwrap_or_default().parse::<u32>() else {
                return fail(out, &verb, "VERSION must be a whole number");
            };
            match store.index_downgrade(to, flag(values, "yes")) {
                Ok(text) => {
                    say!(out, "{text}");
                    0
                }
                Err(error) => fail_vfs(out, &verb, &error),
            }
        }
        _ => unreachable!("vfs verb specification and dispatcher diverged"),
    }
}

fn writer() -> Result<String, VfsError> {
    match crate::cluster::nodes()? {
        Some((identity, _)) => Ok(identity.node_fp),
        None => Err(VfsError::new(
            VfsErrorKind::NoCluster,
            "this node has no cluster",
        )),
    }
}

fn parse_replicas(text: &str) -> Result<u8, String> {
    text.parse::<u8>()
        .ok()
        .filter(|replicas| rules::validate_replicas(*replicas).is_ok())
        .ok_or_else(|| format!("--replicas must be 1-{}", rules::MAX_REPLICAS))
}

fn put(store: &VfsStore, values: &Map<String, Value>, bytes: &[u8], out: &mut Out) -> u8 {
    let uri = match store.parse_uri(value(values, "URI").unwrap_or_default()) {
        Ok(uri) => uri,
        Err(error) => return fail_vfs(out, "put", &error),
    };
    let replicas = match value(values, "replicas") {
        Some(text) => match parse_replicas(text) {
            Ok(replicas) => replicas,
            Err(error) => return fail(out, "put", error),
        },
        None => match store.settings() {
            Ok(settings) => settings.default_replicas,
            Err(error) => return fail_vfs(out, "put", &error),
        },
    };
    let min_copies = match value(values, "min-copies") {
        Some(_) if flag(values, "wait") => {
            return fail(out, "put", "use --wait or --min-copies, not both")
        }
        Some(text) => match parse_min_copies(text, replicas) {
            Ok(min) => Some(min),
            Err(error) => return fail(out, "put", error),
        },
        None => None,
    };
    let writer = match writer() {
        Ok(writer) => writer,
        Err(error) => return fail_vfs(out, "put", &error),
    };
    let entry = match store.put(&uri.path, bytes, replicas, &writer) {
        Ok(entry) => entry,
        Err(error) => return fail_vfs(out, "put", &error),
    };
    let pushed = vfs_sync::push_entries(std::slice::from_ref(&entry));
    let cluster = store.settings().map(|s| s.cluster_name).unwrap_or_default();
    say!(
        out,
        "Stored {} (version {}, sha256 {}, {} bytes, {} copies wanted).",
        VfsUri::format(&cluster, &entry.path),
        entry.version,
        &entry.sha256[..12],
        entry.size,
        entry.replicas
    );
    let reached = pushed.iter().filter(|result| result.reached).count();
    let outcome = push_outcome(pushed.len(), reached);
    if let Some(told) = &outcome.told {
        say!(out, "{told}");
    }
    if let Some(warning) = &outcome.warning {
        warn!(out, "remuda: vfs put: {warning}");
        warn!(out, "Next: start the other nodes, or check `remuda cluster nodes`; peers copy the file when they next reach this node.");
    }
    if flag(values, "wait") {
        match vfs_sync::wait_for_replicas(&entry, WAIT_TIMEOUT) {
            Ok(holders) => say!(out, "{holders} copies confirmed."),
            Err(error) => {
                fail_vfs(out, "put", &error);
                return if outcome.only_here { EXIT_TEMPFAIL } else { 1 };
            }
        }
        return 0;
    }
    if let Some(min) = min_copies {
        return match vfs_sync::wait_for_copies(&entry, min, WAIT_TIMEOUT) {
            Ok(holders) => {
                let (said, code) = min_copies_outcome(holders, min);
                if code == 0 {
                    say!(out, "{said}");
                } else {
                    warn!(out, "remuda: vfs put: {said}");
                }
                code
            }
            Err(error) => {
                fail_vfs(out, "put", &error);
                EXIT_TEMPFAIL
            }
        };
    }
    if outcome.only_here {
        EXIT_TEMPFAIL
    } else {
        0
    }
}

/// `--copies` asks peers for at most this long.
const STATUS_COPIES_DEADLINE: Duration = Duration::from_secs(30);

fn status(values: &Map<String, Value>, out: &mut Out) -> u8 {
    let copies = flag(values, "copies");
    let report = match crate::cluster::vfs_status::status(copies, STATUS_COPIES_DEADLINE) {
        Ok(report) => report,
        Err(error) => return fail_vfs(out, "status", &error),
    };
    let short = report.copies.as_ref().is_some_and(|c| !c.short.is_empty());
    let code = if short { EXIT_TEMPFAIL } else { 0 };
    if flag(values, "json") {
        say!(
            out,
            "{}",
            serde_json::to_string(&report).unwrap_or_default()
        );
        return code;
    }
    for line in status_lines(&report) {
        say!(out, "{line}");
    }
    code
}

/// Where the stores set aside by leave or a new identity sit, if any.
fn old_stores_line(report: &StatusReport) -> Option<String> {
    if report.old_stores.is_empty() {
        return None;
    }
    let bytes = report.old_stores.iter().map(|store| store.bytes).sum();
    let paths: Vec<&str> = report.old_stores.iter().map(|s| s.path.as_str()).collect();
    Some(format!(
        "Old stores: {} set aside by `cluster leave` or `init --new-identity`, {}; delete each once you no longer need it: {}",
        report.old_stores.len(),
        bytes_text(bytes),
        paths.join(", ")
    ))
}

/// The text form of `remuda vfs status`.
fn status_lines(report: &StatusReport) -> Vec<String> {
    use crate::cluster::vfs::{MAX_ENTRIES, MAX_INDEX_ENTRIES};
    let mut lines = vec![
        format!(
            "Files: {} live of {MAX_ENTRIES}; index {} of {MAX_INDEX_ENTRIES} entries ({} deletes remembered)",
            report.live,
            report.entries,
            report.entries.saturating_sub(report.live)
        ),
        match report.conflict_paths {
            0 => "Conflicts: none".to_owned(),
            n => format!("Conflicts: {n} paths (remuda vfs ls --conflicts)"),
        },
        format!(
            "This node ({}): {} copies, {}; {} copies waiting to retry",
            report.node,
            report.local_copies,
            bytes_text(report.local_bytes),
            report.retry_waiting
        ),
    ];
    if report.peers.is_empty() {
        lines.push("Peers: none".to_owned());
    } else {
        lines.push("Peers:".to_owned());
    }
    for peer in &report.peers {
        let health = match (peer.down_secs, peer.old_build) {
            (Some(secs), _) => format!("down {}", duration_text(secs)),
            (None, true) => "old build".to_owned(),
            (None, false) => "up".to_owned(),
        };
        let pulled = peer
            .last_pull
            .map_or("never pulled from here".to_owned(), |at| {
                format!(
                    "pulled from here {} ago",
                    duration_text(report.now.saturating_sub(at))
                )
            });
        let deletes = if peer.stale {
            format!(
                "stale (over {} days): no longer holds deletes back",
                report.stale_days
            )
        } else if peer.deletes_unpulled > 0 {
            format!("holds back {} deletes", peer.deletes_unpulled)
        } else {
            String::new()
        };
        let caps = match &peer.caps {
            Some(caps) if !caps.is_empty() => format!("caps: {}", caps.join(",")),
            _ => String::new(),
        };
        let parts: Vec<&str> = [health.as_str(), &pulled, &deletes, &caps]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect();
        lines.push(format!("  {:<14} {}", peer.node, parts.join("; ")));
    }
    lines.push(format!(
        "Settings: replicas {}, tombstone-days {}, stale-days {}",
        report.replicas, report.tombstone_days, report.stale_days
    ));
    lines.extend(old_stores_line(report));
    if let Some(copies) = &report.copies {
        let first: Vec<String> = copies
            .short
            .iter()
            .take(5)
            .map(|(path, have, want)| format!("{path} {have} of {want}"))
            .collect();
        let mut line = format!("Copies: {} files complete", copies.complete);
        if !copies.short.is_empty() {
            line.push_str(&format!(
                "; {} short (first: {})",
                copies.short.len(),
                first.join(", ")
            ));
        }
        if copies.no_copy > 0 {
            line.push_str(&format!("; {} with no confirmed copy", copies.no_copy));
        }
        lines.push(line);
        if !copies.unanswered.is_empty() {
            lines.push(format!(
                "Not answered in time: {}",
                copies.unanswered.join(", ")
            ));
        }
    } else {
        lines.push("Copies: run `remuda vfs status --copies` to ask the nodes.".to_owned());
    }
    lines
}

fn duration_text(secs: u64) -> String {
    match secs {
        0..=119 => format!("{secs}s"),
        120..=7_199 => format!("{}m", secs / 60),
        7_200..=172_799 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn bytes_text(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn drain(values: &Map<String, Value>, out: &mut Out) -> u8 {
    let timeout = match value(values, "timeout").map(str::parse::<u64>) {
        None => DRAIN_TIMEOUT,
        Some(Ok(secs @ 1..=110)) => Duration::from_secs(secs),
        Some(_) => return fail(out, "drain", "--timeout must be 1-110 seconds"),
    };
    match crate::cluster::vfs_drain::drain(timeout) {
        Ok(report) => {
            let (lines, code) = drain_outcome(&report);
            for line in lines {
                if code == 0 {
                    say!(out, "{line}");
                } else {
                    warn!(out, "{line}");
                }
            }
            code
        }
        Err(error) => fail_vfs(out, "drain", &error),
    }
}

/// What drain prints, and its exit code: 0 once no file is short.
fn drain_outcome(report: &DrainReport) -> (Vec<String>, u8) {
    if report.short.is_empty() {
        return (
            vec![
                format!(
                    "All {} files held here have their copies on the other {} nodes.",
                    report.files, report.remaining
                ),
                format!(
                    "Safe to leave: on another node, run `remuda cluster revoke {}`.",
                    report.node
                ),
            ],
            0,
        );
    }
    let first: Vec<&str> = report.short.iter().take(5).map(String::as_str).collect();
    let mut lines = vec![format!(
        "remuda: vfs drain: {} of {} files still lack copies without this node (first: {}); still copying (exit code {EXIT_TEMPFAIL})",
        report.short.len(),
        report.files,
        first.join(", ")
    )];
    if report.remaining == 0 {
        lines.push(
            "Next: start or join another node; no other node is up to take the files.".into(),
        );
    } else if !report.too_old.is_empty() {
        lines.push(format!(
            "Next: upgrade {}; too old to take copies.",
            report.too_old.join(", ")
        ));
    } else {
        lines.push("Next: run `remuda vfs drain` again; peers are still copying.".into());
    }
    (lines, EXIT_TEMPFAIL)
}

/// `--min-copies K`: 1 up to the copies wanted, since nodes outside the
/// placement do not keep a copy.
fn parse_min_copies(text: &str, replicas: u8) -> Result<usize, String> {
    text.parse::<usize>()
        .ok()
        .filter(|min| (1..=usize::from(replicas)).contains(min))
        .ok_or_else(|| format!("--min-copies must be 1-{replicas} (the copies wanted)"))
}

/// The line and exit code after waiting for `min` copies. A short count is
/// not a rollback: the write stands and peers keep copying it.
fn min_copies_outcome(holders: usize, min: usize) -> (String, u8) {
    if holders >= min {
        return (format!("{holders} copies confirmed."), 0);
    }
    (
        format!(
            "committed here, but only {holders} of {min} copies confirmed in {} s; still replicating (exit code {EXIT_TEMPFAIL})",
            WAIT_TIMEOUT.as_secs()
        ),
        EXIT_TEMPFAIL,
    )
}

/// What `put` reports after telling peers about a new entry.
#[derive(Debug, PartialEq, Eq)]
struct PushOutcome {
    told: Option<String>,
    warning: Option<String>,
    /// Peers exist but none was reached: the only copy is on this node.
    only_here: bool,
}

/// `peers` is how many reachable peers this node knows; `reached` is how many
/// acknowledged the entry. With no peers at all (a one-node cluster) this
/// node is the whole placement, so that is not a failure.
fn push_outcome(peers: usize, reached: usize) -> PushOutcome {
    if peers == 0 {
        return PushOutcome {
            told: None,
            warning: None,
            only_here: false,
        };
    }
    let only_here = reached == 0;
    PushOutcome {
        told: Some(format!("Told {reached} of {peers} peers about it.")),
        warning: only_here.then(|| {
            format!(
                "stored on this node only: none of {peers} peers was reached (exit code {EXIT_TEMPFAIL})"
            )
        }),
        only_here,
    }
}

fn fetch(
    store: &VfsStore,
    uri: &VfsUri,
    local_only: bool,
) -> Result<(VfsEntry, Vec<u8>), VfsError> {
    let index = store.load_index()?;
    let entry = index
        .live(&uri.path)
        .cloned()
        .ok_or_else(|| VfsError::new(VfsErrorKind::NotFound, format!("no file at {}", uri.path)))?;
    match store.read_blob(&entry.sha256) {
        Ok(bytes) => Ok((entry, bytes)),
        Err(error)
            if !local_only
                && matches!(error.kind, VfsErrorKind::NotLocal | VfsErrorKind::Corrupt) =>
        {
            let bytes = vfs_sync::fetch_blob(store, &entry)?;
            Ok((entry, bytes))
        }
        Err(error) => Err(error),
    }
}

fn get(store: &VfsStore, values: &Map<String, Value>, out: &mut Out) -> u8 {
    let uri = match store.parse_uri(value(values, "URI").unwrap_or_default()) {
        Ok(uri) => uri,
        Err(error) => return fail_vfs(out, "get", &error),
    };
    let (_, bytes) = match fetch(store, &uri, flag(values, "local")) {
        Ok(found) => found,
        Err(error) => return fail_vfs(out, "get", &error),
    };
    out.stdout.extend_from_slice(&bytes);
    0
}

fn resolve(store: &VfsStore, values: &Map<String, Value>, out: &mut Out) -> u8 {
    let uri = match store.parse_uri(value(values, "URI").unwrap_or_default()) {
        Ok(uri) => uri,
        Err(error) => return fail_vfs(out, "resolve", &error),
    };
    // Unmark first, so a trim cannot delete the path we print; fetch again
    // if one already did.
    let local = flag(values, "local");
    let kept = fetch(store, &uri, local).and_then(|(entry, _)| {
        store.keep_blob(&entry.sha256)?;
        if !store.has_blob(&entry.sha256, entry.size) {
            fetch(store, &uri, local)?;
        }
        Ok(entry)
    });
    match kept.and_then(|entry| store.blob_path(&entry.sha256)) {
        Ok(path) => {
            say!(out, "{}", path.display());
            0
        }
        Err(error) => fail_vfs(out, "resolve", &error),
    }
}

fn entry_json(store: &VfsStore, entry: &VfsEntry) -> Value {
    serde_json::json!({
        "path": entry.path,
        "version": entry.version,
        "sha256": entry.sha256,
        "size": entry.size,
        "replicas": entry.replicas,
        "writer": entry.writer,
        "mtime": entry.mtime,
        "local": store.has_blob(&entry.sha256, entry.size),
        "deleted": entry.deleted,
        "conflicts": entry.conflicts.iter().map(|conflict| serde_json::json!({
            "version": conflict.version,
            "writer": conflict.writer,
            "sha256": conflict.sha256,
            "size": conflict.size,
            "deleted": conflict.deleted,
            "mtime": conflict.mtime,
            "local": !conflict.deleted && store.has_blob(&conflict.sha256, conflict.size),
        })).collect::<Vec<_>>(),
    })
}

/// One line per kept concurrent write, under its path.
fn say_conflicts(out: &mut Out, store: &VfsStore, entry: &VfsEntry) {
    for conflict in &entry.conflicts {
        let what = if conflict.deleted {
            "deleted".to_owned()
        } else {
            format!(
                "{} bytes  {}{}",
                conflict.size,
                &conflict.sha256[..12.min(conflict.sha256.len())],
                if store.has_blob(&conflict.sha256, conflict.size) {
                    ""
                } else {
                    "  (not here)"
                }
            )
        };
        say!(
            out,
            "  conflict v{} by {}  {what}",
            conflict.version,
            crate::cluster::node_label(&conflict.writer)
        );
    }
}

fn settle(store: &VfsStore, values: &Map<String, Value>, out: &mut Out) -> u8 {
    let uri = match store.parse_uri(value(values, "URI").unwrap_or_default()) {
        Ok(uri) => uri,
        Err(error) => return fail_vfs(out, "resolve-conflict", &error),
    };
    let keep = match value(values, "keep").unwrap_or_default() {
        "winner" => None,
        text => match text.parse::<u64>() {
            Ok(version) => Some(version),
            Err(_) => {
                return fail(
                    out,
                    "resolve-conflict",
                    "--keep takes winner or a version number",
                )
            }
        },
    };
    let writer = match writer() {
        Ok(writer) => writer,
        Err(error) => return fail_vfs(out, "resolve-conflict", &error),
    };
    // The kept side may live only on another node: copy it here first.
    if let Some(conflict) = store.load_index().ok().and_then(|index| {
        let entry = index.entries.get(&uri.path)?;
        let conflict = entry.conflicts.iter().find(|c| Some(c.version) == keep)?;
        (!conflict.deleted && !store.has_blob(&conflict.sha256, conflict.size))
            .then(|| conflict.entry(&uri.path))
    }) {
        if let Err(error) = vfs_sync::fetch_blob(store, &conflict) {
            return fail_vfs(out, "resolve-conflict", &error);
        }
    }
    match store.settle_conflict(&uri.path, keep, &writer) {
        Ok(entry) => {
            vfs_sync::push_entries(std::slice::from_ref(&entry));
            say!(
                out,
                "Settled {}: kept {}, now version {}.",
                uri.path,
                keep.map_or("the winner".to_owned(), |version| format!(
                    "version {version}"
                )),
                entry.version
            );
            0
        }
        Err(error) => fail_vfs(out, "resolve-conflict", &error),
    }
}

fn stat(store: &VfsStore, values: &Map<String, Value>, out: &mut Out) -> u8 {
    let uri = match store.parse_uri(value(values, "URI").unwrap_or_default()) {
        Ok(uri) => uri,
        Err(error) => return fail_vfs(out, "stat", &error),
    };
    let index = match store.load_index() {
        Ok(index) => index,
        Err(error) => return fail_vfs(out, "stat", &error),
    };
    let Some(entry) = index.live(&uri.path) else {
        return fail_vfs(
            out,
            "stat",
            &VfsError::new(VfsErrorKind::NotFound, format!("no file at {}", uri.path)),
        );
    };
    if flag(values, "json") {
        say!(out, "{}", entry_json(store, entry));
        return 0;
    }
    let holders = vfs_sync::placement_for(entry);
    say!(out, "path      {}", entry.path);
    say!(out, "version   {}", entry.version);
    say!(out, "sha256    {}", entry.sha256);
    say!(out, "size      {}", entry.size);
    say!(out, "replicas  {}", entry.replicas);
    say!(
        out,
        "writer    {}",
        crate::cluster::node_label(&entry.writer)
    );
    say!(
        out,
        "here      {}",
        if store.has_blob(&entry.sha256, entry.size) {
            "yes"
        } else {
            "no"
        }
    );
    say!(
        out,
        "placed on {}",
        holders
            .iter()
            .map(|fp| crate::cluster::node_label(fp))
            .collect::<Vec<_>>()
            .join(", ")
    );
    say_conflicts(out, store, entry);
    0
}

fn list(store: &VfsStore, values: &Map<String, Value>, out: &mut Out) -> u8 {
    let prefix = match value(values, "PREFIX") {
        None => String::new(),
        Some(text) => match list_prefix(store, text) {
            Ok(prefix) => prefix,
            Err(error) => return fail_vfs(out, "ls", &error),
        },
    };
    let index = match store.load_index() {
        Ok(index) => index,
        Err(error) => return fail_vfs(out, "ls", &error),
    };
    let only_conflicts = flag(values, "conflicts");
    let entries: Vec<&VfsEntry> = if only_conflicts {
        index
            .entries
            .range(prefix.clone()..)
            .take_while(|(path, _)| path.starts_with(&prefix))
            .map(|(_, entry)| entry)
            .filter(|entry| !entry.conflicts.is_empty())
            .collect()
    } else {
        index.list(&prefix)
    };
    if flag(values, "json") {
        let rows: Vec<Value> = entries
            .iter()
            .map(|entry| entry_json(store, entry))
            .collect();
        say!(out, "{}", Value::Array(rows));
        return 0;
    }
    if only_conflicts {
        if entries.is_empty() {
            say!(out, "No conflicts.");
        }
        for entry in &entries {
            say!(
                out,
                "{}  v{} by {}{}",
                entry.path,
                entry.version,
                crate::cluster::node_label(&entry.writer),
                if entry.deleted { "  (deleted)" } else { "" }
            );
            say_conflicts(out, store, entry);
        }
        if !entries.is_empty() {
            say!(
                out,
                "Next: remuda vfs resolve-conflict URI --keep winner|VERSION."
            );
        }
        return 0;
    }
    for entry in entries {
        say!(
            out,
            "{:>10}  v{:<5} {}  {}{}",
            entry.size,
            entry.version,
            &entry.sha256[..12],
            entry.path,
            if store.has_blob(&entry.sha256, entry.size) {
                ""
            } else {
                "  (not here)"
            }
        );
    }
    0
}

/// A listing prefix is a URI whose path may end in `/` (or be empty).
pub fn list_prefix(store: &VfsStore, text: &str) -> Result<String, VfsError> {
    let bad = |detail: String| VfsError::new(VfsErrorKind::BadUri, detail);
    let rest = text
        .strip_prefix(rules::SCHEME)
        .ok_or_else(|| bad(format!("not a remuda:// URI: {text}")))?;
    let (cluster, path) = rest.split_once('/').unwrap_or((rest, ""));
    let probe = format!(
        "{}{}/{}",
        rules::SCHEME,
        cluster,
        if path.is_empty() {
            "x".to_owned()
        } else {
            format!("{}x", path)
        }
    );
    store.parse_uri(&probe)?;
    Ok(path.to_owned())
}

fn remove(store: &VfsStore, values: &Map<String, Value>, out: &mut Out) -> u8 {
    let uri = match store.parse_uri(value(values, "URI").unwrap_or_default()) {
        Ok(uri) => uri,
        Err(error) => return fail_vfs(out, "rm", &error),
    };
    let writer = match writer() {
        Ok(writer) => writer,
        Err(error) => return fail_vfs(out, "rm", &error),
    };
    match store.delete(&uri.path, &writer) {
        Ok(entry) => {
            vfs_sync::push_entries(std::slice::from_ref(&entry));
            say!(out, "Deleted {} (version {}).", uri.path, entry.version);
            0
        }
        Err(error) => fail_vfs(out, "rm", &error),
    }
}

fn config(store: &VfsStore, values: &Map<String, Value>, out: &mut Out) -> u8 {
    let mut settings = match store.settings() {
        Ok(settings) => settings,
        Err(error) => return fail_vfs(out, "config", &error),
    };
    let mut changed = false;
    if let Some(name) = value(values, "cluster") {
        if let Err(error) = rules::validate_cluster_name(name) {
            return fail(out, "config", error);
        }
        settings.cluster_name = name.to_owned();
        changed = true;
    }
    if let Some(text) = value(values, "replicas") {
        match parse_replicas(text) {
            Ok(replicas) => settings.default_replicas = replicas,
            Err(error) => return fail(out, "config", error),
        }
        changed = true;
    }
    for (key, slot) in [
        ("tombstone-days", &mut settings.tombstone_days),
        ("stale-days", &mut settings.stale_days),
    ] {
        if let Some(text) = value(values, key) {
            match text
                .parse::<u32>()
                .ok()
                .filter(|days| (1..=3650).contains(days))
            {
                Some(days) => *slot = days,
                None => return fail(out, "config", format!("--{key} must be 1-3650")),
            }
            changed = true;
        }
    }
    if changed {
        if let Err(error) = store.save_settings(&settings) {
            return fail_vfs(out, "config", &error);
        }
    }
    say!(out, "cluster   {}", settings.cluster_name);
    say!(out, "replicas  {}", settings.default_replicas);
    say!(
        out,
        "deletes   forgotten {} days after every node has them; a node away {} days stops holding them",
        settings.tombstone_days,
        settings.stale_days
    );
    say!(out, "store     {}", store.root().display());
    0
}

#[cfg(test)]
mod tests {

    fn forwarded(args: &[&str]) -> Vec<String> {
        let mut out = Out::default();
        let (verb, values) = parse(args, &mut out).unwrap();
        forward(&verb, &values)
    }

    #[test]
    fn the_daemon_gets_options_but_never_local_paths() {
        assert_eq!(
            forwarded(&[
                "put",
                "./notes.md",
                "remuda:///n.md",
                "--replicas",
                "2",
                "--wait"
            ]),
            ["put", "-", "remuda:///n.md", "--replicas", "2", "--wait"]
        );
        assert_eq!(
            forwarded(&["get", "remuda:///n.md", "out.md", "--local"]),
            ["get", "remuda:///n.md", "--local"]
        );
        assert_eq!(forwarded(&["ls", "--json"]), ["ls", "--json"]);
        assert_eq!(
            forwarded(&["resolve-conflict", "remuda:///n.md", "--keep", "winner"]),
            ["resolve-conflict", "remuda:///n.md", "--keep", "winner"]
        );
        assert_eq!(forwarded(&["ls", "--conflicts"]), ["ls", "--conflicts"]);
        assert_eq!(
            forwarded(&["config", "--cluster", "home"]),
            ["config", "--cluster", "home"]
        );
    }

    #[test]
    fn help_and_parse_errors_need_no_daemon() {
        let mut out = Out::default();
        assert_eq!(parse(&["help"], &mut out).unwrap_err(), 0);
        assert!(String::from_utf8_lossy(&out.stdout).contains("export-index"));
        let mut out = Out::default();
        assert_eq!(parse(&["put"], &mut out).unwrap_err(), 2);
    }

    use super::*;

    #[test]
    fn every_verb_has_help() {
        for verb in spec().verbs {
            assert!(usage(&verb.name).contains("Usage"), "{}", verb.name);
        }
        assert!(usage("").contains("resolve"));
    }

    fn prepared(names: &[&str]) -> Prepared {
        let files: Vec<_> = names
            .iter()
            .map(|name| crate::vfs_publish::Planned {
                source: std::path::PathBuf::from("/x"),
                relative: (*name).into(),
                size: 1024,
                modified: None,
                identity: None,
                canonical: std::path::PathBuf::from("/x"),
            })
            .collect();
        Prepared {
            prefix: "design".into(),
            base: "design/v1".into(),
            release: "v1".into(),
            plan: crate::vfs_publish::Plan {
                bytes: files.len() as u64 * 1024,
                files,
                skipped: vec!["link".into()],
            },
        }
    }

    #[test]
    fn a_dry_run_report_names_the_release_and_the_skips() {
        let mut out = Out::default();
        say_plan(&prepared(&["a.html"]), &mut out);
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("Would publish 1 files (1.0 KiB) as v1 to remuda:///design/v1/."),
            "{text}"
        );
        assert!(
            text.contains("skipped (not a regular file): link"),
            "{text}"
        );
    }

    fn finish_with_call(call: &DaemonCall<'_>, pointer: bool) -> (u8, String) {
        let base = "design/v1".to_string();
        let prefix = "design".to_string();
        let release = "v1".to_string();
        let plan = crate::vfs_publish::Plan {
            files: Vec::new(),
            bytes: 0,
            skipped: Vec::new(),
        };
        let mut out = Out::default();
        let finished = Finished {
            socket: Path::new("/x"),
            prefix: &prefix,
            base: &base,
            release: &release,
            plan: &plan,
            min_copies: None,
        };
        let code = finish_with(call, finished, pointer, &mut out);
        (code, String::from_utf8_lossy(&out.stderr).to_string())
    }

    #[test]
    fn a_refused_complete_does_not_touch_current() {
        use std::cell::RefCell;
        let seen = RefCell::new(Vec::new());
        let call = |args: Vec<String>, _input: Option<Vec<u8>>| {
            seen.borrow_mut().push(args[2].clone());
            Ok((1, Vec::new(), "bad path".to_string()))
        };
        let (code, error) = finish_with_call(&call, true);
        assert_eq!(code, 1);
        assert_eq!(seen.borrow().len(), 1, "CURRENT must not follow");
        assert!(seen.borrow()[0].ends_with("/COMPLETE"));
        assert!(error.contains("COMPLETE was not written"), "{error}");
        assert!(!error.contains("CURRENT"), "{error}");
    }

    #[test]
    fn an_unconfirmed_complete_says_unknown_and_asks_for_a_check() {
        use std::cell::RefCell;
        let seen = RefCell::new(Vec::new());
        let call = |args: Vec<String>, _input: Option<Vec<u8>>| {
            seen.borrow_mut().push(args[2].clone());
            Err("timed out".to_string())
        };
        let (code, error) = finish_with_call(&call, true);
        assert_eq!(code, 1);
        assert_eq!(seen.borrow().len(), 1);
        assert!(error.contains("could not be confirmed"), "{error}");
        assert!(error.contains("vfs stat"), "{error}");
    }

    #[test]
    fn a_short_complete_still_writes_current_and_exits_75() {
        use std::cell::RefCell;
        let seen = RefCell::new(Vec::new());
        let call = |args: Vec<String>, _input: Option<Vec<u8>>| {
            let uri = args[2].clone();
            let short = uri.ends_with("/COMPLETE");
            seen.borrow_mut().push(uri);
            Ok((if short { 75 } else { 0 }, Vec::new(), "short".to_string()))
        };
        let (code, error) = finish_with_call(&call, true);
        assert_eq!(code, EXIT_TEMPFAIL);
        assert_eq!(seen.borrow().len(), 2, "CURRENT follows a written marker");
        assert!(error.contains("COMPLETE is written here"), "{error}");
    }

    #[test]
    fn a_short_current_exits_75_after_both_writes() {
        use std::cell::RefCell;
        let seen = RefCell::new(Vec::new());
        let call = |args: Vec<String>, _input: Option<Vec<u8>>| {
            let uri = args[2].clone();
            let short = uri.ends_with("/CURRENT");
            seen.borrow_mut().push(uri);
            Ok((if short { 75 } else { 0 }, Vec::new(), "short".to_string()))
        };
        let (code, error) = finish_with_call(&call, true);
        assert_eq!(code, EXIT_TEMPFAIL);
        assert_eq!(seen.borrow().len(), 2);
        assert!(error.contains("CURRENT is written here"), "{error}");
    }

    #[test]
    fn a_refused_current_is_a_failure_after_complete() {
        use std::cell::RefCell;
        let seen = RefCell::new(Vec::new());
        let call = |args: Vec<String>, _input: Option<Vec<u8>>| {
            let uri = args[2].clone();
            let current = uri.ends_with("/CURRENT");
            seen.borrow_mut().push(uri);
            if current {
                Ok((1, Vec::new(), "bad pointer".to_string()))
            } else {
                Ok((0, Vec::new(), String::new()))
            }
        };
        let (code, error) = finish_with_call(&call, true);
        assert_eq!(code, 1);
        assert_eq!(seen.borrow().len(), 2);
        assert!(error.contains("CURRENT was not written"), "{error}");
    }

    #[test]
    fn a_marker_failure_says_whether_it_is_certain() {
        let refusal = MarkerError::Refused("bad path".into());
        let refused = marker_words(&refusal);
        assert_eq!(refused.0, "was not written");
        assert_eq!(refused.1, "bad path");
        let timeout = MarkerError::Unconfirmed("timed out".into());
        let unknown = marker_words(&timeout);
        assert_eq!(unknown.0, "could not be confirmed");
        assert_eq!(unknown.1, "timed out");
    }

    fn plain_plan(names: &[&str]) -> crate::vfs_publish::Plan {
        let files: Vec<_> = names
            .iter()
            .map(|name| crate::vfs_publish::Planned {
                source: std::path::PathBuf::from("/x"),
                relative: (*name).into(),
                size: 1,
                modified: None,
                identity: None,
                canonical: std::path::PathBuf::from("/x"),
            })
            .collect();
        crate::vfs_publish::Plan {
            bytes: files.len() as u64,
            files,
            skipped: Vec::new(),
        }
    }

    #[test]
    fn a_read_refusal_stops_before_any_put() {
        use std::cell::Cell;
        let plan = plain_plan(&["a.txt", "b.txt"]);
        let calls = Cell::new(0);
        let call = |_args: Vec<String>, _input: Option<Vec<u8>>| {
            calls.set(calls.get() + 1);
            Ok((0, Vec::new(), String::new()))
        };
        let refuse = |_file: &crate::vfs_publish::Planned| Err("changed".to_string());
        let uploaded = upload_release_with(&call, &refuse, "design/v1", &plan, None);
        assert_eq!(uploaded.files, 0);
        assert_eq!(calls.get(), 0, "no put may run after a read refusal");
        assert!(uploaded.failure.is_some());
    }

    #[test]
    fn a_read_refusal_after_one_file_keeps_that_count() {
        use std::cell::Cell;
        let plan = plain_plan(&["a.txt", "b.txt"]);
        let reads = Cell::new(0);
        let calls = Cell::new(0);
        let verify = |_file: &crate::vfs_publish::Planned| {
            let n = reads.get();
            reads.set(n + 1);
            if n == 0 {
                Ok(b"x".to_vec())
            } else {
                Err("changed".to_string())
            }
        };
        let call = |_args: Vec<String>, _input: Option<Vec<u8>>| {
            calls.set(calls.get() + 1);
            Ok((0, Vec::new(), String::new()))
        };
        let uploaded = upload_release_with(&call, &verify, "design/v1", &plan, None);
        assert_eq!(uploaded.files, 1);
        assert_eq!(uploaded.bytes, 1);
        assert_eq!(calls.get(), 1);
        assert!(uploaded.failure.is_some());
    }

    #[test]
    fn put_args_name_the_file_and_optional_copies() {
        assert_eq!(
            put_args("design/v1", "a/b.html", None),
            ["put", "-", "remuda:///design/v1/a/b.html"]
        );
        assert_eq!(
            put_args("design/v1", "a.html", Some("2")),
            [
                "put",
                "-",
                "remuda:///design/v1/a.html",
                "--min-copies",
                "2"
            ]
        );
    }

    #[test]
    fn a_failed_first_put_is_not_counted_as_uploaded() {
        let plan = plain_plan(&["a.txt", "b.txt"]);
        let call = |_args: Vec<String>, _input: Option<Vec<u8>>| Err("gone".to_string());
        let verify = |_file: &crate::vfs_publish::Planned| Ok(b"x".to_vec());
        let uploaded = upload_release_with(&call, &verify, "design/v1", &plan, None);
        assert_eq!(uploaded.files, 0);
        assert_eq!(uploaded.bytes, 0);
        assert!(uploaded.failure.is_some());
    }

    #[test]
    fn a_short_put_counts_and_the_upload_continues() {
        use std::cell::Cell;
        let plan = plain_plan(&["a.txt", "b.txt"]);
        let calls = Cell::new(0);
        let call = |_args: Vec<String>, _input: Option<Vec<u8>>| {
            let n = calls.get();
            calls.set(n + 1);
            if n == 0 {
                Ok((EXIT_TEMPFAIL, Vec::new(), "short".to_string()))
            } else {
                Ok((0, Vec::new(), String::new()))
            }
        };
        let verify = |_file: &crate::vfs_publish::Planned| Ok(b"x".to_vec());
        let uploaded = upload_release_with(&call, &verify, "design/v1", &plan, None);
        assert_eq!(uploaded.files, 2);
        assert_eq!(uploaded.short, 1);
        assert!(uploaded.failure.is_none());
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn a_stopped_upload_counts_only_the_files_that_arrived() {
        let prepared = prepared(&["a.html", "b.html"]);
        let uploaded = Uploaded {
            files: 0,
            bytes: 0,
            short: 0,
            failure: None,
        };
        let mut out = Out::default();
        say_upload_failure(&prepared, &uploaded, "b.html: nope", &mut out);
        let text = String::from_utf8_lossy(&out.stderr);
        assert!(text.contains("0 of 2 files are in"), "{text}");
        assert!(text.contains("1 failed and 1 were not attempted"), "{text}");
        assert!(text.contains("COMPLETE was not written"), "{text}");
    }

    #[test]
    fn a_put_code_means_done_short_or_failed() {
        assert_eq!(put_result(0), Put::Done);
        assert_eq!(put_result(EXIT_TEMPFAIL), Put::Short);
        assert_eq!(put_result(1), Put::Failed);
        assert_eq!(put_result(2), Put::Failed);
    }

    #[test]
    fn a_daemon_answer_keeps_its_exit_code_and_decodes_text() {
        let reply = Ok(Response::CommandResult {
            exit_code: 75,
            stdout_base64: BASE64.encode(b"ok\n"),
            stderr_base64: BASE64.encode(b"short copies\n"),
        });
        let (code, stdout, stderr) = decode_answer(reply).unwrap();
        assert_eq!(code, 75);
        assert_eq!(stdout, b"ok\n");
        assert_eq!(stderr, "short copies");
        assert_eq!(
            decode_answer(Ok(Response::Error("nope".into()))).unwrap_err(),
            "nope"
        );
        assert!(decode_answer(Ok(Response::Ok))
            .unwrap_err()
            .contains("unexpected daemon answer"));
        let io_error = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "gone");
        assert!(decode_answer(Err(io_error))
            .unwrap_err()
            .contains("daemon request failed"));
    }

    #[test]
    fn a_destination_is_empty_only_for_an_empty_json_array() {
        assert!(destination_answer(b"[]", "design", "v1").is_ok());
        assert!(destination_answer(b"[{\"path\":\"a\"}]", "design", "v1")
            .unwrap_err()
            .contains("already has files"));
        assert!(destination_answer(b"not json", "design", "v1")
            .unwrap_err()
            .contains("cannot read the listing"));
        assert!(destination_answer(b"{}", "design", "v1")
            .unwrap_err()
            .contains("cannot read the listing"));
    }

    #[test]
    fn a_publish_prefix_must_name_this_cluster() {
        assert_eq!(publish_prefix("remuda:///design").unwrap(), "design");
        assert_eq!(publish_prefix("remuda:///").unwrap(), "");
        assert!(publish_prefix("remuda://home/design").is_err());
        assert!(publish_prefix("/design").is_err());
        assert!(publish_prefix("remuda:///a/../b").is_err());
    }

    #[test]
    fn put_with_no_peer_reached_is_a_temporary_failure() {
        let none = push_outcome(4, 0);
        assert!(none.only_here);
        assert_eq!(none.told.as_deref(), Some("Told 0 of 4 peers about it."));
        assert!(none
            .warning
            .as_deref()
            .is_some_and(|text| text.contains("stored on this node only") && text.contains("75")));

        let some = push_outcome(4, 1);
        assert!(!some.only_here);
        assert_eq!(some.told.as_deref(), Some("Told 1 of 4 peers about it."));
        assert_eq!(some.warning, None);

        // A one-node cluster has nobody to tell; that is not a failure.
        let alone = push_outcome(0, 0);
        assert!(!alone.only_here);
        assert_eq!(alone.told, None);
        assert_eq!(alone.warning, None);
    }

    #[test]
    fn min_copies_is_checked_against_the_copies_wanted() {
        assert_eq!(parse_min_copies("2", 3), Ok(2));
        assert_eq!(parse_min_copies("3", 3), Ok(3));
        assert!(parse_min_copies("4", 3).is_err_and(|e| e.contains("1-3")));
        assert!(parse_min_copies("0", 3).is_err());
        assert!(parse_min_copies("x", 3).is_err());
        assert_eq!(
            forwarded(&["put", "a", "remuda:///n.md", "--min-copies", "2"]),
            ["put", "-", "remuda:///n.md", "--min-copies", "2"]
        );
    }

    #[test]
    fn too_few_copies_in_time_is_a_temporary_failure_not_a_rollback() {
        assert_eq!(min_copies_outcome(2, 2), ("2 copies confirmed.".into(), 0));
        let (said, code) = min_copies_outcome(1, 3);
        assert_eq!(code, EXIT_TEMPFAIL);
        assert!(said.starts_with("committed here, but only 1 of 3 copies confirmed"));
        assert!(said.contains("still replicating"));
    }

    fn peer(node: &str) -> crate::cluster::vfs_status::PeerStatus {
        crate::cluster::vfs_status::PeerStatus {
            node: node.into(),
            down_secs: None,
            last_pull: Some(1_000 - 150),
            stale: false,
            old_build: false,
            caps: Some(vec!["take".into()]),
            deletes_unpulled: 0,
        }
    }

    fn report() -> StatusReport {
        StatusReport {
            node: "node-me".into(),
            live: 12,
            entries: 15,
            conflict_paths: 0,
            local_copies: 9,
            local_bytes: 3 * 1024 * 1024 / 2,
            retry_waiting: 2,
            replicas: 3,
            tombstone_days: 7,
            stale_days: 30,
            now: 1_000,
            peers: vec![peer("node-a")],
            copies: None,
            old_stores: Vec::new(),
        }
    }

    #[test]
    fn status_reports_counts_peers_and_settings() {
        let mut report = report();
        report.conflict_paths = 2;
        let mut down = peer("node-b");
        (down.down_secs, down.last_pull, down.caps) = (Some(3 * 3_600), None, None);
        down.deletes_unpulled = 3;
        let mut old = peer("node-c");
        (old.old_build, old.stale, old.caps) = (true, true, None);
        report.peers.extend([down, old]);
        report.old_stores = vec![crate::cluster::vfs_status::OldStore {
            path: "/s/cluster.left-20261022/vfs".into(),
            bytes: 2 * 1024 * 1024,
        }];
        let text = status_lines(&report).join("\n");
        for want in [
            "Old stores: 1 set aside by `cluster leave` or `init --new-identity`, 2.0 MiB; delete each once you no longer need it: /s/cluster.left-20261022/vfs",
            "Files: 12 live of 10000; index 15 of 30000 entries (3 deletes remembered)",
            "Conflicts: 2 paths (remuda vfs ls --conflicts)",
            "This node (node-me): 9 copies, 1.5 MiB; 2 copies waiting to retry",
            "  node-a         up; pulled from here 2m ago; caps: take",
            "  node-b         down 3h; never pulled from here; holds back 3 deletes",
            "  node-c         old build; pulled from here 2m ago; stale (over 30 days): no longer holds deletes back",
            "Settings: replicas 3, tombstone-days 7, stale-days 30",
            "Copies: run `remuda vfs status --copies` to ask the nodes.",
        ] {
            assert!(text.contains(want), "missing {want:?} in:\n{text}");
        }
    }

    #[test]
    fn status_copies_name_short_files_and_silent_nodes() {
        let mut report = report();
        report.copies = Some(crate::cluster::vfs_status::CopyStatus {
            complete: 10,
            short: vec![("a.txt".into(), 1, 3), ("b.txt".into(), 0, 2)],
            no_copy: 1,
            unanswered: vec!["node-b".into()],
        });
        let text = status_lines(&report).join("\n");
        assert!(
            text.contains("Copies: 10 files complete; 2 short (first: a.txt 1 of 3, b.txt 0 of 2); 1 with no confirmed copy"),
            "{text}"
        );
        assert!(text.contains("Not answered in time: node-b"), "{text}");
        assert_eq!(duration_text(59), "59s");
        assert_eq!(duration_text(3 * 86_400), "3d");
        assert_eq!(bytes_text(512), "512 B");
    }

    #[test]
    fn drain_says_when_it_is_safe_to_leave_and_what_blocks_it() {
        let mut report = DrainReport {
            node: "node-abcd".into(),
            files: 3,
            remaining: 2,
            ..DrainReport::default()
        };
        let (lines, code) = drain_outcome(&report);
        assert_eq!(code, 0);
        assert!(lines[1].contains("remuda cluster revoke node-abcd"));

        report.short = vec!["a.txt".into()];
        report.too_old = vec!["node-old".into()];
        let (lines, code) = drain_outcome(&report);
        assert_eq!(code, EXIT_TEMPFAIL);
        assert!(lines[0].contains("1 of 3 files") && lines[0].contains("a.txt"));
        assert!(lines[1].contains("upgrade node-old"));

        report.remaining = 0;
        assert!(drain_outcome(&report).0[1].contains("no other node is up"));
    }

    #[test]
    fn replicas_are_range_checked() {
        assert_eq!(parse_replicas("3"), Ok(3));
        assert!(parse_replicas("0").is_err());
        assert!(parse_replicas("17").is_err());
        assert!(parse_replicas("x").is_err());
    }
}
