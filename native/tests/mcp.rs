//! MCP: what an agent driving remuda can reach, and what a refusal looks like.
//!
//! 정수님, 2026-09-10: *"MCP server는 제공을 하고, 필요에 따라서 tool을
//! 추가해나갈 수 있도록."* The claims worth exercising are that the served list
//! is the frame plus the image's registry (a tool defined in Lua is listed and
//! dispatched with no rebuild), that a call actually moves a session, and that a
//! refusal arrives as an error rather than as a successful empty result — the
//! failure a model turns into a confident story about an idle terminal.

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon, mcp};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(10);

/// The one `Request::New` field that is server-computed, never caller-supplied
/// on any surface — excluded from the wire/schema comparison below on purpose.
const SERVER_COMPUTED_FIELD: &str = "size";

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remuda-m{}-{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Start a daemon and return once it actually answers, not once it was spawned.
fn daemon_at(path: &Path) -> impl Drop {
    let serving = path.to_path_buf();
    std::thread::spawn(move || {
        let _ = daemon::serve(&serving);
    });
    let deadline = Instant::now() + PATIENCE;
    while remuda_native::ipc::connect(path).is_err() {
        assert!(Instant::now() < deadline, "daemon never bound {path:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    Cleanup(path.to_path_buf())
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// One round trip through the real dispatch.
fn ask(path: &Path, request: Value) -> Value {
    let line = mcp::handle(path, &request.to_string()).expect("a request with an id gets a reply");
    serde_json::from_str(&line).expect("the reply is JSON")
}

fn call(path: &Path, name: &str, arguments: Value) -> Value {
    ask(
        path,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": name, "arguments": arguments}}),
    )
}

fn text_of(reply: &Value) -> String {
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn listed(path: &Path) -> Vec<String> {
    let reply = ask(
        path,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    );
    let mut names: Vec<String> = reply["result"]["tools"]
        .as_array()
        .expect("tools is a list")
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect();
    names.sort();
    names
}

#[test]
fn the_tool_surface_is_the_frame_plus_the_image() {
    // Checked in both directions — a tool served but not declared fails, and a
    // name declared but never served fails too. On a fresh daemon the whole
    // list is `TOOLS` plus what `src/tools.lua` registers, which is `wait_for`.
    //
    // `attach` is absent: it hands a terminal to a human and an MCP client has
    // no terminal. The absence is a decision, recorded here so it stays one.
    let dir = scratch("surface");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path);

    let reply = ask(
        &path,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    );
    let served = listed(&path);

    let mut expected = mcp::TOOLS.map(str::to_string).to_vec();
    expected.push("wait_for".to_string());
    expected.sort();
    assert_eq!(
        served, expected,
        "the served tools and mcp::TOOLS + the image's registry disagree"
    );
    assert!(
        !served.contains(&"attach".to_string()),
        "attach is deliberately not an MCP tool — see mcp.rs module docs"
    );

    // Every declared tool needs a schema a client can actually call with.
    for tool in reply["result"]["tools"].as_array().unwrap() {
        assert!(
            tool["inputSchema"]["type"] == "object",
            "{} has no object inputSchema: {tool}",
            tool["name"]
        );
        assert!(
            tool["description"].as_str().is_some_and(|d| d.len() > 20),
            "{} needs a description a model can choose from",
            tool["name"]
        );
    }
}

#[test]
fn a_tool_call_moves_a_real_session() {
    // Arithmetic rather than a literal, per PRINCIPLES.md §4 — a pty echoes its
    // input, so finding a string that appears in the command proves only that
    // the echo happened.
    let dir = scratch("call");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path);

    let made = call(&path, "new", json!({"name": "driven", "command": ["sh"]}));
    assert_eq!(made["result"]["isError"], false, "new failed: {made}");

    let sent = call(
        &path,
        "send",
        json!({"session": "driven", "text": "echo $((6*7))-through-mcp"}),
    );
    assert_eq!(sent["result"]["isError"], false, "send failed: {sent}");

    // Read it back through the tool, not through the client, so the capture
    // path is the one under test.
    let deadline = Instant::now() + PATIENCE;
    loop {
        let seen = text_of(&call(&path, "capture", json!({"session": "driven"})));
        if seen.contains("42-through-mcp") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the shell never ran the command. screen:\n{seen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // And `ls` sees what the tools created — a second, independent view.
    let listed = text_of(&call(&path, "ls", json!({})));
    assert!(
        listed.contains("driven") && listed.contains("alive=true"),
        "ls does not show the session the tools made:\n{listed}"
    );
}

#[test]
fn a_refusal_is_an_error_not_an_empty_success() {
    // The shell failure this rules out: `$(remuda -e ...capture...)` is "" and the
    // caller proceeds over a session that was never created. A model does worse
    // than proceed — it explains the empty screen.
    let dir = scratch("refusal");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path);

    let reply = call(&path, "capture", json!({"session": "typo-in-the-name"}));
    assert_eq!(
        reply["result"]["isError"], true,
        "a missing session must be an error: {reply}"
    );
    let said = text_of(&reply);
    assert!(
        said.contains("no such session"),
        "the daemon's own words must survive into MCP, got: {said}"
    );
    assert!(
        !said.is_empty(),
        "an error with no words is the empty-success failure wearing a flag"
    );

    // An unknown tool is the same shape, so a client cannot read a typo as ok.
    // It is the registry that answers this now, so the words are Lua's.
    let bogus = call(&path, "capture-all", json!({}));
    assert_eq!(bogus["result"]["isError"], true, "unknown tool: {bogus}");
    assert!(
        text_of(&bogus).contains("no such tool"),
        "the registry must say what it did not find: {bogus}"
    );
}

#[test]
fn a_tool_defined_in_lua_is_listed_and_dispatched() {
    // Ruling ③, 정수님 2026-09-10: the MCP server is a frame and tools get added
    // as needed. The claim under test is that a Lua function marked exported
    // becomes a real MCP tool — listed with its arguments and callable — with no
    // rebuild between defining it and calling it.
    let dir = scratch("registry");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path);

    assert!(
        !listed(&path).contains(&"add".to_string()),
        "the tool exists before it was defined — this test proves nothing"
    );

    let defined = call(
        &path,
        "run_script",
        json!({"code": r#"
            remuda.tool{
              name = "add",
              about = "Add two numbers, to prove the registry carries arguments.",
              args = {a = "left addend", b = "right addend"},
              needs = {"a"},
              run = function(x) return tonumber(x.a) + tonumber(x.b or 0) end,
            }
            return "defined"
        "#}),
    );
    assert_eq!(defined["result"]["isError"], false, "define: {defined}");

    // Listed, with the schema the definition described.
    let reply = ask(
        &path,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    );
    let added = reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "add")
        .unwrap_or_else(|| panic!("`add` never reached tools/list: {reply}"));
    assert_eq!(added["inputSchema"]["required"], json!(["a"]), "{added}");
    assert_eq!(
        added["inputSchema"]["properties"]["b"]["description"], "right addend",
        "{added}"
    );

    // Dispatched. Arithmetic, per PRINCIPLES.md §4 — the answer cannot appear
    // in the echo of the question.
    let sum = call(&path, "add", json!({"a": "6", "b": "7"}));
    assert_eq!(sum["result"]["isError"], false, "add: {sum}");
    assert_eq!(text_of(&sum), "13", "{sum}");

    // An optional argument may be omitted; a needed one may not.
    assert_eq!(text_of(&call(&path, "add", json!({"a": "5"}))), "5");
    let missing = call(&path, "add", json!({"b": "5"}));
    assert_eq!(missing["result"]["isError"], true, "{missing}");
    assert!(text_of(&missing).contains("needs `a`"), "{missing}");

    // A string argument is escaped, not interpolated: this one is Lua source
    // that would run if the dispatch built its call by concatenation.
    let quoted = call(
        &path,
        "run_script",
        json!({"code": r#"
            remuda.tool{
              name = "echo",
              about = "Answer with the argument, so quoting can be checked.",
              args = {text = "anything at all"},
              needs = {"text"},
              run = function(x) return x.text end,
            }
            return "ok"
        "#}),
    );
    assert_eq!(quoted["result"]["isError"], false, "{quoted}");
    let hostile = r#"" .. os.exit() .. ""#;
    let echoed = call(&path, "echo", json!({"text": hostile}));
    assert_eq!(echoed["result"]["isError"], false, "{echoed}");
    assert_eq!(text_of(&echoed), hostile, "the argument was not escaped");
}

#[test]
fn run_script_reaches_the_one_shared_image() {
    // Ruling ②, 정수님 2026-09-10: one daemon, `RunScript` from outside and
    // inside alike. What makes it the *shared* image rather than a fresh
    // interpreter per call is that state set by one call is seen by the next —
    // and that the herd it drives is the herd `ls` shows.
    let dir = scratch("runscript");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path);

    let set = call(&path, "run_script", json!({"code": "fleet = 6 * 7"}));
    assert_eq!(set["result"]["isError"], false, "{set}");

    let read = call(&path, "run_script", json!({"code": "fleet"}));
    assert_eq!(
        text_of(&read),
        "42",
        "state did not survive the call: {read}"
    );

    // The image drives the same sessions the other tools do.
    let made = call(
        &path,
        "run_script",
        json!({"code": r#"return remuda.new("by-script", {"sh"})"#}),
    );
    assert_eq!(text_of(&made), "by-script", "{made}");
    assert!(
        text_of(&call(&path, "ls", json!({}))).contains("by-script"),
        "the script's session is not in the herd `ls` shows"
    );

    // A Lua error is a refusal, not a successful empty answer.
    let bad = call(&path, "run_script", json!({"code": "error('by hand')"}));
    assert_eq!(bad["result"]["isError"], true, "{bad}");
    assert!(text_of(&bad).contains("by hand"), "{bad}");

    // Empty source would `load` fine and return nothing, which is the empty
    // success this whole surface refuses.
    let empty = call(&path, "run_script", json!({"code": "   "}));
    assert_eq!(empty["result"]["isError"], true, "{empty}");
}

#[test]
fn wait_for_answers_a_screen_and_refuses_a_deadline() {
    // The tool the frame ships through its own registry, so the path is
    // exercised rather than merely present. Both directions matter: it must
    // answer with a matching screen, and it must FAIL on a deadline rather than
    // answer with a screen that does not match.
    let dir = scratch("waitfor");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path);

    assert_eq!(
        call(&path, "new", json!({"name": "waited", "command": ["sh"]}))["result"]["isError"],
        false
    );
    assert_eq!(
        call(
            &path,
            "send",
            json!({"session": "waited", "text": "echo $((6*7))-awaited"})
        )["result"]["isError"],
        false
    );

    let seen = call(
        &path,
        "wait_for",
        json!({"session": "waited", "pattern": "42%-awaited", "seconds": "10"}),
    );
    assert_eq!(seen["result"]["isError"], false, "wait_for: {seen}");
    assert!(text_of(&seen).contains("42-awaited"), "{seen}");

    // Negative control: the same call for something that will never appear must
    // go red, or the pass above could have come from any screen at all.
    let never = call(
        &path,
        "wait_for",
        json!({"session": "waited", "pattern": "99%-never", "seconds": "0.3"}),
    );
    assert_eq!(never["result"]["isError"], true, "{never}");
    assert!(text_of(&never).contains("never matched"), "{never}");
}

#[test]
fn the_real_binary_completes_a_handshake_over_stdio() {
    // `serve` is a stdlib line loop and the tests above bypass it. This one runs
    // the actual `remuda mcp` process so the loop, the framing and the flush are
    // not taken on faith — and it asserts a notification draws no reply, which
    // is the one place an extra line corrupts the stream.
    // The binary resolves its own socket from REMUDA_RUNTIME_DIR, so the daemon
    // has to be bound at exactly the path that resolution produces. Pointing the
    // child anywhere else would let it find the developer's real daemon and pass
    // while testing nothing — the socket has to be *ours* for the last assertion
    // in this test to mean anything.
    let dir = scratch("stdio");
    let path = daemon::socket_path_in(&dir, "default");
    let _daemon = daemon_at(&path);

    let mut child = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .arg("mcp")
        .env("REMUDA_RUNTIME_DIR", &dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn remuda mcp");

    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": "2025-06-18"}})
    )
    .unwrap();
    // A notification: no id, so it must produce no line at all.
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
    )
    .unwrap();
    drop(stdin);

    let out = child
        .wait_with_output()
        .expect("mcp exits when stdin closes");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();

    assert_eq!(
        lines.len(),
        2,
        "expected exactly two replies (the notification must draw none), got:\n{stdout}"
    );
    let first: Value = serde_json::from_str(lines[0]).expect("reply 1 is JSON");
    assert_eq!(first["id"], 1);
    assert!(
        first["result"]["serverInfo"]["name"] == "remuda",
        "initialize did not identify the server: {first}"
    );
    let second: Value = serde_json::from_str(lines[1]).expect("reply 2 is JSON");
    assert_eq!(second["id"], 2);
    // The frame's own five plus whatever the image registered — the shipped
    // binary must reflect the registry, not just the constant compiled into it.
    assert_eq!(
        second["result"]["tools"].as_array().map(Vec::len),
        Some(mcp::TOOLS.len() + 1),
        "the binary serves a different tool count than mcp::TOOLS + wait_for"
    );

    // The daemon really was the one answering, not a stub inside the child.
    match client::request(&path, &Request::List) {
        Ok(Response::Sessions(_)) => {}
        other => panic!("daemon unreachable after the handshake: {other:?}"),
    }
}

/// The keys `Request::New` actually carries on the wire, minus the one field
/// that is server-computed rather than caller-supplied.
fn wire_new_fields() -> BTreeSet<String> {
    let wire = serde_json::to_value(Request::New {
        name: None,
        command: vec![],
        size: Size::new(80, 24),
        cwd: None,
        env: None,
    })
    .expect("Request::New serializes");
    wire["New"]
        .as_object()
        .expect("New is externally tagged over an object")
        .keys()
        .filter(|k| *k != SERVER_COMPUTED_FIELD)
        .cloned()
        .collect()
}

/// The `new` tool's declared schema keys, the way a real MCP client would see
/// them via `tools/list` — not read out of `mcp.rs` source.
fn mcp_new_fields(path: &Path) -> BTreeSet<String> {
    let reply = ask(
        path,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    );
    let tools = reply["result"]["tools"]
        .as_array()
        .expect("tools is a list");
    let new_tool = tools
        .iter()
        .find(|t| t["name"] == "new")
        .expect("the new tool is listed");
    new_tool["inputSchema"]["properties"]
        .as_object()
        .expect("new has an object schema")
        .keys()
        .cloned()
        .collect()
}

#[test]
fn the_new_tools_schema_matches_what_the_wire_actually_carries() {
    // Today, before this test existed, BOTH surfaces silently lacked `cwd`/
    // `env` and nothing caught it. This compares them structurally so the two
    // cannot drift apart again without a test failing.
    let dir = scratch("schema-parity");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path);

    let wire = wire_new_fields();
    let mcp = mcp_new_fields(&path);
    assert_eq!(
        wire,
        mcp,
        "wire has {:?} that mcp lacks, mcp has {:?} that the wire lacks",
        wire.difference(&mcp).collect::<Vec<_>>(),
        mcp.difference(&wire).collect::<Vec<_>>()
    );

    // Negative control: drop one field from the wire side and confirm the
    // comparison actually fails, so a vacuous pass (e.g. two empty sets) is
    // ruled out.
    let mut short = wire;
    short.remove("cwd");
    assert_ne!(
        short, mcp,
        "removing a field from the expectation should have broken the match"
    );
}

#[test]
fn a_tool_call_can_set_cwd_and_env_on_the_launched_process() {
    // The launched process's OWN view — its own `pwd`, its own environment —
    // not the request/response round trip, and not typed input a pty would
    // just echo back (PRINCIPLES.md §4): `command` runs immediately as argv.
    let dir = scratch("new-cwd-env");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path);

    let target = scratch("new-cwd-env-target");
    let made = call(
        &path,
        "new",
        json!({
            "name": "probed",
            "command": ["sh", "-c", "pwd && echo $PROBE_VAR"],
            "cwd": target.to_string_lossy(),
            "env": {"PROBE_VAR": "remuda-env-probe-7f3a"},
        }),
    );
    assert_eq!(made["result"]["isError"], false, "new failed: {made}");

    let needle = target
        .file_name()
        .and_then(|n| n.to_str())
        .expect("scratch dir has a name")
        .to_string();

    let deadline = Instant::now() + PATIENCE;
    loop {
        let seen = text_of(&call(&path, "capture", json!({"session": "probed"})));
        if seen.contains(&needle) && seen.contains("remuda-env-probe-7f3a") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cwd/env never showed up on screen:\n{seen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
