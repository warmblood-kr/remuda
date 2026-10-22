//! `remuda db …`: schema migrations of mod databases from the command line
//! (#778). The CLI parses; the daemon runs the command (`Request::Db`), as it
//! is the only process that opens the databases. With no daemon, one line.
//!
//! `downgrade` runs the down SQL stored in the database itself, so it needs
//! no files from the newer mod. It writes a private backup first and, without
//! `--yes`, only prints what it would undo.

use crate::cli_parse::{self, ArgSpec, OptionSpec, RepeatPolicy, Spec, VerbSpec};
use crate::sqlite_migrate::{self, Applied};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use remuda_core::protocol::{Request, Response};
use serde_json::{Map, Value};
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn spec() -> Spec {
    let arg = |name: &str, help: &str, required: bool| ArgSpec {
        name: name.into(),
        help: help.into(),
        multiple: false,
        required,
    };
    let verb = |name: &str, about: &str, args, options, next: &str| VerbSpec {
        name: name.into(),
        about: about.into(),
        usage: next.strip_prefix("remuda db ").map(str::to_owned),
        args,
        options,
        next: next.into(),
    };
    let yes = OptionSpec {
        long: "yes".into(),
        short: None,
        value: None,
        help: "Run it; without this, only print what would be undone".into(),
        global: false,
        required: false,
        repeat_policy: RepeatPolicy::Reject,
    };
    Spec {
        name: "remuda db".into(),
        options: vec![],
        verbs: vec![
            verb("status", "Show each mod database's schema version and steps. Example: remuda db status butler",
                vec![arg("MOD", "Only this mod's databases", false)], vec![], "remuda db status [MOD]"),
            verb("upgrade", "Apply the installed mod's migration files. Example: remuda db upgrade butler notes",
                vec![arg("MOD", "Mod name", true), arg("NAME", "Database name", true)], vec![],
                "remuda db upgrade MOD NAME"),
            verb("downgrade", "Undo steps down to VERSION with the stored down SQL, after a backup. Example: remuda db downgrade butler notes 1 --yes",
                vec![arg("MOD", "Mod name", true), arg("NAME", "Database name", true), arg("VERSION", "Version to step back to", true)],
                vec![yes], "remuda db downgrade MOD NAME VERSION [--yes]"),
            verb("help", "Print help for a db command", vec![arg("COMMAND", "db command name", false)], vec![], "remuda db help [COMMAND]"),
        ],
    }
}

fn usage(verb: &str) -> String {
    let spec = spec();
    if verb.is_empty() {
        let mut output = "Usage: remuda db <COMMAND>\n\nSchema migrations of mod databases (remuda.db.open).\n\nCommands:\n".to_owned();
        for command in &spec.verbs {
            let _ = writeln!(output, "  {:<10} {}", command.name, command.about);
        }
        return output;
    }
    cli_parse::parse(&spec, &[verb, "--help"]).text
}

#[derive(Default)]
struct Out {
    stdout: String,
    stderr: String,
}

fn value<'a>(values: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    values.get(key).and_then(Value::as_str)
}

fn parse(args: &[&str], out: &mut Out) -> Result<(String, Map<String, Value>), u8> {
    let help_alias = ["help"];
    let args = match args.first() {
        None | Some(&"-h" | &"--help") => &help_alias[..],
        Some(_) => args,
    };
    let verb = args[0];
    let spec = spec();
    if !spec.verbs.iter().any(|known| known.name == verb) {
        let _ = writeln!(
            out.stderr,
            "remuda: unknown db command '{}'",
            verb.escape_debug()
        );
        out.stderr.push_str(&usage(""));
        return Err(2);
    }
    let report = cli_parse::parse(&spec, args);
    if verb == "help" {
        let topic = value(&report.values, "COMMAND").filter(|topic| *topic != "help");
        out.stdout.push_str(&usage(topic.unwrap_or("")));
        return Err(0);
    }
    if !report.ok {
        if report.kind.as_deref() == Some("help") {
            out.stdout.push_str(&usage(verb));
            return Err(0);
        }
        let _ = writeln!(out.stderr, "{}", report.text.trim_end());
        return Err(2);
    }
    Ok((verb.to_owned(), report.values))
}

/// Run `remuda db ARGS…`: parse here, run in the daemon of `server`.
pub fn run(server: &str, socket: &Path, args: &[&str]) -> ExitCode {
    let mut out = Out::default();
    let verb = match parse(args, &mut out) {
        Ok((verb, _)) => verb,
        Err(code) => return finish(&out, code),
    };
    if crate::ipc::connect(socket).is_err() {
        eprintln!(
            "remuda: db {verb} needs the remuda daemon, and none is running for {server:?}; start it with `remuda run ...` or `remuda -e 1`, then retry"
        );
        return ExitCode::FAILURE;
    }
    let request = Request::Db {
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
    };
    match crate::client::request(socket, &request) {
        Ok(Response::CommandResult {
            exit_code,
            stdout_base64,
            stderr_base64,
        }) => {
            let _ = std::io::stderr().write_all(&BASE64.decode(stderr_base64).unwrap_or_default());
            let _ = std::io::stdout().write_all(&BASE64.decode(stdout_base64).unwrap_or_default());
            ExitCode::from(exit_code)
        }
        Ok(Response::Error(error)) => fail_now(&verb, error),
        Ok(other) => fail_now(&verb, format!("unexpected daemon answer: {other:?}")),
        Err(error) => fail_now(&verb, format!("daemon request failed: {error}")),
    }
}

fn finish(out: &Out, code: u8) -> ExitCode {
    print!("{}", out.stdout);
    eprint!("{}", out.stderr);
    ExitCode::from(code)
}

fn fail_now(verb: &str, error: impl std::fmt::Display) -> ExitCode {
    eprintln!("remuda: db {verb}: {error}");
    ExitCode::FAILURE
}

/// The daemon's half of [`Request::Db`].
pub fn serve_request(args: &[String]) -> Response {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut out = Out::default();
    let code = match crate::db_lua::db_root() {
        Ok(root) => serve(&root, &args, &mut out),
        Err(error) => {
            let _ = writeln!(out.stderr, "remuda: db: {error}");
            1
        }
    };
    Response::CommandResult {
        exit_code: code,
        stdout_base64: BASE64.encode(&out.stdout),
        stderr_base64: BASE64.encode(&out.stderr),
    }
}

/// Run one parsed command against the databases under `root`.
fn serve(root: &Path, args: &[&str], out: &mut Out) -> u8 {
    let (verb, values) = match parse(args, out) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let result = match verb.as_str() {
        "status" => status(root, value(&values, "MOD"), out),
        "upgrade" => upgrade(root, &values, out),
        "downgrade" => downgrade(root, &values, out),
        _ => unreachable!("db verb specification and dispatcher diverged"),
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(out.stderr, "remuda: db {verb}: {error}");
            1
        }
    }
}

fn plain(what: &str, text: &str) -> Result<(), String> {
    let ok = text.len() <= 64
        && text
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(format!("{what} {text:?} is not a plain name"))
    }
}

/// An existing database `MOD/NAME.db`, opened through `crate::sqlite`.
fn open_existing(
    root: &Path,
    owner: &str,
    name: &str,
) -> Result<(PathBuf, rusqlite::Connection), String> {
    plain("mod", owner)?;
    plain("database", name)?;
    let dir = root.join(owner);
    let path = dir.join(format!("{name}.db"));
    if !path.is_file() {
        return Err(format!("no database {owner}/{name}.db"));
    }
    let conn =
        crate::sqlite::open(&dir, &format!("{name}.db")).map_err(|error| format!("io: {error}"))?;
    Ok((path, conn))
}

fn status(root: &Path, only: Option<&str>, out: &mut Out) -> Result<(), String> {
    let mut found = Vec::new();
    for owner in sorted_entries(root)? {
        if only.is_some_and(|only| only != owner) || !root.join(&owner).is_dir() {
            continue;
        }
        for file in sorted_entries(&root.join(&owner))? {
            if let Some(name) = file.strip_suffix(".db") {
                found.push((owner.clone(), name.to_owned()));
            }
        }
    }
    if found.is_empty() {
        let _ = writeln!(
            out.stdout,
            "No mod databases{}.",
            only.map(|m| format!(" for {m}")).unwrap_or_default()
        );
        return Ok(());
    }
    for (owner, name) in found {
        let (_, conn) = open_existing(root, &owner, &name)?;
        let version = sqlite_migrate::version(&conn)?;
        let applied = sqlite_migrate::status(&conn)?;
        let floor = applied
            .iter()
            .rev()
            .find(|step| !step.reversible)
            .map_or(0, |step| step.version);
        let _ = writeln!(
            out.stdout,
            "{owner}/{name}.db  version {version}, can step back to {floor}{}",
            files_note(&owner, &name, &applied)
        );
        for step in &applied {
            let undo = if step.reversible {
                ""
            } else {
                "  (irreversible)"
            };
            let _ = writeln!(out.stdout, "  {:>4}  {}{undo}", step.version, step.name);
        }
    }
    Ok(())
}

/// How the installed mod's files compare with the recorded steps.
fn files_note(owner: &str, name: &str, applied: &[Applied]) -> String {
    let Ok(Some(dir)) = crate::packages::package_dir(owner) else {
        return String::new();
    };
    let dir = dir.join("migrations").join(name);
    if !dir.is_dir() {
        return String::new();
    }
    let steps = match sqlite_migrate::load_dir(&dir) {
        Ok(steps) => steps,
        Err(error) => return format!("; files: {error}"),
    };
    for step in applied {
        let same = steps.get(step.version as usize - 1).is_some_and(|file| {
            file.name == step.name && sqlite_migrate::sha256_hex(&file.up) == step.up_sha256
        });
        if !same {
            return format!(
                "; files: step {} differs from what was applied (drift)",
                step.version
            );
        }
    }
    match steps.len().checked_sub(applied.len()) {
        Some(0) => "; files: up to date".into(),
        Some(pending) => format!("; files: {pending} pending"),
        None => format!(
            "; files: only {} steps installed (database is newer)",
            steps.len()
        ),
    }
}

fn sorted_entries(dir: &Path) -> Result<Vec<String>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("io: {}: {error}", dir.display())),
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(names)
}

fn upgrade(root: &Path, values: &Map<String, Value>, out: &mut Out) -> Result<(), String> {
    let owner = value(values, "MOD").unwrap_or_default();
    let name = value(values, "NAME").unwrap_or_default();
    let (_, mut conn) = open_existing(root, owner, name)?;
    let dir = crate::packages::package_dir(owner)?
        .ok_or_else(|| format!("mod {owner} is not installed"))?
        .join("migrations")
        .join(name);
    let steps = sqlite_migrate::load_dir(&dir)?;
    let label = format!("{owner}/{name}.db");
    let moved = sqlite_migrate::upgrade(&mut conn, &label, &steps, false)?;
    if moved.from == moved.to {
        let _ = writeln!(out.stdout, "{label} is already at version {}.", moved.to);
    } else {
        let _ = writeln!(
            out.stdout,
            "{label} is now at version {} (was {}).",
            moved.to, moved.from
        );
    }
    Ok(())
}

fn downgrade(root: &Path, values: &Map<String, Value>, out: &mut Out) -> Result<(), String> {
    let owner = value(values, "MOD").unwrap_or_default();
    let name = value(values, "NAME").unwrap_or_default();
    let to: u32 = value(values, "VERSION")
        .unwrap_or_default()
        .parse()
        .map_err(|_| "VERSION must be a whole number".to_owned())?;
    let (path, mut conn) = open_existing(root, owner, name)?;
    let label = format!("{owner}/{name}.db");
    let from = sqlite_migrate::version(&conn)?;
    if to >= from {
        return Err(format!(
            "{label} is at version {from}; nothing to undo down to {to}"
        ));
    }
    let plan = sqlite_migrate::planned_downs(&conn, to)?;
    if let Some((number, step, _)) = plan.iter().find(|(_, _, down)| down.is_none()) {
        return Err(format!(
            "{label} step {number} ({step}) is irreversible; nothing was changed"
        ));
    }
    let undo: Vec<String> = plan
        .iter()
        .map(|(number, step, _)| format!("{number} ({step})"))
        .collect();
    if !values.get("yes").and_then(Value::as_bool).unwrap_or(false) {
        let _ = writeln!(
            out.stdout,
            "Would back up {label}, then undo steps {} with their stored down SQL. Run again with --yes.",
            undo.join(", ")
        );
        return Ok(());
    }
    let copy = sqlite_migrate::backup(&conn, &path, from)?;
    let moved = sqlite_migrate::downgrade(&mut conn, &label, to)?;
    let _ = writeln!(out.stdout, "Backed up to {}.", copy.display());
    let _ = writeln!(
        out.stdout,
        "{label} is now at version {} (was {}).",
        moved.to, moved.from
    );
    Ok(())
}

#[cfg(test)]
mod tests;
