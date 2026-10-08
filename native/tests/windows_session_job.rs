//! Assigned session processes and descendants started after assignment are
//! `session` callers, even when a parent has exited or asks to leave the job.
//! They end with the session or daemon. A command in an ordinary terminal
//! still reads `unknown` here.
#![cfg(windows)]

use remuda_core::protocol::{Request, Response};
use remuda_core::{Registry, Session, Size};
use remuda_native::{client, daemon, CommandBuilder, PtyAgent, SystemClock};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[path = "daemon_support/spawn.rs"]
mod spawn;

const ASK: &str = "return remuda.caller().kind";
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// A directory of our own, removed when the test ends, passing or not.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("remuda-j{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A batch file that asks the daemon who it is and writes the answer to
/// `out`, after `wait` seconds when its starter must be gone by then.
fn asking_script(dir: &Path, out: &Path, wait: u32) -> PathBuf {
    let pause = if wait > 0 {
        format!("ping -n {} 127.0.0.1 >nul\r\n", wait + 1)
    } else {
        String::new()
    };
    let body = format!(
        "@echo off\r\n{pause}\"{}\" -s s -e \"{ASK}\" >\"{}\" 2>&1\r\n",
        env!("CARGO_BIN_EXE_remuda"),
        out.display()
    );
    let script = dir.join("ask.cmd");
    std::fs::write(&script, body).expect("write ask.cmd");
    script
}

/// Runs `code` in the scratch daemon; the session must start.
fn start(dir: &Path, code: String) {
    let request = Request::Eval { code, name: None };
    let socket = daemon::socket_path_in(dir, "s");
    let started = client::request(&socket, &request).expect("eval request");
    assert!(matches!(started, Response::Value(_)), "{started:?}");
}

/// What appears in `out` within 40 seconds, trimmed; empty when nothing did.
fn answer(out: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let text = std::fs::read_to_string(out).unwrap_or_default();
        if !text.trim().is_empty() || Instant::now() >= deadline {
            return text.trim().to_owned();
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn fact(line: std::fmt::Arguments) {
    // Straight to stderr, so the facts are in the log for a passing test too.
    let _ = writeln!(std::io::stderr(), "FACT job {line}");
}

fn lua(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("utf-8 scratch path"))
}

#[test]
fn a_session_child_is_a_session_caller() {
    let scratch = Scratch::new("child");
    let _daemon = spawn::Daemon::spawn(&scratch.0);
    let out = scratch.0.join("kind.txt");
    let script = asking_script(&scratch.0, &out, 0);
    start(
        &scratch.0,
        format!(
            "return remuda.new('child', {{ 'cmd.exe', '/c', {} }})",
            lua(&script)
        ),
    );
    let kind = answer(&out);
    fact(format_args!("session child: kind {kind:?}"));
    assert_eq!(kind, "session");
}

/// A session whose program starts a process in its own console (`start`
/// without /b, so the pty's end does not close it) and then lives for `life`
/// seconds. That process marks `started.txt`, waits `wait` seconds, then asks.
fn session_with_a_lingering_process(dir: &Path, out: &Path, life: u32, wait: u32) -> PathBuf {
    let linger = dir.join("linger.cmd");
    let body = format!(
        "@echo off\r\n>\"{}\" echo started\r\nping -n {} 127.0.0.1 >nul\r\n\"{}\" -s s -e \"{ASK}\" >\"{}\" 2>&1\r\n",
        dir.join("started.txt").display(),
        wait + 1,
        env!("CARGO_BIN_EXE_remuda"),
        out.display()
    );
    std::fs::write(&linger, body).expect("write linger.cmd");
    let session = dir.join("session.cmd");
    let body = format!(
        "@echo off\r\nstart \"\" /min cmd.exe /c \"{}\"\r\nping -n {} 127.0.0.1 >nul\r\n",
        linger.display(),
        life + 1
    );
    std::fs::write(&session, body).expect("write session.cmd");
    session
}

/// The PIDs of the lingering processes of this scratch directory, from the
/// system's process list.
fn lingering(dir: &Path) -> Vec<u32> {
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("scratch name");
    let list = format!(
        "Get-CimInstance Win32_Process | Where-Object {{ $_.Name -eq 'cmd.exe' -and \
         $_.CommandLine -like '*{name}*linger.cmd*' }} | ForEach-Object {{ $_.ProcessId }}"
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &list])
        .output()
        .expect("list processes");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// Starts a session with a lingering process, ends it with `end`, and
/// requires that the lingering process is gone and never asks.
fn the_lingering_process_ends_with(tag: &str, life: u32, end: impl FnOnce(&Path, spawn::Daemon)) {
    const WAIT: u32 = 12;
    let scratch = Scratch::new(tag);
    let daemon = spawn::Daemon::spawn(&scratch.0);
    let out = scratch.0.join("kind.txt");
    let session = session_with_a_lingering_process(&scratch.0, &out, life, WAIT);
    start(
        &scratch.0,
        format!(
            "return remuda.new('linger', {{ 'cmd.exe', '/c', {} }})",
            lua(&session)
        ),
    );
    let began = Instant::now();
    assert_eq!(answer(&scratch.0.join("started.txt")), "started");
    let before = lingering(&scratch.0);

    end(&scratch.0, daemon);
    // Past the moment it would have asked, had it lived.
    while began.elapsed() < Duration::from_secs(u64::from(WAIT) + 6) {
        std::thread::sleep(Duration::from_millis(200));
    }
    let after = lingering(&scratch.0);
    let asked = std::fs::read_to_string(&out).unwrap_or_default();
    fact(format_args!(
        "lingering process, session ended by {tag}: alive before {before:?}; alive after {after:?}; asked {:?}",
        asked.trim()
    ));
    assert!(!before.is_empty(), "the lingering process never ran");
    assert!(after.is_empty(), "it outlived its session: {after:?}");
    assert!(
        asked.trim().is_empty(),
        "it asked after its session ended: {asked:?}"
    );
}

/// A session ends when its own program exits: the daemon reaps it. Its assigned
/// child and descendants started after assignment end with it.
#[test]
fn what_a_session_started_ends_when_its_program_exits() {
    the_lingering_process_ends_with("exit", 4, |_, daemon| {
        // Nothing to do: the session's program exits by itself.
        std::thread::sleep(Duration::from_secs(6));
        drop(daemon);
    });
}

#[test]
fn what_a_session_started_ends_when_the_session_is_closed() {
    the_lingering_process_ends_with("close", 120, |dir, daemon| {
        start(dir, "remuda.close('linger') return 'closed'".to_string());
        // The daemon stays up: the session's end alone must do it.
        std::thread::sleep(Duration::from_secs(8));
        drop(daemon);
    });
}

/// Reaping ends the job while a Registry::get handle keeps the Session alive.
/// This is Windows CI coverage; the target is unavailable locally.
#[test]
fn reaping_ends_the_job_while_registry_get_retains_the_session() {
    let scratch = Scratch::new("reap-held");
    let out = scratch.0.join("kind.txt");
    let session_script = session_with_a_lingering_process(&scratch.0, &out, 4, 12);
    let mut command = CommandBuilder::new("cmd.exe");
    command.args(["/c", session_script.to_str().expect("utf-8 scratch path")]);
    let agent = PtyAgent::spawn(command, Size::new(80, 24)).expect("spawn session pty");
    let registry = Registry::new();
    registry
        .register(Session::new(
            "linger",
            Box::new(agent),
            Arc::new(SystemClock::new()),
        ))
        .expect("register session");
    let held = registry.get("linger").expect("hold session from registry");

    let began = Instant::now();
    while !scratch.0.join("started.txt").exists() {
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "child never started"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let before = lingering(&scratch.0);
    fact(format_args!("lingering processes before reap: {before:?}"));
    assert!(!before.is_empty(), "the lingering process never ran");

    let deadline = Instant::now() + Duration::from_secs(10);
    while held.is_alive() {
        assert!(Instant::now() < deadline, "session process never exited");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(registry.reap(), vec![String::from("linger")]);
    assert!(registry.get("linger").is_none(), "session was not reaped");
    assert_eq!(
        Arc::strong_count(&held),
        1,
        "held Arc was unexpectedly dropped"
    );

    let mut after = lingering(&scratch.0);
    while !after.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        after = lingering(&scratch.0);
    }
    fact(format_args!(
        "lingering processes after reap with Registry::get Arc held: {after:?}"
    ));
    assert!(after.is_empty(), "job survived reaping: {after:?}");
}

#[test]
fn what_a_session_started_ends_when_the_daemon_stops() {
    the_lingering_process_ends_with("daemon", 120, |_, daemon| drop(daemon));
}

/// Not a test of its own: the program of the breakaway session. It starts the
/// asking script in a new console and asks Windows to keep it OUT of the job,
/// then exits. Without the variables it does nothing.
#[test]
fn breakaway_helper() {
    let (Some(script), Some(out)) = (
        std::env::var_os("REMUDA_BREAKAWAY_SCRIPT"),
        std::env::var_os("REMUDA_BREAKAWAY_OUT"),
    ) else {
        return;
    };
    let spawned = Command::new("cmd.exe")
        .arg("/c")
        .arg(script)
        .creation_flags(CREATE_NEW_CONSOLE | CREATE_BREAKAWAY_FROM_JOB)
        .spawn();
    if let Err(error) = spawned {
        std::fs::write(out, format!("breakaway refused: {error}")).expect("record the refusal");
    }
}

/// A session's job allows no breakaway: the request is refused by Windows, or
/// the process is started and is still the session's.
#[test]
fn a_child_that_asks_to_leave_the_job_is_still_a_session_caller() {
    let scratch = Scratch::new("break");
    let _daemon = spawn::Daemon::spawn(&scratch.0);
    let out = scratch.0.join("kind.txt");
    let script = asking_script(&scratch.0, &out, 3);
    let helper = std::env::current_exe().expect("locate this test binary");
    start(
        &scratch.0,
        format!(
            "return remuda.new('break', {{ {}, '--exact', 'breakaway_helper', '--nocapture' }}, nil, \
             {{ REMUDA_BREAKAWAY_SCRIPT = {}, REMUDA_BREAKAWAY_OUT = {} }})",
            lua(&helper),
            lua(&script),
            lua(&out)
        ),
    );
    let kind = answer(&out);
    fact(format_args!("child that asked for breakaway: {kind:?}"));
    assert!(
        kind == "session" || kind.starts_with("breakaway refused"),
        "{kind:?}"
    );
}

/// A plain command outside every live session has a confirmed outside chain.
#[test]
fn a_command_outside_every_session_reads_outside() {
    let scratch = Scratch::new("plain");
    let _daemon = spawn::Daemon::spawn(&scratch.0);
    let output = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s", "-e", ASK])
        .env("REMUDA_RUNTIME_DIR", &scratch.0)
        .env("HOME", scratch.0.join("home"))
        .env("LOCALAPPDATA", scratch.0.join("home"))
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run remuda");
    let kind = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    fact(format_args!("plain command: kind {kind:?}"));
    assert_eq!(kind, "outside");
}

/// The same grandchild while its session is still LISTED (the daemon is told
/// to keep exited sessions): the session's job is what answers, since the
/// parent is gone. What happens once a session is removed is decided apart.
#[test]
fn a_grandchild_of_a_listed_session_is_a_session_caller() {
    let scratch = Scratch::new("kept");
    let mut command = spawn::base_command(&scratch.0);
    command.env("REMUDA_KEEP_EXITED", "1");
    let _daemon = spawn::spawn_and_wait(command, &scratch.0);
    let out = scratch.0.join("kind.txt");
    let script = asking_script(&scratch.0, &out, 3);
    start(
        &scratch.0,
        format!(
            "return remuda.new('kept', {{ 'cmd.exe', '/c', 'start', '', '/min', 'cmd.exe', '/c', {} }})",
            lua(&script)
        ),
    );
    let kind = answer(&out);
    fact(format_args!(
        "grandchild of a listed session: kind {kind:?}"
    ));
    assert_eq!(kind, "session");
}
