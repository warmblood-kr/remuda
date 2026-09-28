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

use crate::child_guard;
use crate::image::Image;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// ponytail: fixed cap, not adaptive — a helper that fills this before Lua
// drains it just blocks on its own stdout write (the intended backpressure).
// Raise it if a real helper needs more headroom than one bursty screenful.
const BUFFER_CAP: usize = 4096;

// ponytail: fixed batch size, not tuned — large enough that a normal burst
// drains in one job, small enough that one job can't hog the Image's FIFO
// for long under a flood. Revisit if either edge is hit in practice.
const DRAIN_BATCH: usize = 256;

const RUN_OUTPUT_LIMIT: usize = 1024 * 1024;
const RUN_OUTPUT_MARKER: &[u8] = b"\n[output truncated by remuda.process.run]";
pub const RUN_DEFAULT_TIMEOUT: f64 = 5.0;
pub const RUN_MAX_TIMEOUT: f64 = 30.0;
pub const RUN_TIMEOUT_EXIT_CODE: i32 = 124;

/// Result from the bounded synchronous `remuda.process.run` word. Output is
/// raw bytes (Lua strings are byte strings); each stream is capped separately.
pub struct RunOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub signal: Option<i32>,
}

/// Run one child with argv directly (never through a shell). This is a
/// deliberately synchronous exception: its enforced deadline bounds how long
/// it can hold the daemon's single Lua image.
pub fn run_sync(
    argv: Vec<String>,
    stdin: Option<Vec<u8>>,
    timeout_seconds: f64,
) -> Result<RunOutput, String> {
    validate_run(&argv, timeout_seconds)?;

    let deadline = Instant::now() + Duration::from_secs_f64(timeout_seconds);
    let (program, args) = argv.split_first().expect("argv checked above");
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    child_guard::harden(&mut command);
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let child_stdin = child.stdin.take().expect("stdin was piped");

    let stdout_capture = Arc::new(Mutex::new(BoundedCapture::default()));
    let stderr_capture = Arc::new(Mutex::new(BoundedCapture::default()));
    let stdout_reader = ReaderState::new();
    let stderr_reader = ReaderState::new();
    let stdout_reader_thread = {
        let capture = stdout_capture.clone();
        let state = stdout_reader.clone();
        std::thread::spawn(move || capture_bounded(stdout, capture, state))
    };
    let stderr_reader_thread = {
        let capture = stderr_capture.clone();
        let state = stderr_reader.clone();
        std::thread::spawn(move || capture_bounded(stderr, capture, state))
    };
    let stdin_done = Arc::new(AtomicBool::new(false));
    let stdin_writer = {
        let done = stdin_done.clone();
        std::thread::spawn(move || {
            if let Some(input) = stdin {
                let mut child_stdin = child_stdin;
                let _ = child_stdin.write_all(&input);
            }
            // Dropping stdin signals EOF to children which read until end.
            done.store(true, Ordering::Release);
        })
    };

    let (child_status, timed_out) = wait_for_process_io(
        &mut child,
        deadline,
        &stdout_reader,
        &stderr_reader,
        &stdin_done,
    )?;

    // All three workers are finished on the successful path. On timeout,
    // dropping their handles detaches them so an escaped descendant holding
    // a pipe cannot keep the Lua image blocked past the deadline.
    drop((stdout_reader_thread, stderr_reader_thread, stdin_writer));
    let stdout = stdout_capture.lock().unwrap().snapshot();
    let stderr = stderr_capture.lock().unwrap().snapshot();

    Ok(RunOutput {
        code: if timed_out {
            RUN_TIMEOUT_EXIT_CODE
        } else {
            child_status.code().unwrap_or(-1)
        },
        stdout,
        stderr,
        timed_out,
        signal: exit_signal(&child_status),
    })
}

fn validate_run(argv: &[String], timeout_seconds: f64) -> Result<(), String> {
    if argv.is_empty() || argv[0].is_empty() {
        return Err("a process.run call needs a non-empty argv[1]".into());
    }
    if !timeout_seconds.is_finite() || timeout_seconds <= 0.0 {
        return Err("process.run timeout must be a positive finite number".into());
    }
    if timeout_seconds > RUN_MAX_TIMEOUT {
        return Err(format!(
            "process.run timeout cannot exceed {RUN_MAX_TIMEOUT} seconds"
        ));
    }
    Ok(())
}

fn wait_for_process_io(
    child: &mut Child,
    deadline: Instant,
    stdout_reader: &ReaderState,
    stderr_reader: &ReaderState,
    stdin_done: &AtomicBool,
) -> Result<(std::process::ExitStatus, bool), String> {
    let mut child_status = None;
    loop {
        if child_status.is_none() {
            match child.try_wait() {
                Ok(status) => child_status = status,
                Err(error) => {
                    kill_process_tree(child);
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error.to_string());
                }
            }
        }
        if let Some(error) = stdout_reader.error() {
            terminate_child(child);
            return Err(format!("read process.run stdout: {error}"));
        }
        if let Some(error) = stderr_reader.error() {
            terminate_child(child);
            return Err(format!("read process.run stderr: {error}"));
        }
        if stdout_reader.done() && stderr_reader.done() && stdin_done.load(Ordering::Acquire) {
            if let Some(status) = child_status.take() {
                return Ok((status, false));
            }
        }
        if Instant::now() >= deadline {
            // Descendants can keep pipes open after the direct child exits.
            // Kill its group again and return at the same deadline, without
            // joining any reader or writer that may still be blocked.
            kill_process_tree(child);
            let _ = child.kill();
            let status = match child_status {
                Some(status) => status,
                None => child.wait().map_err(|error| error.to_string())?,
            };
            return Ok((status, true));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn terminate_child(child: &mut Child) {
    kill_process_tree(child);
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Default)]
struct BoundedCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

impl BoundedCapture {
    fn push(&mut self, bytes: &[u8]) {
        let retained_limit = RUN_OUTPUT_LIMIT - RUN_OUTPUT_MARKER.len();
        let retain = retained_limit
            .saturating_sub(self.bytes.len())
            .min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..retain]);
        self.truncated |= retain < bytes.len();
    }

    fn snapshot(&self) -> Vec<u8> {
        let mut output = self.bytes.clone();
        if self.truncated {
            output.extend_from_slice(RUN_OUTPUT_MARKER);
        }
        output
    }
}

#[derive(Clone, Default)]
struct ReaderState {
    done: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
}

impl ReaderState {
    fn new() -> Self {
        Self::default()
    }

    fn done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

fn capture_bounded(mut reader: impl Read, capture: Arc<Mutex<BoundedCapture>>, state: ReaderState) {
    let mut buffer = [0u8; 8192];
    let result = loop {
        match reader.read(&mut buffer) {
            Ok(0) => break Ok(()),
            Ok(read) => capture.lock().unwrap().push(&buffer[..read]),
            Err(error) => break Err(error.to_string()),
        }
    };
    if let Err(error) = result {
        *state.error.lock().unwrap() = Some(error);
    }
    state.done.store(true, Ordering::Release);
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(test)]
mod run_tests {
    use super::{BoundedCapture, RUN_OUTPUT_LIMIT, RUN_OUTPUT_MARKER};
    use std::io::Cursor;

    #[test]
    fn synchronous_process_output_is_capped_with_a_marker() {
        let capture = std::sync::Arc::new(std::sync::Mutex::new(BoundedCapture::default()));
        super::capture_bounded(
            Cursor::new(vec![b'x'; RUN_OUTPUT_LIMIT + 1]),
            capture.clone(),
            super::ReaderState::new(),
        );
        let output = capture.lock().unwrap().snapshot();
        assert_eq!(output.len(), RUN_OUTPUT_LIMIT);
        assert!(output.ends_with(RUN_OUTPUT_MARKER));
    }
}

#[cfg(unix)]
fn kill_process_tree(child: &Child) {
    let pid = child.id() as libc::pid_t;
    // SAFETY: killpg receives only the child process-group id; child_guard
    // creates that group before exec, so it cannot name the daemon's group.
    let _ = unsafe { libc::killpg(pid, libc::SIGKILL) };
}

#[cfg(not(unix))]
fn kill_process_tree(_child: &Child) {}

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
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Every plain-pipe child funnels through the one seam that keeps it
        // from outliving this daemon — see child_guard.rs.
        child_guard::harden(&mut command);
        let mut child = command.spawn().map_err(|e| e.to_string())?;
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

    /// Reap a tracked child's WHOLE process group (unix, via
    /// `child_guard::harden`) — covers a grandchild it forked before this
    /// runs. Best-effort: an already-gone child is nothing to signal.
    pub fn killpg(&self, id: u64) -> Result<(), String> {
        let Some(state) = self.0.lock().unwrap().get(&id).cloned() else {
            return Ok(());
        };
        let child = state.child.lock().unwrap();
        let Some(c) = child.as_ref() else {
            return Ok(());
        };
        #[cfg(unix)]
        {
            let pid = c.id() as libc::pid_t;
            // SAFETY: killpg with a plain pid, no memory involved.
            if unsafe { libc::killpg(pid, libc::SIGKILL) } != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ESRCH) {
                    return Err(err.to_string());
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = c;
        }
        Ok(())
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
