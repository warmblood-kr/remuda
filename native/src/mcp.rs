//! The manager, reachable over MCP — by an agent outside remuda or inside it.
//!
//! **A frame, not a fixed set.** 정수님, 2026-09-10: *"MCP server는 제공을 하고,
//! 필요에 따라서 tool을 추가해나갈 수 있도록."* `TOOLS` is what the frame ships;
//! `tools/list` is that plus whatever `src/tools.lua` has marked exported, and
//! `tools/call` dispatches into the image for anything `TOOLS` does not name. A
//! new tool is a Lua function, not a rebuild. `steps/013` records the ruling.
//!
//! **`attach` is not exposed here, and that is not a rule against it.** Attach
//! exists to hand a terminal to a human; an MCP client has no terminal, and the
//! stdio this speaks JSON-RPC over is the very channel attach would turn into a
//! raw byte pipe. The precondition cannot be met, so offering it would be
//! offering something that can only fail.
//!
//! **Transport is stdio only.** A session name is an *address*, opaque to the
//! protocol, so qualifying it by node later changes the resolver and not the
//! shape of any message.

use crate::client;
use remuda_core::protocol::{Request, Response};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::Path;

/// The MCP protocol revision this speaks.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The tools the frame itself serves. Not the whole list — `tools/list` adds
/// the image's registry to these. A constant so a test can assert both ways:
/// an undecided addition here and an unserved name each fail.
pub const TOOLS: [&str; 5] = ["capture", "ls", "new", "run_script", "send"];

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

/// One request in, at most one reply out. Public so tests can drive the real
/// dispatch against a real daemon with no subprocess in between.
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
        "tools/list" => ok_reply(id, json!({"tools": descriptors(socket)})),
        "tools/call" => {
            let params = request.get("params").cloned().unwrap_or(Value::Null);
            call(socket, id, &params)
        }
        "ping" => ok_reply(id, json!({})),
        other => error_reply(id, -32601, &format!("method not found: {other}")),
    };
    Some(reply)
}

/// Run one tool call. Caution: a refusal must come back as `isError: true` with
/// the daemon's own words, never as success with empty content — a model reads
/// an empty screen as an idle session and narrates a story about it.
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
            // Optional: an agent spawning five workers should not have to
            // invent five unique strings. Omitted, argv[0] names it.
            name: args.get("name").and_then(Value::as_str).map(str::to_string),
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
        // The general door. Empty source is refused rather than run: `load("")`
        // succeeds and returns nothing, which would answer an obvious mistake
        // with a successful empty result.
        "run_script" if text("code").trim().is_empty() => {
            return ok_reply(id, tool_error("run_script needs `code`"))
        }
        "run_script" => Request::Eval {
            code: text("code"),
            name: None,
        },
        // Anything the frame does not name is the image's to answer, including
        // "no such tool" — the registry is what knows, so it is what says so.
        other => Request::Eval {
            code: format!(
                "remuda._call({}, {})",
                lua_string(other),
                lua_literal(&args)
            ),
            name: None,
        },
    };

    match client::request(socket, &request) {
        Err(e) => ok_reply(id, tool_error(&format!("{e}"))),
        Ok(Response::Error(reason)) => ok_reply(id, tool_error(&reason)),
        Ok(Response::Screen(screen)) => ok_reply(id, tool_text(&screen)),
        // No MCP tool asks for `CaptureStyled`, so this never arrives — spelled
        // out rather than a wildcard for the same reason as `Response::Value`
        // below: a real caller appearing later is a compile error to notice.
        Ok(Response::StyledScreen { .. }) => {
            ok_reply(id, tool_error("styled capture is not exposed over MCP"))
        }
        // This arm used to say `TOOLS` exposes no eval, deliberately — because
        // MCP was the door for the agent running *inside* a session, and giving
        // it the image would let it rewrite the manager holding it. **The
        // premise was wrong, and 정수님 corrected it on 2026-09-10**: he meant an
        // agent outside remuda driving it, and said 내부에서 스스로를 변경할 수
        // 있어도 돼요 — one daemon, `run_script` reachable from either side, the
        // precedent being that claude-code-ide.el offers `emacs eval` too. So
        // the arm the old comment called unreachable is now the common one, and
        // the decision it left open was made rather than dropped. `steps/013`.
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

/// What `tools/list` returns: the frame's own, then the image's registry. A
/// registered name that collides with `TOOLS` is dropped — the frame's dispatch
/// would win anyway, and a listed tool that cannot be reached is worse.
fn descriptors(socket: &Path) -> Vec<Value> {
    let mut all = frame();
    for tool in registered(socket) {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
        if !TOOLS.contains(&name) {
            all.push(tool);
        }
    }
    all
}

/// Whatever Lua has marked exported. An unreachable or broken image yields
/// nothing rather than failing the listing: `tools/list` has no per-tool error
/// channel, and the same failure is loud on the next `tools/call`, which does.
fn registered(socket: &Path) -> Vec<Value> {
    let request = Request::Eval {
        code: "remuda._descriptors()".to_string(),
        name: None,
    };
    match client::request(socket, &request) {
        Ok(Response::Value(json)) => serde_json::from_str(&json).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The five the frame serves itself. Order follows `TOOLS`, which is sorted.
fn frame() -> Vec<Value> {
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
            "description": "Start a session, answering with the name it got. `command` is argv; \
                            omit it for the user's shell. Omit `name` and argv[0] names it, \
                            de-duplicated: claude, claude-2, claude-3.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Name for the new session."},
                    "command": {"type": "array", "items": {"type": "string"}},
                },
            },
        }),
        json!({
            "name": "run_script",
            "description": "Evaluate Lua in the daemon's long-lived image — the same interpreter \
                            `remuda -e` and every script share, so state persists between calls. \
                            The general door: `remuda.tool{…}` here adds a tool to this list.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "Lua source. An expression answers with its value."},
                },
                "required": ["code"],
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

/// One JSON value as Lua source, for the `remuda._call(…)` the registry is
/// dispatched with. Every string is escaped rather than interpolated, so an
/// argument cannot close its quote and become code.
fn lua_literal(value: &Value) -> String {
    let joined = |items: Vec<String>| format!("{{{}}}", items.join(", "));
    match value {
        Value::Null => "nil".to_string(),
        Value::Bool(yes) => yes.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => lua_string(s),
        Value::Array(items) => joined(items.iter().map(lua_literal).collect()),
        // Bracketed keys, so a key that is not a Lua identifier still lands.
        Value::Object(map) => joined(
            map.iter()
                .map(|(k, v)| format!("[{}] = {}", lua_string(k), lua_literal(v)))
                .collect(),
        ),
    }
}

/// A Lua string literal. `\ddd` is decimal in Lua; non-ASCII bytes pass through
/// as they are, because a Lua string is bytes and the source is already UTF-8.
fn lua_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\{:03}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
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

#[cfg(test)]
mod tests {
    use super::{lua_literal, lua_string};
    use serde_json::json;

    #[test]
    fn arguments_become_a_lua_table() {
        assert_eq!(lua_literal(&json!(null)), "nil");
        assert_eq!(lua_literal(&json!(true)), "true");
        assert_eq!(lua_literal(&json!(30)), "30");
        assert_eq!(lua_literal(&json!(["a", 1])), r#"{"a", 1}"#);
        assert_eq!(
            lua_literal(&json!({"session": "build"})),
            r#"{["session"] = "build"}"#
        );
    }

    #[test]
    fn a_string_cannot_close_its_quote_and_become_code() {
        // The whole reason this function exists rather than `format!`: an MCP
        // client is not trusted to send an argument that is not an attack.
        let escaped = lua_string(r#"x" ; os.exit() --"#);
        assert!(
            escaped.starts_with(r#""x\" "#),
            "the quote survived: {escaped}"
        );
        assert_eq!(escaped.matches('"').count(), 3, "{escaped}");
    }

    #[test]
    fn a_control_character_becomes_a_decimal_escape() {
        // `\ddd` is DECIMAL in Lua, not hex. Getting that wrong is silent: the
        // string still parses and carries a different byte.
        assert_eq!(lua_string("a\nb\tc"), r#""a\nb\009c""#);
        assert_eq!(lua_string("한"), r#""한""#);
    }
}
