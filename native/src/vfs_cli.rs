//! `remuda vfs …`: the cluster virtual file store from the command line
//! (#742). Talks to no daemon: it reads and writes this node's store and
//! reaches peers directly over the cluster channel.

use crate::cli_parse::{self, ArgSpec, OptionSpec, RepeatPolicy, Spec, VerbSpec};
use crate::cluster::vfs::{VfsError, VfsErrorKind, VfsStore};
use crate::cluster::vfs_sync;
use remuda_core::vfs::{self as rules, VfsEntry, VfsUri};
use serde_json::{Map, Value};
use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;

const WAIT_TIMEOUT: Duration = Duration::from_secs(60);

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
                     option("wait", None, "Wait until the copies exist on their nodes")],
                "remuda vfs put SOURCE URI [--replicas N] [--wait]"),
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
                vec![arg("PREFIX", "remuda:// URI prefix; omit for all", false)], vec![option("json", None, "Print JSON")],
                "remuda vfs ls [PREFIX] [--json]"),
            verb("rm", "Delete a file everywhere. Example: remuda vfs rm remuda:///notes.md",
                vec![arg("URI", "remuda:// URI", true)], vec![], "remuda vfs rm URI"),
            verb("sync", "Pull indexes from peers and repair missing copies now. Example: remuda vfs sync",
                vec![], vec![], "remuda vfs sync"),
            verb("config", "Show or set the cluster name and default replicas. Example: remuda vfs config --cluster home --replicas 2",
                vec![], vec![option("cluster", Some("NAME"), "Cluster name used in remuda://NAME/ URIs"),
                             option("replicas", Some("N"), "Default copies for new files (1-16)")],
                "remuda vfs config [--cluster NAME] [--replicas N]"),
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

fn fail(verb: &str, error: impl std::fmt::Display) -> ExitCode {
    eprintln!("remuda: vfs {verb}: {error}");
    ExitCode::FAILURE
}

fn fail_vfs(verb: &str, error: &VfsError) -> ExitCode {
    eprintln!("remuda: vfs {verb}: {error}");
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
        eprintln!("Next: {next}.");
    }
    ExitCode::FAILURE
}

/// Run `remuda vfs ARGS…`.
pub fn run(args: &[&str]) -> ExitCode {
    let help_alias = ["help"];
    let args = match args.first() {
        None | Some(&"-h" | &"--help") => &help_alias[..],
        Some(_) => args,
    };
    let verb = args[0];
    let spec = spec();
    if !spec.verbs.iter().any(|known| known.name == verb) {
        eprintln!("remuda: unknown vfs command '{}'", verb.escape_debug());
        eprint!("{}", usage(""));
        return ExitCode::from(2);
    }
    let report = cli_parse::parse(&spec, args);
    if verb == "help" {
        let topic = value(&report.values, "COMMAND").filter(|topic| *topic != "help");
        print!("{}", usage(topic.unwrap_or("")));
        return ExitCode::SUCCESS;
    }
    if !report.ok {
        if report.kind.as_deref() == Some("help") {
            print!("{}", usage(verb));
            return ExitCode::SUCCESS;
        }
        eprintln!("{}", report.text.trim_end());
        return ExitCode::from(2);
    }
    let values = &report.values;
    let store = match VfsStore::open_default() {
        Ok(store) => store,
        Err(error) => return fail_vfs(verb, &error),
    };
    match verb {
        "put" => put(&store, values),
        "get" => get(&store, values),
        "resolve" => resolve(&store, values),
        "stat" => stat(&store, values),
        "ls" => list(&store, values),
        "rm" => remove(&store, values),
        "sync" => {
            let report = vfs_sync::sync_once();
            println!("{report}");
            ExitCode::SUCCESS
        }
        "config" => config(&store, values),
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

fn put(store: &VfsStore, values: &Map<String, Value>) -> ExitCode {
    let source = value(values, "SOURCE").unwrap_or_default();
    let uri = match store.parse_uri(value(values, "URI").unwrap_or_default()) {
        Ok(uri) => uri,
        Err(error) => return fail_vfs("put", &error),
    };
    let replicas = match value(values, "replicas") {
        Some(text) => match parse_replicas(text) {
            Ok(replicas) => replicas,
            Err(error) => return fail("put", error),
        },
        None => match store.settings() {
            Ok(settings) => settings.default_replicas,
            Err(error) => return fail_vfs("put", &error),
        },
    };
    let limit = crate::cluster::vfs::MAX_BLOB_BYTES;
    let mut bytes = Vec::new();
    let read = if source == "-" {
        std::io::stdin()
            .lock()
            .take(limit + 1)
            .read_to_end(&mut bytes)
    } else {
        std::fs::File::open(source).and_then(|file| file.take(limit + 1).read_to_end(&mut bytes))
    };
    if let Err(error) = read {
        return fail("put", format!("cannot read {source}: {error}"));
    }
    let writer = match writer() {
        Ok(writer) => writer,
        Err(error) => return fail_vfs("put", &error),
    };
    let entry = match store.put(&uri.path, &bytes, replicas, &writer) {
        Ok(entry) => entry,
        Err(error) => return fail_vfs("put", &error),
    };
    let pushed = vfs_sync::push_entries(std::slice::from_ref(&entry));
    let cluster = store.settings().map(|s| s.cluster_name).unwrap_or_default();
    println!(
        "Stored {} (version {}, sha256 {}, {} bytes, {} copies wanted).",
        VfsUri::format(&cluster, &entry.path),
        entry.version,
        &entry.sha256[..12],
        entry.size,
        entry.replicas
    );
    if !pushed.is_empty() {
        let reached = pushed.iter().filter(|result| result.reached).count();
        println!("Told {reached} of {} peers about it.", pushed.len());
    }
    if flag(values, "wait") {
        match vfs_sync::wait_for_replicas(&entry, WAIT_TIMEOUT) {
            Ok(holders) => println!("{holders} copies confirmed."),
            Err(error) => return fail_vfs("put", &error),
        }
    }
    ExitCode::SUCCESS
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

fn get(store: &VfsStore, values: &Map<String, Value>) -> ExitCode {
    let uri = match store.parse_uri(value(values, "URI").unwrap_or_default()) {
        Ok(uri) => uri,
        Err(error) => return fail_vfs("get", &error),
    };
    let (_, bytes) = match fetch(store, &uri, flag(values, "local")) {
        Ok(found) => found,
        Err(error) => return fail_vfs("get", &error),
    };
    let written = match value(values, "DEST") {
        None | Some("-") => std::io::stdout().lock().write_all(&bytes),
        Some(dest) => std::fs::write(dest, &bytes),
    };
    match written {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail("get", error),
    }
}

fn resolve(store: &VfsStore, values: &Map<String, Value>) -> ExitCode {
    let uri = match store.parse_uri(value(values, "URI").unwrap_or_default()) {
        Ok(uri) => uri,
        Err(error) => return fail_vfs("resolve", &error),
    };
    match fetch(store, &uri, flag(values, "local"))
        .and_then(|(entry, _)| store.blob_path(&entry.sha256))
    {
        Ok(path) => {
            println!("{}", path.display());
            ExitCode::SUCCESS
        }
        Err(error) => fail_vfs("resolve", &error),
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
        "local": store.has_blob(&entry.sha256),
    })
}

fn stat(store: &VfsStore, values: &Map<String, Value>) -> ExitCode {
    let uri = match store.parse_uri(value(values, "URI").unwrap_or_default()) {
        Ok(uri) => uri,
        Err(error) => return fail_vfs("stat", &error),
    };
    let index = match store.load_index() {
        Ok(index) => index,
        Err(error) => return fail_vfs("stat", &error),
    };
    let Some(entry) = index.live(&uri.path) else {
        return fail_vfs(
            "stat",
            &VfsError::new(VfsErrorKind::NotFound, format!("no file at {}", uri.path)),
        );
    };
    if flag(values, "json") {
        println!("{}", entry_json(store, entry));
        return ExitCode::SUCCESS;
    }
    let holders = vfs_sync::placement_for(entry);
    println!("path      {}", entry.path);
    println!("version   {}", entry.version);
    println!("sha256    {}", entry.sha256);
    println!("size      {}", entry.size);
    println!("replicas  {}", entry.replicas);
    println!("writer    {}", crate::cluster::node_label(&entry.writer));
    println!(
        "here      {}",
        if store.has_blob(&entry.sha256) {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "placed on {}",
        holders
            .iter()
            .map(|fp| crate::cluster::node_label(fp))
            .collect::<Vec<_>>()
            .join(", ")
    );
    ExitCode::SUCCESS
}

fn list(store: &VfsStore, values: &Map<String, Value>) -> ExitCode {
    let prefix = match value(values, "PREFIX") {
        None => String::new(),
        Some(text) => match list_prefix(store, text) {
            Ok(prefix) => prefix,
            Err(error) => return fail_vfs("ls", &error),
        },
    };
    let index = match store.load_index() {
        Ok(index) => index,
        Err(error) => return fail_vfs("ls", &error),
    };
    let entries = index.list(&prefix);
    if flag(values, "json") {
        let rows: Vec<Value> = entries
            .iter()
            .map(|entry| entry_json(store, entry))
            .collect();
        println!("{}", Value::Array(rows));
        return ExitCode::SUCCESS;
    }
    for entry in entries {
        println!(
            "{:>10}  v{:<5} {}  {}{}",
            entry.size,
            entry.version,
            &entry.sha256[..12],
            entry.path,
            if store.has_blob(&entry.sha256) {
                ""
            } else {
                "  (not here)"
            }
        );
    }
    ExitCode::SUCCESS
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

fn remove(store: &VfsStore, values: &Map<String, Value>) -> ExitCode {
    let uri = match store.parse_uri(value(values, "URI").unwrap_or_default()) {
        Ok(uri) => uri,
        Err(error) => return fail_vfs("rm", &error),
    };
    let writer = match writer() {
        Ok(writer) => writer,
        Err(error) => return fail_vfs("rm", &error),
    };
    match store.delete(&uri.path, &writer) {
        Ok(entry) => {
            vfs_sync::push_entries(std::slice::from_ref(&entry));
            println!("Deleted {} (version {}).", uri.path, entry.version);
            ExitCode::SUCCESS
        }
        Err(error) => fail_vfs("rm", &error),
    }
}

fn config(store: &VfsStore, values: &Map<String, Value>) -> ExitCode {
    let mut settings = match store.settings() {
        Ok(settings) => settings,
        Err(error) => return fail_vfs("config", &error),
    };
    let mut changed = false;
    if let Some(name) = value(values, "cluster") {
        if let Err(error) = rules::validate_cluster_name(name) {
            return fail("config", error);
        }
        settings.cluster_name = name.to_owned();
        changed = true;
    }
    if let Some(text) = value(values, "replicas") {
        match parse_replicas(text) {
            Ok(replicas) => settings.default_replicas = replicas,
            Err(error) => return fail("config", error),
        }
        changed = true;
    }
    if changed {
        if let Err(error) = store.save_settings(&settings) {
            return fail_vfs("config", &error);
        }
    }
    println!("cluster   {}", settings.cluster_name);
    println!("replicas  {}", settings.default_replicas);
    println!("store     {}", store.root().display());
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_verb_has_help() {
        for verb in spec().verbs {
            assert!(usage(&verb.name).contains("Usage"), "{}", verb.name);
        }
        assert!(usage("").contains("resolve"));
    }

    #[test]
    fn replicas_are_range_checked() {
        assert_eq!(parse_replicas("3"), Ok(3));
        assert!(parse_replicas("0").is_err());
        assert!(parse_replicas("17").is_err());
        assert!(parse_replicas("x").is_err());
    }
}
