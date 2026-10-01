//! A `.cmd` file as the program of a session (through the pty) and of
//! `remuda.process.run`: what the program behind the batch file receives.
//! The fixture is shaped like an npm shim: it hands `%*` to a real program,
//! which records its argv. A session delivers the arguments cmd.exe reads as
//! text and refuses the rest in one line; `process.run` (std) delivers or
//! refuses; no argument ever runs a second command.
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
    /// The session route must refuse it; `process.run` may refuse or deliver.
    refused: bool,
}

const fn delivered(name: &'static str, args: &'static [&'static str]) -> Case {
    Case {
        name,
        args,
        refused: false,
    }
}

const fn refused(name: &'static str, args: &'static [&'static str]) -> Case {
    Case {
        name,
        args,
        refused: true,
    }
}

const CASES: &[Case] = &[
    delivered("none", &[]),
    delivered("plain", &["plain", "two words"]),
    delivered("quote", &["say \"hi\""]),
    // It holds a space, so the pty quotes it and cmd.exe reads `&` as text.
    delivered("amp-quoted", &["a&echo INJECTED>marker.txt"]),
    refused("newline", &["line1\necho INJECTED>marker.txt"]),
    refused("percent", &["100%", "%OS%"]),
    refused("amp-bare", &["a&echo.INJECTED>marker.txt"]),
    refused("quote-amp", &["\"&echo INJECTED>marker.txt&rem "]),
];

/// What the session route says when it refuses, on the first line.
const REFUSAL: &str = "cannot be passed to a .cmd or .bat program safely";
const NEXT: &str = "Next: start the .exe, or pass this text in a file.";

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
/// the word did not start the program; `strict` is the session route, where
/// the rule decides which cases are refused and how the refusal reads.
fn judge(case: &Case, dir: &Path, refusal: Option<&str>, strict: bool) -> Option<String> {
    let Some(message) = refusal else {
        if strict && case.refused {
            return Some("started, but this argument must be refused".into());
        }
        let finished = wait_for(&dir.join("done.txt"));
        let want = format!("{:?}", case.args);
        return match std::fs::read_to_string(dir.join("argv.txt")) {
            Ok(got) if got == want => None,
            Ok(got) => Some(format!("arguments arrived as {got}, want {want}")),
            Err(_) => Some(format!("no arguments recorded (finished: {finished})")),
        };
    };
    // Only the first line: a Rust error read through pcall carries a traceback.
    let first = message.lines().next().unwrap_or_default();
    if !case.refused {
        return Some(format!("refused: {first:?}"));
    }
    if strict && !(first.contains(REFUSAL) && first.contains(NEXT)) {
        return Some(format!("the refusal does not say what to do: {first:?}"));
    }
    // The argument may be a prompt: it is never echoed.
    case.args
        .iter()
        .any(|arg| first.contains(arg))
        .then(|| format!("the refusal shows the argument: {first:?}"))
}

/// Runs every case through `start` (Lua source for one case, given its argv
/// and directory; it must return `started` or raise) and reports them all.
fn run_cases(tag: &str, strict: bool, start: impl Fn(&str, &str, &str) -> String) {
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
        let verdict = judge(case, &dir, refusal, strict);
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
    run_cases("session", true, |name, argv, dir| {
        format!(r#"remuda.new("cmd-{name}", {argv}, {dir}) return "started""#)
    });
}

#[test]
fn a_cmd_file_through_process_run_gets_its_arguments_or_a_refusal() {
    run_cases("run", false, |_, argv, dir| {
        format!(
            r#"remuda.process.run({{ argv = {argv}, cwd = {dir}, timeout = 15 }})
            return "started""#
        )
    });
}

/// A bare name that the pty resolves to a `.cmd` through PATHEXT follows the
/// same rule as a path that ends in `.cmd`.
#[test]
fn a_bare_name_that_finds_a_cmd_file_follows_the_same_rule() {
    let scratch = Scratch::new("bare");
    let _daemon = spawn::Daemon::spawn(&scratch.0);
    let start = |case: &str, arg: &str| {
        let dir = scratch.0.join(case);
        write_fixture(&dir);
        let dir_lua = lua_string(dir.to_str().expect("utf-8 scratch path"));
        let said = outcome(
            &scratch.0,
            &format!(
                r#"remuda.new("bare-{case}", {{ "args", {} }}, {dir_lua}, {{ PATH = {dir_lua} }})
                return "started""#,
                lua_string(arg)
            ),
        );
        (dir, said)
    };

    let (dir, said) = start("plain", "two words");
    assert_eq!(said, "started");
    assert!(wait_for(&dir.join("done.txt")), "the .cmd did not run");
    let got = std::fs::read_to_string(dir.join("argv.txt")).expect("recorded argv");
    assert_eq!(got, r#"["two words"]"#);

    let (dir, said) = start("percent", "%OS%");
    let first = said.lines().next().unwrap_or_default();
    assert!(first.contains(REFUSAL) && first.contains(NEXT), "{said:?}");
    assert!(!dir.join("argv.txt").exists(), "the refused program ran");
}

/// cmd.exe may split a command NAME at `=`, `,` and `;`: a program in such a
/// directory must be the file that runs, or be refused, never the `a.cmd`
/// beside it. One more fact: an argument holding 0x1A (end of a batch file).
#[test]
fn a_program_path_with_a_cmd_separator_runs_that_file_or_is_refused() {
    let scratch = Scratch::new("prog");
    let _daemon = spawn::Daemon::spawn(&scratch.0);
    let mut wrong = Vec::new();
    let mut fact = |case: &str, dir: &Path, arg: &str, decoy: &Path| {
        let program = write_fixture(dir);
        let said = outcome(
            &scratch.0,
            &format!(
                r#"remuda.new("prog-{case}", {}, {}) return "started""#,
                lua_argv(&program, &[arg]),
                lua_string(dir.to_str().expect("utf-8 scratch path"))
            ),
        );
        let started = said == "started";
        let finished = started && wait_for(&dir.join("done.txt"));
        let argv = std::fs::read_to_string(dir.join("argv.txt")).ok();
        // A decoy needs a moment too when the right file never ran.
        let decoy_ran = wait_for_short(decoy);
        let _ = writeln!(
            std::io::stderr(),
            "FACT program {case}: said {said:?}; finished {finished}; argv {argv:?}; decoy {decoy_ran}"
        );
        if decoy_ran {
            wrong.push(format!("{case}: another file ran"));
        }
        if started && argv.as_deref() != Some(format!("{:?}", [arg]).as_str()) {
            wrong.push(format!(
                "{case}: started, but the arguments arrived as {argv:?}"
            ));
        }
        // A refusal says what to do about the path, on its first line.
        let first = said.lines().next().unwrap_or_default();
        if !started && !first.contains("Next: move or rename the folder, or start the .exe.") {
            wrong.push(format!("{case}: refused without the Next: step: {first:?}"));
        }
    };

    for (case, name) in [("equals", "a=b"), ("comma", "a,b"), ("semicolon", "a;b")] {
        let root = scratch.0.join(case);
        std::fs::create_dir_all(&root).expect("create case root");
        // What cmd.exe would start if it cut the name at the separator.
        let decoy = root.join("decoy.txt");
        std::fs::write(
            root.join("a.cmd"),
            "@echo off\r\n>\"%~dp0decoy.txt\" echo decoy\r\n",
        )
        .expect("write the decoy");
        fact(case, &root.join(name), "plain", &decoy);
    }
    let dir = scratch.0.join("sub");
    fact("ctrl-z", &dir, "a\u{1a}b", &dir.join("decoy.txt"));
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

fn wait_for_short(file: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if file.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}
