//! A `.cmd` file as the program of a session (through the pty) and of
//! `remuda.process.run`: what the program behind the batch file receives.
//! The fixture is shaped like an npm shim: it hands `%*` to a real program,
//! which records its argv. Each case states what SHOULD hold: plain arguments
//! arrive intact; hostile ones arrive intact or are refused in one line; no
//! argument ever runs a second command.
#![cfg(windows)]

use remuda_core::protocol::{Request, Response};
use remuda_native::{client, daemon};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[path = "daemon_support/spawn.rs"]
mod spawn;

/// What an injected command would leave in the case's directory.
const MARKER: &str = "marker.txt";

struct Case {
    name: &'static str,
    args: &'static [&'static str],
    /// A hostile case may be refused; a plain one must be delivered.
    hostile: bool,
}

const CASES: &[Case] = &[
    Case {
        name: "none",
        args: &[],
        hostile: false,
    },
    Case {
        name: "plain",
        args: &["plain", "two words"],
        hostile: false,
    },
    Case {
        name: "quote",
        args: &["say \"hi\""],
        hostile: true,
    },
    Case {
        name: "newline",
        args: &["line1\necho INJECTED>marker.txt"],
        hostile: true,
    },
    Case {
        name: "percent",
        args: &["100%", "%OS%"],
        hostile: true,
    },
    Case {
        name: "amp",
        args: &["a&echo INJECTED>marker.txt"],
        hostile: true,
    },
    Case {
        name: "quote-amp",
        args: &["\"&echo INJECTED>marker.txt&rem "],
        hostile: true,
    },
];

/// Not a test of its own: the program behind the fixture. The batch file
/// starts this test binary again with the arguments after `--`, and this
/// writes them down. Without the variable it does nothing.
#[test]
fn argv_recorder() {
    let Some(out) = std::env::var_os("REMUDA_ARGV_OUT") else {
        return;
    };
    let args: Vec<String> = std::env::args()
        .skip_while(|arg| arg != "--")
        .skip(1)
        .collect();
    std::fs::write(out, format!("{args:?}")).expect("record argv");
}

/// A directory of our own, removed when the test ends, passing or not.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("remuda-b{}-{tag}", std::process::id()));
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

/// The case's own directory with its `args.cmd`; returns the batch file.
fn write_fixture(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).expect("create case directory");
    let recorder = std::env::current_exe().expect("locate this test binary");
    let body = format!(
        "@echo off\r\n\
         set \"REMUDA_ARGV_OUT=%~dp0argv.txt\"\r\n\
         \"{}\" --exact argv_recorder --nocapture -- %*\r\n\
         >\"%~dp0done.txt\" echo done\r\n",
        recorder.display()
    );
    let file = dir.join("args.cmd");
    std::fs::write(&file, body).expect("write args.cmd");
    file
}

fn lua_string(text: &str) -> String {
    format!("{text:?}")
}

/// `{ "<args.cmd>", "<arg>", ... }` as Lua source.
fn lua_argv(program: &Path, args: &[&str]) -> String {
    let mut words = vec![lua_string(program.to_str().expect("utf-8 scratch path"))];
    words.extend(args.iter().map(|arg| lua_string(arg)));
    format!("{{ {} }}", words.join(", "))
}

/// What `code` returns in the daemon under `runtime`, or the Lua error it
/// raises, as text.
fn outcome(runtime: &Path, code: &str) -> String {
    let request = Request::Eval {
        code: format!("local _, value = pcall(function() {code} end)\nreturn tostring(value)"),
        name: None,
    };
    let socket = daemon::socket_path_in(runtime, "s");
    match client::request(&socket, &request).expect("eval request") {
        Response::Value(value) => value,
        other => format!("{other:?}"),
    }
}

fn wait_for(file: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if file.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// What is wrong with one case, if anything. `refusal` is the error text when
/// the word did not start the program.
fn judge(case: &Case, dir: &Path, refusal: Option<&str>) -> Option<String> {
    if let Some(message) = refusal {
        if !case.hostile {
            return Some(format!("refused: {message:?}"));
        }
        if message.contains('\n') {
            return Some(format!("the refusal is not one line: {message:?}"));
        }
    } else {
        let finished = wait_for(&dir.join("done.txt"));
        let want = format!("{:?}", case.args);
        match std::fs::read_to_string(dir.join("argv.txt")) {
            Ok(got) if got == want => {}
            Ok(got) => return Some(format!("arguments arrived as {got}, want {want}")),
            Err(_) => return Some(format!("no arguments recorded (finished: {finished})")),
        }
    }
    None
}

/// Runs every case through `start` (Lua source for one case, given its argv
/// and directory; it must return `started` or raise) and reports them all.
fn run_cases(tag: &str, start: impl Fn(&str, &str, &str) -> String) {
    let scratch = Scratch::new(tag);
    let _daemon = spawn::Daemon::spawn(&scratch.0);
    let mut wrong = Vec::new();
    for case in CASES {
        let dir = scratch.0.join(case.name);
        let program = write_fixture(&dir);
        let code = start(
            case.name,
            &lua_argv(&program, case.args),
            &lua_string(dir.to_str().expect("utf-8 scratch path")),
        );
        let said = outcome(&scratch.0, &code);
        let refusal = (said != "started").then_some(said.as_str());
        let verdict = judge(case, &dir, refusal);
        let injected = dir.join(MARKER).exists();
        // Straight to stderr, so the facts are in the log for a passing case too.
        let _ = writeln!(
            std::io::stderr(),
            "FACT {tag} {}: said {said:?}; argv {:?}; marker {injected}; wrong {verdict:?}",
            case.name,
            std::fs::read_to_string(dir.join("argv.txt")).ok(),
        );
        if let Some(verdict) = verdict {
            wrong.push(format!("{}: {verdict}", case.name));
        }
        // Whatever else happened: refused, mangled or delivered.
        if injected {
            wrong.push(format!("{}: an argument ran a second command", case.name));
        }
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

#[test]
fn a_cmd_file_as_a_session_gets_its_arguments_or_a_refusal() {
    run_cases("session", |name, argv, dir| {
        format!(r#"remuda.new("cmd-{name}", {argv}, {dir}) return "started""#)
    });
}

#[test]
fn a_cmd_file_through_process_run_gets_its_arguments_or_a_refusal() {
    run_cases("run", |_, argv, dir| {
        format!(
            r#"remuda.process.run({{ argv = {argv}, cwd = {dir}, timeout = 15 }})
            return "started""#
        )
    });
}
