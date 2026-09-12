//! The image: one Lua interpreter that lives as long as the daemon.
//!
//! # Why a thread and a channel, and not a mutex around `Lua`
//!
//! `mlua::Lua` is `!Send` without the `send` feature — a Lua state is
//! single-threaded, and no lock changes that. So the state is *pinned* to one
//! thread that owns it outright, and every other thread reaches it by posting a
//! job and waiting for the answer. A thread serving a queue is an event loop.
//!
//! Unlike Emacs, a long-running script freezes nothing else: sessions here are
//! not Lua objects but Rust structs behind their own locks, pumped by threads
//! that never touch Lua. A script that loops forever makes only the *next Lua
//! caller* wait — pty output keeps being read, `remuda ls` keeps answering.
//!
//! ⚠ The corollary, named now so it is not discovered as a deadlock later: an
//! output pump may never *call into* Lua. If `on_output(session, fn)` is ever
//! added, the pump must post a job to this loop, not run the callback itself.

use crate::script;
use mlua::Lua;
use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{channel, Sender};

/// One unit of work for the image: source to evaluate, and where the answer
/// goes. The reply channel is per-job rather than shared, so two callers
/// waiting at once cannot receive each other's result.
struct Job {
    code: String,
    name: Option<String>,
    reply: Sender<Result<String, String>>,
}

/// A handle to the daemon's Lua image. Cloneable and `Send`; the interpreter
/// itself never leaves its thread.
#[derive(Clone)]
pub struct Image {
    jobs: Sender<Job>,
}

impl Image {
    /// Start the interpreter and return a handle to it. `socket` is the daemon's
    /// own: the `remuda` table calls back over it rather than reaching the
    /// `Registry`, keeping one definition of the vocabulary instead of two.
    pub fn spawn(socket: &Path) -> Self {
        let (jobs, inbox) = channel::<Job>();
        let socket: PathBuf = socket.to_path_buf();

        std::thread::spawn(move || {
            let lua = Lua::new();
            // Everything `print` writes during one job, so it can travel back
            // to whoever asked instead of vanishing. `Rc` rather than `Arc`
            // because this never leaves the thread — the whole reason the
            // interpreter is pinned here.
            let printed = Rc::new(RefCell::new(String::new()));

            // A failure here means no image at all, so every eval must say so
            // rather than the thread dying quietly and every caller hanging.
            let ready = script::bindings(&lua, &socket)
                .and_then(|table| lua.globals().set("remuda", table))
                // The tool frame is Lua over those bindings, not a second set of
                // them. It must load *after* the table exists and *before* any
                // caller, so a `tools/list` on a fresh daemon is already true.
                .and_then(|()| {
                    lua.load(include_str!("tools.lua"))
                        .set_name("@remuda/tools.lua")
                        .exec()
                })
                .and_then(|()| capture_print(&lua, Rc::clone(&printed)))
                .map_err(|e| e.to_string());

            for job in inbox {
                printed.borrow_mut().clear();
                let answer = match &ready {
                    Err(why) => Err(format!("image failed to start: {why}")),
                    Ok(()) => eval(&lua, &job.code, job.name.as_deref())
                        .map(|value| join_output(&printed.borrow(), &value)),
                };
                // A caller that gave up and dropped its receiver is not an
                // error: `remuda -e` can be Ctrl-C'd mid-evaluation, and the
                // work still ran.
                let _ = job.reply.send(answer);
            }
        });

        Self { jobs }
    }

    /// Send `code` without waiting for it to finish — `tick.rs` needs this so
    /// its own callback's duration never blocks it or the FIFO queue behind it.
    pub fn submit(
        &self,
        code: &str,
        name: Option<&str>,
    ) -> Result<std::sync::mpsc::Receiver<Result<String, String>>, String> {
        let (reply, answer) = channel();
        self.jobs
            .send(Job {
                code: code.to_string(),
                name: name.map(str::to_string),
                reply,
            })
            .map_err(|_| "the image is not running".to_string())?;
        Ok(answer)
    }

    /// Evaluate `code` in the image and wait for the result. State persists
    /// between calls — a `-e`, a script and a REPL line are all doors into one
    /// interpreter, and a variable set by any of them outlives the call.
    pub fn eval(&self, code: &str, name: Option<&str>) -> Result<String, String> {
        self.submit(code, name)?
            .recv()
            .map_err(|_| "the image stopped without answering".to_string())?
    }
}

/// Evaluate one chunk, expression-first: `return <code>` is tried before plain
/// `<code>` (the reference REPL's `addreturn` trick), so `-e "1+1"` prints `2`
/// and `-e "x = 1"` still works. Results are `tostring`ed and tab-joined.
fn eval(lua: &Lua, code: &str, name: Option<&str>) -> Result<String, String> {
    // Lua's own convention: `@` means "this is a filename", `=` means "use
    // this verbatim". A script keeps the path it came from so a traceback
    // names a file an editor can jump to; a `-e` or REPL line has no file, and
    // `(eval)` is what it should be called instead of a quoted copy of itself.
    let chunk = name.map_or_else(|| "=(eval)".to_string(), |n| format!("@{n}"));

    let as_expression = lua
        .load(format!("return {code}"))
        .set_name(&chunk)
        .eval::<mlua::MultiValue>();

    let values = match as_expression {
        Ok(values) => values,
        // Not an expression. Run it as statements, and report *that* error if
        // it fails — reporting the expression-parse error instead would name a
        // `return` the user never wrote, which is the single most confusing
        // thing this function could do.
        Err(_) => lua
            .load(code)
            .set_name(&chunk)
            .eval::<mlua::MultiValue>()
            .map_err(|e| e.to_string())?,
    };

    let rendered: Vec<String> = values.iter().map(render).collect();
    Ok(rendered.join("\t"))
}

/// Point `print` at a buffer instead of the daemon's stdout, which is
/// `Stdio::null()` — without this a script's `print` vanishes and the caller
/// sees nothing. Lua semantics kept: `tostring`ed, tab-separated, newline.
fn capture_print(lua: &Lua, into: Rc<RefCell<String>>) -> mlua::Result<()> {
    let print = lua.create_function(move |_, values: mlua::MultiValue| {
        let line: Vec<String> = values.iter().map(render).collect();
        let mut buffer = into.borrow_mut();
        buffer.push_str(&line.join("\t"));
        buffer.push('\n');
        Ok(())
    })?;
    lua.globals().set("print", print)
}

/// What the caller sees: anything printed, then whatever the chunk came to.
/// Either half may be empty, and neither leaves a stray blank line behind.
fn join_output(printed: &str, value: &str) -> String {
    match (printed.trim_end_matches('\n'), value) {
        ("", value) => value.to_string(),
        (printed, "") => printed.to_string(),
        (printed, value) => format!("{printed}\n{value}"),
    }
}

/// Deliberately small: a table printed at a REPL is for reading, and anything
/// past this is a dump. Both caps elide with `…` rather than truncating
/// silently, so a short rendering never reads as a whole table.
const MAX_DEPTH: usize = 4;
const MAX_ITEMS: usize = 32;

/// One value as a Lua user expects to see it: `tostring` semantics, minus the
/// address on functions and threads — an address differs every run, so an
/// expected output could not be written down. Tables render their contents.
fn render(value: &mlua::Value) -> String {
    match value {
        mlua::Value::Nil => "nil".to_string(),
        mlua::Value::Boolean(b) => b.to_string(),
        mlua::Value::Integer(i) => i.to_string(),
        mlua::Value::Number(n) => n.to_string(),
        mlua::Value::String(s) => s.to_string_lossy().to_string(),
        mlua::Value::Table(t) => render_table(t, 0, &mut Vec::new()),
        other => format!("<{}>", other.type_name()),
    }
}

/// A value seen *inside* a table, where `1` and `"1"` must not look alike —
/// so strings are quoted here and bare at top level, keeping `tostring`
/// semantics for `print` and for `remuda -e "s"`.
fn element(value: &mlua::Value, depth: usize, seen: &mut Vec<*const c_void>) -> String {
    match value {
        mlua::Value::String(s) => format!("{:?}", s.to_string_lossy()),
        mlua::Value::Table(t) => render_table(t, depth, seen),
        other => render(other),
    }
}

/// A table by its contents. `seen` holds the tables on the path from the root,
/// so `t.self = t` renders `<cycle>` instead of recursing forever.
fn render_table(t: &mlua::Table, depth: usize, seen: &mut Vec<*const c_void>) -> String {
    let identity = t.to_pointer();
    if seen.contains(&identity) {
        return "<cycle>".to_string();
    }
    if depth >= MAX_DEPTH {
        return "{…}".to_string();
    }
    seen.push(identity);
    let body = table_body(t, depth, seen);
    seen.pop();
    body
}

/// Array part in index order, then every other key sorted — an address is not
/// the only thing that varies between runs, and Lua's hash order is the other.
fn table_body(t: &mlua::Table, depth: usize, seen: &mut Vec<*const c_void>) -> String {
    let len = t.raw_len();
    let mut items: Vec<String> = (1..=len)
        .take(MAX_ITEMS)
        .map(|i| element(&t.raw_get(i).unwrap_or(mlua::Value::Nil), depth + 1, seen))
        .collect();

    let mut keyed: Vec<(String, String, String)> = t
        .pairs::<mlua::Value, mlua::Value>()
        .flatten()
        .filter(|(k, _)| !in_array_part(k, len))
        .map(|(k, v)| {
            let sort_by = element(&k, depth + 1, seen);
            let shown = format!(
                "{} = {}",
                key(&k, depth, seen),
                element(&v, depth + 1, seen)
            );
            (k.type_name().to_string(), sort_by, shown)
        })
        .collect();
    keyed.sort();

    let elided = items.len() < len || items.len() + keyed.len() > MAX_ITEMS;
    items.extend(
        keyed
            .into_iter()
            .take(MAX_ITEMS.saturating_sub(items.len()))
            .map(|(_, _, shown)| shown),
    );
    if elided {
        items.push("…".to_string());
    }
    format!("{{{}}}", items.join(", "))
}

/// Whether `raw_len`'s array part already rendered this key.
fn in_array_part(k: &mlua::Value, len: usize) -> bool {
    matches!(k, mlua::Value::Integer(i) if *i >= 1 && (*i as usize) <= len)
}

/// `name = v` when the key is a bare Lua identifier, `["a b"] = v` otherwise —
/// both are what you would type to build the table back.
fn key(k: &mlua::Value, depth: usize, seen: &mut Vec<*const c_void>) -> String {
    if let mlua::Value::String(s) = k {
        let text = s.to_string_lossy();
        let identifier = !text.is_empty()
            && !text.starts_with(|c: char| c.is_ascii_digit())
            && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if identifier {
            return text.to_string();
        }
    }
    format!("[{}]", element(k, depth + 1, seen))
}

#[cfg(test)]
mod tests {
    use super::render;
    use mlua::Lua;

    /// What `remuda -e <code>` would print, without a daemon in the way.
    fn shown(code: &str) -> String {
        let lua = Lua::new();
        let value: mlua::Value = lua.load(format!("return {code}")).eval().unwrap();
        render(&value)
    }

    #[test]
    fn scalars_keep_tostring_semantics() {
        assert_eq!(shown("nil"), "nil");
        assert_eq!(shown("true"), "true");
        assert_eq!(shown("42"), "42");
        assert_eq!(shown("'hello'"), "hello");
        assert_eq!(shown("print"), "<function>");
    }

    #[test]
    fn the_owners_table_shows_its_contents() {
        assert_eq!(
            shown("(function() local a = 2 return {a, 1, 'a', 1, \"a\", 2} end)()"),
            r#"{2, 1, "a", 1, "a", 2}"#
        );
    }

    #[test]
    fn a_string_is_quoted_so_it_cannot_be_read_as_a_number() {
        assert_eq!(shown("{1, '1'}"), r#"{1, "1"}"#);
    }

    #[test]
    fn keys_are_sorted_so_the_output_can_be_written_down() {
        // Insertion order differs from this on every run; the rendering must
        // not. Sorted by the key's own value, so bracketing does not reorder.
        assert_eq!(
            shown("{zulu = 1, alpha = 2, [3.5] = 3, ['two words'] = 4}"),
            r#"{[3.5] = 3, alpha = 2, ["two words"] = 4, zulu = 1}"#
        );
    }

    #[test]
    fn the_array_part_comes_first_in_index_order() {
        assert_eq!(shown("{10, 20, name = 'x'}"), r#"{10, 20, name = "x"}"#);
    }

    #[test]
    fn a_cycle_terminates() {
        assert_eq!(
            shown("(function() local t = {} t.self = t return t end)()"),
            "{self = <cycle>}"
        );
    }

    #[test]
    fn two_references_to_one_table_are_not_a_cycle() {
        // `seen` holds the path, not everything visited — a diamond is finite.
        assert_eq!(
            shown("(function() local leaf = {1} return {leaf, leaf} end)()"),
            "{{1}, {1}}"
        );
    }

    #[test]
    fn depth_is_capped() {
        assert_eq!(shown("{{{{{1}}}}}"), "{{{{{…}}}}}");
    }

    #[test]
    fn width_is_capped() {
        let out = shown("(function() local t = {} for i = 1, 100 do t[i] = i end return t end)()");
        assert!(out.starts_with("{1, 2, 3,"), "{out}");
        assert!(out.ends_with(", 32, …}"), "{out}");
    }

    #[test]
    fn an_empty_table_is_empty() {
        assert_eq!(shown("{}"), "{}");
    }
}
