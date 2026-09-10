//! MCP: what a program inside a session can reach, and what a refusal looks like.
//!
//! 정수님, 2026-09-10: *"그 pty manager에서 claude code를 구동하고, pty manager와
//! mcp 등으로 연결하는 장치를 두고."* The claims worth exercising are that the tool
//! surface is exactly the decided set (a third binding must not widen the API),
//! that a call actually moves a session, and that a refusal arrives as an error
//! rather than as a successful empty result — the failure a model turns into a
//! confident story about an idle terminal.

use remuda_core::protocol::{Request, Response};
use remuda_native::{client, daemon, mcp};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(10);

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

#[test]
fn the_tool_surface_is_exactly_the_decided_set() {
    // The property that makes a third surface safe: the vocabulary is `Request`
    // and nothing else. Checked in both directions — a tool served but not
    // declared fails, and a name declared but never served fails too.
    //
    // Four, not five. `attach` hands a terminal to a human and an MCP client has
    // no terminal; the absence is a decision, recorded here so it stays one.
    let dir = scratch("surface");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path);

    let reply = ask(
        &path,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    );
    let mut served: Vec<String> = reply["result"]["tools"]
        .as_array()
        .expect("tools is a list")
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect();
    served.sort();

    assert_eq!(
        served,
        mcp::TOOLS.to_vec(),
        "the served tools and mcp::TOOLS disagree"
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
    let bogus = call(&path, "capture-all", json!({}));
    assert_eq!(bogus["result"]["isError"], true, "unknown tool: {bogus}");
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
    assert_eq!(
        second["result"]["tools"].as_array().map(Vec::len),
        Some(mcp::TOOLS.len()),
        "the binary serves a different tool count than mcp::TOOLS"
    );

    // The daemon really was the one answering, not a stub inside the child.
    match client::request(&path, &Request::List) {
        Ok(Response::Sessions(_)) => {}
        other => panic!("daemon unreachable after the handshake: {other:?}"),
    }
}
