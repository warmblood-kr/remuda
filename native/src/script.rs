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
use remuda_core::InputSubmitOutcome;
use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

/// Every name in the live `remuda` table: the operations bound here, plus
/// what `tools.lua` adds in pure Lua. Asserted against the live table, both
/// directions.
pub const BINDINGS: [&str; 84] = [
    "_advice_reattach",
    "_call",
    "_descriptors",
    "_dispatch_extension_command",
    "_event_counts",
    "_extension_commands",
    "_function_source",
    "_input_submit",
    "_input_text",
    "_input_type_text",
    "_module_readiness",
    "_pending_create",
    "_pending_events",
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
    "attach",
    "buffer",
    "buffers",
    "cancel",
    "capture",
    "capture_styled",
    "clear_hooks",
    "click",
    "close",
    "contribute",
    "contributions",
    "emit",
    "emit_filter",
    "emit_until_failure",
    "emit_until_success",
    "event_counts",
    "exec",
    "expect",
    "expect_option",
    "extension_command",
    "fail",
    "feed",
    "fs",
    "hook_list",
    "hooks",
    "http",
    "input",
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
    "reload",
    "remove_dir_all",
    "request_counts",
    "schedule",
    "schedule_fires",
    "schedule_skips",
    "schedules",
    "send",
    "session",
    "sleep",
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
        "Deliver text and submit it while holding one input lock; returns 'submitted' or 'unverified'.",
        "_input_type_text(name, text, settle?) -> status",
    ),
    (
        "_module_readiness",
        "Internal readiness poll for remuda exec.",
        "_module_readiness(name) -> {status, timeout_ms?, message?}",
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
        "_session_resize",
        "Resize a session after validating its requested dimensions.",
        "_session_resize(name, cols, rows) -> true, nil | nil, error",
    ),
    (
        "http",
        "Start an asynchronous bounded HTTP request; completion is delivered on the Lua image queue.",
        "http.request(options) -> {cancel()}",
    ),
    (
        "fs",
        "Atomic replacement of files for trusted Lua callers.",
        "table",
    ),
    (
        "fs.write_atomic",
        "Write bytes through a same-directory temporary file and atomically replace the target.",
        "fs.write_atomic(path, bytes) -> true, nil | nil, error",
    ),
    (
        "fs.mkdir_new",
        "Create one new directory without creating parents or trusting an existing path.",
        "fs.mkdir_new(path) -> true | nil, 'exists' | nil, error",
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
        "sleep",
        "Block the calling image for a number of seconds.",
        "sleep(seconds) -> nil",
    ),
    (
        "_registry",
        "The word registry itself: name, about and signature for every bound word.",
        "table",
    ),
    (
        "_process_run",
        "Run an argv process synchronously with a bounded timeout and captured output; internal, called by `remuda.process.run`.",
        "_process_run(argv, stdin?, timeout) -> result",
    ),
    (
        "_process_spawn",
        "Spawn a plain-pipe child process; internal, wrapped by `remuda.process`.",
        "_process_spawn(argv, on_line?, on_exit?) -> id",
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
    fs_bindings(lua, table)?;
    table.set("_registry", registry)
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
                })
                .map_err(|error| mlua::Error::runtime(error.to_string()))
        })?,
    )?;

    table.set(
        "_input_type_text",
        lua.create_function(
            move |_, (name, text, settle): (String, String, Option<f64>)| {
                let settle = settle.unwrap_or(0.1);
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
                    })
                    .map_err(|error| mlua::Error::runtime(error.to_string()))
            },
        )?,
    )?;
    Ok(())
}

pub fn bindings(
    lua: &Lua,
    socket: &Path,
    registry: std::sync::Arc<remuda_core::Registry>,
    counters: std::sync::Arc<crate::tick::Counters>,
    image: crate::image::Image,
) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    let at = || socket.to_path_buf();
    let input_registry = registry.clone();
    fail_binding(lua, &table, image.clone())?;
    pending_bindings(lua, &table, image.pending_replies())?;

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
    // Like `sleep`, this blocks the calling Image — for as long as `steps`'s
    // pauses sum to: `client::request` waits synchronously for the daemon's
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
    process_bindings(lua, &table, image)?;

    // Blocks the whole Image while this Rust call sleeps. The Lua instruction
    // budget does not count time spent in Rust bindings, C-library functions,
    // or Lua 5.4 `__gc` finalizers (which run with hooks disabled); it bounds
    // Lua VM instructions only. A long `string.find` backtrack or `string.rep`
    // can therefore still occupy the image until that call returns. Loops of
    // cheap Rust/C binding calls take longer to reach the 200M-instruction
    // limit too, and the hook cannot interrupt one blocking call.
    // Each coroutine create/resume also reserves 10K instructions; this caps
    // generators at roughly 20K such operations in one job.
    // This is not a wait or timer primitive; remuda has no periodic-execution
    // mechanism yet, and a sleep-and-poll loop holds the Image hostage too.
    sleep_binding(lua, &table)?;

    Ok(table)
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

fn sleep_binding(lua: &Lua, table: &Table) -> mlua::Result<()> {
    table.set(
        "sleep",
        lua.create_function(|_, seconds: f64| {
            // Negative or NaN durations do nothing instead of panicking in
            // `Duration::from_secs_f64`.
            if seconds.is_finite() && seconds > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(seconds));
            }
            Ok(())
        })?,
    )
}

fn pending_bindings(
    lua: &Lua,
    table: &Table,
    pending: crate::pending::PendingReplies,
) -> mlua::Result<()> {
    let create = pending.clone();
    table.set(
        "_pending_create",
        lua.create_function(move |lua, timeout: Option<f64>| {
            let seconds = timeout.unwrap_or(30.0);
            if !seconds.is_finite() || seconds <= 0.0 || seconds > 300.0 {
                return Err(mlua::Error::runtime(
                    "pending timeout must be a positive number no greater than 300 seconds",
                ));
            }
            let duration = Duration::from_secs_f64(seconds.max(0.000_000_001));
            let (id, handle) = create.create(duration).map_err(|message| {
                mlua::Error::external(crate::image::TypedFailure { message, code: 1 })
            })?;
            Ok((id, lua.create_userdata(handle)?))
        })?,
    )?;
    table.set(
        "_pending_events",
        lua.create_function(move |lua, ()| {
            let events = pending.drain_events();
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
            lua.create_function(|lua, name: String| execute_package(lua, &name, false))?,
        )
        .and_then(|()| {
            table.set(
                "reload",
                lua.create_function(|lua, name: String| execute_package(lua, &name, true))?,
            )
        })
}

/// A top-level mod first gets its `requires` checked as a whole, then its
/// lifecycle hosts activated in order (exec leaves an active one alone).
fn execute_package(lua: &Lua, name: &str, require_lifecycle: bool) -> mlua::Result<()> {
    let loaded = load_package(lua, name, require_lifecycle);
    // A mod that redefined an advised function keeps its advice: the new
    // definition becomes the base (hook-design §2). Even after a failed
    // load, which may have redefined some before it stopped.
    let remuda: Table = lua.globals().get("remuda")?;
    if let Ok(reattach) = remuda.get::<mlua::Function>("_advice_reattach") {
        reattach.call::<()>(())?;
    }
    loaded
}

fn load_package(lua: &Lua, name: &str, require_lifecycle: bool) -> mlua::Result<()> {
    if !name.contains('/') {
        for host in crate::packages::requirement_order(name).map_err(mlua::Error::runtime)? {
            let lifecycle = crate::packages::resolve(&host)
                .map_err(mlua::Error::runtime)?
                .is_some_and(|package| package.lifecycle.is_some());
            if lifecycle {
                activate_package(lua, &host, false)?;
            }
        }
    }
    activate_package(lua, name, require_lifecycle)
}

fn activate_package(lua: &Lua, name: &str, require_lifecycle: bool) -> mlua::Result<()> {
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
        let (_, state, start, rollback): (bool, Value, Value, Value) =
            activate.call((name, declaration, require_lifecycle))?;
        active.set(true);
        if let (Value::Function(start), Value::Function(rollback)) = (start, rollback) {
            let remuda_global: Table = lua.globals().get("remuda")?;
            let was_active: bool = remuda_global
                .get("_lifecycle_start_active")
                .unwrap_or(false);
            remuda_global.set("_lifecycle_start_active", true)?;
            let result = start.call::<()>(state);
            remuda_global.set("_lifecycle_start_active", was_active)?;
            if let Err(error) = result {
                rollback.call::<()>(())?;
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
        lua.create_function(|_, (path, bytes): (String, mlua::LuaString)| {
            match crate::fs_atomic::write_atomic(Path::new(&path), &bytes.as_bytes(), 0o644) {
                Ok(()) => Ok((Some(true), None::<String>)),
                Err(error) => Ok((None::<bool>, Some(error.to_string()))),
            }
        })?,
    )?;
    table.set("fs", fs)
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

/// `remuda.process`'s Rust half — split out of `bindings` to stay under its
/// line cap.
// `remuda.process` itself (the validated, Lua-facing spec-table word) lives
// in `tools.lua` and calls `_process_spawn` here; `kill` and `processes` are
// plain Rust words with nothing to validate.
fn process_bindings(lua: &Lua, table: &Table, image: crate::image::Image) -> mlua::Result<()> {
    let processes = crate::process::Processes::new();

    let spawner = processes.clone();
    let spawn_image = image.clone();
    table.set(
        "_process_spawn",
        lua.create_function(
            move |_, (argv, on_line, on_exit): (Vec<String>, Option<String>, Option<String>)| {
                spawner
                    .spawn(spawn_image.clone(), argv, on_line, on_exit)
                    .map_err(mlua::Error::external)
            },
        )?,
    )?;

    table.set(
        "_process_run",
        lua.create_function(
            |lua, (argv, stdin, timeout): (Vec<String>, Option<mlua::LuaString>, f64)| {
                let output = crate::process::run_sync(
                    argv,
                    stdin.map(|value| value.as_bytes().to_vec()),
                    timeout,
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
                Ok(result)
            },
        )?,
    )?;

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

fn ask(socket: &Path, request: Request) -> mlua::Result<Response> {
    client::request(socket, &request).map_err(mlua::Error::external)
}

/// A Lua array of `{burst = "..."}` / `{pause = seconds}` entries into the
/// wire `Step`s `feed` delivers — `pause` is seconds, matching `sleep`, and
/// travels the wire as whole milliseconds.
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
        Response::Busy => Err(mlua::Error::runtime("session input is busy")),
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
        Response::ClusterRegistryPage { .. } | Response::ClusterRegistryAck { .. } => Err(
            mlua::Error::runtime("cluster registry responses are not exposed to scripts"),
        ),
    }
}

#[cfg(test)]
mod binding_tests {
    use super::{fs_bindings, lua_steps_to_wire, BINDINGS};
    use mlua::Lua;
    use remuda_core::protocol::Step;

    #[test]
    fn binding_names_are_sorted_and_unique() {
        assert!(BINDINGS.windows(2).all(|pair| pair[0] < pair[1]));
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
