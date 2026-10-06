//! The image: one Lua interpreter that lives as long as the daemon.
//!
//! # Why a thread and a channel, and not a mutex around `Lua`
//!
//! `mlua::Lua` is `!Send` without the `send` feature — a Lua state is
//! single-threaded, and no lock changes that. So the state is *pinned* to one
//! thread that owns it outright, and every other thread reaches it by posting a
//! job and waiting for the answer. A thread serving a queue is an event loop.
//!
//! Lua work is bounded by an instruction-count hook shared by the main state
//! and its coroutines. Rust calls and C-library functions run outside that
//! count; sessions remain Rust structs behind their own locks.
//!
//! ⚠ The corollary, named now so it is not discovered as a deadlock later: an
//! output pump may never *call into* Lua. If `on_output(session, fn)` is ever
//! added, the pump must post a job to this loop, not run the callback itself.

use crate::reply_limit::MAX_REPLY_BYTES;
use crate::script;
use mlua::debug::Debug;
use mlua::{HookTriggers, Lua, Thread, VmState};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

#[path = "timers.rs"]
pub(crate) mod timers;

// `Response::Error` remains a plain string on the wire. This reserved control
// prefix carries typed failures to the CLI without changing ordinary errors.
const TYPED_FAILURE_PREFIX: &str = "\u{1e}REMUDA_FAIL:";
const LUA_HOOK_INTERVAL: u32 = 10_000;
const MAX_TIMER_CALLBACKS_PER_TURN: usize = 1;
// About 0.6 seconds at the measured 360M instructions/second in release builds.
const LUA_INSTRUCTION_LIMIT: u64 = 200_000_000;
const LUA_EXECUTION_LIMIT_MESSAGE: &str = "Lua execution limit exceeded";

#[derive(Clone, Default)]
struct LuaExecutionBudget {
    depth: Rc<Cell<usize>>,
    instructions: Rc<Cell<u64>>,
    expired: Rc<Cell<bool>>,
    coroutines: Rc<RefCell<Vec<Thread>>>,
    coroutine_pointers: Rc<RefCell<HashSet<usize>>>,
}

struct LuaExecutionGuard<'lua> {
    lua: &'lua Lua,
    depth: Rc<Cell<usize>>,
    instructions: Rc<Cell<u64>>,
    expired: Rc<Cell<bool>>,
    coroutines: Rc<RefCell<Vec<Thread>>>,
    coroutine_pointers: Rc<RefCell<HashSet<usize>>>,
}

impl Drop for LuaExecutionGuard<'_> {
    fn drop(&mut self) {
        let depth = self.depth.get().saturating_sub(1);
        self.depth.set(depth);
        if depth == 0 {
            self.lua.remove_hook();
            for thread in self.coroutines.borrow().iter() {
                thread.remove_hook();
            }
            self.coroutines.borrow_mut().clear();
            self.coroutine_pointers.borrow_mut().clear();
            self.instructions.set(0);
            self.expired.set(false);
        }
    }
}

impl LuaExecutionBudget {
    fn hook(&self) -> impl Fn(&Lua, &Debug) -> mlua::Result<VmState> + 'static {
        let instructions = Rc::clone(&self.instructions);
        let expired = Rc::clone(&self.expired);
        move |_, _| {
            advance_budget(&instructions, &expired, u64::from(LUA_HOOK_INTERVAL))?;
            Ok(VmState::Continue)
        }
    }

    fn install_thread(&self, thread: Thread) -> mlua::Result<()> {
        let pointer = thread.to_pointer() as usize;
        let mut pointers = self.coroutine_pointers.borrow_mut();
        if !pointers.insert(pointer) {
            return Ok(());
        }
        drop(pointers);
        if let Err(error) = thread.set_hook(
            HookTriggers::new().every_nth_instruction(LUA_HOOK_INTERVAL),
            self.hook(),
        ) {
            self.coroutine_pointers.borrow_mut().remove(&pointer);
            return Err(error);
        }
        self.coroutines.borrow_mut().push(thread);
        Ok(())
    }

    fn charge_coroutine(&self) -> mlua::Result<()> {
        advance_budget(
            &self.instructions,
            &self.expired,
            u64::from(LUA_HOOK_INTERVAL),
        )
    }

    /// One outer instruction budget covers nested execution and all coroutines.
    /// Its guard removes every hook after success, error, or unwinding.
    fn run<T, E: ToString>(
        &self,
        lua: &Lua,
        operation: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, String> {
        let outermost = self.depth.get() == 0;
        if outermost {
            self.instructions.set(0);
            self.expired.set(false);
            let result = lua.set_hook(
                HookTriggers::new().every_nth_instruction(LUA_HOOK_INTERVAL),
                self.hook(),
            );
            if let Err(error) = result {
                return Err(error.to_string());
            }
        }
        self.depth.set(self.depth.get() + 1);
        let _guard = LuaExecutionGuard {
            lua,
            depth: Rc::clone(&self.depth),
            instructions: Rc::clone(&self.instructions),
            expired: Rc::clone(&self.expired),
            coroutines: Rc::clone(&self.coroutines),
            coroutine_pointers: Rc::clone(&self.coroutine_pointers),
        };
        let result = operation();
        if self.expired.get() {
            Err(LUA_EXECUTION_LIMIT_MESSAGE.into())
        } else {
            result.map_err(|error| error.to_string())
        }
    }
}

fn advance_budget(instructions: &Cell<u64>, expired: &Cell<bool>, amount: u64) -> mlua::Result<()> {
    let count = instructions.get().saturating_add(amount);
    instructions.set(count);
    if count >= LUA_INSTRUCTION_LIMIT {
        expired.set(true);
        Err(mlua::Error::RuntimeError(
            LUA_EXECUTION_LIMIT_MESSAGE.into(),
        ))
    } else {
        Ok(())
    }
}

/// A deliberate CLI failure raised by Lua code with `remuda.fail`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedFailure {
    pub message: String,
    pub code: u8,
}

impl TypedFailure {
    fn wire_message(&self) -> String {
        format!("{TYPED_FAILURE_PREFIX}{}\n{}", self.code, self.message)
    }
}

impl fmt::Display for TypedFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for TypedFailure {}

#[derive(Debug)]
struct CapturedPrintLimit {
    size: usize,
}

impl fmt::Display for CapturedPrintLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "captured print output exceeds the {} MiB output limit (at least {} bytes)",
            MAX_REPLY_BYTES / (1024 * 1024),
            self.size
        )
    }
}

impl Error for CapturedPrintLimit {}

/// Read a typed failure encoded by the image, if the response carries one.
pub fn typed_failure_message(value: &str) -> Option<(u8, &str)> {
    let value = value.strip_prefix(TYPED_FAILURE_PREFIX)?;
    let (code, message) = value.split_once('\n')?;
    let code = code.parse::<u8>().ok()?;
    (code != 0).then_some((code, message))
}

/// One unit of work for the image: source to evaluate, and where the answer
/// goes. The reply channel is per-job rather than shared, so two callers
/// waiting at once cannot receive each other's result.
struct Job {
    kind: JobKind,
    reply: Option<Sender<Result<String, String>>>,
}

enum JobKind {
    Eval {
        code: String,
        name: Option<String>,
        allow_pending: bool,
        caller: CallerContext,
    },
    StopModules,
    RunSchedules {
        now: f64,
    },
    HttpComplete {
        id: u64,
        result: Result<crate::net::HttpResponse, String>,
    },
    PeerCertificateComplete {
        id: u64,
        result: Result<crate::net::http_client::PeerCertificate, String>,
    },
    SessionOutput(SessionOutputNotifier),
    SessionOutputFlush(SessionOutputNotifier),
    #[cfg(test)]
    StopImage,
}

/// How much the daemon can say about the process that submitted this Eval.
#[derive(Clone, Debug)]
pub(crate) struct CallerContext {
    pub kind: CallerKind,
    pub session: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) enum CallerKind {
    Session,
    Outside,
    Unknown,
}

impl Default for CallerContext {
    fn default() -> Self {
        Self {
            kind: CallerKind::Unknown,
            session: None,
        }
    }
}

struct SessionOutputState {
    name: String,
    latest_version: AtomicU64,
    delivered_version: AtomicU64,
    queued: AtomicBool,
    accepting: AtomicBool,
    monitor_finished: Mutex<bool>,
    monitor_finished_cv: Condvar,
}

/// A coalescing wake for one session's output hook.
#[derive(Clone)]
pub struct SessionOutputNotifier {
    jobs: Sender<Job>,
    state: Arc<SessionOutputState>,
}

impl SessionOutputNotifier {
    pub fn notify(&self, version: u64) {
        if !self.state.accepting.load(Ordering::Acquire) {
            return;
        }
        self.state.latest_version.store(version, Ordering::Release);
        self.enqueue();
    }

    pub fn deactivate(&self) {
        self.state.accepting.store(false, Ordering::Release);
    }

    /// Queue the final generation before the monitor is marked finished. The
    /// caller may itself be a Lua request, so waiting here would deadlock the
    /// image worker; the exit notification is queued after this flush instead.
    pub fn flush(&self, version: u64) {
        self.state.latest_version.store(version, Ordering::Release);
        self.deactivate();
        let _ = self.jobs.send(Job {
            kind: JobKind::SessionOutputFlush(self.clone()),
            reply: None,
        });
    }

    pub fn finish_monitor(&self) {
        if let Ok(mut finished) = self.state.monitor_finished.lock() {
            *finished = true;
            self.state.monitor_finished_cv.notify_all();
        }
    }

    /// Bounded: ConPTY can hold a dead session's output pipe open, and the
    /// caller may be the image thread itself (`remuda.ls()` reaps there).
    fn wait_monitor(&self) {
        if let Ok(finished) = self.state.monitor_finished.lock() {
            let _ = self.state.monitor_finished_cv.wait_timeout_while(
                finished,
                crate::pty::PTY_WRITE_TIMEOUT,
                |finished| !*finished,
            );
        }
    }

    fn enqueue(&self) {
        if !self.state.accepting.load(Ordering::Acquire)
            || self
                .state
                .queued
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        if self
            .jobs
            .send(Job {
                kind: JobKind::SessionOutput(self.clone()),
                reply: None,
            })
            .is_err()
        {
            self.state.accepting.store(false, Ordering::Release);
            self.state.queued.store(false, Ordering::Release);
        }
    }

    fn finish(&self, delivered_version: u64) {
        self.state.queued.store(false, Ordering::Release);
        if self.state.accepting.load(Ordering::Acquire)
            && self.state.latest_version.load(Ordering::Acquire) != delivered_version
        {
            self.enqueue();
        }
    }
}

fn deliver_session_output(lua: &Lua, notifier: &SessionOutputNotifier, version: u64) {
    if version <= notifier.state.delivered_version.load(Ordering::Acquire) {
        return;
    }
    let code = format!(
        "remuda.emit('session_output', {}, {{version={version}}})",
        crate::mcp::lua_string(&notifier.state.name)
    );
    if let Err(error) = eval(lua, &code, None) {
        eprintln!("remuda session_output hook error: {error}");
    }
    notifier
        .state
        .delivered_version
        .store(version, Ordering::Release);
}

/// A handle to the daemon's Lua image. Cloneable and `Send`; the interpreter
/// itself never leaves its thread.
#[derive(Clone)]
pub struct Image {
    jobs: Sender<Job>,
    pending: crate::pending::PendingReplies,
    http: crate::net::HttpClient,
    session_outputs: Arc<Mutex<HashMap<String, SessionOutputNotifier>>>,
}

impl Image {
    /// Start the interpreter and return a handle to it. `socket` is the
    /// daemon's own, for bindings that act on a session by name (attach,
    /// capture, ...).
    // `registry` lets a read-only binding like `ls` answer in-process
    // instead of looping a request back over that same socket. `counters`
    // is the daemon's own named `Counters`, shared with `Ticker`.
    pub fn spawn(
        socket: &Path,
        registry: Arc<remuda_core::Registry>,
        counters: Arc<crate::tick::Counters>,
    ) -> Self {
        let (jobs, inbox) = channel::<Job>();
        let socket: PathBuf = socket.to_path_buf();
        let image = Self {
            jobs,
            pending: crate::pending::PendingReplies::default(),
            http: crate::net::HttpClient::default(),
            session_outputs: Arc::new(Mutex::new(HashMap::new())),
        };
        let handle = image.clone();

        std::thread::spawn(move || {
            let lua = Lua::new();
            let caller = Rc::new(RefCell::new(CallerContext::default()));
            let timers = Rc::new(RefCell::new(timers::TimerService::new()));
            let budget = LuaExecutionBudget::default();
            // Everything `print` writes during one job, so it can travel back
            // to whoever asked instead of vanishing. `Rc` rather than `Arc`
            // because this never leaves the thread — the whole reason the
            // interpreter is pinned here.
            let printed = Rc::new(RefCell::new(String::new()));

            // A failure here means no image at all, so every eval must say so
            // rather than the thread dying quietly and every caller hanging.
            let ready = budget
                .run(&lua, || {
                    script::bindings(
                        &lua,
                        &socket,
                        registry,
                        counters,
                        handle.clone(),
                        Rc::clone(&caller),
                        Rc::clone(&timers),
                    )
                    .and_then(|table| lua.globals().set("remuda", table))
                    .and_then(|()| install_execution_guards(&lua, budget.clone()))
                    // The tool frame loads after bindings and guard helpers,
                    // before any caller can access it.
                    .and_then(|()| {
                        lua.load(include_str!("tools.lua"))
                            .set_name("@remuda/tools.lua")
                            .exec()
                    })
                    .and_then(|()| remove_execution_guard_helpers(&lua))
                    .and_then(|()| script::hide_module_activator(&lua))
                    .and_then(|()| capture_print(&lua, Rc::clone(&printed)))
                })
                .map_err(|e| e.to_string());

            loop {
                let timeout = timers.borrow_mut().wait_timeout();
                let received = match timeout {
                    Some(timeout) => inbox.recv_timeout(timeout),
                    None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
                };
                match received {
                    Ok(job) => {
                        printed.borrow_mut().clear();
                        #[cfg(test)]
                        let stop_image = matches!(job.kind, JobKind::StopImage);
                        #[cfg(not(test))]
                        let stop_image = false;
                        let answer =
                            process_job(&lua, &budget, &handle, &printed, &ready, &job, &caller);
                        // A caller that gave up and dropped its receiver is not
                        // an error: `remuda -e` can be Ctrl-C'd mid-evaluation,
                        // and the work still ran.
                        if let Some(reply) = job.reply {
                            let _ = reply.send(answer);
                        }
                        if !stop_image {
                            run_due_timers(&lua, &budget, &timers);
                        }
                        if stop_image {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        run_due_timers(&lua, &budget, &timers);
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        });

        image
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
                kind: JobKind::Eval {
                    code: code.to_string(),
                    name: name.map(str::to_string),
                    allow_pending: false,
                    caller: CallerContext::default(),
                },
                reply: Some(reply),
            })
            .map_err(|_| "the image is not running".to_string())?;
        Ok(answer)
    }

    /// Queue a native ticker pass so each due schedule receives its own Lua
    /// instruction budget instead of sharing the entire tick's allowance.
    pub fn submit_due_schedules(
        &self,
        now: f64,
    ) -> Result<std::sync::mpsc::Receiver<Result<String, String>>, String> {
        let (reply, answer) = channel();
        self.jobs
            .send(Job {
                kind: JobKind::RunSchedules { now },
                reply: Some(reply),
            })
            .map_err(|_| "the image is not running".to_string())?;
        Ok(answer)
    }

    pub fn pending_replies(&self) -> crate::pending::PendingReplies {
        self.pending.clone()
    }

    pub fn session_output_notifier(&self, name: &str, id: &str) -> SessionOutputNotifier {
        let notifier = SessionOutputNotifier {
            jobs: self.jobs.clone(),
            state: Arc::new(SessionOutputState {
                name: name.to_string(),
                latest_version: AtomicU64::new(0),
                delivered_version: AtomicU64::new(0),
                queued: AtomicBool::new(false),
                accepting: AtomicBool::new(true),
                monitor_finished: Mutex::new(false),
                monitor_finished_cv: Condvar::new(),
            }),
        };
        if let Ok(mut outputs) = self.session_outputs.lock() {
            outputs.insert(id.to_string(), notifier.clone());
        }
        notifier
    }

    /// Wait for the output monitor to flush its final event before announcing
    /// that this session exited; FIFO submission then preserves event order.
    pub fn wait_session_output_monitor(&self, id: &str) {
        let notifier = self
            .session_outputs
            .lock()
            .ok()
            .and_then(|outputs| outputs.get(id).cloned());
        if let Some(notifier) = notifier {
            notifier.wait_monitor();
            if let Ok(mut outputs) = self.session_outputs.lock() {
                if outputs
                    .get(id)
                    .is_some_and(|current| Arc::ptr_eq(&current.state, &notifier.state))
                {
                    outputs.remove(id);
                }
            }
        }
    }

    pub fn discard_session_output_monitor(&self, id: &str, notifier: &SessionOutputNotifier) {
        notifier.deactivate();
        notifier.finish_monitor();
        if let Ok(mut outputs) = self.session_outputs.lock() {
            if outputs
                .get(id)
                .is_some_and(|current| Arc::ptr_eq(&current.state, &notifier.state))
            {
                outputs.remove(id);
            }
        }
    }

    /// Notify every deferred caller before modules and the Lua runtime stop.
    pub fn shutdown_pending_replies(&self) {
        self.pending.shutdown();
        const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
        if let Ok(answer) = self.submit("__remuda_pending_tick()", Some("@remuda/pending-shutdown"))
        {
            let _ = answer.recv_timeout(DRAIN_TIMEOUT);
        }
    }

    #[cfg(test)]
    pub(crate) fn stop_for_test(&self) {
        let (reply, answer) = channel();
        if self
            .jobs
            .send(Job {
                kind: JobKind::StopImage,
                reply: Some(reply),
            })
            .is_ok()
        {
            let _ = answer.recv_timeout(std::time::Duration::from_secs(2));
        }
    }

    pub fn start_http(&self, request: crate::net::HttpRequest) -> crate::net::HttpTask {
        let jobs = self.jobs.clone();
        self.http.start(request, move |id, result| {
            let _ = jobs.send(Job {
                kind: JobKind::HttpComplete { id, result },
                reply: None,
            });
        })
    }

    pub fn start_peer_certificate(&self, request: crate::net::HttpRequest) -> crate::net::HttpTask {
        let jobs = self.jobs.clone();
        self.http
            .start_peer_certificate(request, move |id, result| {
                let _ = jobs.send(Job {
                    kind: JobKind::PeerCertificateComplete { id, result },
                    reply: None,
                });
            })
    }

    /// Evaluate `code` in the image and wait for the result. State persists
    /// between calls — a `-e`, a script and a REPL line are all doors into one
    /// interpreter, and a variable set by any of them outlives the call.
    pub fn eval(&self, code: &str, name: Option<&str>) -> Result<String, String> {
        self.submit(code, name)?
            .recv()
            .map_err(|_| "the image stopped without answering".to_string())?
    }

    pub(crate) fn eval_request(
        &self,
        code: &str,
        name: Option<&str>,
        caller: CallerContext,
    ) -> Result<String, String> {
        let (reply, answer) = channel();
        self.jobs
            .send(Job {
                kind: JobKind::Eval {
                    code: code.to_string(),
                    name: name.map(str::to_string),
                    allow_pending: true,
                    caller,
                },
                reply: Some(reply),
            })
            .map_err(|_| "the image is not running".to_string())?;
        answer
            .recv()
            .map_err(|_| "the image stopped without answering".to_string())?
    }

    /// Best-effort module cleanup for clean daemon shutdown. A stuck user
    /// callback must not hold shutdown indefinitely.
    pub fn stop_modules_bounded(&self) {
        const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
        let (reply, answer) = channel();
        if self
            .jobs
            .send(Job {
                kind: JobKind::StopModules,
                reply: Some(reply),
            })
            .is_ok()
        {
            let _ = answer.recv_timeout(STOP_TIMEOUT);
        }
    }
}

fn process_job(
    lua: &Lua,
    budget: &LuaExecutionBudget,
    handle: &Image,
    printed: &Rc<RefCell<String>>,
    ready: &Result<(), String>,
    job: &Job,
    caller: &Rc<RefCell<CallerContext>>,
) -> Result<String, String> {
    match ready {
        Err(why) => Err(format!("image failed to start: {why}")),
        Ok(()) => match &job.kind {
            JobKind::Eval {
                code,
                name,
                allow_pending,
                caller: request_caller,
            } => {
                *caller.borrow_mut() = request_caller.clone();
                handle.pending.begin_eval();
                let answer = budget
                    .run(lua, || eval(lua, code, name.as_deref()))
                    .and_then(|value| {
                        if handle.pending.pending_id(&value).is_some() {
                            Ok(value)
                        } else {
                            join_output(&printed.borrow(), value)
                        }
                    });
                let pending_id = answer
                    .as_ref()
                    .ok()
                    .and_then(|value| handle.pending.pending_id(value));
                // Keep daemon-derived process identity scoped to this one Eval;
                // schedules, module lifecycle hooks, and native jobs stay unknown.
                *caller.borrow_mut() = CallerContext::default();
                if pending_id.is_some() && !allow_pending {
                    handle.pending.finish_eval(None);
                    Err("pending replies may only be returned from a daemon request".into())
                } else {
                    handle.pending.finish_eval(pending_id);
                    answer
                }
            }
            JobKind::StopModules => {
                *caller.borrow_mut() = CallerContext::default();
                budget.run(lua, || script::stop_modules(lua).map(|()| String::new()))
            }
            JobKind::RunSchedules { now } => {
                *caller.borrow_mut() = CallerContext::default();
                run_due_schedules(lua, budget, *now)
            }
            JobKind::HttpComplete { id, result } => {
                *caller.borrow_mut() = CallerContext::default();
                if let Err(error) = budget.run(lua, || deliver_http(lua, *id, result.clone())) {
                    eprintln!("remuda: HTTP callback delivery failed: {error}");
                }
                Ok(String::new())
            }
            JobKind::PeerCertificateComplete { id, result } => {
                *caller.borrow_mut() = CallerContext::default();
                if let Err(error) =
                    budget.run(lua, || deliver_peer_certificate(lua, *id, result.clone()))
                {
                    eprintln!("remuda: peer certificate callback delivery failed: {error}");
                }
                Ok(String::new())
            }
            JobKind::SessionOutput(notifier) => {
                *caller.borrow_mut() = CallerContext::default();
                let version = notifier.state.latest_version.load(Ordering::Acquire);
                let _ = budget.run(lua, || -> mlua::Result<()> {
                    deliver_session_output(lua, notifier, version);
                    Ok(())
                });
                notifier.finish(version);
                Ok(String::new())
            }
            JobKind::SessionOutputFlush(notifier) => {
                *caller.borrow_mut() = CallerContext::default();
                let version = notifier.state.latest_version.load(Ordering::Acquire);
                let _ = budget.run(lua, || -> mlua::Result<()> {
                    deliver_session_output(lua, notifier, version);
                    Ok(())
                });
                Ok(String::new())
            }
            #[cfg(test)]
            JobKind::StopImage => {
                *caller.borrow_mut() = CallerContext::default();
                Ok(String::new())
            }
        },
    }
}

fn run_due_schedules(lua: &Lua, budget: &LuaExecutionBudget, now: f64) -> Result<String, String> {
    let due = budget.run(lua, || {
        let remuda: mlua::Table = lua.globals().get("remuda")?;
        let take_due: mlua::Function = remuda.get("_take_due_schedules")?;
        take_due.call::<mlua::Table>(now)
    })?;
    for entry in due.sequence_values::<mlua::Table>() {
        let entry = entry.map_err(|error| error.to_string())?;
        let name: String = entry.get("name").map_err(|error| error.to_string())?;
        let handle: mlua::Table = entry.get("handle").map_err(|error| error.to_string())?;
        let callback_result = budget.run(lua, || {
            let remuda: mlua::Table = lua.globals().get("remuda")?;
            let run_schedule: mlua::Function = remuda.get("_run_schedule")?;
            run_schedule.call::<bool>(handle)
        });
        if let Err(error) = callback_result {
            eprintln!("remuda schedule error for {name}: {error}");
        }
    }
    Ok(String::new())
}

fn run_due_timers(lua: &Lua, budget: &LuaExecutionBudget, timers: &timers::SharedTimerService) {
    // Return to the image inbox between callbacks so an overdue timer batch
    // cannot keep ordinary work queued behind every due callback.
    for _ in 0..MAX_TIMER_CALLBACKS_PER_TURN {
        let fire = match timers.borrow_mut().take_due(lua, Instant::now()) {
            Ok(Some(fire)) => fire,
            Ok(None) => return,
            Err(error) => {
                eprintln!("remuda timer dequeue error: {error}");
                return;
            }
        };
        let callback_result = budget.run(lua, || fire.callback.call::<()>(()));
        if let Err(error) = callback_result {
            eprintln!("remuda timer callback error: {error}");
        }
        if fire.repeating {
            timers.borrow_mut().finish_fire(fire.id, Instant::now());
        }
    }
}

fn install_execution_guards(lua: &Lua, budget: LuaExecutionBudget) -> mlua::Result<()> {
    let expired = budget.clone();
    lua.globals().set(
        "__remuda_budget_expired",
        lua.create_function(move |_, ()| Ok(expired.expired.get()))?,
    )?;
    let coroutine_budget = budget.clone();
    lua.globals().set(
        "__remuda_install_coroutine_hook",
        lua.create_function(move |_, thread: Thread| coroutine_budget.install_thread(thread))?,
    )?;
    let charge_budget = budget.clone();
    lua.globals().set(
        "__remuda_charge_coroutine",
        lua.create_function(move |_, ()| charge_budget.charge_coroutine())?,
    )?;
    lua.load(
        r#"
        local budget_expired = __remuda_budget_expired
        local install_coroutine_hook = __remuda_install_coroutine_hook
        local charge_coroutine = __remuda_charge_coroutine
        local pack, unpack = table.pack, table.unpack
        local function rethrow_if_expired()
          if budget_expired() then error("Lua execution limit exceeded", 0) end
        end

        local raw_pcall, raw_xpcall = pcall, xpcall
        pcall = function(...)
          local result = pack(raw_pcall(...))
          rethrow_if_expired()
          return unpack(result, 1, result.n)
        end
        xpcall = function(fn, handler, ...)
          if type(handler) ~= "function" then
            local result = pack(raw_xpcall(fn, handler, ...))
            rethrow_if_expired()
            return unpack(result, 1, result.n)
          end
          local function guarded_handler(err)
            local handled = pack(raw_pcall(handler, err))
            rethrow_if_expired()
            if not handled[1] then error(handled[2], 0) end
            return unpack(handled, 2, handled.n)
          end
          local result = pack(raw_xpcall(fn, guarded_handler, ...))
          rethrow_if_expired()
          return unpack(result, 1, result.n)
        end

        local co = coroutine
        local raw_create, raw_resume = co.create, co.resume
        co.create = function(fn)
          charge_coroutine()
          return raw_create(fn)
        end
        co.resume = function(thread, ...)
          charge_coroutine()
          install_coroutine_hook(thread)
          local result = pack(raw_resume(thread, ...))
          if not result[1] then rethrow_if_expired() end
          return unpack(result, 1, result.n)
        end
        co.wrap = function(fn)
          local thread = co.create(fn)
          return function(...)
            local result = pack(co.resume(thread, ...))
            if not result[1] then error(result[2], 0) end
            return unpack(result, 2, result.n)
          end
        end
        "#,
    )
    .exec()?;
    Ok(())
}

fn remove_execution_guard_helpers(lua: &Lua) -> mlua::Result<()> {
    lua.globals()
        .set("__remuda_budget_expired", mlua::Value::Nil)?;
    lua.globals()
        .set("__remuda_install_coroutine_hook", mlua::Value::Nil)?;
    lua.globals()
        .set("__remuda_charge_coroutine", mlua::Value::Nil)?;
    Ok(())
}

fn deliver_peer_certificate(
    lua: &Lua,
    id: u64,
    result: Result<crate::net::http_client::PeerCertificate, String>,
) -> mlua::Result<()> {
    let key = format!("remuda.http.callback.{id}");
    let callback: mlua::Function = lua.named_registry_value(&key)?;
    lua.set_named_registry_value(&key, mlua::Value::Nil)?;
    let value = lua.create_table()?;
    match result {
        Ok(peer) => {
            value.set("sha256", peer.sha256)?;
            value.set("spki_sha256", peer.spki_sha256)?;
            value.set("not_before", peer.not_before)?;
            value.set("not_after", peer.not_after)?;
            value.set("trusted", peer.trusted)?;
            if let Some(reason) = peer.reason {
                value.set("reason", reason)?;
            }
        }
        Err(error) => value.set("error", error)?,
    }
    callback.call::<()>(value)
}

fn deliver_http(
    lua: &Lua,
    id: u64,
    result: Result<crate::net::HttpResponse, String>,
) -> mlua::Result<()> {
    let key = format!("remuda.http.callback.{id}");
    let callback: mlua::Function = lua.named_registry_value(&key)?;
    lua.set_named_registry_value(&key, mlua::Value::Nil)?;
    let value = lua.create_table()?;
    match result {
        Ok(response) => {
            value.set("status", response.status)?;
            let headers = lua.create_table()?;
            for (name, values) in response.headers {
                if values.len() == 1 {
                    headers.set(name, lua.create_string(&values[0])?)?;
                } else {
                    let array = lua.create_table()?;
                    for (i, entry) in values.iter().enumerate() {
                        array.set(i + 1, lua.create_string(entry)?)?;
                    }
                    headers.set(name, array)?;
                }
            }
            value.set("headers", headers)?;
            value.set("body", lua.create_string(&response.body)?)?;
            if let Some(peer) = response.peer_certificate {
                let certificate = lua.create_table()?;
                certificate.set("sha256", peer.sha256)?;
                certificate.set("spki_sha256", peer.spki_sha256)?;
                certificate.set("not_before", peer.not_before)?;
                certificate.set("not_after", peer.not_after)?;
                certificate.set("trusted", peer.trusted)?;
                value.set("peer_certificate", certificate)?;
            }
        }
        Err(error) => value.set("error", error)?,
    }
    callback.call::<()>(value)
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

    // Only a compile failure means "not an expression" — a runtime error in
    // an expression must not run the code a second time as statements (#128).
    let function = match lua
        .load(format!("return {code}"))
        .set_name(&chunk)
        .into_function()
    {
        Ok(function) => function,
        // Not an expression. Run it as statements, and report *that* error if
        // it fails — reporting the expression-parse error instead would name a
        // `return` the user never wrote, which is the single most confusing
        // thing this function could do.
        Err(_) => lua
            .load(code)
            .set_name(&chunk)
            .into_function()
            .map_err(|e| e.to_string())?,
    };
    let values = function.call::<mlua::MultiValue>(()).map_err(|error| {
        error
            .downcast_ref::<CapturedPrintLimit>()
            .map(ToString::to_string)
            .or_else(|| {
                error
                    .downcast_ref::<TypedFailure>()
                    .map(TypedFailure::wire_message)
            })
            .unwrap_or_else(|| error.to_string())
    })?;

    if values.len() == 1 {
        if let Some(value) = values.iter().next() {
            if let Some(marker) = pending_marker(value) {
                return Ok(marker);
            }
        }
    }

    let mut rendered = String::new();
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            ensure_reply_size(rendered.len().saturating_add(1))?;
            rendered.push('\t');
        }
        let remaining = MAX_REPLY_BYTES.saturating_sub(rendered.len());
        rendered.push_str(&render_for_reply(value, remaining)?);
    }
    Ok(rendered)
}

fn pending_marker(value: &mlua::Value) -> Option<String> {
    match value {
        mlua::Value::UserData(data) => data
            .borrow::<crate::pending::PendingHandle>()
            .ok()
            .map(|handle| handle.marker().to_string()),
        mlua::Value::Table(table) => {
            let handle = match table
                .raw_get::<mlua::Value>("__remuda_pending_handle")
                .ok()?
            {
                mlua::Value::UserData(data) => data,
                _ => return None,
            };
            handle
                .borrow::<crate::pending::PendingHandle>()
                .ok()
                .map(|handle| handle.marker().to_string())
        }
        _ => None,
    }
}

/// Point `print` at a buffer instead of the daemon's stdout, which is
/// `Stdio::null()` — without this a script's `print` vanishes and the caller
/// sees nothing. Lua semantics kept: `tostring`ed, tab-separated, newline.
fn capture_print(lua: &Lua, into: Rc<RefCell<String>>) -> mlua::Result<()> {
    let print = lua.create_function(move |_, values: mlua::MultiValue| {
        let mut buffer = into.borrow_mut();
        let mut line = String::new();
        for (index, value) in values.iter().enumerate() {
            if index != 0 {
                let next = buffer.len().saturating_add(line.len()).saturating_add(2);
                ensure_print_reply_size(next)?;
                line.push('\t');
            }
            let used = buffer.len().saturating_add(line.len()).saturating_add(1);
            let remaining = MAX_REPLY_BYTES.saturating_sub(used);
            match render_for_reply(value, remaining) {
                Ok(rendered) => line.push_str(&rendered),
                Err(_) => {
                    let value_len = match value {
                        mlua::Value::String(value) => value.to_string_lossy().len(),
                        _ => render(value).len(),
                    };
                    return Err(mlua::Error::external(CapturedPrintLimit {
                        size: buffer
                            .len()
                            .saturating_add(line.len())
                            .saturating_add(value_len)
                            .saturating_add(1),
                    }));
                }
            }
        }
        let next = buffer.len().saturating_add(line.len()).saturating_add(1);
        ensure_print_reply_size(next)?;
        buffer.push_str(&line);
        buffer.push('\n');
        Ok(())
    })?;
    lua.globals().set("print", print)
}

fn ensure_print_reply_size(size: usize) -> mlua::Result<()> {
    if size > MAX_REPLY_BYTES {
        Err(mlua::Error::external(CapturedPrintLimit { size }))
    } else {
        Ok(())
    }
}

/// What the caller sees: anything printed, then whatever the chunk came to.
/// Either half may be empty, and neither leaves a stray blank line behind.
fn join_output(printed: &str, value: String) -> Result<String, String> {
    match (printed.trim_end_matches('\n'), value) {
        ("", value) => Ok(value),
        (printed, value) if value.is_empty() => {
            ensure_reply_size(printed.len())?;
            Ok(printed.to_string())
        }
        (printed, value) => {
            let size = printed.len().saturating_add(value.len()).saturating_add(1);
            ensure_reply_size(size)?;
            Ok(format!("{printed}\n{value}"))
        }
    }
}

fn render_for_reply(value: &mlua::Value, remaining: usize) -> Result<String, String> {
    if let mlua::Value::String(string) = value {
        let bytes = string.as_bytes();
        ensure_reply_size(bytes.len())?;
        if bytes.len() > remaining {
            return Err(reply_limit_error(bytes.len()));
        }
        let rendered = string.to_string_lossy();
        ensure_reply_size(rendered.len())?;
        if rendered.len() > remaining {
            return Err(reply_limit_error(rendered.len()));
        }
        return Ok(rendered);
    }

    let rendered = render(value);
    ensure_reply_size(rendered.len())?;
    if rendered.len() > remaining {
        return Err(reply_limit_error(rendered.len()));
    }
    Ok(rendered)
}

fn ensure_reply_size(size: usize) -> Result<(), String> {
    if size > MAX_REPLY_BYTES {
        Err(reply_limit_error(size))
    } else {
        Ok(())
    }
}

fn reply_limit_error(size: usize) -> String {
    format!(
        "synchronous reply exceeds the {} MiB output limit (at least {size} bytes)",
        MAX_REPLY_BYTES / (1024 * 1024)
    )
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
    use super::{capture_print, eval, render};
    use mlua::Lua;
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::reply_limit::MAX_REPLY_BYTES;

    fn lifecycle_lua() -> Lua {
        let lua = Lua::new();
        let remuda = lua.create_table().unwrap();
        remuda
            .set("_registry", lua.create_table().unwrap())
            .unwrap();
        lua.globals().set("remuda", remuda).unwrap();
        lua.load(include_str!("tools.lua")).exec().unwrap();
        lua
    }

    #[test]
    fn prelude_loads_without_the_native_cli_binding() {
        let lua = lifecycle_lua();
        let cli: mlua::Value = lua.load("return remuda.cli").eval().unwrap();
        assert!(cli.is_nil(), "harness must stay cli-less");
        let reg: mlua::Value = lua
            .load("return remuda._registry['cli.require']")
            .eval()
            .unwrap();
        assert!(reg.is_nil(), "cli.require exists only with the cli binding");
    }

    /// What `remuda -e <code>` would print, without a daemon in the way.
    fn shown(code: &str) -> String {
        let lua = Lua::new();
        let value: mlua::Value = lua.load(format!("return {code}")).eval().unwrap();
        render(&value)
    }

    #[test]
    fn oversized_returned_lua_string_is_rejected() {
        let lua = Lua::new();
        let result = eval(
            &lua,
            &format!("string.rep('x', {})", MAX_REPLY_BYTES + 1),
            None,
        );
        assert!(matches!(result, Err(error) if error.contains("16 MiB output limit")));
    }

    #[test]
    fn oversized_print_is_rejected_without_growing_the_print_buffer() {
        let lua = Lua::new();
        let printed = Rc::new(RefCell::new(String::new()));
        capture_print(&lua, Rc::clone(&printed)).unwrap();
        let result = eval(
            &lua,
            &format!(
                "print(string.rep('x', {})); print('y')",
                MAX_REPLY_BYTES - 1
            ),
            None,
        );

        let error = result.unwrap_err();
        assert_eq!(
            error,
            format!(
                "captured print output exceeds the 16 MiB output limit (at least {} bytes)",
                MAX_REPLY_BYTES + 2
            )
        );
        assert!(!error.contains("runtime error:"));
        assert!(!error.contains("stack traceback:"));
        assert_eq!(printed.borrow().len(), MAX_REPLY_BYTES);
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
    fn module_reload_replaces_owned_hooks_and_tools_but_keeps_initialized_state() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            local initialized = 0
            local function declaration(step, tool_name)
              return {
                api = "remuda-module-v1",
                state_version = 1,
                initialize = function()
                  initialized = initialized + 1
                  return { count = 0 }
                end,
                hooks = {{ event = "probe", run = function(state)
                  state.count = state.count + step
                end }},
                tools = {{
                  name = tool_name,
                  about = "A sufficiently long description for this test tool.",
                  run = function(state) return tostring(state.count) end,
                }},
              }
            end
            assert(remuda._activate_module("sample", declaration(1, "sample_old")))
            remuda.emit("probe")
            assert(remuda.tools.sample_old() == "1")
            assert(remuda._activate_module("sample", declaration(10, "sample_new")))
            remuda.emit("probe")
            assert(initialized == 1)
            assert(remuda.tools.sample_old == nil)
            assert(remuda.tools.sample_new() == "11")
            assert(#remuda.hooks.probe == 1)
            "#,
        )
        .exec()
        .expect("reload should keep one hook and the original state");
    }

    #[test]
    fn failed_state_migration_keeps_the_previous_state_and_registrations() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            local v1 = {
              api = "remuda-module-v1", state_version = 1,
              initialize = function() return { nested = { count = 4 } } end,
              hooks = {{ event = "probe", run = function(state)
                state.nested.count = state.nested.count + 1
              end }},
            }
            assert(remuda._activate_module("sample", v1))
            local v2 = {
              api = "remuda-module-v1", state_version = 2,
              initialize = function() error("must not initialize on reload") end,
              migrations = {[1] = function(state)
                state.nested.count = state.nested.count + 10
                return { total = state.nested.count }
              end},
              hooks = {{ event = "probe", run = function(state)
                state.total = state.total + 1
                remuda._observed_total = state.total
              end }},
            }
            assert(remuda._activate_module("sample", v2))
            local broken = {
              api = "remuda-module-v1", state_version = 3,
              initialize = function() return {} end,
              migrations = {[2] = function(state)
                state.total = -1
                error("migration failed")
              end},
              hooks = {},
            }
            local ok = pcall(remuda._activate_module, "sample", broken)
            assert(not ok)
            remuda.emit("probe")
            assert(remuda._observed_total == 15)
            assert(#remuda.hooks.probe == 1)
            "#,
        )
        .exec()
        .expect("failed migration must leave the prior module active");
    }

    #[test]
    fn contributions_are_ordered_replaced_by_id_and_handed_out_as_copies() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            remuda.contribute("p", "b", { order = 10 })
            remuda.contribute("p", "a", { order = 10 })
            remuda.contribute("p", "z", { order = -5 })
            remuda.contribute("p", "c", {})
            remuda.contribute("other", "x", {})
            local ids = {}
            for i, item in ipairs(remuda.contributions("p")) do ids[i] = item.id end
            assert(table.concat(ids, ",") == "z,c,a,b", table.concat(ids, ","))

            remuda.contribute("p", "a", { order = -10, label = "new" })
            local first = remuda.contributions("p")[1]
            assert(first.id == "a" and first.entry.label == "new" and first.owner == nil)
            assert(#remuda.contributions("p") == 4, "same id replaces")
            assert(#remuda.contributions("none") == 0)

            first.id, first.owner = "hacked", "me"
            assert(remuda.contributions("p")[1].id == "a", "wrappers are copies")
            for _, bad in ipairs({ { "", "i", {} }, { "p", "", {} }, { "p", "i", "x" },
                                   { "p", "i", { order = "1" } } }) do
              assert(not pcall(remuda.contribute, bad[1], bad[2], bad[3]), "refused: " .. tostring(bad[2]))
            end
            "#,
        )
        .exec()
        .expect("contribute/contributions");
    }

    #[test]
    fn declared_contributions_are_owned_bound_to_state_and_replaced_on_reload() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            local function declaration(ids)
              local entries = {}
              for i, id in ipairs(ids) do
                entries[i] = { id = id, order = i, run = function(state, x) return state.tag .. ":" .. x end }
              end
              return { api = "remuda-module-v1", state_version = 1,
                initialize = function() return { tag = "s" } end,
                contributes = { ["host.command"] = entries } }
            end
            remuda.contribute("host.command", "foreign", { order = 99 })
            assert(remuda._activate_module("guest", declaration({ "one", "two" })))
            local list = remuda.contributions("host.command")
            assert(#list == 3 and list[1].id == "one" and list[1].owner == "guest")
            assert(list[1].entry.run("x") == "s:x", "function fields are bound to state")
            assert(list[3].id == "foreign" and list[3].owner == nil)

            assert(remuda._activate_module("guest", declaration({ "three" })))
            local ids = {}
            for i, item in ipairs(remuda.contributions("host.command")) do ids[i] = item.id end
            assert(table.concat(ids, ",") == "three,foreign", "reload replaces only its own: " .. table.concat(ids, ","))
            "#,
        )
        .exec()
        .expect("declared contributions");
    }

    #[test]
    fn a_failed_start_rolls_back_declared_contributions() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            local function declaration(id)
              return { api = "remuda-module-v1", state_version = 1,
                initialize = function() return {} end,
                contributes = { p = {{ id = id }} } }
            end
            assert(remuda._activate_module("guest", declaration("old")))
            local ok, _, _, rollback = remuda._activate_module("guest", declaration("new"))
            assert(ok and remuda.contributions("p")[1].id == "new")
            rollback()
            local list = remuda.contributions("p")
            assert(#list == 1 and list[1].id == "old" and list[1].owner == "guest", "rollback restores")
            "#,
        )
        .exec()
        .expect("rollback of contributions");
    }

    #[test]
    fn invalid_declared_contributions_are_refused_before_anything_changes() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            local function with(contributes)
              return { api = "remuda-module-v1", state_version = 1,
                initialize = function() return {} end, contributes = contributes }
            end
            assert(remuda._activate_module("guest", with({ p = {{ id = "kept" }} })))
            for _, bad in ipairs({ { p = {{ id = "a" }, { id = "a" }} }, { p = {{}} },
                                   { p = { "x" } }, { [""] = {{ id = "a" }} }, "x" }) do
              assert(not pcall(remuda._activate_module, "guest", with(bad)))
              local list = remuda.contributions("p")
              assert(#list == 1 and list[1].id == "kept", "a refused declaration changed nothing")
            end
            "#,
        )
        .exec()
        .expect("invalid declarations");
    }

    #[test]
    fn a_declared_contribution_owned_by_another_mod_is_refused() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            local function with(id)
              return { api = "remuda-module-v1", state_version = 1,
                initialize = function() return {} end,
                contributes = { ["host.command"] = {{ id = id, label = "mine" }} } }
            end
            assert(remuda._activate_module("alpha", with("shared")))
            local ok, err = pcall(remuda._activate_module, "beta", with("shared"))
            assert(not ok, "a cross-mod replace must be refused")
            err = tostring(err)
            for _, part in ipairs({ "alpha", "beta", "host.command/shared" }) do
              assert(err:find(part, 1, true), "error must name " .. part .. ": " .. err)
            end
            local list = remuda.contributions("host.command")
            assert(#list == 1 and list[1].owner == "alpha", "the owner's entry is untouched")
            assert(remuda._activate_module("alpha", with("shared")), "the owner may still reload it")
            local replaced, err = pcall(remuda.contribute, "host.command", "shared", { label = "imperative" })
            assert(not replaced and tostring(err):find("alpha", 1, true),
              "an owner-less caller cannot replace a mod's contribution")
            assert(remuda.contributions("host.command")[1].entry.label == "mine",
              "the owner's entry remains untouched")
            "#,
        )
        .exec()
        .expect("cross-mod contribution refusal");
    }

    #[test]
    fn a_returned_entry_is_a_copy_that_cannot_reach_the_registry() {
        let lua = lifecycle_lua();
        lua.load(
            r#"
            assert(remuda._activate_module("alpha", { api = "remuda-module-v1", state_version = 1,
              initialize = function() return {} end,
              contributes = { p = {{ id = "owned", order = 5, label = "alpha" }} } }))
            remuda.contribute("p", "free", { order = 10, label = "free" })
            for _, row in ipairs(remuda.contributions("p")) do
              row.entry.order, row.entry.label = -999, "hacked"
            end
            local list = remuda.contributions("p")
            assert(list[1].id == "owned" and list[1].entry.order == 5 and list[1].entry.label == "alpha",
              "an owned entry changed through a returned row")
            assert(list[2].id == "free" and list[2].entry.order == 10 and list[2].entry.label == "free",
              "an imperative entry changed through a returned row")
            "#,
        )
        .exec()
        .expect("returned entries are copies");
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

    // #128: only a `return CODE` that fails to compile falls back to running
    // CODE as statements; an expression that errors at run time ran twice.
    #[test]
    fn an_expression_that_errors_at_run_time_runs_once() {
        let lua = Lua::new();
        lua.load("calls = 0; function boom() calls = calls + 1; error('boom') end")
            .exec()
            .unwrap();
        let error = super::eval(&lua, "boom()", None).unwrap_err();
        assert!(error.contains("boom"), "{error}");
        assert_eq!(lua.globals().get::<i64>("calls").unwrap(), 1);
        assert_eq!(super::eval(&lua, "x = 1", None).unwrap(), "");
    }

    // ConPTY can hold a dead session's output pipe open, so its monitor never
    // reports the end. `remuda.ls()` reaps on the image thread itself, and a
    // wait there with no bound parks every later Eval — the TUI's included.
    #[test]
    fn ls_returns_when_a_dead_sessions_output_monitor_never_finishes() {
        use std::sync::Arc;
        use std::time::Duration;

        let registry = Arc::new(remuda_core::Registry::new());
        let mut agent = remuda_core::ScriptedAgent::new(Vec::new());
        agent.kill();
        let session = registry
            .register(remuda_core::Session::new(
                "stuck",
                Box::new(agent),
                Arc::new(remuda_core::ManualClock::default()),
            ))
            .unwrap_or_else(|_| panic!("the name is free"));
        let socket =
            std::env::temp_dir().join(format!("unused-ls-monitor-image-{}", std::process::id()));
        let image = super::Image::spawn(
            &socket,
            Arc::clone(&registry),
            Arc::new(crate::tick::Counters::default()),
        );
        // Registered, and never finished: no monitor thread is behind it.
        let _monitor = image.session_output_notifier("stuck", session.id());
        image
            .eval(
                "remuda.on('session_exited', function(name) remuda._exited = name end)",
                None,
            )
            .unwrap();

        let listed = image
            .submit("return #remuda.ls()", None)
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .expect(
                "remuda.ls() parked the image thread behind an output monitor that never finished",
            );
        assert_eq!(listed.unwrap(), "0", "the dead session is reaped");
        // The exit is still announced once the bound passes.
        assert_eq!(image.eval("return remuda._exited", None).unwrap(), "stuck");
        image.stop_for_test();
    }
}
