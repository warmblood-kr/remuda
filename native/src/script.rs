//! A programming runtime, with the atomic functions wired to it.
//!
//! This is safe to do because the vocabulary handed to Lua is exactly
//! [`Request`] — the surface that has no raw write. A
//! script gets Turing-completeness over *which* operations run in *what order*;
//! it gets no operation the CLI lacks, because there is no such variant to
//! bind. Invariant 3 is enforced by the daemon when the call arrives, not by
//! the caller behaving.
//!
//! That is `PRINCIPLES.md` §6 collecting its rent: because the dangerous
//! operations were *removed from the API* rather than forbidden by rule, a
//! whole scripting language could be pointed at it without re-auditing anything.
//!
//! ⚠ **This is not a sandbox.** `remuda lua script.lua` is as trusted as
//! `sh script.sh`, and the full standard library is what makes a setup script
//! worth writing. That changes the day a script arrives from another node; the
//! restricted stdlib belongs in *that* change, where the boundary appears.

use crate::client;
use mlua::{Lua, Table, Value};
use remuda_core::keys;
use remuda_core::protocol::{Request, Response, Step};
use remuda_core::{InputSubmitOutcome, DEFAULT_INPUT_SETTLE};
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

/// Every name in the live `remuda` table: the operations bound here, plus
/// what `tools.lua` adds in pure Lua. Asserted against the live table, both
/// directions.
pub const BINDINGS: [&str; 98] = [
    "_advice_reattach",
    "_call",
    "_descriptors",
    "_dispatch_extension_command",
    "_event_counts",
    "_exec_commands",
    "_extension_commands",
    "_function_source",
    "_input_submit",
    "_input_text",
    "_input_type_text",
    "_load_extension_command",
    "_module_readiness",
    "_pending_create",
    "_pending_events",
    "_pending_line_events",
    "_pending_secret_events",
    "_process_drain",
    "_process_killpg",
    "_process_run",
    "_process_spawn",
    "_refresh_sessions_buffer",
    "_registry",
    "_registry_dump",
    "_run_due_schedules",
    "_run_schedule",
    "_schedule_fire_counts",
    "_session_resize",
    "_sync_window_shown",
    "_take_due_schedules",
    "advice_list",
    "advice_member",
    "advise",
    "after",
    "attach",
    "buffer",
    "buffers",
    "caller",
    "cancel",
    "capture",
    "capture_styled",
    "clear_hooks",
    "clear_input",
    "cli",
    "click",
    "clock",
    "close",
    "contribute",
    "contributions",
    "emit",
    "emit_filter",
    "emit_until_failure",
    "emit_until_success",
    "event_counts",
    "every",
    "exec",
    "expect",
    "expect_option",
    "extension_command",
    "fail",
    "feed",
    "fs",
    "hook_list",
    "hooks",
    "hostname",
    "http",
    "input",
    "input_line_empty",
    "insert",
    "json",
    "key",
    "kill",
    "list_dir",
    "ls",
    "mkdir",
    "new",
    "on",
    "pending",
    "process",
    "processes",
    "random_bytes",
    "reload",
    "remove_dir_all",
    "request_counts",
    "schedule",
    "schedule_fires",
    "schedule_skips",
    "schedules",
    "send",
    "session",
    "storage",
    "system",
    "tool",
    "tools",
    "type_text",
    "unadvise",
    "window",
    "windows",
];

/// name, about, signature — one row per Rust-bound word. `tools.lua` adds its
/// own rows for the words it defines in pure Lua, into the same table.
const WORDS: &[(&str, &str, &str)] = &[
    (
        "_input_submit",
        "Submit visible session text; returns 'submitted' or 'unverified'.",
        "_input_submit(name, expect) -> status",
    ),
    (
        "_input_text",
        "Deliver a normalized text burst, bracketed when enabled by the child.",
        "_input_text(name, text) -> nil",
    ),
    (
        "_input_type_text",
        "Deliver text and submit it while holding one input lock; returns 'submitted', 'unverified' or 'late'. 'late': the text is still being written to a slow pane and its Return follows when it lands (dropped after 30 s); do not resend, check the pane.",
        "_input_type_text(name, text, settle?) -> status",
    ),
    (
        "caller",
        "ADVISORY only: peer ancestry identifies a managed session, outside, or unknown; outside does not prove operator identity. Same-UID Lua can run ``remuda -e`` and wrap ``_dispatch_extension_command``; Windows parent PIDs may be stale or chosen, so this is not an authentication boundary.",
        "caller() -> {kind: 'session'|'outside'|'unknown', session?: string}",
    ),
    (
        "clear_input",
        "Write the agent-specific clear-line key to a session as one atomic input act. Refuses while a human typed in the last 2 seconds or a PTY writer is busy. OpenAI's Codex TUI binds Ctrl+U (byte 0x15) to kill text from the cursor to the line start; at the end of the composer line this clears it ([Codex issue #20698](https://github.com/openai/codex/issues/20698)). Terminal screens do not generally identify composer contents, so `cleared` is nil when unknown.",
        "clear_input(name, key) -> {cleared = string|nil}",
    ),
    (
        "_module_readiness",
        "Internal readiness poll for remuda exec.",
        "_module_readiness(name) -> {status, timeout_ms?, message?}",
    ),
    (
        "_timer_after",
        "Internal event-loop one-shot timer binding.",
        "_timer_after(seconds, callback, owner?) -> handle",
    ),
    (
        "_timer_cancel_owner",
        "Internal lifecycle cleanup for an owner's event-loop timers.",
        "_timer_cancel_owner(owner) -> nil",
    ),
    (
        "_timer_every",
        "Internal event-loop repeating timer binding.",
        "_timer_every(seconds, callback, owner?) -> handle",
    ),
    (
        "clock",
        "Monotonic milliseconds since this Lua image started.",
        "clock() -> milliseconds",
    ),
    (
        "cli",
        "Declarative command-line parsing for extension handlers.",
        "table",
    ),
    (
        "cli.parse",
        "Parse a word list against a runtime command declaration without printing or exiting. Returns {ok, verb?, values, kind?, text, code}; set required = false on an optional positional (positionals are required by default), and set multiple = true on the final positional argument to collect message body words.",
        "cli.parse(spec, argv) -> report",
    ),
    (
        "cli.capabilities",
        "Report the spec versions, report versions and features this core supports; set report_version = 2 on a spec for the stable report envelope.",
        "cli.capabilities() -> {spec_versions, report_versions, features}",
    ),
    (
        "_pending_create",
        "Create a private bounded reply handle for remuda.pending.",
        "_pending_create(timeout?) -> id, handle",
    ),
    (
    "_pending_events",
        "Drain deferred-reply completion and cancellation notifications for the Lua tick.",
        "_pending_events() -> {{id, reason?}...}",
    ),
    (
        "_pending_secret_events",
        "Drain deferred secret-prompt results for the Lua tick. Any Lua code in this image, including MCP run_script, can read these secret events; the prompt protects terminal input and display, not code inside the image.",
        "_pending_secret_events() -> {{id, prompt_id, secret? | error?}...}",
    ),
    (
        "_pending_line_events",
        "Drain deferred visible line-prompt results for the Lua tick.",
        "_pending_line_events() -> {{id, prompt_id, line? | error?}...}",
    ),
    (
        "_session_resize",
        "Resize a session after validating its requested dimensions.",
        "_session_resize(name, cols, rows) -> true, nil | nil, error",
    ),
    (
        "http",
        "Start an asynchronous bounded HTTP request; completion is delivered on the Lua image queue. pin_only must be a boolean; when true on http.request or http.peer_certificate, it replaces chain validation while preserving hostname, validity-date, and TLS signature checks. A valid SPKI pin and HTTPS are required; plain pin remains additive, and ca_file does not contribute chain trust in pin-only mode.",
        "http.request(options) -> {cancel()}",
    ),
    (
        "fs",
        "Atomic replacement of files for trusted Lua callers.",
        "table",
    ),
    (
        "fs.write_atomic",
        "Write bytes through a same-directory temporary file and atomically replace the target; private mode makes the file owner-only (mode 0600 on Unix, owner-only ACL on Windows).",
        "fs.write_atomic(path, bytes, options?) -> true, nil | nil, error",
    ),
    (
        "fs.mkdir_new",
        "Create one new directory without creating parents or trusting an existing path.",
        "fs.mkdir_new(path) -> true | nil, 'exists' | nil, error",
    ),
    (
        "fs.realpath",
        "Resolve a path to the absolute path of what it names, following every symlink and removing '.' and '..'; the file or directory must exist. Pass an absolute path: a relative one is resolved against the daemon's working directory. On Windows the answer is a verbatim path: a prefix of two backslashes, a question mark and one backslash, then the drive (C:) or, for a network path, UNC and the server and share; a junction is followed like a symlink. Returns nil and a reason: 'not_found' (nothing there, or a link whose target is gone), or one starting with 'denied: ' or 'unavailable: '. An empty or non-string path raises a Lua error. The answer is true when it is made: a link changed afterwards is not seen.",
        "fs.realpath(path) -> path | nil, reason",
    ),
    (
        "fs.is_symlink",
        "Whether the path itself is a link, without following it: true for a symlink, a dangling one included, and on Windows for a junction too (any reparse point that names another path); false for a plain file or directory. Only the last component is asked about: a link in a parent directory is followed. Returns nil and a reason: 'not_found', or one starting with 'denied: ' or 'unavailable: '. An empty or non-string path raises a Lua error.",
        "fs.is_symlink(path) -> true | false | nil, reason",
    ),
    (
        "fs.lock",
        "Take an exclusive, non-blocking OS advisory lock on the file at an absolute path the caller chooses; it is held until handle:release() or until this daemon exits, and the same path returns the same handle. The lock file is created owner-only, stays empty and is not opened through a symlink. The owner's line (session, pid, since) is kept in PATH.info and returned as info when another process holds the lock: it is message text only, never decide on it. Any other failure returns nil, error. It guards against accidents, such as a second daemon of the same user; it is not a security boundary: a hostile process of that user can delete the lock file while it is held, and a second owner can then lock a new file there.",
        "fs.lock(path) -> handle | nil, 'held', info | nil, error",
    ),
    (
        "ls",
        "List every session in the registry, reaping exited ones unless REMUDA_KEEP_EXITED is set.",
        "ls() -> {session...}",
    ),
    (
        "new",
        "Start a new session, defaulting the command to the user's shell.",
        "new(name?, argv?, cwd?, env?) -> string",
    ),
    (
        "send",
        "Deliver a line of text to a session, with Enter appended.",
        "send(name, text) -> nil",
    ),
    (
        "insert",
        "Insert raw bytes into a session with nothing appended.",
        "insert(name, text) -> nil",
    ),
    (
        "json",
        "Bounded JSON conversion for Lua values and UTF-8 JSON text.",
        "table",
    ),
    (
        "json.decode",
        "Decode strict UTF-8 JSON; repeated object keys and over-limit input return nil, error.",
        "json.decode(text) -> value, nil | nil, error",
    ),
    (
        "json.encode",
        "Encode a Lua value as bounded JSON; unsupported values raise a clear error.",
        "json.encode(value, options?) -> string",
    ),
    (
        "json.null",
        "The sentinel that represents JSON null in Lua tables.",
        "value",
    ),
    (
        "json.array",
        "Tag a dense Lua table as a JSON array, including an empty table.",
        "json.array(table) -> table",
    ),
    (
        "json.object",
        "Tag a string-keyed Lua table as a JSON object, including an empty table.",
        "json.object(table) -> table",
    ),
    (
        "key",
        "Press a named key, in Emacs kbd notation.",
        "key(name, spec) -> nil",
    ),
    (
        "click",
        "Send a mouse click at a terminal cell.",
        "click(name, col, row, button?) -> nil",
    ),
    (
        "feed",
        "Deliver a sequence of bursts and pauses as one indivisible act.",
        "feed(name, steps) -> nil",
    ),
    (
        "capture",
        "Read a session's current screen as plain text.",
        "capture(name) -> string",
    ),
    (
        "capture_styled",
        "Read a session's screen as rows of {text, dim} spans, plus its cursor.",
        "capture_styled(name) -> {rows, cursor = {row, col, visible}}",
    ),
    (
        "attach",
        "Enter raw mode on a session (a no-op inside the daemon's own image).",
        "attach(name) -> nil",
    ),
    (
        "close",
        "End a session, live or already self-exited.",
        "close(name) -> nil",
    ),
    (
        "exec",
        "Run an installed mod's entry source, by name, in this same image.",
        "exec(name) -> nil",
    ),
    (
        "fail",
        "Raise a deliberate CLI failure with a message and exit code.",
        "fail(message, code?) -> never (default code 1; valid codes are 1..255)",
    ),
    (
        "reload",
        "Reload a lifecycle-managed mod in this image, preserving state and replacing its registrations.",
        "reload(name) -> nil",
    ),
    (
        "list_dir",
        "List a directory's entries.",
        "list_dir(dir) -> {string...}",
    ),
    (
        "mkdir",
        "Create a directory, including its parents.",
        "mkdir(dir) -> nil",
    ),
    (
        "remove_dir_all",
        "Remove a directory and everything under it.",
        "remove_dir_all(dir) -> nil",
    ),
    (
        "schedule_skips",
        "How many ticker periods were skipped because the previous callback was still running.",
        "schedule_skips() -> {consecutive, total}",
    ),
    (
        "request_counts",
        "The daemon's own request-dispatch counts, by Request variant.",
        "request_counts() -> {list, eval, capture_styled}",
    ),
    (
        "random_bytes",
        "Return n binary-safe bytes from the OS CSPRNG. n must be a whole number from 1 through 65536; integer-valued Lua floats such as 32.0 are accepted. Raises a Lua error if the OS source fails.",
        "random_bytes(n) -> string",
    ),
    (
        "storage",
        "Namespace for resolving per-user config, data, state and cache directories.",
        "table",
    ),
    (
        "system",
        "OS services for trusted Lua callers.",
        "table",
    ),
    (
        "system.credential.put",
        "Store a secret in the OS credential store under service 'remuda' and account name, replacing any earlier value. name is 1 to 255 printable ASCII characters without spaces; secret is 1 to 2048 bytes; anything else raises a Lua error. Returns nil and a reason starting with 'unavailable: ' or 'denied: ' when the store cannot be used. The store is not a sandbox: any Lua code in this image, MCP run_script included, can read, replace or delete what is stored here.",
        "system.credential.put(name, secret) -> true | nil, reason",
    ),
    (
        "system.credential.get",
        "Read a secret back, binary-safe. The reason is 'not_found' when nothing is stored under name, or starts with 'unavailable: ' or 'denied: '. On macOS the Keychain may ask the user to allow access; the call, and the whole Lua image with it, waits until the user answers.",
        "system.credential.get(name) -> secret | nil, reason",
    ),
    (
        "system.credential.delete",
        "Remove a stored secret. The reason is 'not_found' when nothing is stored under name, or starts with 'unavailable: ' or 'denied: '.",
        "system.credential.delete(name) -> true | nil, reason",
    ),
    (
        "system.credential.backend",
        "The OS credential store in use: 'keychain' (the macOS login Keychain), 'wincred' (Windows Credential Manager), or nil where there is none, in which case put, get and delete return nil, 'unavailable: no credential store on this OS'.",
        "system.credential.backend() -> 'keychain' | 'wincred' | nil",
    ),
    (
        "storage.dir",
        "Return the absolute user directory for config, data, state or cache. On Windows, an absolute ``XDG_*_HOME`` value takes precedence over Local AppData. Returns nil and an unavailable reason when the path cannot be resolved; unknown kinds raise a Lua usage error.",
        "storage.dir(kind) -> path | nil, 'unavailable: reason'",
    ),
    (
        "hostname",
        "The OS host name, read from the OS itself (not the environment). Returned unchanged and not sanitized for use in identifiers; callers slug it. Returns nil, error if the OS call fails or the name is empty, not UTF-8, or holds a control, line-separator (U+2028, U+2029) or bidi-control (U+061C, U+200E, U+200F, U+202A-U+202E, U+2066-U+2069) character.",
        "hostname() -> string, nil | nil, error",
    ),
    (
        "_registry",
        "The word registry itself: name, about and signature for every bound word.",
        "table",
    ),
    (
        "_process_run",
        "Run an argv process synchronously with a bounded timeout and captured output; internal, called by `remuda.process.run`. Its optional stdin_hold_until_lines keeps stdin open until stdout has that many newlines, the child exits, or timeout.",
        "_process_run(argv, stdin?, timeout, cwd?, stdin_hold_until_lines?, env?, clear_env?) -> result | nil, refusal",
    ),
    (
        "_process_spawn",
        "Spawn a plain-pipe child process; internal, wrapped by `remuda.process`.",
        "_process_spawn(argv, on_line?, on_exit?, cwd?, env?, clear_env?) -> id | nil, refusal",
    ),
    (
        "_process_drain",
        "Deliver buffered process output as emit events; internal, an Image job only.",
        "_process_drain(id) -> nil",
    ),
    (
        "kill",
        "Terminate a process started with `remuda.process`, by id.",
        "kill(id) -> nil",
    ),
    (
        "_function_source",
        "Where a Lua function was defined, as `source:line`; internal, for \
         `hook_list`, since scripts get no `debug` library.",
        "_function_source(fn) -> string",
    ),
    (
        "_process_killpg",
        "Reap a process's whole process group (Linux only); internal, called \
         by the daemon's own clean-shutdown sweep, not meant for scripts.",
        "_process_killpg(id) -> nil",
    ),
    (
        "processes",
        "List the ids of every process started with `remuda.process` that is still running.",
        "processes() -> {id...}",
    ),
];

/// Populate `remuda._registry` with one row per entry in [`WORDS`]. Called
/// before `tools.lua` loads, so its own registrations land in the same table.
fn registry_bindings(lua: &Lua, table: &Table) -> mlua::Result<()> {
    let registry = lua.create_table()?;
    for (name, about, signature) in WORDS {
        let row = lua.create_table()?;
        row.set("name", *name)?;
        row.set("about", *about)?;
        row.set("signature", *signature)?;
        registry.set(*name, row)?;
    }
    table.set("json", crate::json::bindings(lua)?)?;
    table.set("cli", cli_parse_bindings(lua)?)?;
    table.set("system", crate::credential::bindings(lua)?)?;
    table.set("storage", crate::storage::bindings(lua)?)?;
    fs_bindings(lua, table)?;
    table.set("_registry", registry)
}

fn cli_parse_bindings(lua: &Lua) -> mlua::Result<Table> {
    let cli = lua.create_table()?;
    cli.set(
        "parse",
        lua.create_function(|lua, (spec_table, argv_table): (Table, Table)| {
            // Raw lookup: no __index, no coercion; only an exact integer 2 opts in.
            let spec_table_wants_v2 = matches!(
                spec_table.raw_get::<Value>("report_version")?,
                Value::Integer(2)
            );
            let strict = match crate::cli_spec_check::check(&spec_table) {
                Ok(strict) => strict,
                Err((kind, text)) => {
                    let spec = crate::cli_parse::Spec {
                        name: String::new(),
                        options: vec![],
                        verbs: vec![],
                    };
                    let report = crate::cli_parse::Report::failure(kind, text, 2);
                    return cli_report_to_lua(lua, &spec, &report, spec_table_wants_v2);
                }
            };
            let spec = match strict {
                Some(spec) => spec,
                None => cli_spec_from_lua(spec_table)?,
            };
            let argv = cli_argv_from_lua(argv_table)?;
            let words = argv.iter().map(String::as_str).collect::<Vec<_>>();
            let report = crate::cli_parse::parse(&spec, &words);
            cli_report_to_lua(lua, &spec, &report, spec_table_wants_v2)
        })?,
    )?;
    cli.set(
        "capabilities",
        lua.create_function(|lua, ()| {
            let caps = serde_json::json!({
                "spec_versions": crate::cli_parse::SPEC_VERSIONS,
                "report_versions": crate::cli_parse::REPORT_VERSIONS,
                "features": crate::cli_parse::FEATURES,
            });
            json_value_to_lua(lua, &caps)
        })?,
    )?;
    Ok(cli)
}

fn cli_report_to_lua(
    lua: &Lua,
    spec: &crate::cli_parse::Spec,
    report: &crate::cli_parse::Report,
    v2: bool,
) -> mlua::Result<Value> {
    if v2 {
        return json_value_to_lua(lua, &crate::cli_parse::report_v2(spec, report));
    }
    let result = lua.create_table()?;
    result.set("ok", report.ok)?;
    result.set("verb", report.verb.clone())?;
    result.set("kind", report.kind.clone())?;
    result.set("text", report.text.clone())?;
    result.set("code", report.code)?;
    let values = lua.create_table()?;
    for (key, value) in &report.values {
        values.set(key.as_str(), json_value_to_lua(lua, value)?)?;
    }
    result.set("values", values)?;
    Ok(Value::Table(result))
}

fn cli_spec_from_lua(table: Table) -> mlua::Result<crate::cli_parse::Spec> {
    use crate::cli_parse::{ArgSpec, Spec, VerbSpec};

    let name = table.get::<String>("name")?;
    let options = cli_options_from_lua(table.get::<Option<Table>>("options")?)?;
    let verbs_table = table.get::<Table>("verbs")?;
    let mut verb_entries = verbs_table
        .pairs::<String, Table>()
        .collect::<mlua::Result<Vec<_>>>()?;
    verb_entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut verbs = Vec::new();
    for (name, verb) in verb_entries {
        let about = verb.get::<Option<String>>("about")?.unwrap_or_default();
        let next = verb.get::<String>("next")?;
        let args_table = verb.get::<Option<Table>>("args")?;
        let mut args = Vec::new();
        if let Some(args_table) = args_table {
            for item in args_table.sequence_values::<Table>() {
                let item = item?;
                args.push(ArgSpec {
                    name: item.get("name")?,
                    help: item.get("help")?,
                    multiple: item.get::<Option<bool>>("multiple")?.unwrap_or(false),
                    required: item.get::<Option<bool>>("required")?.unwrap_or(true),
                });
            }
        }
        verbs.push(VerbSpec {
            name,
            about,
            args,
            next,
            options: cli_options_from_lua(verb.get::<Option<Table>>("options")?)?,
        });
    }
    let spec = Spec {
        name,
        options,
        verbs,
    };
    validate_cli_spec(&spec)?;
    Ok(spec)
}

fn cli_argv_from_lua(table: Table) -> mlua::Result<Vec<String>> {
    let mut indexed = std::collections::BTreeMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair?;
        let index = match key {
            Value::Integer(index) if index > 0 => usize::try_from(index).ok(),
            Value::Number(index) if index.is_finite() && index.fract() == 0.0 && index >= 1.0 => {
                usize::try_from(index as u64).ok()
            }
            _ => None,
        }
        .ok_or_else(|| mlua::Error::runtime("remuda.cli.parse argv must be a dense array"))?;
        let word = match value {
            Value::String(value) => value.to_str()?.to_owned(),
            _ => {
                return Err(mlua::Error::runtime(format!(
                    "remuda.cli.parse argv[{index}] must be a string"
                )))
            }
        };
        indexed.insert(index, word);
    }

    let mut argv = Vec::with_capacity(indexed.len());
    for (expected, (index, word)) in indexed.into_iter().enumerate() {
        if index != expected + 1 {
            return Err(mlua::Error::runtime(
                "remuda.cli.parse argv must be a dense array",
            ));
        }
        argv.push(word);
    }
    Ok(argv)
}

fn cli_spec_valid_token(value: &str, allow_underscore: bool) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && chars
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || (allow_underscore && ch == '_'))
}

fn validate_cli_options(
    options: &[crate::cli_parse::OptionSpec],
    ids: &mut std::collections::HashSet<String>,
    shorts: &mut std::collections::HashSet<char>,
) -> mlua::Result<()> {
    for option in options {
        if !cli_spec_valid_token(&option.long, false) {
            return Err(mlua::Error::runtime(format!(
                "remuda.cli.parse option long must be a non-empty alphanumeric/hyphen name: {:?}",
                option.long
            )));
        }
        if option.long == "help" {
            return Err(mlua::Error::runtime(
                "remuda.cli.parse option long 'help' is reserved",
            ));
        }
        if option.long == "__help" {
            return Err(mlua::Error::runtime(
                "remuda.cli.parse argument id '__help' is reserved",
            ));
        }
        if !ids.insert(option.long.clone()) {
            return Err(mlua::Error::runtime(format!(
                "remuda.cli.parse argument id '{}' is duplicated",
                option.long
            )));
        }
        if let Some(short) = option.short {
            if !short.is_ascii_alphanumeric() || short == 'h' {
                return Err(mlua::Error::runtime(format!(
                    "remuda.cli.parse option short '{}' is invalid or reserved",
                    short
                )));
            }
            if !shorts.insert(short) {
                return Err(mlua::Error::runtime(format!(
                    "remuda.cli.parse option short '{}' is duplicated",
                    short
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_cli_spec(spec: &crate::cli_parse::Spec) -> mlua::Result<()> {
    use std::collections::HashSet;

    let mut verb_names = HashSet::new();
    for verb in &spec.verbs {
        if verb.name == "help" {
            return Err(mlua::Error::runtime(
                "remuda.cli.parse verb name 'help' is reserved",
            ));
        }
        if !cli_spec_valid_token(&verb.name, false) || !verb_names.insert(verb.name.as_str()) {
            return Err(mlua::Error::runtime(format!(
                "remuda.cli.parse verb name is invalid or duplicated: {:?}",
                verb.name
            )));
        }
    }

    let mut global_ids = HashSet::new();
    let mut global_shorts = HashSet::new();
    validate_cli_options(&spec.options, &mut global_ids, &mut global_shorts)?;

    for verb in &spec.verbs {
        if verb
            .args
            .iter()
            .enumerate()
            .any(|(index, arg)| arg.multiple && index + 1 != verb.args.len())
        {
            return Err(mlua::Error::runtime(format!(
                "remuda.cli.parse multiple positional argument must be last for verb '{}'",
                verb.name
            )));
        }
        let mut saw_optional = false;
        for arg in &verb.args {
            if saw_optional && arg.required {
                return Err(mlua::Error::runtime(format!(
                    "remuda.cli.parse required positional argument must not follow an optional positional argument for verb '{}'",
                    verb.name
                )));
            }
            saw_optional |= !arg.required;
        }
        let mut ids = global_ids.clone();
        let mut shorts = global_shorts.clone();
        validate_cli_options(&verb.options, &mut ids, &mut shorts)?;
        for arg in &verb.args {
            if !cli_spec_valid_token(&arg.name, true) {
                return Err(mlua::Error::runtime(format!(
                    "remuda.cli.parse positional name is invalid: {:?}",
                    arg.name
                )));
            }
            if arg.name == "__help" {
                return Err(mlua::Error::runtime(
                    "remuda.cli.parse argument id '__help' is reserved",
                ));
            }
            if !ids.insert(arg.name.clone()) {
                return Err(mlua::Error::runtime(format!(
                    "remuda.cli.parse argument id '{}' is duplicated",
                    arg.name
                )));
            }
        }
    }
    Ok(())
}

fn cli_options_from_lua(table: Option<Table>) -> mlua::Result<Vec<crate::cli_parse::OptionSpec>> {
    let mut options = Vec::new();
    if let Some(table) = table {
        for item in table.sequence_values::<Table>() {
            let item = item?;
            let short = item
                .get::<Option<String>>("short")?
                .map(|short| {
                    let mut chars = short.chars();
                    match (chars.next(), chars.next()) {
                        (Some(value), None) => Ok(value),
                        _ => Err(mlua::Error::runtime(
                            "remuda.cli.parse option short must be one character",
                        )),
                    }
                })
                .transpose()?;
            options.push(crate::cli_parse::OptionSpec {
                long: item.get("long")?,
                short,
                value: item.get("value")?,
                help: item.get("help")?,
                global: item.get::<Option<bool>>("global")?.unwrap_or(false),
                repeat_policy: Default::default(),
            });
        }
    }
    Ok(options)
}

fn json_value_to_lua(lua: &Lua, value: &serde_json::Value) -> mlua::Result<Value> {
    Ok(match value {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(value) => Value::Boolean(*value),
        serde_json::Value::Number(value) => match value.as_i64() {
            Some(value) => Value::Integer(value),
            None => Value::Number(value.as_f64().unwrap_or_default()),
        },
        serde_json::Value::String(value) => Value::String(lua.create_string(value)?),
        serde_json::Value::Array(items) => {
            let table = lua.create_table()?;
            for (index, item) in items.iter().enumerate() {
                table.raw_set(index + 1, json_value_to_lua(lua, item)?)?;
            }
            Value::Table(table)
        }
        serde_json::Value::Object(items) => {
            let table = lua.create_table()?;
            for (key, item) in items {
                table.set(key.as_str(), json_value_to_lua(lua, item)?)?;
            }
            Value::Table(table)
        }
    })
}

/// Run source text **in the daemon's image**, the same as `run` but for a
/// chunk with no file on disk. `name` becomes the chunk name, so a traceback
/// still names it.
pub fn run_source(socket: &Path, name: &str, source: &str) -> Result<(), String> {
    let output = eval_source(socket, name, source)?;
    if !output.is_empty() {
        println!("{output}");
    }
    Ok(())
}

/// Evaluate source in the daemon's image and return captured output without
/// relaying it. The CLI uses this for private status probes between user-facing
/// commands.
pub fn eval_source(socket: &Path, name: &str, source: &str) -> Result<String, String> {
    let request = Request::Eval {
        code: source.to_string(),
        name: Some(name.to_string()),
    };
    match client::request(socket, &request).map_err(|e| e.to_string())? {
        Response::Value(output) => Ok(output),
        Response::Error(reason) => Err(reason),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Run a script file **in the daemon's image**, never in a fresh `Lua::new()`
/// here — a script must see the state `-e` and the REPL share. The chunk name
/// travels with the source so a traceback still names the file.
pub fn run(socket: &Path, script: &Path) -> Result<(), String> {
    let source = std::fs::read_to_string(script).map_err(|e| e.to_string())?;
    run_source(socket, &script.display().to_string(), &source)
}

/// `new`'s wire shape, from the four positional Lua arguments `bindings`
/// hands it. `env` is a Lua table, already converted by the caller.
fn new_request(
    name: Option<String>,
    argv: Option<Vec<String>>,
    cwd: Option<String>,
    env: Option<std::collections::HashMap<String, String>>,
) -> Request {
    Request::New {
        name,
        command: argv.unwrap_or_default(),
        size: crate::terminal_size(),
        cwd,
        env,
    }
}

fn caller_binding(
    lua: &Lua,
    table: &Table,
    caller: Rc<RefCell<crate::image::CallerContext>>,
) -> mlua::Result<()> {
    table.set(
        "caller",
        lua.create_function(move |lua, ()| {
            let caller = caller.borrow().clone();
            let value = lua.create_table()?;
            let kind = match caller.kind {
                crate::image::CallerKind::Session => "session",
                crate::image::CallerKind::Outside => "outside",
                crate::image::CallerKind::Unknown => "unknown",
            };
            value.set("kind", kind)?;
            value.set("session", caller.session)?;
            Ok(value)
        })?,
    )
}

fn input_bindings(
    lua: &Lua,
    table: &Table,
    input_registry: std::sync::Arc<remuda_core::Registry>,
) -> mlua::Result<()> {
    let text_registry = input_registry.clone();
    table.set(
        "_input_text",
        lua.create_function(move |_, (name, text): (String, String)| {
            let session = text_registry
                .get(&name)
                .ok_or_else(|| mlua::Error::runtime(format!("no such session: {name}")))?;
            session
                .input_text(&text)
                .map_err(|error| mlua::Error::runtime(error.to_string()))
        })?,
    )?;

    let submit_registry = input_registry.clone();
    table.set(
        "_input_submit",
        lua.create_function(move |_, (name, expect): (String, String)| {
            let session = submit_registry
                .get(&name)
                .ok_or_else(|| mlua::Error::runtime(format!("no such session: {name}")))?;
            session
                .submit(&expect)
                .map(|outcome| match outcome {
                    InputSubmitOutcome::Submitted => "submitted",
                    InputSubmitOutcome::Unverified => "unverified",
                    InputSubmitOutcome::Late => "late",
                })
                .map_err(|error| mlua::Error::runtime(error.to_string()))
        })?,
    )?;

    table.set(
        "_input_type_text",
        lua.create_function(
            move |_, (name, text, settle): (String, String, Option<f64>)| {
                let settle = settle.unwrap_or(DEFAULT_INPUT_SETTLE.as_secs_f64());
                if !settle.is_finite() || !(0.0..=5.0).contains(&settle) {
                    return Err(mlua::Error::runtime(
                        "settle must be between 0 and 5 seconds",
                    ));
                }
                let session = input_registry
                    .get(&name)
                    .ok_or_else(|| mlua::Error::runtime(format!("no such session: {name}")))?;
                session
                    .type_text(&text, Duration::from_secs_f64(settle))
                    .map(|outcome| match outcome {
                        InputSubmitOutcome::Submitted => "submitted",
                        InputSubmitOutcome::Unverified => "unverified",
                        InputSubmitOutcome::Late => "late",
                    })
                    .map_err(|error| mlua::Error::runtime(error.to_string()))
            },
        )?,
    )?;
    Ok(())
}

pub(crate) fn bindings(
    lua: &Lua,
    socket: &Path,
    registry: std::sync::Arc<remuda_core::Registry>,
    counters: std::sync::Arc<crate::tick::Counters>,
    image: crate::image::Image,
    caller: Rc<RefCell<crate::image::CallerContext>>,
    timers: crate::image::timers::SharedTimerService,
) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    let at = || socket.to_path_buf();
    let input_registry = registry.clone();
    fail_binding(lua, &table, image.clone())?;
    pending_bindings(lua, &table, image.pending_replies(), Rc::clone(&caller))?;
    timer_bindings(lua, &table, timers)?;
    caller_binding(lua, &table, caller)?;
    random_bytes_binding(lua, &table)?;
    hostname_binding(lua, &table)?;

    // In-process, not a loopback: the image always runs inside the same
    // daemon this `Registry` belongs to (image.rs), so asking over the wire
    // for an answer this call already has bought nothing but a socket round
    // trip on every non-skip_list tick refresh. See steps/031.
    let image_for_ls = image.clone();
    table.set(
        "ls",
        lua.create_function(move |lua, ()| {
            if !crate::daemon::keep_exited() {
                crate::daemon::reap_and_notify(&registry, &image_for_ls);
            }
            value(lua, Response::Sessions(registry.list()))
        })?,
    )?;

    session_resize_binding(lua, &table, at())?;

    new_binding(lua, &table, at())?;

    let path = at();
    table.set(
        "send",
        lua.create_function(move |lua, (name, text): (String, String)| {
            value(lua, ask(&path, Request::SendLine { name, text })?)
        })?,
    )?;

    clear_input_binding(lua, &table, at())?;

    // The `insert-char` analogue: exactly these bytes, nothing appended. An
    // `mlua::String` rather than a Rust `String` so a script can hand over any
    // byte sequence — an escape sequence is text, but a caller building one
    // should not have to satisfy UTF-8 to reach the pty.
    let path = at();
    table.set(
        "insert",
        lua.create_function(move |lua, (name, text): (String, mlua::LuaString)| {
            let bytes = text.as_bytes().to_vec();
            value(lua, ask(&path, Request::Send { name, bytes })?)
        })?,
    )?;

    input_bindings(lua, &table, input_registry)?;

    // Named keys, in Emacs's `kbd` notation. An unknown name is raised, not
    // quietly encoded as an empty burst — a script that presses nothing and
    // reports success is the failure mode `value` exists to prevent, and this
    // is the same failure one layer earlier.
    let path = at();
    table.set(
        "key",
        lua.create_function(move |lua, (name, spec): (String, String)| {
            let bytes = keys::key(&spec)
                .ok_or_else(|| mlua::Error::runtime(format!("no such key: {spec}")))?;
            value(lua, ask(&path, Request::Send { name, bytes })?)
        })?,
    )?;

    // Column and row are 1-based, matching the terminal's own coordinates and
    // Lua's own indexing, so nothing has to be converted at the call site.
    let path = at();
    table.set(
        "click",
        lua.create_function(
            move |lua, (name, col, row, button): (String, u16, u16, Option<String>)| {
                let button = button.unwrap_or_else(|| "left".into());
                let bytes = keys::mouse(&button, col, row).ok_or_else(|| {
                    mlua::Error::runtime(format!("no such click: {button} at {col},{row}"))
                })?;
                value(lua, ask(&path, Request::Send { name, bytes })?)
            },
        )?,
    )?;

    // A sequence of bursts and pauses delivered as one indivisible act — the
    // primitive `send`/`insert` are the one-`Burst` case of. `steps` is a Lua
    // array of `{burst = "..."}` / `{pause = seconds}` entries, in order.
    // This blocks the calling Image — for as long as `steps`'s pauses sum to:
    // `client::request` waits synchronously for the daemon's
    // reply, and the daemon does not answer until the whole act is done. The
    // daemon refuses a total pause over a few seconds rather than trust a
    // units mistake (or a runaway caller) not to hold an Image hostage.
    let path = at();
    table.set(
        "feed",
        lua.create_function(move |lua, (name, steps): (String, Table)| {
            let steps = lua_steps_to_wire(steps)?;
            value(lua, ask(&path, Request::Feed { name, steps })?)
        })?,
    )?;

    let path = at();
    table.set(
        "capture",
        lua.create_function(move |lua, name: String| {
            value(lua, ask(&path, Request::Capture { name })?)
        })?,
    )?;

    let path = at();
    table.set(
        "attach",
        // v1 froze this as a function that exists, not one that works: the
        // image runs inside the daemon, whose stdin is /dev/null, so raw mode
        // cannot be entered. Returns nil, as it always did.
        lua.create_function(move |_, name: String| {
            client::attach(&path, &name)
                .map(|_| ())
                .map_err(mlua::Error::external)
        })?,
    )?;

    close_binding(lua, &table, at())?;

    exec_binding(lua, &table)?;
    function_source_binding(lua, &table)?;

    capture_styled_binding(lua, &table, at())?;
    dir_bindings(lua, &table, &at)?;
    tick_bindings(lua, &table, counters.clone())?;
    request_count_bindings(lua, &table, counters)?;
    registry_bindings(lua, &table)?;
    fs_lock_binding(lua, &table, socket)?;
    process_bindings(lua, &table, image)?;

    removed_sleep_error(lua, &table)?;

    Ok(table)
}

fn removed_sleep_error(lua: &Lua, table: &Table) -> mlua::Result<()> {
    let metatable = lua.create_table()?;
    metatable.set(
        "__index",
        lua.create_function(|_, (_table, key): (Table, Value)| {
            let is_sleep = match key {
                Value::String(key) => key
                    .to_str()
                    .map(|key| key.as_ref() == "sleep")
                    .unwrap_or(false),
                _ => false,
            };
            if is_sleep {
                return Err(mlua::Error::runtime(
                    "remuda.sleep was removed; use remuda.after(seconds, callback) instead",
                ));
            }
            Ok(Value::Nil)
        })?,
    )?;
    table.set_metatable(Some(metatable))
}

fn clear_input_binding(lua: &Lua, table: &Table, path: std::path::PathBuf) -> mlua::Result<()> {
    table.set(
        "clear_input",
        lua.create_function(move |lua, (name, key): (String, mlua::LuaString)| {
            value(
                lua,
                ask(
                    &path,
                    Request::ClearInput {
                        name,
                        key: key.as_bytes().to_vec(),
                    },
                )?,
            )
        })?,
    )
}

fn session_resize_binding(lua: &Lua, table: &Table, path: std::path::PathBuf) -> mlua::Result<()> {
    table.set(
        "_session_resize",
        lua.create_function(move |_, (name, cols, rows): (String, Value, Value)| {
            let cols = match resize_dimension(cols) {
                Ok(value) => value,
                Err(error) => return Ok((Value::Nil, Some(error))),
            };
            let rows = match resize_dimension(rows) {
                Ok(value) => value,
                Err(error) => return Ok((Value::Nil, Some(error))),
            };
            let size = match requested_size(cols, rows) {
                Ok(size) => size,
                Err(error) => return Ok((Value::Nil, Some(error))),
            };
            match ask(&path, Request::Resize { name, size })? {
                Response::Ok => Ok((Value::Boolean(true), None)),
                Response::Error(reason) => Ok((Value::Nil, Some(reason))),
                other => Ok((Value::Nil, Some(format!("unexpected response: {other:?}")))),
            }
        })?,
    )
}

fn requested_size(cols: i64, rows: i64) -> Result<remuda_core::Size, String> {
    use remuda_core::Size;
    if !(i64::from(Size::MIN_RESIZE_COLS)..=i64::from(Size::MAX_RESIZE_COLS)).contains(&cols)
        || !(i64::from(Size::MIN_ROWS)..=i64::from(Size::MAX_RESIZE_ROWS)).contains(&rows)
    {
        return Err(format!(
            "resize dimensions must be cols {}..{}, rows {}..{}",
            Size::MIN_RESIZE_COLS,
            Size::MAX_RESIZE_COLS,
            Size::MIN_ROWS,
            Size::MAX_RESIZE_ROWS
        ));
    }
    let (cols, rows) = (cols as u16, rows as u16);
    Ok(if cols < Size::MIN_COLS {
        Size::for_pane(cols, rows)
    } else {
        Size::new(cols, rows)
    })
}

fn resize_dimension(value: Value) -> Result<i64, String> {
    match value {
        Value::Integer(number) => Ok(number),
        Value::Number(number) if number.is_finite() && number.fract() == 0.0 => {
            if number >= i64::MIN as f64 && number <= i64::MAX as f64 {
                Ok(number as i64)
            } else {
                Err("resize dimensions must be whole numbers".into())
            }
        }
        _ => Err("resize dimensions must be whole numbers".into()),
    }
}

fn close_binding(lua: &Lua, table: &Table, path: std::path::PathBuf) -> mlua::Result<()> {
    // End a session — live, or already self-exited (step 006). A dead session
    // stays listed with its last screen intact until this is called; nothing
    // reaps it on its own, on purpose (`steps/006-lifetime.md`).
    table.set(
        "close",
        lua.create_function(move |lua, name: String| {
            value(
                lua,
                ask(
                    &path,
                    Request::Close {
                        name,
                        instance_id: None,
                        confirm: None,
                    },
                )?,
            )
        })?,
    )
}

fn timer_bindings(
    lua: &Lua,
    table: &Table,
    timers: crate::image::timers::SharedTimerService,
) -> mlua::Result<()> {
    let clock = Rc::clone(&timers);
    table.set(
        "clock",
        lua.create_function(move |_, ()| Ok(clock.borrow().clock_ms()))?,
    )?;
    for (name, repeating) in [("_timer_after", false), ("_timer_every", true)] {
        let timers = Rc::clone(&timers);
        table.set(
            name,
            lua.create_function(
                move |lua, (seconds, callback, owner): (f64, mlua::Function, Option<String>)| {
                    let id = timers
                        .borrow_mut()
                        .schedule(lua, seconds, callback, owner, repeating)?;
                    lua.create_userdata(crate::image::timers::TimerHandle::new(
                        id,
                        Rc::clone(&timers),
                    ))
                },
            )?,
        )?;
    }
    table.set(
        "_timer_cancel_owner",
        lua.create_function(move |lua, owner: String| {
            timers.borrow_mut().cancel_owner(lua, &owner);
            Ok(())
        })?,
    )?;
    Ok(())
}

fn random_bytes_binding(lua: &Lua, table: &Table) -> mlua::Result<()> {
    table.set(
        "random_bytes",
        lua.create_function(|lua, value: Value| {
            // Cap each Lua allocation at 64 KiB: sufficient for keys and
            // tokens while preventing a script from requesting huge buffers.
            const MAX_BYTES: usize = 65_536;
            let length = match value {
                Value::Integer(n) if (1..=MAX_BYTES as i64).contains(&n) => n as usize,
                // Lua 5.4 distinguishes integer and float values; accept an
                // exact, finite whole-number float like 32.0 as an integer.
                Value::Number(n)
                    if n.is_finite()
                        && n.fract() == 0.0
                        && (1.0..=MAX_BYTES as f64).contains(&n) =>
                {
                    n as usize
                }
                _ => {
                    return Err(mlua::Error::runtime(
                        "remuda.random_bytes n must be a whole number in 1..=65536",
                    ));
                }
            };

            let mut bytes = vec![0_u8; length];
            getrandom::fill(&mut bytes).map_err(|error| {
                mlua::Error::runtime(format!("remuda.random_bytes OS CSPRNG failed: {error}"))
            })?;
            lua.create_string(bytes)
        })?,
    )
}

fn hostname_binding(lua: &Lua, table: &Table) -> mlua::Result<()> {
    table.set(
        "hostname",
        lua.create_function(|_, ()| match crate::hostname::hostname() {
            Ok(name) => Ok((Some(name), None)),
            Err(error) => Ok((None, Some(error.to_string()))),
        })?,
    )
}

fn pending_bindings(
    lua: &Lua,
    table: &Table,
    pending: crate::pending::PendingReplies,
    caller: Rc<RefCell<crate::image::CallerContext>>,
) -> mlua::Result<()> {
    let create = pending.clone();
    let caller_context = Rc::clone(&caller);
    table.set(
        "_pending_create",
        lua.create_function(move |lua, timeout: Option<f64>| {
            let seconds = timeout.unwrap_or(30.0);
            if !seconds.is_finite() || seconds <= 0.0 || seconds > 301.0 {
                return Err(mlua::Error::runtime(
                    "pending timeout must be a positive number no greater than 301 seconds",
                ));
            }
            let duration = Duration::from_secs_f64(seconds.max(0.000_000_001));
            let (id, handle) = create.create(duration).map_err(|message| {
                mlua::Error::external(crate::image::TypedFailure { message, code: 1 })
            })?;
            let context = caller_context.borrow();
            let caller_session = if matches!(&context.kind, crate::image::CallerKind::Session) {
                context.session.clone()
            } else {
                None
            };
            drop(context);
            let mut handle = handle;
            handle.set_caller_session(caller_session);
            Ok((id, lua.create_userdata(handle)?))
        })?,
    )?;
    let event_pending = pending.clone();
    table.set(
        "_pending_events",
        lua.create_function(move |lua, ()| {
            let events = event_pending.drain_events();
            let rows = lua.create_table_with_capacity(events.len(), 0)?;
            for (index, event) in events.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("id", event.id)?;
                row.set("reason", event.reason)?;
                rows.set(index + 1, row)?;
            }
            Ok(rows)
        })?,
    )?;
    let line_pending = pending.clone();
    table.set(
        "_pending_line_events",
        lua.create_function(move |lua, ()| {
            let events = line_pending.drain_line_events();
            let rows = lua.create_table_with_capacity(events.len(), 0)?;
            for (index, event) in events.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("id", event.pending_id)?;
                row.set("prompt_id", event.prompt_id)?;
                match event.answer {
                    Ok(Some(line)) => row.set("line", line)?,
                    Ok(None) => row.set("error", "refused")?,
                    Err(error) => row.set("error", error)?,
                }
                rows.set(index + 1, row)?;
            }
            Ok(rows)
        })?,
    )?;
    let secret_pending = pending.clone();
    table.set(
        "_pending_secret_events",
        lua.create_function(move |lua, ()| {
            let events = secret_pending.drain_secret_events();
            let rows = lua.create_table_with_capacity(events.len(), 0)?;
            for (index, event) in events.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("id", event.pending_id)?;
                row.set("prompt_id", event.prompt_id)?;
                match event.answer {
                    Ok(Some(secret)) => row.set("secret", lua.create_string(secret.as_bytes())?)?,
                    Ok(None) => row.set("error", "refused")?,
                    Err(error) => row.set("error", error)?,
                }
                rows.set(index + 1, row)?;
            }
            Ok(rows)
        })?,
    )?;
    Ok(())
}

/// `remuda.new(name, argv, cwd, env)` — split out of `bindings` to stay under
/// its line cap. The last two arguments are trailing and optional, so every
/// existing 2-argument call site keeps working unchanged.
fn new_binding(lua: &Lua, table: &Table, path: std::path::PathBuf) -> mlua::Result<()> {
    table.set(
        "new",
        lua.create_function(
            move |lua,
                  (name, argv, cwd, env): (
                Option<String>,
                Option<Vec<String>>,
                Option<String>,
                Option<Table>,
            )| {
                let env = env.map(lua_env_to_wire).transpose()?;
                value(lua, ask(&path, new_request(name, argv, cwd, env))?)
            },
        )?,
    )
}

/// `source:line` of a Lua function, for `hook_list`; scripts get no `debug`.
fn function_source_binding(lua: &Lua, table: &Table) -> mlua::Result<()> {
    table.set(
        "_function_source",
        lua.create_function(|_, function: mlua::Function| {
            let info = function.info();
            Ok(format!(
                "{}:{}",
                info.short_src.unwrap_or_else(|| "?".into()),
                info.line_defined.unwrap_or(0)
            ))
        })?,
    )
}

/// `remuda.exec(name)` — split out of `bindings` for its line cap. Reentrant-
/// safe: a nested `lua.load(...).exec()` on this same `Lua`, not a new
/// interpreter. Mods must be installed independently of the Remuda binary.
fn exec_binding(lua: &Lua, table: &Table) -> mlua::Result<()> {
    table
        .set(
            "exec",
            lua.create_function(|lua, name: String| execute_package(lua, &name, false, false))?,
        )
        .and_then(|()| {
            table.set(
                "_exec_commands",
                lua.create_function(|lua, name: String| execute_package(lua, &name, false, true))?,
            )
        })
        .and_then(|()| {
            table.set(
                "reload",
                lua.create_function(|lua, name: String| execute_package(lua, &name, true, false))?,
            )
        })
}

/// A top-level mod first gets its `requires` checked as a whole, then its
/// lifecycle hosts activated in order (exec leaves an active one alone).
fn execute_package(
    lua: &Lua,
    name: &str,
    require_lifecycle: bool,
    commands_only: bool,
) -> mlua::Result<()> {
    let loaded = load_package(lua, name, require_lifecycle, commands_only);
    // A mod that redefined an advised function keeps its advice: the new
    // definition becomes the base (hook-design §2). Even after a failed
    // load, which may have redefined some before it stopped.
    let remuda: Table = lua.globals().get("remuda")?;
    if let Ok(reattach) = remuda.get::<mlua::Function>("_advice_reattach") {
        reattach.call::<()>(())?;
    }
    loaded
}

fn load_package(
    lua: &Lua,
    name: &str,
    require_lifecycle: bool,
    commands_only: bool,
) -> mlua::Result<()> {
    if !name.contains('/') {
        for host in crate::packages::requirement_order(name).map_err(mlua::Error::runtime)? {
            let lifecycle = crate::packages::resolve(&host)
                .map_err(mlua::Error::runtime)?
                .is_some_and(|package| package.lifecycle.is_some());
            if lifecycle {
                activate_package(lua, &host, false, commands_only)?;
            }
        }
    }
    activate_package(lua, name, require_lifecycle, commands_only)
}

fn activate_package(
    lua: &Lua,
    name: &str,
    require_lifecycle: bool,
    commands_only: bool,
) -> mlua::Result<()> {
    let package = crate::packages::resolve(name)
        .map_err(mlua::Error::runtime)?
        .ok_or_else(|| mlua::Error::runtime(format!("no such package: {name}")))?;
    if require_lifecycle && package.lifecycle.is_none() {
        return Err(mlua::Error::runtime(format!(
            "mod {name} uses the legacy entry format and cannot be reloaded in-process"
        )));
    }
    if package.lifecycle.is_some() {
        let active = Rc::new(Cell::new(false));
        let environment = lua.create_table()?;
        environment.set("_G", environment.clone())?;
        let environment_meta = lua.create_table()?;
        environment_meta.set("__index", lua.globals())?;
        environment.set_metatable(Some(environment_meta))?;
        let remuda_proxy = lua.create_table()?;
        let remuda_meta = lua.create_table()?;
        let active_for_lookup = Rc::clone(&active);
        remuda_meta.set(
            "__index",
            lua.create_function(move |lua, (_table, key): (Table, String)| {
                if !active_for_lookup.get() {
                    return Err(mlua::Error::runtime(
                        "lifecycle declarations and migrations cannot call remuda APIs",
                    ));
                }
                let remuda: Table = lua.globals().get("remuda")?;
                remuda.get::<Value>(key)
            })?,
        )?;
        // #145: the mod's own writes reach the real `remuda` table, through
        // the lifecycle manager's owner checks, once it is active.
        let active_for_write = Rc::clone(&active);
        let owner = name.to_string();
        remuda_meta.set(
            "__newindex",
            lua.create_function(move |lua, (_table, key, value): (Table, Value, Value)| {
                if !active_for_write.get() {
                    return Err(mlua::Error::runtime(
                        "lifecycle declarations and migrations cannot call remuda APIs",
                    ));
                }
                let set: mlua::Function = lua.named_registry_value("remuda.lifecycle.set_field")?;
                set.call::<()>((owner.as_str(), key, value))
            })?,
        )?;
        remuda_proxy.set_metatable(Some(remuda_meta))?;
        environment.set("remuda", remuda_proxy)?;
        let declaration: Value = lua
            .load(&package.source)
            .set_name(package.chunk_name)
            .set_environment(environment)
            .eval()?;
        let activate: mlua::Function =
            lua.named_registry_value("remuda.lifecycle.activate_module")?;
        let (_, state, start, rollback, commands): (bool, Value, Value, Value, Value) =
            activate.call((name, declaration, require_lifecycle, commands_only))?;
        active.set(true);
        if let Value::Function(commands) = &commands {
            if let Err(error) = call_lifecycle_callback(lua, commands, state.clone()) {
                if let Value::Function(rollback) = &rollback {
                    rollback.call::<()>(())?;
                }
                return Err(error);
            }
        }
        if let Value::Function(start) = &start {
            let result = call_lifecycle_callback(lua, start, state);
            if let Err(error) = result {
                if let Value::Function(rollback) = &rollback {
                    rollback.call::<()>(())?;
                }
                return Err(error);
            }
        }
        Ok(())
    } else {
        lua.load(&package.source)
            .set_name(package.chunk_name)
            .exec()
    }
}

/// Run lifecycle callbacks under the same error policy, so errors from hooks
/// they emit are propagated to the activation transaction for rollback.
fn call_lifecycle_callback(lua: &Lua, callback: &mlua::Function, state: Value) -> mlua::Result<()> {
    let remuda: Table = lua.globals().get("remuda")?;
    let was_active: bool = remuda.get("_lifecycle_start_active").unwrap_or(false);
    remuda.set("_lifecycle_start_active", true)?;
    let result = callback.call::<()>(state);
    remuda.set("_lifecycle_start_active", was_active)?;
    result
}

/// Keep the Lua lifecycle manager callable by the loader without exposing its
/// implementation helper on the public `remuda` table.
pub(crate) fn hide_module_activator(lua: &Lua) -> mlua::Result<()> {
    let remuda: Table = lua.globals().get("remuda")?;
    let activate: mlua::Function = remuda.get("_activate_module")?;
    lua.set_named_registry_value("remuda.lifecycle.activate_module", activate)?;
    let stop_modules: mlua::Function = remuda.get("_stop_modules")?;
    lua.set_named_registry_value("remuda.lifecycle.stop_modules", stop_modules)?;
    let set_field: mlua::Function = remuda.get("_module_set_field")?;
    lua.set_named_registry_value("remuda.lifecycle.set_field", set_field)?;
    remuda.set("_module_set_field", Value::Nil)?;
    remuda.set("_stop_modules", Value::Nil)?;
    remuda.set("_activate_module", Value::Nil)
}

pub(crate) fn stop_modules(lua: &Lua) -> mlua::Result<()> {
    let stop_modules: mlua::Function = lua.named_registry_value("remuda.lifecycle.stop_modules")?;
    stop_modules.call(())
}

/// `remuda.capture_styled(name)` — split out of `bindings` for its line cap.
/// Only what a script needs to tell a TUI's dim ghost text from typed text,
/// and where the caret is (#137): not colours or the other attributes.
fn fail_binding(lua: &Lua, table: &Table, image: crate::image::Image) -> mlua::Result<()> {
    table.set(
        "fail",
        lua.create_function(
            |_, (message, code): (String, Option<i64>)| -> mlua::Result<()> {
                let code = code.unwrap_or(1);
                if !(1..=255).contains(&code) {
                    return Err(mlua::Error::runtime(
                        "remuda.fail exit code must be an integer from 1 through 255",
                    ));
                }
                Err(mlua::Error::external(crate::image::TypedFailure {
                    message,
                    code: code as u8,
                }))
            },
        )?,
    )?;
    crate::net::http_client::install(lua, table, image)
}

fn capture_styled_binding(lua: &Lua, table: &Table, path: std::path::PathBuf) -> mlua::Result<()> {
    table.set(
        "capture_styled",
        lua.create_function(move |lua, name: String| {
            match ask(
                &path,
                Request::CaptureStyled {
                    name,
                    scrollback: 0,
                },
            )? {
                Response::StyledScreen { rows, cursor, .. } => {
                    let screen = lua.create_table()?;
                    let out = lua.create_table()?;
                    for (index, runs) in rows.into_iter().enumerate() {
                        let row = lua.create_table()?;
                        for (at, run) in runs.into_iter().enumerate() {
                            let span = lua.create_table()?;
                            span.set("text", run.text)?;
                            span.set("dim", run.dim)?;
                            row.set(at + 1, span)?;
                        }
                        out.set(index + 1, row)?;
                    }
                    screen.set("rows", out)?;
                    // 1-based like the rows above, not the wire's 0-based.
                    let caret = lua.create_table()?;
                    caret.set("row", cursor.row + 1)?;
                    caret.set("col", cursor.col + 1)?;
                    caret.set("visible", cursor.visible)?;
                    screen.set("cursor", caret)?;
                    Ok(Value::Table(screen))
                }
                other => value(lua, other),
            }
        })?,
    )
}

/// Plain filesystem primitives for topic directories, no session involved —
/// split out of `bindings` to stay under its line cap. `dir`, not `path`, for
/// the Lua-supplied argument: `at()` is always this table's own socket path.
fn dir_bindings(
    lua: &Lua,
    table: &Table,
    at: &impl Fn() -> std::path::PathBuf,
) -> mlua::Result<()> {
    let path = at();
    table.set(
        "list_dir",
        lua.create_function(move |lua, dir: String| {
            value(lua, ask(&path, Request::ListDir { path: dir })?)
        })?,
    )?;

    let path = at();
    table.set(
        "mkdir",
        lua.create_function(move |lua, dir: String| {
            value(lua, ask(&path, Request::Mkdir { path: dir })?)
        })?,
    )?;

    let path = at();
    table.set(
        "remove_dir_all",
        lua.create_function(move |lua, dir: String| {
            value(lua, ask(&path, Request::RemoveDirAll { path: dir })?)
        })?,
    )?;

    Ok(())
}

/// Filesystem operations with explicit creation and replacement semantics.
fn fs_bindings(lua: &Lua, table: &Table) -> mlua::Result<()> {
    let fs = lua.create_table()?;
    fs.set(
        "mkdir_new",
        lua.create_function(|_, path: String| match mkdir_new(Path::new(&path), &path) {
            Ok(()) => Ok((Some(true), None::<String>)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Ok((None::<bool>, Some("exists".to_string())))
            }
            Err(error) => Ok((None::<bool>, Some(error.to_string()))),
        })?,
    )?;
    fs.set(
        "write_atomic",
        lua.create_function(
            |_, (path, bytes, options): (String, mlua::LuaString, Option<Table>)| {
                let mut private = false;
                if let Some(options) = options {
                    for pair in options.pairs::<String, mlua::Value>() {
                        let (key, value) = pair?;
                        if key != "private" {
                            return Err(mlua::Error::RuntimeError(format!(
                                "unknown write_atomic option: {key}"
                            )));
                        }
                        private = match value {
                            mlua::Value::Boolean(private) => private,
                            _ => {
                                return Err(mlua::Error::RuntimeError(
                                    "write_atomic option 'private' must be a boolean".into(),
                                ));
                            }
                        };
                    }
                }
                let result = if private {
                    crate::fs_atomic::write_atomic_lua_private(Path::new(&path), &bytes.as_bytes())
                } else {
                    crate::fs_atomic::write_atomic(Path::new(&path), &bytes.as_bytes(), 0o644)
                };
                match result {
                    Ok(()) => Ok((Some(true), None::<String>)),
                    Err(error) => Ok((None::<bool>, Some(error.to_string()))),
                }
            },
        )?,
    )?;
    fs.set(
        "realpath",
        lua.create_function(|_, path: String| {
            let resolved =
                std::fs::canonicalize(fs_path(&path)?).map_err(|error| fs_reason(&error));
            Ok(
                match resolved.map(|real| real.into_os_string().into_string()) {
                    Ok(Ok(real)) => (Some(real), None),
                    Ok(Err(_)) => (
                        None,
                        Some("unavailable: the resolved path is not UTF-8".into()),
                    ),
                    Err(reason) => (None, Some(reason)),
                },
            )
        })?,
    )?;
    fs.set(
        "is_symlink",
        lua.create_function(|_, path: String| {
            Ok(match std::fs::symlink_metadata(fs_path(&path)?) {
                Ok(metadata) => (Some(metadata.file_type().is_symlink()), None),
                Err(error) => (None, Some(fs_reason(&error))),
            })
        })?,
    )?;
    table.set("fs", fs)
}

fn fs_path(path: &str) -> mlua::Result<&Path> {
    if path.is_empty() {
        return Err(mlua::Error::RuntimeError("path must not be empty".into()));
    }
    Ok(Path::new(path))
}

/// Why a path could not be read, in the reason form `system.credential` uses.
fn fs_reason(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "not_found".into(),
        std::io::ErrorKind::PermissionDenied => format!("denied: {error}"),
        _ => format!("unavailable: {error}"),
    }
}

/// Where the image keeps every held `remuda.fs.lock`, path -> handle, so a
/// lock outlives the Lua references to it and one path has one handle.
const FS_LOCKS: &str = "remuda.fs.locks";

/// A held lock: the open file is the lock, and dropping it releases.
struct FsLock {
    path: String,
    file: RefCell<Option<std::fs::File>>,
}

impl mlua::UserData for FsLock {
    fn add_fields<F: mlua::UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("path", |_, this| Ok(this.path.clone()));
    }

    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("release", |lua, this, ()| {
            let Some(file) = this.file.borrow_mut().take() else {
                return Ok(false);
            };
            drop(file);
            let held: Table = lua.named_registry_value(FS_LOCKS)?;
            held.set(this.path.as_str(), Value::Nil)?;
            Ok(true)
        });
    }
}

/// The daemon's session name, read back from `daemon::socket_path_in`.
fn session_of(socket: &Path) -> String {
    let name = if cfg!(windows) {
        // `remuda-<16 hex>-<server>`: the server follows the 24-byte prefix.
        let pipe = socket.file_name().unwrap_or_default().to_string_lossy();
        pipe.get(24..).unwrap_or(&pipe).to_owned()
    } else {
        let stem = socket.file_stem().unwrap_or_default();
        stem.to_string_lossy().into_owned()
    };
    name
}

fn fs_lock_binding(lua: &Lua, table: &Table, socket: &Path) -> mlua::Result<()> {
    use crate::fs_lock::{acquire, info_line, Outcome};

    let session = session_of(socket);
    if lua.named_registry_value::<Value>(FS_LOCKS)?.is_nil() {
        lua.set_named_registry_value(FS_LOCKS, lua.create_table()?)?;
    }
    let fs: Table = table.get("fs")?;
    fs.set(
        "lock",
        lua.create_function(move |lua, path: String| {
            let held: Table = lua.named_registry_value(FS_LOCKS)?;
            if let Some(handle) = held.get::<Option<mlua::AnyUserData>>(path.as_str())? {
                return Ok((Value::UserData(handle), None, None));
            }
            match acquire(Path::new(&path), &info_line(&session)) {
                Ok(Outcome::Acquired(file)) => {
                    let handle = lua.create_userdata(FsLock {
                        path: path.clone(),
                        file: RefCell::new(Some(file)),
                    })?;
                    held.set(path, &handle)?;
                    Ok((Value::UserData(handle), None, None))
                }
                Ok(Outcome::Held(info)) => Ok((Value::Nil, Some("held".to_string()), Some(info))),
                Err(error) => Ok((Value::Nil, Some(error.to_string()), None)),
            }
        })?,
    )
}

fn mkdir_new(path: &Path, raw_path: &str) -> std::io::Result<()> {
    if !path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path must be absolute",
        ));
    }
    if raw_path.ends_with(std::path::MAIN_SEPARATOR) || cfg!(windows) && raw_path.ends_with('/') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path must not end with a separator",
        ));
    }

    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// The `Ticker`'s own skip counters, read-only — no threshold or alarm here,
/// split out of `bindings` to stay under its line cap. See `tick.rs`'s own
/// hook-point comment for why acting on them is a separate, undecided step.
fn tick_bindings(
    lua: &Lua,
    table: &Table,
    counters: std::sync::Arc<crate::tick::Counters>,
) -> mlua::Result<()> {
    table.set(
        "schedule_skips",
        lua.create_function(move |lua, ()| {
            let skips = counters.counter("ticker_skip");
            let row = lua.create_table()?;
            row.set("consecutive", skips.consecutive())?;
            row.set("total", skips.total())?;
            Ok(row)
        })?,
    )
}

/// The daemon's own request-dispatch counts, read-only — one row per
/// `Request` variant `daemon::handle` counts, under the same names.
// See `tick.rs`'s `Counters` for the shared registry both sides read; not in
// `mcp::TOOLS`, so an MCP client can only reach it if something registers it
// under `remuda.tools` (`remuda._call` looks up only that table, not every
// top-level `remuda` name) — not done here, on purpose.
fn request_count_bindings(
    lua: &Lua,
    table: &Table,
    counters: std::sync::Arc<crate::tick::Counters>,
) -> mlua::Result<()> {
    table.set(
        "request_counts",
        lua.create_function(move |lua, ()| {
            let row = lua.create_table()?;
            row.set("list", counters.counter("request_list").total())?;
            row.set("eval", counters.counter("request_eval").total())?;
            row.set(
                "capture_styled",
                counters.counter("request_capture_styled").total(),
            )?;
            Ok(row)
        })?,
    )
}

fn parse_stdin_hold_until_lines(value: Value) -> Result<Option<usize>, String> {
    match value {
        Value::Nil => Ok(None),
        Value::Integer(lines) if (1..=1000).contains(&lines) => Ok(Some(lines as usize)),
        Value::Number(lines)
            if lines.is_finite() && lines.fract() == 0.0 && (1.0..=1000.0).contains(&lines) =>
        {
            Ok(Some(lines as usize))
        }
        _ => Err("process.run stdin_hold_until_lines must be an integer from 1 through 1000. Next: pass a whole number in that range.".to_string()),
    }
}

fn parse_process_environment(
    env: Value,
    clear_env: Value,
    word: &str,
) -> Result<crate::process::ChildEnvironment, String> {
    let clear = match clear_env {
        Value::Nil => false,
        Value::Boolean(clear) => clear,
        _ => return Err(format!("{word} clear_env must be a boolean")),
    };
    let mut vars = Vec::new();
    match env {
        Value::Nil => {}
        Value::Table(table) => {
            for pair in table.pairs::<Value, Value>() {
                let (key, value) = pair.map_err(|_| format!("{word} env must be a string map"))?;
                let name = match key {
                    Value::String(name) => name
                        .to_str()
                        .map_err(|_| format!("{word} env names must be UTF-8 strings"))?
                        .to_owned(),
                    _ => return Err(format!("{word} env names must be strings")),
                };
                let value = match value {
                    Value::String(value) => value
                        .to_str()
                        .map_err(|_| format!("{word} env values must be UTF-8 strings"))?
                        .to_owned(),
                    _ => return Err(format!("{word} env values must be strings")),
                };
                vars.push((name, value));
            }
        }
        _ => return Err(format!("{word} env must be a table of strings")),
    }
    crate::process::ChildEnvironment::new(word, clear, vars)
}

/// `remuda.process`'s Rust half — split out of `bindings` to stay under its
/// line cap.
// `remuda.process` itself (the validated, Lua-facing spec-table word) lives
// in `tools.lua` and calls `_process_spawn` here; `kill` and `processes` are
// plain Rust words with nothing to validate.
fn process_bindings(lua: &Lua, table: &Table, image: crate::image::Image) -> mlua::Result<()> {
    let processes = crate::process::Processes::new();
    table.set(
        "_process_spawn",
        process_spawn_binding(lua, processes.clone(), image.clone())?,
    )?;
    table.set("_process_run", process_run_binding(lua)?)?;

    let drainer = processes.clone();
    let drain_image = image;
    table.set(
        "_process_drain",
        lua.create_function(move |lua, id: u64| drainer.drain(id, lua, &drain_image))?,
    )?;

    let killer = processes.clone();
    table.set(
        "kill",
        lua.create_function(move |_, id: u64| killer.kill(id).map_err(mlua::Error::external))?,
    )?;

    let pg_killer = processes.clone();
    table.set(
        "_process_killpg",
        lua.create_function(move |_, id: u64| pg_killer.killpg(id).map_err(mlua::Error::external))?,
    )?;

    table.set(
        "processes",
        lua.create_function(move |lua, ()| {
            let rows = lua.create_table()?;
            for (i, id) in processes.list().into_iter().enumerate() {
                rows.set(i + 1, id)?;
            }
            Ok(rows)
        })?,
    )
}

fn process_spawn_binding(
    lua: &Lua,
    spawner: crate::process::Processes,
    image: crate::image::Image,
) -> mlua::Result<mlua::Function> {
    lua.create_function(
        move |_,
              (mut argv, on_line, on_exit, cwd, env, clear_env): (
            Vec<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Value,
            Value,
        )| {
            let child_environment = match parse_process_environment(env, clear_env, "process") {
                Ok(environment) => environment,
                Err(refused) => return Ok((None, Some(refused))),
            };
            let cwd = match crate::process::checked_cwd("process", cwd.as_deref(), &mut argv) {
                Ok(cwd) => cwd,
                Err(refused) => return Ok((None, Some(refused))),
            };
            let id = spawner
                .spawn_with_env(
                    image.clone(),
                    argv,
                    on_line,
                    on_exit,
                    cwd,
                    child_environment,
                )
                .map_err(mlua::Error::external)?;
            Ok((Some(id), None))
        },
    )
}

fn process_run_binding(lua: &Lua) -> mlua::Result<mlua::Function> {
    lua.create_function(
        |lua,
         (mut argv, stdin, timeout, cwd, stdin_hold_until_lines, env, clear_env): (
            Vec<String>,
            Option<mlua::LuaString>,
            f64,
            Option<String>,
            Value,
            Value,
            Value,
        )| {
            let child_environment = match parse_process_environment(env, clear_env, "process.run") {
                Ok(environment) => environment,
                Err(refused) => return Ok((Value::Nil, Some(refused))),
            };
            let stdin_hold_until_lines = match parse_stdin_hold_until_lines(stdin_hold_until_lines)
            {
                Ok(lines) => lines,
                Err(refused) => return Ok((Value::Nil, Some(refused))),
            };
            let cwd = match crate::process::checked_cwd("process.run", cwd.as_deref(), &mut argv) {
                Ok(cwd) => cwd,
                Err(refused) => return Ok((Value::Nil, Some(refused))),
            };
            let output = crate::process::run_sync_with_env(
                argv,
                stdin.map(|value| value.as_bytes().to_vec()),
                timeout,
                cwd,
                stdin_hold_until_lines,
                child_environment,
            )
            .map_err(mlua::Error::runtime)?;
            let result = lua.create_table()?;
            result.set("code", output.code)?;
            result.set("stdout", lua.create_string(&output.stdout)?)?;
            result.set("stderr", lua.create_string(&output.stderr)?)?;
            result.set("timed_out", output.timed_out)?;
            if let Some(signal) = output.signal {
                result.set("signal", signal)?;
            }
            Ok((Value::Table(result), None))
        },
    )
}

fn ask(socket: &Path, request: Request) -> mlua::Result<Response> {
    client::request(socket, &request).map_err(mlua::Error::external)
}

/// A Lua array of `{burst = "..."}` / `{pause = seconds}` entries into the
/// wire `Step`s `feed` delivers — `pause` is seconds, like event-loop timers,
/// and travels the wire as whole milliseconds.
fn lua_steps_to_wire(steps: Table) -> mlua::Result<Vec<Step>> {
    let mut wire = Vec::with_capacity(steps.raw_len());
    for step in steps.sequence_values::<Table>() {
        let step = step?;
        if let Ok(burst) = step.get::<mlua::LuaString>("burst") {
            wire.push(Step::Burst(burst.as_bytes().to_vec()));
        } else if let Ok(seconds) = step.get::<f64>("pause") {
            wire.push(Step::Pause((seconds.max(0.0) * 1000.0) as u64));
        } else {
            return Err(mlua::Error::runtime(
                "each feed step needs a `burst` string or a `pause` number",
            ));
        }
    }
    Ok(wire)
}

/// A Lua table of string keys to string values, into the map `Request::New`'s
/// `env` field carries. Mirrors `lua_steps_to_wire`'s conversion style.
fn lua_env_to_wire(env: Table) -> mlua::Result<std::collections::HashMap<String, String>> {
    env.pairs::<String, String>().collect()
}

/// Turn a response into what the script sees. Caution: [`Response::Error`]
/// raises rather than returning a value — a script that forgot to check one
/// would otherwise carry on over a session that was never created.
fn value(lua: &Lua, response: Response) -> mlua::Result<Value> {
    match response {
        Response::Ok => Ok(Value::Nil),
        Response::Ack { .. } => Ok(Value::Nil),
        Response::Uncertain => Err(mlua::Error::runtime(
            "input outcome is uncertain; bytes may be partial or late",
        )),
        Response::WrongInstance => Err(mlua::Error::runtime("session instance changed")),
        Response::RateLimited => Err(mlua::Error::runtime("session input rate limit exceeded")),
        Response::SyncAtCapacity => Err(mlua::Error::runtime("Sync is at capacity; retry shortly")),
        Response::Busy => Err(mlua::Error::runtime(crate::BUSY_RETRY_MESSAGE)),
        Response::WriteTimeout => Err(mlua::Error::runtime(
            "session PTY write timed out; delivery may be partial or late",
        )),
        Response::RemoteControlDisabled => {
            Err(mlua::Error::runtime("remote control disabled on this node"))
        }
        Response::AttachStarted { .. } | Response::AttachStatus { .. } => Err(
            mlua::Error::runtime("attach responses are not exposed to scripts"),
        ),
        Response::Screen(text) => Ok(Value::String(lua.create_string(&text)?)),
        Response::ClearInput { cleared } => {
            let result = lua.create_table()?;
            if let Some(cleared) = cleared {
                result.set("cleared", cleared)?;
            }
            Ok(Value::Table(result))
        }
        // No binding here asks for an `Eval`, so this arm is unreachable in
        // practice — spelled out rather than folded into a wildcard so that
        // adding one later is a compile error to think about, not a silent
        // fall-through that returns the wrong shape.
        Response::Value(text) => Ok(Value::String(lua.create_string(&text)?)),
        Response::CommandResult { .. } => Err(mlua::Error::runtime(
            "deferred command replies cannot be consumed as a Lua value",
        )),
        Response::Sessions(list) => {
            let rows = lua.create_table()?;
            for (index, session) in list.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("name", session.name)?;
                row.set("instance_id", session.instance_id.as_deref())?;
                row.set("alive", session.alive)?;
                row.set("idle", session.idle.as_secs_f64())?;
                row.set(
                    "output_idle",
                    session.output_idle.unwrap_or(session.idle).as_secs_f64(),
                )?;
                row.set("cols", session.size.cols())?;
                row.set("rows", session.size.rows())?;
                // Additive: `tests/api/v1.lua` asserts specific fields exist,
                // never that no others do, so a new field widens v1 rather
                // than breaking it. Needed so a Lua-authored panel (the
                // "*sessions*" buffer, `tools.lua`) can show the same
                // attached state the list has always drawn.
                row.set("attached", session.attached)?;
                // Seconds since an attached human typed; math.huge if never, so
                // the field is always present and nil means an older core.
                row.set(
                    "human_idle",
                    session
                        .human_idle
                        .map_or(f64::INFINITY, |idle| idle.as_secs_f64()),
                )?;
                rows.set(index + 1, row)?;
            }
            Ok(Value::Table(rows))
        }
        Response::Entries(names) => {
            let rows = lua.create_table()?;
            for (index, name) in names.into_iter().enumerate() {
                rows.set(index + 1, name)?;
            }
            Ok(Value::Table(rows))
        }
        Response::Error(reason) => {
            if let Some((code, message)) = crate::image::typed_failure_message(&reason) {
                Err(mlua::Error::external(crate::image::TypedFailure {
                    code,
                    message: message.to_string(),
                }))
            } else {
                Err(mlua::Error::runtime(reason))
            }
        }
        // No binding here asks for `CaptureStyled` either — same reasoning as
        // `Response::Value` above.
        Response::StyledScreen { .. } => Err(mlua::Error::runtime(
            "styled capture is not exposed to scripts",
        )),
        Response::Sync { .. } => Err(mlua::Error::runtime("Sync is not exposed to scripts")),
        Response::MouseState(_) => Err(mlua::Error::runtime(
            "mouse state is not exposed to scripts",
        )),
        Response::ClusterRegistryPage { .. }
        | Response::ClusterRegistryAck { .. }
        | Response::ClusterListenerStatus(_) => Err(mlua::Error::runtime(
            "cluster control responses are not exposed to scripts",
        )),
        Response::PromptSecret { .. } => Err(mlua::Error::runtime(
            "secret prompts are not supported by this client path",
        )),
        Response::PromptLine { .. } => Err(mlua::Error::runtime(
            "line prompts are not supported by this client path",
        )),
    }
}

#[cfg(test)]
mod binding_tests {
    use super::{
        cli_parse_bindings, cli_spec_from_lua, fs_bindings, lua_steps_to_wire, value, BINDINGS,
    };
    use mlua::{Lua, Table};
    use remuda_core::protocol::{Response, Step};

    #[test]
    fn lua_busy_error_has_the_cli_retry_guidance() {
        let error = value(&Lua::new(), Response::Busy).unwrap_err().to_string();

        assert!(
            error.contains(
                "session input is busy; nothing was written, retry\nNext: wait for the previous write to finish (see it with remuda capture NAME), then run the command again."
            ),
            "Lua Busy error did not include retry guidance: {error}"
        );
    }

    #[test]
    fn binding_names_are_sorted_and_unique() {
        assert!(BINDINGS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    fn rejects_cli_spec(spec: &str) {
        let lua = Lua::new();
        let table = lua.load(spec).eval::<Table>().unwrap();
        let error = cli_spec_from_lua(table).unwrap_err();
        assert!(error.to_string().contains("remuda.cli.parse"), "{error}");
    }

    fn install_cli_binding(lua: &Lua) {
        let remuda = lua.create_table().unwrap();
        remuda.set("cli", cli_parse_bindings(lua).unwrap()).unwrap();
        lua.globals().set("remuda", remuda).unwrap();
    }

    fn rejects_cli_argv(argv: &str) {
        let lua = Lua::new();
        install_cli_binding(&lua);
        lua.globals()
            .set("argv", lua.load(argv).eval::<Table>().unwrap())
            .unwrap();
        lua.load(
            r#"
            local spec = {name="remuda", verbs={go={next="remuda"}}}
            local ok, err = pcall(remuda.cli.parse, spec, argv)
            assert(not ok, tostring(err))
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn cli_spec_rejects_duplicate_option_ids() {
        rejects_cli_spec(
            r#"return {name="remuda", options={{long="json",help="a"},{long="json",help="b"}}, verbs={go={next="remuda"}}}"#,
        );
    }

    #[test]
    fn cli_spec_rejects_reserved_help_long() {
        rejects_cli_spec(
            r#"return {name="remuda", options={{long="help",help="a"}}, verbs={go={next="remuda"}}}"#,
        );
    }

    #[test]
    fn cli_spec_rejects_reserved_help_verb() {
        rejects_cli_spec(r#"return {name="remuda", verbs={help={next="remuda"}}}"#);
    }

    #[test]
    fn cli_spec_rejects_reserved_help_short() {
        rejects_cli_spec(
            r#"return {name="remuda", options={{long="json",short="h",help="a"}}, verbs={go={next="remuda"}}}"#,
        );
    }

    #[test]
    fn cli_spec_rejects_option_positional_id_collision() {
        rejects_cli_spec(
            r#"return {name="remuda", options={{long="event-id",help="a"}}, verbs={go={next="remuda",args={{name="event-id",help="b"}}}}}"#,
        );
    }

    #[test]
    fn cli_spec_rejects_empty_option_long() {
        rejects_cli_spec(
            r#"return {name="remuda", options={{long="",help="a"}}, verbs={go={next="remuda"}}}"#,
        );
    }

    #[test]
    fn cli_spec_rejects_equals_in_option_long() {
        rejects_cli_spec(
            r#"return {name="remuda", options={{long="bad=name",help="a"}}, verbs={go={next="remuda"}}}"#,
        );
    }

    #[test]
    fn cli_spec_rejects_non_last_multiple_positional() {
        rejects_cli_spec(
            r#"return {name="remuda", verbs={go={next="remuda",args={{name="BODY",help="a",multiple=true},{name="END",help="b"}}}}}"#,
        );
    }

    #[test]
    fn cli_spec_rejects_required_positional_after_optional() {
        rejects_cli_spec(
            r#"return {name="remuda", verbs={go={next="remuda",args={{name="OPTIONAL",help="a",required=false},{name="REQUIRED",help="b"}}}}}"#,
        );
    }

    #[test]
    fn cli_parse_accepts_optional_positionals() {
        let lua = Lua::new();
        install_cli_binding(&lua);
        lua.load(
            r#"
            local spec = {
                name="remuda",
                verbs={
                    go={next="remuda",args={
                        {name="KIND",help="kind"},
                        {name="NAME",help="name",required=false},
                    }},
                    body={next="remuda",args={
                        {name="WORDS",help="words",multiple=true,required=false},
                    }},
                },
            }
            local absent = remuda.cli.parse(spec, {"go", "claude"})
            assert(absent.ok and absent.values.KIND == "claude" and absent.values.NAME == nil, "optional trailing positional should be absent")
            local present = remuda.cli.parse(spec, {"go", "claude", "x"})
            assert(present.ok and present.values.KIND == "claude" and present.values.NAME == "x", "optional trailing positional should be captured")
            local missing = remuda.cli.parse(spec, {"go"})
            assert(not missing.ok and missing.kind == "error" and missing.text:find("Usage: remuda go KIND [NAME]", 1, true), "required first positional should still produce a usage error")
            local empty_body = remuda.cli.parse(spec, {"body"})
            assert(empty_body.ok and empty_body.values.WORDS == nil, "empty optional multiple positional should parse with no value")
            "#,
        )
        .exec()
        .unwrap();
    }

    const DUMP: &str = r#"
        function dump(v)
            if type(v) ~= "table" then return type(v) == "string" and '"' .. v:gsub("\n", "\\n") .. '"' or tostring(v) end
            local keys = {}
            for k in pairs(v) do keys[#keys + 1] = k end
            table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
            local out = {}
            for _, k in ipairs(keys) do out[#out + 1] = tostring(k) .. "=" .. dump(v[k]) end
            return "{" .. table.concat(out, ",") .. "}"
        end
        spec = {
            name="remuda",
            options={{long="json",help="j",global=true},{long="room",value="R",help="r"}},
            verbs={
                send={next="remuda send --help",args={{name="TO",help="to"},{name="BODY",help="b",multiple=true,required=false}},
                    options={{long="loud",help="l"},{long="tag",value="T",help="t"}}},
            },
        }
    "#;

    fn dump_of(call: &str) -> String {
        let lua = Lua::new();
        install_cli_binding(&lua);
        lua.load(DUMP).exec().unwrap();
        lua.load(format!("return dump({call})")).eval().unwrap()
    }

    // Goldens captured from the pre-v2 parser; v1 output must never change.
    #[test]
    fn cli_parse_v1_reports_are_unchanged() {
        assert_eq!(
            dump_of(r#"remuda.cli.parse(spec, {"--json", "send", "a", "x", "y"})"#),
            r#"{code=0,ok=true,text="",values={BODY={1="x",2="y"},TO="a",json=true,loud=false},verb="send"}"#
        );
        assert_eq!(
            dump_of(r#"remuda.cli.parse(spec, {"send", "a", "x"})"#),
            r#"{code=0,ok=true,text="",values={BODY="x",TO="a",json=false,loud=false},verb="send"}"#
        );
        assert_eq!(
            dump_of(r#"remuda.cli.parse(spec, {"send", "--help"})"#),
            r#"{code=0,kind="help",ok=false,text="\nUsage: remuda send [OPTIONS] TO [BODY]\n\nArguments:\n  <TO>       to\n  [BODY]...  b\n\nOptions:\n  -h, --help     Print help\n      --json     j\n      --loud     l\n      --tag <T>  t\n\nNext: remuda send --help\n",values={}}"#
        );
        assert_eq!(
            dump_of(r#"remuda.cli.parse(spec, {"send", "--nope"})"#),
            r#"{code=2,kind="error",ok=false,text="remuda: remuda send: unknown option '--nope'. put the text after --\nUsage: remuda send [OPTIONS] TO [BODY]\nNext: remuda send --help",values={}}"#
        );
    }

    #[test]
    fn cli_capabilities_advertise_only_what_exists() {
        let out = dump_of("remuda.cli.capabilities()");
        assert_eq!(
            out,
            r#"{features={1="stable_report",2="strict_v2",3="repeat_policy"},report_versions={1=1,2=2},spec_versions={1=1,2=2}}"#
        );
    }

    #[test]
    fn cli_parse_v2_success_envelope_and_vectors() {
        let many = dump_of(
            r#"remuda.cli.parse((function() spec.report_version = 2 return spec end)(), {"send", "a", "x", "y"})"#,
        );
        assert_eq!(
            many,
            r#"{bodies={},boundaries={},code=0,handler="",kind="success",ok=true,origins={},path={},shape="",text="",values={BODY={1="x",2="y"},TO="a",json=false,loud=false},verb="send"}"#
        );
        let spec2 = "(function() spec.report_version = 2 return spec end)()";
        let one = dump_of(&format!(
            r#"remuda.cli.parse({spec2}, {{"send", "a", "x"}})"#
        ));
        assert!(one.contains(r#"BODY={1="x"}"#), "{one}");
        let zero = dump_of(&format!(r#"remuda.cli.parse({spec2}, {{"send", "a"}})"#));
        assert!(
            zero.contains("BODY={}") && zero.contains(r#"TO="a""#),
            "{zero}"
        );
        assert!(
            !zero.contains("room="),
            "absent scalar stays absent: {zero}"
        );
        assert!(zero.contains("json=false"), "absent flag is false: {zero}");
    }

    #[test]
    fn cli_parse_v2_help_and_error_clear_values() {
        let spec2 = "(function() spec.report_version = 2 return spec end)()";
        let help = dump_of(&format!(
            r#"remuda.cli.parse({spec2}, {{"send", "--help"}})"#
        ));
        assert!(
            help.contains(r#"kind="help""#)
                && help.contains("values={}")
                && help.contains("bodies={}"),
            "{help}"
        );
        let err = dump_of(&format!(
            r#"remuda.cli.parse({spec2}, {{"send", "--nope"}})"#
        ));
        assert!(
            err.contains(r#"kind="error""#)
                && err.contains("values={}")
                && err.contains(r#"handler="""#),
            "{err}"
        );
        let unknown = dump_of(&format!(r#"remuda.cli.parse({spec2}, {{"zzz"}})"#));
        assert!(
            unknown.contains(r#"kind="error""#) && unknown.contains("path={}"),
            "{unknown}"
        );
    }

    #[test]
    fn cli_parse_other_report_versions_stay_v1() {
        let v1 = dump_of(r#"remuda.cli.parse(spec, {"send", "a"})"#);
        for odd in [
            "1",
            "false",
            "{}",
            "function() end",
            "2.5",
            "'2'",
            "0",
            "-2",
        ] {
            let call = format!(
                r#"remuda.cli.parse((function() spec.report_version = {odd} return spec end)(), {{"send", "a"}})"#
            );
            assert_eq!(v1, dump_of(&call), "report_version={odd}");
        }
        assert!(!v1.contains("path="));
    }

    const V2: &str = "(function() spec.version = 2 return spec end)()";

    fn strict(mutate: &str) -> String {
        dump_of(&format!(
            r#"remuda.cli.parse((function() spec.version = 2 {mutate} return spec end)(), {{"send", "a"}})"#
        ))
    }

    #[test]
    fn cli_strict_v2_accepts_a_clean_spec_and_keeps_the_legacy_report() {
        let out = dump_of(&format!(r#"remuda.cli.parse({V2}, {{"send", "a", "x"}})"#));
        assert_eq!(
            out,
            dump_of(r#"remuda.cli.parse(spec, {"send", "a", "x"})"#)
        );
        let both = strict("spec.report_version = 2 spec.requires = {'stable_report', 'strict_v2'}");
        assert!(both.contains(r#"kind="success""#), "{both}");
    }

    #[test]
    fn cli_strict_v2_rejects_bad_specs_with_safe_paths() {
        for (mutate, path) in [
            ("spec.vrbs = {}", "vrbs"),
            ("spec.name = 5", "name"),
            ("spec.verbs.send.next = {}", "verbs.send.next"),
            (
                "spec.verbs.send.args[1].mutliple = true",
                "verbs.send.args[1].mutliple",
            ),
            ("spec.options[1].global = 'yes'", "options[1].global"),
            (
                "spec.verbs.send.args[4] = {name='Z', help='z'}",
                "verbs.send.args",
            ),
            ("spec.requires = {5}", "requires[1]"),
            ("spec.report_version = 2.5", "report_version"),
            ("spec.version = '2'", "version"),
            ("spec.verbs.send.options[1].long = 'help'", "combination"),
        ] {
            let out = strict(mutate);
            assert!(
                out.contains(r#"kind="spec""#) && out.contains("ok=false") && out.contains(path),
                "{mutate}: {out}"
            );
        }
    }

    #[test]
    fn cli_strict_v2_unsupported_has_upgrade_and_next() {
        for mutate in [
            "spec.version = 3",
            "spec.report_version = 3",
            "spec.requires = {'nope'}",
        ] {
            let out = strict(mutate);
            assert!(out.contains(r#"kind="unsupported""#), "{mutate}: {out}");
            assert!(
                out.contains("remuda upgrade") && out.contains("Next:"),
                "{out}"
            );
        }
    }

    #[test]
    fn cli_strict_v2_never_echoes_supplied_values() {
        for mutate in [
            "spec.verbs.send.options[1].long = 'SENTINEL_SECRET'",
            "spec.verbs.send.options[1].short = 'SENTINEL_SECRET'",
            "spec.requires = {'SENTINEL_SECRET'}",
            "spec.SENTINEL_SECRET_KEY_THAT_IS_LONG_AND_ODD_NAME_XYZ = 1",
            "spec.verbs['SENTINEL SECRET'] = {next='n'}",
            "spec.name = {SENTINEL_SECRET=1}",
        ] {
            let out = strict(mutate);
            assert!(!out.contains("SENTINEL"), "{mutate}: {out}");
            assert!(out.contains("ok=false"), "{mutate}: {out}");
        }
    }

    #[test]
    fn cli_report_version_alone_keeps_g8a_behavior() {
        let out = dump_of(
            r#"remuda.cli.parse((function() spec.report_version = 2 spec.oops = 1 return spec end)(), {"send", "a"})"#,
        );
        assert!(
            out.contains(r#"kind="success""#) && out.contains("origins={}"),
            "{out}"
        );
        let both = dump_of(
            r#"remuda.cli.parse((function() spec.version = 2 spec.report_version = 2 spec.oops = 1 return spec end)(), {"send", "a"})"#,
        );
        assert!(
            both.contains(r#"kind="spec""#) && both.contains("origins={}"),
            "{both}"
        );
        let legacy_shape = strict("spec.oops = 1");
        assert!(!legacy_shape.contains("origins"), "{legacy_shape}");
    }

    #[test]
    fn cli_future_versions_are_unsupported_in_the_integer_domain() {
        for v in [
            "4294967297",
            "4294967298",
            "8589934593",
            "math.maxinteger",
            "3",
        ] {
            for call in [
                format!(
                    r#"remuda.cli.parse((function() spec.version = {v} return spec end)(), {{"send", "a"}})"#
                ),
                strict(&format!("spec.report_version = {v}")),
            ] {
                let out = if call.starts_with("remuda") {
                    dump_of(&call)
                } else {
                    call
                };
                assert!(
                    out.contains(r#"kind="unsupported""#) && out.contains("Next:"),
                    "{v}: {out}"
                );
            }
        }
    }

    #[test]
    fn cli_strict_v2_never_runs_metamethods() {
        let call = r#"(function()
            local hits = 0
            local s = {version=2, report_version=2, name="remuda",
              verbs={go={next="remuda", args={{name="X", help="x", required=false}}}}}
            setmetatable(s, {__index=function(t, k)
                hits = hits + 1
                if k == "options" then t.verbs.go.args[1].name = 5 end
            end})
            local r = remuda.cli.parse(s, {"go", "value"})
            r.hits = hits
            return r
        end)()"#;
        let out = dump_of(call);
        assert!(
            out.contains("hits=0") && out.contains(r#"X="value""#),
            "{out}"
        );
        let bad = call.replace("version=2, report", "version=2, oops=1, report");
        let out = dump_of(&bad);
        assert!(
            out.contains("hits=0") && out.contains(r#"kind="spec""#),
            "{out}"
        );
    }

    #[test]
    fn cli_strict_v2_first_problem_is_deterministic() {
        for _ in 0..40 {
            let out = strict("spec.zzz = true spec.aaa = true");
            assert!(out.contains("aaa") && !out.contains("zzz"), "{out}");
        }
    }

    // First lines captured from unmodified origin/main (c1bcaf4).
    #[test]
    fn cli_legacy_exceptions_and_precedence_match_main() {
        for (call, want) in [
            (
                r#"{name={}, verbs={go={next="remuda"}}}, {"go"}"#,
                "error converting Lua table to String (expected string or number)",
            ),
            (
                r#"{name="remuda", verbs={help={next="remuda"}}}, {"go"}"#,
                "runtime error: remuda.cli.parse verb name 'help' is reserved",
            ),
            (
                r#"{name="remuda", verbs={help={next="remuda"}}}, {1}"#,
                "runtime error: remuda.cli.parse verb name 'help' is reserved",
            ),
            (
                r#"{name="remuda", verbs={go={next="remuda"}}}, {1}"#,
                "runtime error: remuda.cli.parse argv[1] must be a string",
            ),
        ] {
            let lua = Lua::new();
            install_cli_binding(&lua);
            let got: String = lua
                .load(format!(
                    "local ok, e = pcall(remuda.cli.parse, {call}) return (tostring(e):match('^[^\\n]*'))"
                ))
                .eval()
                .unwrap();
            assert_eq!(got, want, "{call}");
        }
    }

    #[test]
    fn cli_strict_v2_survives_hostile_tables() {
        for mutate in [
            "spec.verbs.send = spec",
            "spec[1.5] = 1 spec[spec] = 2",
            "spec.verbs[1] = {}",
            "spec.options = {[1]=spec.options[1], [3]=spec.options[1]}",
            "spec.verbs.send.args = {[math.maxinteger]={}}",
        ] {
            let out = strict(mutate);
            assert!(out.contains(r#"kind="spec""#), "{mutate}: {out}");
        }
    }

    fn policy(options: &str, argv: &str) -> String {
        dump_of(&format!(
            r#"remuda.cli.parse({{version=2, name="remuda", requires={{"repeat_policy"}},
            verbs={{go={{next="n", options={{{options}}}}}}}}}, {{"go"{argv}}})"#
        ))
    }

    const APPEND: &str = r#"{long="w",short="w",value="D",help="h",repeat_policy="append"}"#;
    const LAST: &str = r#"{long="m",short="m",value="M",help="h",repeat_policy="last"}"#;
    const COALESCE: &str = r#"{long="j",short="j",help="h",repeat_policy="coalesce"}"#;

    #[test]
    fn cli_repeat_policy_append_is_an_array_for_zero_one_many() {
        for (argv, want) in [
            ("", "w={}"),
            (r#","--w","a""#, r#"w={1="a"}"#),
            (
                r#","--w","a","-w","b","--w=c","-wd""#,
                r#"w={1="a",2="b",3="c",4="d"}"#,
            ),
        ] {
            let out = policy(APPEND, argv);
            assert!(
                out.contains(want) && out.contains("ok=true"),
                "{argv}: {out}"
            );
        }
    }

    #[test]
    fn cli_repeat_policy_last_wins_in_every_spelling() {
        let out = policy(LAST, r#","--m","a","-m","b","--m=c","-md""#);
        assert!(out.contains(r#"m="d""#), "{out}");
        assert!(
            !policy(LAST, "").contains("m="),
            "absent scalar stays absent"
        );
        assert!(policy(LAST, r#","-m","x""#).contains(r#"m="x""#));
    }

    #[test]
    fn cli_repeat_policy_coalesce_is_a_bool() {
        assert!(policy(COALESCE, "").contains("j=false"));
        assert!(policy(COALESCE, r#","-j""#).contains("j=true"));
        let out = policy(COALESCE, r#","--j","-j","-jj""#);
        assert!(out.contains("j=true") && out.contains("ok=true"), "{out}");
    }

    #[test]
    fn cli_repeat_policy_reject_is_the_default_and_errors_on_duplicates() {
        for opt in [
            r#"{long="m",short="m",value="M",help="h"}"#,
            r#"{long="m",short="m",value="M",help="h",repeat_policy="reject"}"#,
        ] {
            assert!(policy(opt, r#","-m","a""#).contains("ok=true"));
            let out = policy(opt, r#","-m","a","--m=b""#);
            assert!(
                out.contains("kind=\"error\"") && out.contains("code=2"),
                "{out}"
            );
        }
        let flag = policy(
            r#"{long="j",help="h",repeat_policy="reject"}"#,
            r#","--j","--j""#,
        );
        assert!(flag.contains("ok=false"), "{flag}");
    }

    #[test]
    fn cli_repeat_policy_rejects_bad_combinations_safely() {
        for opt in [
            r#"{long="a",help="h",repeat_policy="append"}"#,
            r#"{long="a",help="h",repeat_policy="last"}"#,
            r#"{long="a",value="V",help="h",repeat_policy="coalesce"}"#,
            r#"{long="a",value="V",help="h",repeat_policy="SENTINEL SECRET"}"#,
            r#"{long="a",value="V",help="h",repeat_policy=5}"#,
            r#"{long="a",value="V",help="h",global=true,repeat_policy="append"}"#,
        ] {
            let out = policy(opt, "");
            assert!(
                out.contains(r#"kind="spec""#)
                    && out.contains("code=2")
                    && !out.contains("SENTINEL"),
                "{opt}: {out}"
            );
        }
    }

    #[test]
    fn cli_repeat_policy_is_strict_only_and_v1_ignores_it() {
        let legacy = r#"remuda.cli.parse({name="remuda", verbs={go={next="n",
            options={{long="m",value="M",help="h",repeat_policy="append"}}}}}, {"go","--m","a"})"#;
        assert_eq!(
            dump_of(legacy),
            dump_of(&legacy.replace(r#",repeat_policy="append""#, ""))
        );
        assert!(dump_of(legacy).contains(r#"m="a""#));
        let dup = legacy.replace(r#""--m","a""#, r#""--m","a","--m","b""#);
        assert!(
            dump_of(&dup).contains("ok=false"),
            "v1 duplicates still error"
        );
    }

    #[test]
    fn cli_repeat_policy_hostile_tables_reject_without_panic() {
        for opt in [
            r#"{long="a",value="V",help="h",repeat_policy={}}"#,
            r#"{long="a",value="V",help="h",repeat_policy=true}"#,
            r#"{long="a",value="V",help="h",repeat_policy="APPEND"}"#,
            r#"{long="a",value="V",help="h",repeat_policy=""}"#,
            r#"{long="a",value="V",help="h",repeat_policy="append",[1]="x"}"#,
        ] {
            assert!(policy(opt, "").contains(r#"kind="spec""#), "{opt}");
        }
    }

    #[test]
    fn cli_v1_ignores_unknown_keys_and_never_touches_metatables() {
        let call = r#"(function()
            local raw = {name=spec.name, options=spec.options, verbs=spec.verbs, typo=1, version=1}
            local seen = {}
            setmetatable(raw, {__index=function(_, k)
                if k == "version" or k == "report_version" or k == "requires" then seen[#seen + 1] = k end
            end})
            raw.version = nil
            local r = remuda.cli.parse(raw, {"send", "a", "x"})
            r.touched = #seen
            return r
        end)()"#;
        let out = dump_of(call);
        assert!(
            out.contains("touched=0") && out.contains(r#"verb="send""#),
            "{out}"
        );
        let one = dump_of(
            r#"remuda.cli.parse((function() spec.version = 1 spec.typo = 1 return spec end)(), {"send", "a", "x"})"#,
        );
        assert_eq!(
            one,
            dump_of(r#"remuda.cli.parse(spec, {"send", "a", "x"})"#)
        );
    }

    #[test]
    fn cli_parse_report_version_ignores_metatables() {
        let call = r#"(function()
            hits = {}
            local raw = {name=spec.name, options=spec.options, verbs=spec.verbs}
            setmetatable(raw, {__index=function(_, k) hits[#hits + 1] = k; if k == "report_version" then return 2 end end})
            local r = remuda.cli.parse(raw, {"send", "a"})
            r.hit_report_version = #hits > 0 and (hits[1] == "report_version" or hits[2] == "report_version")
            return r
        end)()"#;
        let out = dump_of(call);
        assert!(out.contains("hit_report_version=false"), "{out}");
        assert!(out.contains("verb=") && !out.contains("path="), "{out}");
    }

    #[test]
    fn cli_spec_rejects_duplicate_short_options() {
        rejects_cli_spec(
            r#"return {name="remuda", options={{long="one",short="x",help="a"},{long="two",short="x",help="b"}}, verbs={go={next="remuda"}}}"#,
        );
    }

    #[test]
    fn cli_spec_rejects_non_alphanumeric_short_options() {
        rejects_cli_spec(
            r#"return {name="remuda", options={{long="one",short="!",help="a"}}, verbs={go={next="remuda"}}}"#,
        );
    }

    #[test]
    fn cli_spec_rejects_bad_verb_tokens() {
        rejects_cli_spec(r#"return {name="remuda", verbs={ ["bad name"]={next="remuda"} }}"#);
    }

    #[test]
    fn cli_spec_sorts_verb_names() {
        let lua = Lua::new();
        let table: Table = lua
            .load(r#"return {name="remuda", verbs={zeta={next="remuda"},alpha={next="remuda"}}}"#)
            .eval()
            .unwrap();
        let spec = cli_spec_from_lua(table).unwrap();
        let names = spec
            .verbs
            .iter()
            .map(|verb| verb.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["alpha", "zeta"]);
    }

    #[test]
    fn cli_parse_accepts_nul_in_argv_values() {
        let lua = Lua::new();
        install_cli_binding(&lua);
        lua.load(
            r#"
            local spec = {name="remuda", verbs={send={next="remuda",args={{name="TEXT",help="text"}}}}}
            local result = remuda.cli.parse(spec, {"send", "left\0right"})
            assert(result.ok and result.values.TEXT == "left\0right")
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn cli_parse_rejects_non_string_argv_entries() {
        let lua = Lua::new();
        install_cli_binding(&lua);
        lua.load(
            r#"
            local spec = {name="remuda", verbs={go={next="remuda"}}}
            local ok, err = pcall(remuda.cli.parse, spec, {"go", 7})
            assert(not ok, tostring(err))
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn cli_parse_rejects_argv_tables_with_holes() {
        rejects_cli_argv(r#"return {[1]="go", [3]="later"}"#);
    }

    #[cfg(unix)]
    #[test]
    fn mkdir_new_creates_a_private_directory() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir()
            .join(format!("remuda-mkdir-new-{}-{nonce}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let lua = Lua::new();
        let remuda = lua.create_table().unwrap();
        fs_bindings(&lua, &remuda).unwrap();
        lua.globals().set("remuda", remuda).unwrap();
        lua.globals().set("target", path.as_str()).unwrap();

        lua.load("assert(remuda.fs.mkdir_new(target) == true)")
            .exec()
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        std::fs::remove_dir(&path).unwrap();
        assert_eq!(mode, 0o700, "new directories must be owner-only");
    }

    #[test]
    fn feed_return_burst_is_the_same_byte_as_named_return() {
        let lua = mlua::Lua::new();
        let steps: mlua::Table = lua.load(r#"{{burst='\r'}}"#).eval().unwrap();
        let wire = lua_steps_to_wire(steps).unwrap();
        assert_eq!(
            wire,
            [Step::Burst(remuda_core::keys::RETURN_BYTES.to_vec())]
        );
        assert_eq!(
            remuda_core::keys::key("RET").as_deref(),
            Some(remuda_core::keys::RETURN_BYTES)
        );
    }
}
