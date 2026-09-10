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

    /// Evaluate `code` in the image and wait for the result. State persists
    /// between calls — a `-e`, a script and a REPL line are all doors into one
    /// interpreter, and a variable set by any of them outlives the call.
    pub fn eval(&self, code: &str, name: Option<&str>) -> Result<String, String> {
        let (reply, answer) = channel();
        self.jobs
            .send(Job {
                code: code.to_string(),
                name: name.map(str::to_string),
                reply,
            })
            .map_err(|_| "the image is not running".to_string())?;
        answer
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

/// One value as a Lua user expects to see it: `tostring` semantics, minus the
/// address on tables and functions — an address differs every run, so an
/// expected output could not be written down.
fn render(value: &mlua::Value) -> String {
    match value {
        mlua::Value::Nil => "nil".to_string(),
        mlua::Value::Boolean(b) => b.to_string(),
        mlua::Value::Integer(i) => i.to_string(),
        mlua::Value::Number(n) => n.to_string(),
        mlua::Value::String(s) => s.to_string_lossy().to_string(),
        other => format!("<{}>", other.type_name()),
    }
}
