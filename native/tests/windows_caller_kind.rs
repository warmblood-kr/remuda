//! What `remuda.caller().kind` is for a command typed in an ordinary Windows
//! terminal, outside every session. It should be `outside`. The parent chain
//! the daemon has to walk is printed beside each answer, as this test sees it.
#![cfg(windows)]

use remuda_core::protocol::{Request, Response};
use remuda_native::{client, daemon};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[path = "daemon_support/spawn.rs"]
mod spawn;

const ASK: &str = "return remuda.caller().kind";

/// A directory of our own, removed when the test ends, passing or not.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("remuda-k{}-{tag}", std::process::id()));
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

/// `program` with the environment that points a `remuda` child at the scratch
/// daemon, as `spawn::base_command` gives the daemon itself.
fn pointed_at(dir: &Path, program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .env("REMUDA_RUNTIME_DIR", dir)
        .env("HOME", dir.join("home"))
        .env("LOCALAPPDATA", dir.join("home"))
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .env_remove("XDG_CONFIG_HOME");
    command
}

/// What the command printed, with its error output when it printed none.
fn said(mut command: Command) -> String {
    match command.output() {
        Ok(output) => {
            let out = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let err = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if out.is_empty() {
                format!("(no output; {}; {err})", output.status)
            } else {
                out
            }
        }
        Err(error) => format!("(not started: {error})"),
    }
}

/// pid -> (parent pid, creation time, name), from the system's process list.
fn processes() -> HashMap<u32, (u32, u64, String)> {
    let list = "Get-CimInstance Win32_Process | ForEach-Object { $c = 0; \
        if ($_.CreationDate) { $c = $_.CreationDate.ToFileTimeUtc() }; \
        '{0} {1} {2} {3}' -f $_.ProcessId, $_.ParentProcessId, $c, $_.Name }";
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", list])
        .output()
        .expect("list processes");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.splitn(4, ' ');
            let pid = words.next()?.parse().ok()?;
            let parent = words.next()?.parse().ok()?;
            let created = words.next()?.parse().ok()?;
            Some((pid, (parent, created, words.next()?.trim().to_owned())))
        })
        .collect()
}

/// The chain above this test process, one hop per entry: is the parent in the
/// list at all, and was it created before its child (else the PID was reused).
fn chain() -> String {
    let table = processes();
    let mut hops = Vec::new();
    let mut pid = std::process::id();
    while hops.len() < 32 {
        let Some((parent, created, name)) = table.get(&pid) else {
            hops.push(format!("{pid} NOT IN LIST"));
            break;
        };
        let above = match table.get(parent) {
            Some((_, parent_created, _)) if parent_created <= created => "listed, older",
            Some(_) => "listed, YOUNGER (reused pid)",
            None => "NOT IN LIST",
        };
        hops.push(format!("{pid} {name} <- parent {parent} ({above})"));
        if *parent == pid || *parent == 0 {
            break;
        }
        pid = *parent;
    }
    hops.join("; ")
}

fn fact(line: std::fmt::Arguments) {
    // Straight to stderr, so the facts are in the log for a passing test too.
    let _ = writeln!(std::io::stderr(), "FACT caller {line}");
}

#[test]
fn a_command_outside_every_session_is_an_outside_caller() {
    let scratch = Scratch::new("out");
    let _daemon = spawn::Daemon::spawn(&scratch.0);
    let exe = env!("CARGO_BIN_EXE_remuda");
    fact(format_args!("chain above the test: {}", chain()));

    let mut plain = pointed_at(&scratch.0, exe);
    plain.args(["-s", "s", "-e", ASK]);
    let mut through_cmd = pointed_at(&scratch.0, "cmd");
    through_cmd.args(["/c", exe, "-s", "s", "-e", ASK]);
    let mut through_powershell = pointed_at(&scratch.0, "powershell");
    through_powershell.args([
        "-NoProfile",
        "-Command",
        &format!("& '{exe}' -s s -e '{ASK}'"),
    ]);

    for (route, command) in [
        ("plain", plain),
        ("cmd", through_cmd),
        ("powershell", through_powershell),
    ] {
        let kind = said(command);
        fact(format_args!("{route}: kind {kind:?}"));
        assert_eq!(
            kind, "outside",
            "{route} did not return the exact caller kind"
        );
    }
}

/// A process that a session's child starts in a new console, and that
/// outlives that child, is still not an outside caller.
#[test]
fn a_session_grandchild_that_outlives_its_parent_is_not_outside() {
    let scratch = Scratch::new("esc");
    let _daemon = spawn::Daemon::spawn(&scratch.0);
    let out = scratch.0.join("escape.txt");
    let script = scratch.0.join("escape.cmd");
    let body = format!(
        "@echo off\r\nping -n 4 127.0.0.1 >nul\r\n\"{}\" -s s -e \"{ASK}\" >\"{}\" 2>&1\r\n",
        env!("CARGO_BIN_EXE_remuda"),
        out.display()
    );
    std::fs::write(&script, body).expect("write escape.cmd");

    // `start` without /b gives the new process its own console, so it is not
    // closed with the session's pty when the session's child exits at once.
    let code = format!(
        "return remuda.new('escape', {{ 'cmd.exe', '/c', 'start', '', '/min', 'cmd.exe', '/c', {:?} }})",
        script.to_str().expect("utf-8 scratch path")
    );
    let request = Request::Eval { code, name: None };
    let socket = daemon::socket_path_in(&scratch.0, "s");
    let started = client::request(&socket, &request).expect("eval request");
    assert!(matches!(started, Response::Value(_)), "{started:?}");

    let deadline = Instant::now() + Duration::from_secs(40);
    let mut kind = String::new();
    while kind.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        kind = std::fs::read_to_string(&out)
            .unwrap_or_default()
            .trim()
            .to_owned();
    }
    fact(format_args!(
        "session grandchild after its parent exited: kind {kind:?}"
    ));
    assert_ne!(kind, "outside", "a session's descendant reads as outside");
}
