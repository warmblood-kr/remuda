use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, ExitCode, Stdio};

pub fn run(args: &[&str]) -> ExitCode {
    let mut status_path = None;
    let mut model = None;
    let mut mcp_config = None;
    let mut instructions = None;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--status" => status_path = args.get(i + 1).map(|s| (*s).to_string()),
            "--model" => model = args.get(i + 1).map(|s| (*s).to_string()),
            "--mcp-config" => mcp_config = args.get(i + 1).map(|s| (*s).to_string()),
            "--instructions" => instructions = args.get(i + 1).map(|s| (*s).to_string()),
            _ => return fail("unknown Codex app-server bridge argument"),
        }
        i += 2;
    }
    let Some(status_path) = status_path else {
        return fail("Codex app-server bridge needs --status");
    };
    let config = match mcp_config {
        Some(config) => match serde_json::from_str(&config) {
            Ok(config) => config,
            Err(error) => return fail(&format!("invalid Codex MCP config: {error}")),
        },
        None => json!({}),
    };
    let mut child = match Command::new("codex")
        .args(["app-server", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return fail(&format!("start Codex app-server: {error}")),
    };
    let mut input = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut output = BufReader::new(stdout);
    let mut next_id = 1;
    if let Err(error) = request(
        &mut input,
        next_id,
        "initialize",
        json!({
            "clientInfo": { "name": "remuda-butler", "version": env!("CARGO_PKG_VERSION") }
        }),
    ) {
        return fail(&error);
    }
    if let Err(error) = wait_response(&mut output, next_id, &status_path) {
        return fail(&error);
    }
    next_id += 1;
    if let Err(error) = notify(&mut input, "initialized", json!({})) {
        return fail(&error);
    }
    let mut start = json!({
        "cwd": std::env::current_dir().ok().and_then(|path| path.into_os_string().into_string().ok()),
        "ephemeral": true,
        "config": config,
        "approvalsReviewer": "auto_review",
    });
    if let Some(model) = model {
        start["model"] = Value::String(model)
    }
    if let Some(instructions) = instructions {
        start["developerInstructions"] = Value::String(instructions)
    }
    if let Err(error) = request(&mut input, next_id, "thread/start", start) {
        return fail(&error);
    }
    let thread = match wait_response(&mut output, next_id, &status_path) {
        Ok(result) => result,
        Err(error) => return fail(&error),
    };
    let thread_id = match thread.pointer("/thread/id").and_then(Value::as_str) {
        Some(id) => id.to_string(),
        None => return fail("Codex app-server did not return a thread id"),
    };
    let active_model = thread
        .pointer("/thread/model")
        .and_then(Value::as_str)
        .unwrap_or("?");
    write_status(&status_path, active_model, "?", "?");
    eprintln!("Codex app-server ready. Type a prompt and press Return.");
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.is_empty() {
            continue;
        }
        next_id += 1;
        if let Err(error) = request(
            &mut input,
            next_id,
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{ "type": "text", "text": line }],
            }),
        ) {
            return fail(&error);
        }
        if let Err(error) = wait_turn(&mut output, next_id, &status_path, active_model) {
            return fail(&error);
        }
    }
    let _ = child.kill();
    ExitCode::SUCCESS
}

fn request(input: &mut impl Write, id: u64, method: &str, params: Value) -> Result<(), String> {
    writeln!(
        input,
        "{}",
        json!({ "id": id, "method": method, "params": params })
    )
    .map_err(|e| e.to_string())?;
    input.flush().map_err(|e| e.to_string())
}

fn notify(input: &mut impl Write, method: &str, params: Value) -> Result<(), String> {
    writeln!(input, "{}", json!({ "method": method, "params": params }))
        .map_err(|e| e.to_string())?;
    input.flush().map_err(|e| e.to_string())
}

fn wait_response(output: &mut impl BufRead, id: u64, status: &str) -> Result<Value, String> {
    loop {
        let value = read_event(output)?;
        update_status(&value, status, "?");
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            return value
                .get("result")
                .cloned()
                .ok_or_else(|| value.to_string());
        }
    }
}

fn wait_turn(output: &mut impl BufRead, id: u64, status: &str, model: &str) -> Result<(), String> {
    let mut started = false;
    loop {
        let value = read_event(output)?;
        update_status(&value, status, model);
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            started = true
        }
        if started && value.get("method").and_then(Value::as_str) == Some("turn/completed") {
            return Ok(());
        }
        if let Some(text) = value.pointer("/params/delta").and_then(Value::as_str) {
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
    }
}

fn read_event(output: &mut impl BufRead) -> Result<Value, String> {
    let mut line = String::new();
    if output.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
        return Err("Codex app-server closed".into());
    }
    serde_json::from_str(&line).map_err(|e| format!("Codex app-server JSON: {e}"))
}

fn update_status(value: &Value, path: &str, model: &str) {
    if value.get("method").and_then(Value::as_str) != Some("thread/tokenUsage/updated") {
        return;
    }
    let usage = &value["params"]["tokenUsage"];
    let Some(window) = usage["modelContextWindow"].as_u64() else {
        return;
    };
    let last = &usage["last"];
    let used = last["inputTokens"].as_u64().unwrap_or(0)
        + last["cachedInputTokens"].as_u64().unwrap_or(0)
        + last["cacheWriteInputTokens"].as_u64().unwrap_or(0);
    write_status(path, model, &used.to_string(), &window.to_string());
}

fn write_status(path: &str, model: &str, used: &str, window: &str) {
    let percent = match (used.parse::<u64>(), window.parse::<u64>()) {
        (Ok(used), Ok(window)) if window > 0 => ((used * 100) / window).to_string(),
        _ => "?".into(),
    };
    let _ = std::fs::write(
        path,
        format!("MODEL:{model} CTX:{used} CTXWIN:{window} CTXPCT:{percent}\n"),
    );
}

fn fail(message: &str) -> ExitCode {
    eprintln!("remuda: Codex app-server bridge: {message}");
    ExitCode::FAILURE
}
