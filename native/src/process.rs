//! `remuda.process`: a plain-pipe (non-pty) child process whose stdout lines
//! arrive in the Image as `remuda.emit` calls — never by Lua waiting on the
//! network or the child. `image.rs`'s own invariant is why: a script may
//! never block the interpreter on I/O, so this delivers lines *into* the
//! Image from a thread that never touches Lua directly.
//!
//! Backpressure, not buffering: a per-process buffer is capped. When it is
//! full, the reader thread simply stops reading — the OS pipe fills, and the
//! child's own `write()` blocks. Lines are drained in batches, one Image job
//! per batch, and only one such job is ever outstanding per process, so a
//! single noisy process never holds the Image's FIFO for more than one
//! batch at a time — the schedule ticker and any other caller queued behind
//! it still gets a turn between batches.

use crate::image::Image;
use std::collections::{HashMap, VecDeque};
use std::io::BufRead;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

// ponytail: fixed cap, not adaptive — a helper that fills this before Lua
// drains it just blocks on its own stdout write (the intended backpressure).
// Raise it if a real helper needs more headroom than one bursty screenful.
const BUFFER_CAP: usize = 4096;

// ponytail: fixed batch size, not tuned — large enough that a normal burst
// drains in one job, small enough that one job can't hog the Image's FIFO
// for long under a flood. Revisit if either edge is hit in practice.
const DRAIN_BATCH: usize = 256;

struct ProcessState {
    lines: Mutex<VecDeque<String>>,
    space_freed: Condvar,
    in_flight: AtomicBool,
    finished: AtomicBool,
    exit_code: Mutex<Option<i32>>,
    on_line: Option<String>,
    on_exit: Option<String>,
    child: Mutex<Option<Child>>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The daemon's running `remuda.process` handles, by id. One per `Image`
/// (constructed fresh in `bindings`, so it does not outlive a daemon
/// restart — the same lifetime schedules already have).
#[derive(Clone, Default)]
pub struct Processes(Arc<Mutex<HashMap<u64, Arc<ProcessState>>>>);

impl Processes {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn `argv` as a plain-pipe child: stdin null, stdout piped, stderr
    /// null, nothing else.
    // `Stdio::piped()`/`Stdio::null()` are never shared with the parent's own
    // handles on any platform (only `Stdio::inherit()` is), so this asks for
    // exactly these three and no more.
    pub fn spawn(
        &self,
        image: Image,
        argv: Vec<String>,
        on_line: Option<String>,
        on_exit: Option<String>,
    ) -> Result<u64, String> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| "a process needs a non-empty argv".to_string())?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let stdout = child.stdout.take().expect("stdout was piped");

        let state = Arc::new(ProcessState {
            lines: Mutex::new(VecDeque::new()),
            space_freed: Condvar::new(),
            in_flight: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            exit_code: Mutex::new(None),
            on_line,
            on_exit,
            child: Mutex::new(Some(child)),
        });

        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        self.0.lock().unwrap().insert(id, state.clone());

        let reader_state = state;
        std::thread::spawn(move || read_lines(id, reader_state, image, stdout));

        Ok(id)
    }

    /// Deliver up to `DRAIN_BATCH` buffered lines to `remuda.emit`, called
    /// only as an Image job (`remuda._process_drain(id)`) — `lua` is always
    /// the one interpreter thread, never touched from anywhere else.
    // Resubmits itself if more lines remain; fires the exit event once the
    // buffer is empty and the child has finished.
    pub fn drain(&self, id: u64, lua: &mlua::Lua, image: &Image) -> mlua::Result<()> {
        let Some(state) = self.0.lock().unwrap().get(&id).cloned() else {
            return Ok(());
        };

        let (batch, now_empty) = {
            let mut lines = state.lines.lock().unwrap();
            let mut batch = Vec::new();
            while batch.len() < DRAIN_BATCH {
                match lines.pop_front() {
                    Some(line) => batch.push(line),
                    None => break,
                }
            }
            let now_empty = lines.is_empty();
            if now_empty {
                // Cleared inside the same critical section as the emptiness
                // check, so a concurrent push cannot be missed: it either
                // sees the flag still true (this drain is not done) or
                // acquires the lock after this and finds it cleared.
                state.in_flight.store(false, Ordering::SeqCst);
            }
            (batch, now_empty)
        };

        if !batch.is_empty() {
            state.space_freed.notify_all();
        }

        if let Some(event) = &state.on_line {
            let remuda: mlua::Table = lua.globals().get("remuda")?;
            let emit: mlua::Function = remuda.get("emit")?;
            for line in &batch {
                // A hook that errors must not stop the rest of this batch,
                // or the buffer's own draining: an emit error is the hook's
                // bug, not a reason to wedge this process's delivery.
                let _ = emit.call::<()>((event.as_str(), line.as_str()));
            }
        }

        if !now_empty {
            let _ = image.submit(&format!("remuda._process_drain({id})"), None);
            return Ok(());
        }

        if state.finished.load(Ordering::SeqCst) {
            let code = state.exit_code.lock().unwrap().unwrap_or(-1);
            self.0.lock().unwrap().remove(&id);
            if let Some(event) = &state.on_exit {
                let remuda: mlua::Table = lua.globals().get("remuda")?;
                let emit: mlua::Function = remuda.get("emit")?;
                let _ = emit.call::<()>((event.as_str(), code));
            }
        }

        Ok(())
    }

    pub fn kill(&self, id: u64) -> Result<(), String> {
        let Some(state) = self.0.lock().unwrap().get(&id).cloned() else {
            return Err(format!("no such process: {id}"));
        };
        let mut child = state.child.lock().unwrap();
        match child.as_mut() {
            Some(c) => c.kill().map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    pub fn list(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.0.lock().unwrap().keys().copied().collect();
        ids.sort_unstable();
        ids
    }
}

/// Runs on its own thread, never touching Lua. Reads lines until EOF (the
/// child exited, or was killed and its pipe closed as a result), buffering
/// them with backpressure.
// Submits exactly one drain job whenever the buffer goes from empty to
// non-empty (mirrored by `drain` clearing the flag only when it leaves the
// buffer empty, under the same lock). On EOF, reaps the child's exit status
// and forces one more flag transition, so whichever drain job runs last is
// guaranteed to see both "empty" and "finished" together and fire the exit
// event exactly once.
fn read_lines(id: u64, state: Arc<ProcessState>, image: Image, stdout: ChildStdout) {
    let reader = std::io::BufReader::new(stdout);
    for line in reader.lines() {
        let Ok(line) = line else { break };

        let mut lines = state.lines.lock().unwrap();
        while lines.len() >= BUFFER_CAP {
            lines = state.space_freed.wait(lines).unwrap();
        }
        lines.push_back(line);
        drop(lines);

        let was_in_flight = state.in_flight.swap(true, Ordering::SeqCst);
        if !was_in_flight {
            let _ = image.submit(&format!("remuda._process_drain({id})"), None);
        }
    }

    let status = {
        let mut child = state.child.lock().unwrap();
        child.as_mut().and_then(|c| c.wait().ok())
    };
    *state.exit_code.lock().unwrap() = status.and_then(|s| s.code());
    state.finished.store(true, Ordering::SeqCst);

    let was_in_flight = state.in_flight.swap(true, Ordering::SeqCst);
    if !was_in_flight {
        let _ = image.submit(&format!("remuda._process_drain({id})"), None);
    }
}
