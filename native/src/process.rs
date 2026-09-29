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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
const RUN_READER_WORKER_LIMIT: usize = 16;
static ACTIVE_RUN_READER_WORKERS: AtomicUsize = AtomicUsize::new(0);
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
    #[cfg(test)]
    let diagnose = argv
        .iter()
        .any(|argument| argument.contains("remuda-process-run-grandchild"));
    #[cfg(not(test))]
    let diagnose = false;
    let (stdout_permit, stderr_permit) = reserve_run_reader_workers()?;
    let process_tree = ProcessTree::new().map_err(|error| error.to_string())?;

    let started_at = Instant::now();
    let deadline = started_at + Duration::from_secs_f64(timeout_seconds);
    let (program, args) = argv.split_first().expect("argv checked above");
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    child_guard::harden(&mut command);
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    if let Err(error) = process_tree.assign(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error.to_string());
    }
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
        std::thread::spawn(move || {
            let _permit = stdout_permit;
            capture_bounded(
                stdout,
                capture,
                state,
                "stdout",
                diagnose.then_some(started_at),
            )
        })
    };
    let stderr_reader_thread = {
        let capture = stderr_capture.clone();
        let state = stderr_reader.clone();
        std::thread::spawn(move || {
            let _permit = stderr_permit;
            capture_bounded(
                stderr,
                capture,
                state,
                "stderr",
                diagnose.then_some(started_at),
            )
        })
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
        &process_tree,
        &stdout_reader,
        &stderr_reader,
        &stdin_done,
        diagnose,
        started_at,
    )?;

    if timed_out {
        // Terminating the Windows Job closes descendant-held pipe handles.
        // Give the readers a short bounded window to consume bytes that were
        // already written before taking the output snapshots below.
        #[cfg(windows)]
        wait_for_readers(&stdout_reader, &stderr_reader, Duration::from_millis(500));
    }

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

struct RunReaderPermit;

impl Drop for RunReaderPermit {
    fn drop(&mut self) {
        ACTIVE_RUN_READER_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn reserve_run_reader_workers() -> Result<(RunReaderPermit, RunReaderPermit), String> {
    let mut active = ACTIVE_RUN_READER_WORKERS.load(Ordering::Acquire);
    loop {
        if active + 2 > RUN_READER_WORKER_LIMIT {
            return Err(format!(
                "process.run refused: the limit of {RUN_READER_WORKER_LIMIT} output-reader workers is reached because timed-out descendants still hold pipes"
            ));
        }
        match ACTIVE_RUN_READER_WORKERS.compare_exchange_weak(
            active,
            active + 2,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok((RunReaderPermit, RunReaderPermit)),
            Err(current) => active = current,
        }
    }
}

fn wait_for_process_io(
    child: &mut Child,
    deadline: Instant,
    process_tree: &ProcessTree,
    stdout_reader: &ReaderState,
    stderr_reader: &ReaderState,
    stdin_done: &AtomicBool,
    diagnose: bool,
    started_at: Instant,
) -> Result<(std::process::ExitStatus, bool), String> {
    #[cfg(not(windows))]
    let _ = (diagnose, started_at);
    let mut child_status = None;
    loop {
        if child_status.is_none() {
            match child.try_wait() {
                Ok(status) => child_status = status,
                Err(error) => {
                    process_tree.terminate(child);
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error.to_string());
                }
            }
        }
        if let Some(error) = stdout_reader.error() {
            terminate_child(child, process_tree);
            return Err(format!("read process.run stdout: {error}"));
        }
        if let Some(error) = stderr_reader.error() {
            terminate_child(child, process_tree);
            return Err(format!("read process.run stderr: {error}"));
        }
        if stdout_reader.done() && stderr_reader.done() && stdin_done.load(Ordering::Acquire) {
            if let Some(status) = child_status.take() {
                return Ok((status, false));
            }
        }
        if Instant::now() >= deadline {
            #[cfg(windows)]
            if diagnose {
                eprintln!(
                    "process.run diagnostic: deadline reached at {:?}; leader exited before timeout: {}",
                    started_at.elapsed(),
                    child_status.is_some()
                );
                eprintln!(
                    "process.run diagnostic: job accounting before termination: {:?}",
                    process_tree.job.active_processes()
                );
            }
            // On Unix, only signal the process group while its direct leader
            // is known to be alive; after reap, its pgid may have been reused.
            // A Windows Job Object handle remains tied to its job after the
            // leader exits, so it is safe to terminate in either case.
            let status = match child_status {
                Some(status) => {
                    // On Windows, the direct child may have exited while a
                    // grandchild still holds either pipe open. The Job Object
                    // remains valid after the leader exits, so terminate it
                    // at the deadline as well.
                    #[cfg(windows)]
                    process_tree.terminate(child);
                    status
                }
                None => match child.try_wait() {
                    Ok(Some(status)) => {
                        #[cfg(windows)]
                        process_tree.terminate(child);
                        status
                    }
                    Ok(None) => {
                        process_tree.terminate(child);
                        let _ = child.kill();
                        child.wait().map_err(|error| error.to_string())?
                    }
                    Err(error) => {
                        #[cfg(windows)]
                        process_tree.terminate(child);
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(error.to_string());
                    }
                },
            };
            return Ok((status, true));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn terminate_child(child: &mut Child, process_tree: &ProcessTree) {
    process_tree.terminate(child);
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

fn capture_bounded(
    mut reader: impl Read,
    capture: Arc<Mutex<BoundedCapture>>,
    state: ReaderState,
    label: &'static str,
    diagnostic_started_at: Option<Instant>,
) {
    let mut buffer = [0u8; 8192];
    let result = loop {
        match reader.read(&mut buffer) {
            Ok(0) => break Ok(()),
            Ok(read) => {
                if let Some(started_at) = diagnostic_started_at {
                    eprintln!(
                        "process.run diagnostic: {label} reader received {read} bytes at {:?}",
                        started_at.elapsed()
                    );
                }
                capture.lock().unwrap().push(&buffer[..read]);
            }
            Err(error) => break Err(error.to_string()),
        }
    };
    if let Err(error) = result {
        *state.error.lock().unwrap() = Some(error);
    }
    if let Some(started_at) = diagnostic_started_at {
        eprintln!(
            "process.run diagnostic: {label} reader reached EOF at {:?}",
            started_at.elapsed()
        );
    }
    state.done.store(true, Ordering::Release);
}

#[cfg(windows)]
fn wait_for_readers(stdout: &ReaderState, stderr: &ReaderState, grace: Duration) {
    let deadline = Instant::now() + grace;
    while !(stdout.done() && stderr.done()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
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
            "test",
            None,
        );
        let output = capture.lock().unwrap().snapshot();
        assert_eq!(output.len(), RUN_OUTPUT_LIMIT);
        assert!(output.ends_with(RUN_OUTPUT_MARKER));
    }

    #[test]
    fn resume_suspended_thread_drains_all_suspend_counts() {
        let mut returned_counts = [3, 2, 1].into_iter();
        let mut calls = 0;
        super::resume_suspended_thread(|| {
            calls += 1;
            Ok(returned_counts.next().expect("expected resume call"))
        })
        .unwrap();
        assert_eq!(calls, 3);
    }

    #[test]
    fn resume_suspended_thread_rejects_an_already_running_thread() {
        let error = super::resume_suspended_thread(|| Ok(0)).unwrap_err();
        assert!(error.to_string().contains("already running"));
    }

    #[test]
    fn resume_suspended_thread_propagates_resume_errors() {
        let error = super::resume_suspended_thread(|| Err(std::io::Error::other("resume failed")))
            .unwrap_err();
        assert_eq!(error.to_string(), "resume failed");
    }

    #[cfg(windows)]
    #[test]
    fn synchronous_process_starts_and_echoes_within_two_seconds() {
        use std::time::{Duration, Instant};

        let started = Instant::now();
        let output = super::run_sync(
            vec![
                "cmd.exe".into(),
                "/d".into(),
                "/c".into(),
                "echo remuda-process-ready".into(),
            ],
            None,
            2.0,
        )
        .expect("the child should start and exit before its deadline");

        assert!(
            !output.timed_out,
            "the child remained suspended until timeout"
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("remuda-process-ready"),
            "child output was not captured: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(windows)]
    #[test]
    fn synchronous_process_timeout_kills_grandchild_holding_pipes() {
        use std::time::{Duration, Instant};

        let stem = std::env::temp_dir().join(format!(
            "remuda-process-run-grandchild-{}",
            std::process::id()
        ));
        let started_path = stem.with_extension("started");
        let finished_path = stem.with_extension("finished");
        let script_path = stem.with_extension("ps1");
        for path in [&started_path, &finished_path, &script_path] {
            let _ = std::fs::remove_file(path);
        }
        let started = started_path.to_string_lossy().replace('\'', "''");
        let finished = finished_path.to_string_lossy().replace('\'', "''");
        std::fs::write(
            &script_path,
            format!(
                "Set-Content -LiteralPath '{started}' -Value started\nStart-Sleep -Seconds 4\nSet-Content -LiteralPath '{finished}' -Value finished\n"
            ),
        )
        .expect("write grandchild script");
        let script = script_path.to_string_lossy();
        let command = format!(
            r#"start "" /b powershell.exe -NoProfile -NonInteractive -File "{script}" & echo spawned"#
        );

        let started = Instant::now();
        let result = super::run_sync(
            vec!["cmd.exe".into(), "/d".into(), "/c".into(), command],
            None,
            2.0,
        )
        .expect("process.run should return at the timeout");
        eprintln!(
            "process.run diagnostic: test returned after {:?}; timed_out={}, code={}, stdout={:?}, stderr={:?}, started file exists={}",
            started.elapsed(),
            result.timed_out,
            result.code,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr),
            started_path.exists()
        );
        assert!(result.timed_out, "the parent should hit its timeout");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "process.run waited for its grandchild's inherited pipes"
        );
        assert!(
            String::from_utf8_lossy(&result.stdout).contains("spawned"),
            "the parent must confirm it launched the long-lived grandchild: {:?}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert!(
            started_path.exists(),
            "the grandchild should have started before process.run timed out"
        );

        std::thread::sleep(Duration::from_secs(3));
        let grandchild_survived = finished_path.exists();
        for path in [&started_path, &finished_path, &script_path] {
            let _ = std::fs::remove_file(path);
        }
        assert!(
            !grandchild_survived,
            "the grandchild survived after process.run timed out"
        );
    }

    #[cfg(windows)]
    #[test]
    fn synchronous_process_timeout_kills_an_immediately_spawned_grandchild() {
        use std::process::Command;
        use std::time::Duration;

        fn ping_process_ids() -> std::collections::BTreeSet<u32> {
            let output = Command::new("tasklist.exe")
                .args(["/FI", "IMAGENAME eq ping.exe", "/FO", "CSV", "/NH"])
                .output()
                .expect("list ping processes");
            assert!(output.status.success(), "tasklist failed: {output:?}");
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| {
                    let mut fields = line.split(',');
                    let image = fields.next()?.trim_matches('"');
                    if !image.eq_ignore_ascii_case("ping.exe") {
                        return None;
                    }
                    fields.next()?.trim_matches('"').parse().ok()
                })
                .collect()
        }

        let before = ping_process_ids();
        let result = super::run_sync(
            vec![
                "cmd.exe".into(),
                "/d".into(),
                "/c".into(),
                "start /b ping -n 30 127.0.0.1 & exit".into(),
            ],
            None,
            0.2,
        )
        .expect("process.run should return at the timeout");
        assert!(result.timed_out, "the direct child should hit its timeout");

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut survivors: Vec<_> = ping_process_ids().difference(&before).copied().collect();
        while !survivors.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            survivors = ping_process_ids().difference(&before).copied().collect();
        }
        // If the assertion catches a regression, clean up only the processes
        // that appeared during this test so a failed run leaves no ping behind.
        for process_id in &survivors {
            let process_id = process_id.to_string();
            let _ = Command::new("taskkill.exe")
                .args(["/PID", process_id.as_str(), "/T", "/F"])
                .status();
        }
        assert_eq!(
            survivors,
            Vec::<u32>::new(),
            "the ping grandchild created immediately by cmd.exe survived the timeout"
        );
    }
}

struct ProcessTree {
    #[cfg(windows)]
    job: KillOnCloseJob,
}

impl ProcessTree {
    fn new() -> std::io::Result<Self> {
        #[cfg(windows)]
        {
            return Ok(Self {
                job: KillOnCloseJob::new()?,
            });
        }
        #[cfg(not(windows))]
        Ok(Self {})
    }

    fn assign(&self, child: &Child) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            self.job.assign(child)?;
        }
        #[cfg(not(windows))]
        let _ = child;
        Ok(())
    }

    fn terminate(&self, child: &Child) {
        #[cfg(unix)]
        {
            let pid = child.id() as libc::pid_t;
            // SAFETY: killpg receives only the child process-group id;
            // child_guard creates that group before exec, so it cannot name
            // the daemon's group.
            let _ = unsafe { libc::killpg(pid, libc::SIGKILL) };
        }
        #[cfg(windows)]
        {
            let _ = child;
            self.job.terminate();
        }
        #[cfg(not(any(unix, windows)))]
        let _ = child;
    }
}

#[cfg(windows)]
struct KillOnCloseJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl KillOnCloseJob {
    fn new() -> std::io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        // SAFETY: a null security descriptor and name request a private,
        // unnamed job owned by this handle.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` is the structure required for this information
        // class, and the pointer and byte length remain valid for the call.
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            let error = std::io::Error::last_os_error();
            // SAFETY: this handle was just returned by CreateJobObjectW.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Err(error);
        }
        Ok(Self(handle))
    }

    fn assign(&self, child: &Child) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        // SAFETY: both handles are live and owned for the duration of this
        // call; Child keeps the process handle open and self owns the job.
        let assigned = unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle()) };
        if assigned == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn resume_primary_thread(&self, process_id: u32) -> std::io::Result<()> {
        use windows_sys::Win32::Foundation::{
            CloseHandle, GetLastError, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        };
        use windows_sys::Win32::System::Threading::{
            OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
        };

        // SAFETY: Toolhelp accepts a zero process id for a system-wide thread
        // snapshot; the returned snapshot is closed below on every path.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }

        let resumed = (|| {
            let mut entry = THREADENTRY32 {
                dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                ..THREADENTRY32::default()
            };
            // SAFETY: `entry` is writable, sized as required, and `snapshot`
            // remains live until the closure completes.
            if unsafe { Thread32First(snapshot, &mut entry) } == 0 {
                return Err(std::io::Error::last_os_error());
            }

            loop {
                if entry.th32OwnerProcessID == process_id {
                    // SAFETY: the id comes from a live snapshot entry; the
                    // handle is closed immediately after ResumeThread.
                    let thread =
                        unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                    if thread.is_null() {
                        return Err(std::io::Error::last_os_error());
                    }
                    // SAFETY: `thread` is an owned handle with the required
                    // suspend/resume access right.
                    let resumed = resume_suspended_thread(|| {
                        let previous = unsafe { ResumeThread(thread) };
                        if previous == u32::MAX {
                            Err(std::io::Error::last_os_error())
                        } else {
                            Ok(previous)
                        }
                    });
                    // SAFETY: this handle was returned by OpenThread above.
                    unsafe { CloseHandle(thread) };
                    resumed?;
                    return Ok(());
                }

                // SAFETY: `entry` and `snapshot` remain valid for the call.
                entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
                if unsafe { Thread32Next(snapshot, &mut entry) } == 0 {
                    // ERROR_NO_MORE_FILES is the normal end of the snapshot.
                    // SAFETY: GetLastError reads the calling thread's error.
                    let error = unsafe { GetLastError() };
                    if error == ERROR_NO_MORE_FILES {
                        return Err(std::io::Error::other(
                            "the suspended process has no resumable thread",
                        ));
                    }
                    return Err(std::io::Error::from_raw_os_error(error as i32));
                }
            }
        })();

        // SAFETY: `snapshot` was returned by CreateToolhelp32Snapshot.
        unsafe { CloseHandle(snapshot) };
        resumed
    }

    fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;

        // SAFETY: self owns a valid Job Object handle. It may already be
        // empty or terminated, in which case this best-effort call is benign.
        unsafe { TerminateJobObject(self.0, RUN_TIMEOUT_EXIT_CODE as u32) };
    }

    fn active_processes(&self) -> std::io::Result<u32> {
        use windows_sys::Win32::System::JobObjects::{
            JobObjectBasicAccountingInformation, QueryInformationJobObject,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        };

        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: `accounting` is writable and its size matches the requested
        // information class; the job handle remains live for this call.
        let queried = unsafe {
            QueryInformationJobObject(
                self.0,
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if queried == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(accounting.ActiveProcesses)
        }
    }
}

#[cfg(any(windows, test))]
fn resume_suspended_thread(
    mut resume_thread: impl FnMut() -> std::io::Result<u32>,
) -> std::io::Result<()> {
    let mut previous = resume_thread()?;
    if previous == 0 {
        return Err(std::io::Error::other(
            "the suspended process thread was already running",
        ));
    }
    // CREATE_SUSPENDED contributes one suspend count, but Windows may report
    // additional counts. ResumeThread decrements only one count per call;
    // treating any positive return as success can leave the child suspended.
    while previous > 1 {
        previous = resume_thread()?;
        if previous == 0 {
            // Another resumer cleared the final count between our calls.
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(windows)]
impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE terminates any remaining
        // descendants when this last owned handle is closed.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

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
