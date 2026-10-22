//! `remuda vfs …`: the cluster virtual file store from the command line
//! (#742). The daemon owns the store (#778): the CLI parses, reads `put`'s
//! source and writes `get`'s destination, and the daemon runs the command
//! (`Request::Vfs`). With no daemon running, it fails with one line.

use crate::cli_parse::{self, ArgSpec, OptionSpec, RepeatPolicy, Spec, VerbSpec};
use crate::cluster::vfs::{VfsError, VfsErrorKind, VfsStore};
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
    match fetch(store, &uri, flag(values, "local"))
        .and_then(|(entry, _)| store.blob_path(&entry.sha256))
    {
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

    #[test]
    fn replicas_are_range_checked() {
        assert_eq!(parse_replicas("3"), Ok(3));
        assert!(parse_replicas("0").is_err());
        assert!(parse_replicas("17").is_err());
        assert!(parse_replicas("x").is_err());
    }
}
