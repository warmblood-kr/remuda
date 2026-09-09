//! A programming runtime, with the atomic functions wired to it.
//!
//! 정수님, 2026-09-10: *"이 pty manager layer에 programming runtime을 심어서
//! 코드를 실행할 수 있게 만들고 atomic function들을 물려서 연결합니다."*
//!
//! The reason this is safe to do at all is that the vocabulary handed to Lua is
//! exactly [`Request`] — the surface that already has no raw write and no
//! resize. A script gets Turing-completeness over *which* operations run in
//! *what order*; it gets no operation the CLI does not already have, because
//! there is no such variant to bind. Invariant 3 is enforced by the daemon when
//! the call arrives, not by the caller behaving, so a script that sends into a
//! human-held session is refused exactly as the CLI is.
//!
//! That is `PRINCIPLES.md` §6 collecting its rent: because the dangerous
//! operations were *removed from the API* rather than forbidden by rule, a
//! whole scripting language could be pointed at it without re-auditing anything.
//!
//! **This is not a sandbox.** `remuda run script.lua` is as trusted as
//! `sh script.sh` — the user runs their own file on their own machine, and the
//! full standard library is what makes a setup script worth writing. That
//! changes the day a script arrives from another node over the transport layer.
//! The restricted stdlib belongs in *that* change, where the boundary actually
//! appears, and putting it here now would be a wall around the wrong thing.

use crate::client;
use mlua::{Lua, Table, Value};
use remuda_core::keys;
use remuda_core::protocol::{Request, Response};
use std::path::Path;
use std::time::Duration;

/// Every name bound into the `remuda` table.
///
/// The protocol operations, plus `sleep`. `sleep` is not a remuda operation and
/// is here because without it the runtime cannot express the one thing it exists
/// for: send, wait, look, decide. The polling loop itself is written in Lua —
/// building that helper in Rust would be doing in the host the exact job the
/// guest language was embedded to do.
///
/// `insert`, `key` and `click` are three names over *one* new operation
/// ([`Request::Send`]). They are spellings, in the sense elisp has `insert`,
/// `insert-char` and `insert-buffer-substring` over one primitive — the shape
/// 정수님 pointed at on 2026-09-10. Adding them widened the protocol by a single
/// variant, not by three.
///
/// `tests/script.rs` asserts the live table's keys against this list in both
/// directions, so a binding added without a decision, or a name listed here and
/// never bound, fails the suite.
pub const BINDINGS: [&str; 10] = [
    "attach", "capture", "click", "close", "insert", "key", "ls", "new", "send", "sleep",
];

/// Execute a Lua script with the atomic functions bound.
pub fn run(socket: &Path, script: &Path) -> mlua::Result<()> {
    let source = std::fs::read_to_string(script)?;
    let lua = Lua::new();
    let table = bindings(&lua, socket)?;
    lua.globals().set("remuda", table)?;
    // The `@` prefix is Lua's own marker for "this chunk name is a filename".
    // Without it a traceback reads `[string "/path/to/x.lua"]:2:`; with it,
    // `/path/to/x.lua:2:` — the form an editor and a human both jump from. One
    // character, and it is the difference between an error that names the file
    // and one that quotes it.
    lua.load(source)
        .set_name(format!("@{}", script.display()))
        .exec()
}

fn bindings(lua: &Lua, socket: &Path) -> mlua::Result<Table> {
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
        lua.create_function(move |lua, (name, argv): (String, Option<Vec<String>>)| {
            let request = Request::New {
                name,
                command: argv.unwrap_or_default(),
                size: crate::terminal_size(),
            };
            value(lua, ask(&path, request)?)
        })?,
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
        lua.create_function(move |_, name: String| {
            client::attach(&path, &name).map_err(mlua::Error::external)
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

/// Turn a response into what the script sees — and a refusal into a raised
/// error.
///
/// [`Response::Error`] deliberately does **not** become a return value. A
/// script that forgets to check one would otherwise carry on over a session
/// that was never created, which is the failure mode the shell version already
/// had: `$(remuda capture x)` yields an empty string for a missing session and
/// the pipeline proceeds on nothing.
fn value(lua: &Lua, response: Response) -> mlua::Result<Value> {
    match response {
        Response::Ok => Ok(Value::Nil),
        Response::Screen(text) => Ok(Value::String(lua.create_string(&text)?)),
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
    }
}
