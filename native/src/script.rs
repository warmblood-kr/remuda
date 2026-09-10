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
use remuda_core::protocol::{Request, Response};
use std::path::Path;
use std::time::Duration;

/// Every name in the live `remuda` table: the protocol operations and `sleep`
/// bound here, then `tool`/`tools`/`_call`/`_descriptors` added by `tools.lua`.
/// Asserted against the live table, both directions.
pub const BINDINGS: [&str; 14] = [
    "_call",
    "_descriptors",
    "attach",
    "capture",
    "click",
    "close",
    "insert",
    "key",
    "ls",
    "new",
    "send",
    "sleep",
    "tool",
    "tools",
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

pub fn bindings(lua: &Lua, socket: &Path) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    let at = || socket.to_path_buf();

    let path = at();
    table.set(
        "ls",
        lua.create_function(move |lua, ()| value(lua, ask(&path, Request::List)?))?,
    )?;

    let path = at();
    table.set(
        "new",
        lua.create_function(
            move |lua, (name, argv): (Option<String>, Option<Vec<String>>)| {
                let request = Request::New {
                    name,
                    command: argv.unwrap_or_default(),
                    size: crate::terminal_size(),
                };
                value(lua, ask(&path, request)?)
            },
        )?,
    )?;

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

fn ask(socket: &Path, request: Request) -> mlua::Result<Response> {
    client::request(socket, &request).map_err(mlua::Error::external)
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
        Response::Error(reason) => Err(mlua::Error::runtime(reason)),
        // No binding here asks for `CaptureStyled` either — same reasoning as
        // `Response::Value` above.
        Response::StyledScreen(_) => Err(mlua::Error::runtime(
            "styled capture is not exposed to scripts",
        )),
    }
}
