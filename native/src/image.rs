//! The image: one Lua interpreter that lives as long as the daemon.
//!
//! 정수님, 2026-09-10: *"네, 이미지 기반이 그 의미라면, 이미지 기반이어야
//! 하겠습니다. 데몬이나 스탠드얼론이 떠있는 동안 수명을 함께 하는 lua 메인
//! 프로세스 혹은 쓰레드, 혹은 이벤트루프."* And, on what reaches it:
//! *"emacs는 scratch buffer, eval-buffer function 등을 제공합니다. 그
//! 오케스트레이터 런타임 안에서 어디서든 내부 런타임에 코드를 전달하여
//! 실행시킬 수 있습니다."*
//!
//! # Why a thread and a channel, and not a mutex around `Lua`
//!
//! `mlua::Lua` is `!Send` without the `send` feature — a Lua state is
//! single-threaded, and no lock changes that. So the state is *pinned* to one
//! thread that owns it outright, and every other thread reaches it by posting a
//! job and waiting for the answer. A thread serving a queue is an event loop,
//! which is the third of the three shapes 정수님 offered; they were never three
//! options, they were one answer seen from three sides. Emacs's command loop is
//! the same design for the same reason.
//!
//! # Where this deliberately differs from Emacs
//!
//! In Emacs, a long-running Lisp function freezes the editor: the one thread
//! that runs Lisp is also the one servicing everything else. We do not inherit
//! that, because sessions here are not Lua objects — they are Rust structs
//! behind their own locks, pumped by threads that never touch Lua. A script
//! that loops forever makes the *next Lua caller* wait, and nothing else: pty
//! output keeps being read, screens keep updating, `remuda ls` keeps answering.
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
    /// Start the interpreter and return a handle to it.
    ///
    /// `socket` is the daemon's own socket: the `remuda` table is bound exactly
    /// as `remuda run` binds it, so a name means the same thing typed at the
    /// CLI, written in a script file, or evaluated in here. That the calls go
    /// back out over the loopback socket rather than reaching the `Registry`
    /// directly is a deliberate first cut — it keeps one definition of the
    /// vocabulary instead of two that could drift, and it cannot deadlock
    /// because the daemon answers each connection on its own thread.
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

    /// Evaluate `code` in the image and wait for the result.
    ///
    /// State persists between calls — that is the whole point. A variable set
    /// by one `-e` is there for the next one, and for a script, and for the
    /// REPL, because all of them are doors into this one interpreter.
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

/// Evaluate one chunk, expression-first.
///
/// `return <code>` is tried before plain `<code>`, which is the technique the
/// reference `lua` interpreter's own REPL uses (`lua.c`, `addreturn`): it makes
/// `remuda -e "1+1"` print `2` while `remuda -e "x = 1"` still works, without
/// asking the user to know which of the two they typed. A statement simply
/// fails to compile as an expression and falls through.
///
/// Results are `tostring`ed and tab-joined, matching what `print` does with
/// multiple values — so `remuda -e "1, 2"` reads the way a Lua user expects.
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

/// Point `print` at a buffer instead of the daemon's stdout.
///
/// Measured, not foreseen (`steps/007-the-image.md` Actual): the daemon is
/// spawned with `Stdio::null()`, so a script's `print` went to `/dev/null` and
/// the caller saw nothing at all. `print` is the first thing anyone types into
/// a scratch buffer, and silently swallowing it is worse than not having one.
///
/// Lua's own `print` semantics are kept: values `tostring`ed, tab-separated,
/// newline at the end.
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
///
/// Both can be empty, and the common cases are exactly the ones that should
/// look clean — a statement that printed nothing returns an empty string, and
/// a `print`-only script returns just its output with no trailing blank.
fn join_output(printed: &str, value: &str) -> String {
    match (printed.trim_end_matches('\n'), value) {
        ("", value) => value.to_string(),
        (printed, "") => printed.to_string(),
        (printed, value) => format!("{printed}\n{value}"),
    }
}

/// One value as a Lua user expects to see it.
///
/// `tostring` semantics, minus the address on tables and functions: an address
/// is noise in a transcript and differs on every run, which makes an expected
/// output impossible to write down. `nil` and booleans print as themselves —
/// an earlier cut rendered them `<nil>` through a type-name fallback, which is
/// not Lua and reads like a placeholder that failed to fill in.
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
