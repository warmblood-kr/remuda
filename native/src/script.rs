//! A programming runtime, with the atomic functions wired to it.
//!
//! This is safe to do because the vocabulary handed to Lua is exactly
//! [`Request`] — the surface that already has no raw write and no resize. A
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
use std::path::Path;
use std::time::Duration;

/// Every name in the live `remuda` table: the operations bound here, plus
/// what `tools.lua` adds in pure Lua. Asserted against the live table, both
/// directions.
pub const BINDINGS: [&str; 23] = [
    "_call",
    "_descriptors",
    "_run_due_schedules",
    "attach",
    "capture",
    "click",
    "close",
    "feed",
    "insert",
    "key",
    "list_dir",
    "ls",
    "mkdir",
    "new",
    "remove_dir_all",
    "schedule",
    "schedule_skips",
    "schedules",
    "send",
    "sleep",
    "tool",
    "tools",
    "type_text",
];

/// Run a script file **in the daemon's image**, never in a fresh `Lua::new()`
/// here — a script must see the state `-e` and the REPL share. The chunk name
/// travels with the source so a traceback still names the file.
pub fn run(socket: &Path, script: &Path) -> Result<(), String> {
    let source = std::fs::read_to_string(script).map_err(|e| e.to_string())?;
    let request = Request::Eval {
        code: source,
        name: Some(script.display().to_string()),
    };
    match client::request(socket, &request).map_err(|e| e.to_string())? {
        // Whatever the script printed comes back in the same string (the
        // daemon's own stdout is /dev/null, so `print` is captured rather than
        // written) and is relayed here. Empty means it printed nothing and
        // returned nothing, which should stay silent.
        Response::Value(output) => {
            if !output.is_empty() {
                println!("{output}");
            }
            Ok(())
        }
        Response::Error(reason) => Err(reason),
        other => Err(format!("unexpected response: {other:?}")),
    }
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

pub fn bindings(
    lua: &Lua,
    socket: &Path,
    counters: std::sync::Arc<crate::tick::SkipCounters>,
) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    let at = || socket.to_path_buf();

    let path = at();
    table.set(
        "ls",
        lua.create_function(move |lua, ()| value(lua, ask(&path, Request::List)?))?,
    )?;

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

    // End a session — live, or already self-exited (step 006). A dead session
    // stays listed with its last screen intact until this is called; nothing
    // reaps it on its own, on purpose (`steps/006-lifetime.md`).
    let path = at();
    table.set(
        "close",
        lua.create_function(move |lua, name: String| {
            value(lua, ask(&path, Request::Close { name })?)
        })?,
    )?;

    dir_bindings(lua, &table, &at)?;
    tick_bindings(lua, &table, counters)?;

    // Blocks the WHOLE Image, not just this call: the interpreter is pinned to
    // one thread (image.rs), so a sleeping script stalls every other job —
    // the REPL, `-e`, any other script — for the full duration. Not a wait or
    // a timer primitive; remuda has no periodic-execution mechanism yet, and
    // faking one with a sleep-and-poll loop holds the Image hostage the same way.
    table.set(
        "sleep",
        lua.create_function(|_, seconds: f64| {
            // A negative or NaN duration would panic in `from_secs_f64`; a
            // script asking to sleep backwards gets nothing rather than a crash.
            if seconds.is_finite() && seconds > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(seconds));
            }
            Ok(())
        })?,
    )?;

    Ok(table)
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

/// The `Ticker`'s own skip counters, read-only — no threshold or alarm here,
/// split out of `bindings` to stay under its line cap. See `tick.rs`'s own
/// hook-point comment for why acting on them is a separate, undecided step.
fn tick_bindings(
    lua: &Lua,
    table: &Table,
    counters: std::sync::Arc<crate::tick::SkipCounters>,
) -> mlua::Result<()> {
    table.set(
        "schedule_skips",
        lua.create_function(move |lua, ()| {
            let row = lua.create_table()?;
            row.set("consecutive", counters.consecutive())?;
            row.set("total", counters.total())?;
            Ok(row)
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
        Response::Screen(text) => Ok(Value::String(lua.create_string(&text)?)),
        // No binding here asks for an `Eval`, so this arm is unreachable in
        // practice — spelled out rather than folded into a wildcard so that
        // adding one later is a compile error to think about, not a silent
        // fall-through that returns the wrong shape.
        Response::Value(text) => Ok(Value::String(lua.create_string(&text)?)),
        Response::Sessions(list) => {
            let rows = lua.create_table()?;
            for (index, session) in list.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("name", session.name)?;
                row.set("alive", session.alive)?;
                row.set("idle", session.idle.as_secs_f64())?;
                row.set("cols", session.size.cols())?;
                row.set("rows", session.size.rows())?;
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
        Response::Error(reason) => Err(mlua::Error::runtime(reason)),
        // No binding here asks for `CaptureStyled` either — same reasoning as
        // `Response::Value` above.
        Response::StyledScreen { .. } => Err(mlua::Error::runtime(
            "styled capture is not exposed to scripts",
        )),
    }
}
