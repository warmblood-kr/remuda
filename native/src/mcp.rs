//! The manager, reachable from inside a session.
//!
//! 정수님, 2026-09-10: *"claude-code-ide.el 같은, 그 pty manager에서 claude code를
//! 구동하고, pty manager와 mcp 등으로 연결하는 장치를 두고."*
//!
//! Step 002 handed the five operations to a script running *outside*. This
//! hands the same vocabulary to whatever is running *inside* a session, which
//! is the last job Emacs is still doing for cc-butler: hosting an MCP server
//! that the Claude in a session calls back into.
//!
//! It is the third surface over one `Request`, and it cost no new access
//! control again — there is no raw write and no resize to expose because the
//! type has no such variant (`PRINCIPLES.md` §6). Invariant 3 is still the
//! daemon's, enforced when the call arrives, which is what makes it hold for a
//! caller that is a language model rather than a person with a shell.
//!
//! **`attach` is not exposed here, and that is not a rule against it.** Attach
//! exists to hand a terminal to a human; an MCP client has no terminal, and the
//! stdio this speaks JSON-RPC over is the very channel attach would turn into a
//! raw byte pipe. The operation's precondition cannot be met, so offering it
//! would be offering something that can only fail. `TOOLS` states the four
//! deliberately, and `tests/mcp.rs` asserts the live list against it in both
//! directions, so a fifth appearing without a decision fails the suite.
//!
//! **Transport is stdio only.** The tunnel and the node registry are their own
//! step with their own trust boundary. What this step does keep is the property
//! that makes that step cheap: a session name is an *address*, opaque to the
//! protocol, so qualifying it by node later changes the resolver and not the
//! shape of any message.

use crate::client;
use remuda_core::protocol::{Request, Response};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::Path;

/// The MCP protocol revision this speaks.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Every tool name exposed over MCP.
///
/// Four, not five: see the module docs for why `attach` is absent. Kept as a
/// constant so the test can assert the live `tools/list` against it in both
/// directions — a tool added without deciding to, or a name listed and never
/// served, both fail.
pub const TOOLS: [&str; 4] = ["capture", "ls", "new", "send"];

/// Serve MCP over stdin/stdout until the client closes the stream.
pub fn serve(socket: &Path) -> std::io::Result<()> {
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // A notification has no id and takes no reply. Answering one is a
        // protocol error, so `handle` returns None rather than an empty object.
        if let Some(reply) = handle(socket, &line) {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// One request in, at most one reply out.
///
/// Public so the tests can drive the real dispatch against a real daemon
/// without a subprocess between them. `serve` above is then the only untested
/// part, and it is a stdlib line loop; one test still runs the actual binary
/// end to end so that loop is not taken on faith either.
pub fn handle(socket: &Path, line: &str) -> Option<String> {
    let request: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        // -32700 is JSON-RPC's parse error. There is no id to answer with,
        // because the id was in the text that did not parse.
        Err(e) => {
            return Some(error_reply(
                Value::Null,
                -32700,
                &format!("parse error: {e}"),
            ))
        }
    };
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");

    // No id means a notification: act if it matters, never reply.
    let id = id?;

    let reply = match method {
        "initialize" => ok_reply(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "remuda", "version": env!("CARGO_PKG_VERSION")},
            }),
        ),
        "tools/list" => ok_reply(id, json!({"tools": descriptors()})),
        "tools/call" => {
            let params = request.get("params").cloned().unwrap_or(Value::Null);
            call(socket, id, &params)
        }
        "ping" => ok_reply(id, json!({})),
        other => error_reply(id, -32601, &format!("method not found: {other}")),
    };
    Some(reply)
}

/// Run one tool call.
///
/// A refusal from the daemon comes back as `isError: true` with the daemon's
/// own words, never as a successful result with empty content. That distinction
/// is the whole reason this is not a shell pipeline: `$(remuda capture nosuch)`
/// yields `""`, and a caller — especially a model — reads an empty screen as an
/// idle session and narrates a plausible story about it.
fn call(socket: &Path, id: Value, params: &Value) -> String {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let text = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    let request = match name {
        "ls" => Request::List,
        "new" => Request::New {
            name: text("name"),
            command: args
                .get("command")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            size: crate::terminal_size(),
        },
        "send" => Request::SendLine {
            name: text("session"),
            text: text("text"),
        },
        "capture" => Request::Capture {
            name: text("session"),
        },
        other => return ok_reply(id, tool_error(&format!("no such tool: {other}"))),
    };

    match client::request(socket, &request) {
        Err(e) => ok_reply(id, tool_error(&format!("{e}"))),
        Ok(Response::Error(reason)) => ok_reply(id, tool_error(&reason)),
        Ok(Response::Screen(screen)) => ok_reply(id, tool_text(&screen)),
        // `TOOLS` exposes no `eval`, deliberately: MCP is the door for the
        // agent running *inside* a session, and handing that agent the image
        // would let it rewrite the manager holding it. So this is unreachable
        // — spelled out rather than folded into a wildcard, so that adding an
        // eval tool becomes a decision made here instead of one inherited.
        Ok(Response::Value(value)) => ok_reply(id, tool_text(&value)),
        Ok(Response::Ok) => ok_reply(id, tool_text("ok")),
        Ok(Response::Sessions(sessions)) => {
            let rows: Vec<String> = sessions
                .iter()
                .map(|s| {
                    format!(
                        "{}\t{}x{}\talive={}\tidle={:.0}s",
                        s.name,
                        s.size.cols(),
                        s.size.rows(),
                        s.alive,
                        s.idle.as_secs_f64()
                    )
                })
                .collect();
            ok_reply(id, tool_text(&rows.join("\n")))
        }
    }
}

/// What `tools/list` returns. Order follows `TOOLS`, which is sorted.
fn descriptors() -> Vec<Value> {
    let session_arg = |verb: &str| {
        json!({
            "type": "object",
            "properties": {"session": {"type": "string", "description": format!("Session to {verb}.")}},
            "required": ["session"],
        })
    };
    vec![
        json!({
            "name": "capture",
            "description": "Read a session's screen as text. Does not take the session over, \
                            so it works while a human is attached.",
            "inputSchema": session_arg("read"),
        }),
        json!({
            "name": "ls",
            "description": "Every session this node holds, with size, liveness and idle time.",
            "inputSchema": {"type": "object", "properties": {}},
        }),
        json!({
            "name": "new",
            "description": "Start a session. `command` is argv; omit it for the user's shell.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Name for the new session."},
                    "command": {"type": "array", "items": {"type": "string"}},
                },
                "required": ["name"],
            },
        }),
        json!({
            "name": "send",
            "description": "Deliver one line to a session as an indivisible act. \
                            Refused while a human is attached to it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": {"type": "string"},
                    "text": {"type": "string", "description": "The line to send."},
                },
                "required": ["session", "text"],
            },
        }),
    ]
}

fn tool_text(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": false})
}

fn tool_error(reason: &str) -> Value {
    json!({"content": [{"type": "text", "text": reason}], "isError": true})
}

fn ok_reply(id: Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

fn error_reply(id: Value, code: i32, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}
