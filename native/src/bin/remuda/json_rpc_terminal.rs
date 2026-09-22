use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, ExitCode, Stdio};

#[allow(clippy::too_many_lines)]
pub fn run(args: &[&str]) -> ExitCode {
    let mut status_path = None;
    let mut spec_text = None;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--status" => status_path = args.get(i + 1).map(|s| (*s).to_string()),
            "--spec" => spec_text = args.get(i + 1).map(|s| (*s).to_string()),
            _ => return fail("unknown JSON-RPC terminal argument"),
        }
        i += 2;
    }
    let Some(status_path) = status_path else {
        return fail("JSON-RPC terminal needs --status");
    };
    let spec: Value = match spec_text {
        Some(spec) => match serde_json::from_str(&spec) {
            Ok(spec) => spec,
            Err(error) => return fail(&format!("invalid JSON-RPC terminal spec: {error}")),
        },
        None => return fail("JSON-RPC terminal needs --spec"),
    };
    let Some(program) = spec["program"].as_array() else {
        return fail("spec needs program");
    };
    let Some((command, args)) = program.split_first() else {
        return fail("spec program is empty");
    };
    let Some(command) = command.as_str() else {
        return fail("spec program must contain strings");
    };
    let args: Option<Vec<&str>> = args.iter().map(Value::as_str).collect();
    let Some(args) = args else {
        return fail("spec program must contain strings");
    };
    let mut child = match Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return fail(&format!("start JSON-RPC program: {error}")),
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
    let Some(start_method) = spec.pointer("/start/method").and_then(Value::as_str) else {
        return fail("spec needs start method");
    };
    let start_params = spec
        .pointer("/start/params")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let Err(error) = request(&mut input, next_id, start_method, start_params) {
        return fail(&error);
    }
    let thread = match wait_response(&mut output, next_id, &status_path) {
        Ok(result) => result,
        Err(error) => return fail(&error),
    };
    let thread_id = match thread
        .pointer(spec["thread_id"].as_str().unwrap_or("/thread/id"))
        .and_then(Value::as_str)
    {
        Some(id) => id.to_string(),
        None => return fail("JSON-RPC start response did not return an id"),
    };
    let active_model = thread
        .pointer(spec["model"].as_str().unwrap_or("/thread/model"))
        .and_then(Value::as_str)
        .unwrap_or("?");
    write_status(&status_path, active_model, "?", "?");
    eprintln!("JSON-RPC agent ready. Type a prompt and press Return.");
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.is_empty() {
            continue;
        }
        next_id += 1;
        let Some(turn_method) = spec.pointer("/turn/method").and_then(Value::as_str) else {
            return fail("spec needs turn method");
        };
        let turn = substitute(
            spec.pointer("/turn/params")
                .cloned()
                .unwrap_or_else(|| json!({})),
            &thread_id,
            &line,
        );
        if let Err(error) = request(&mut input, next_id, turn_method, turn) {
            return fail(&error);
        }
        if let Err(error) = wait_turn(&mut output, next_id, &status_path, active_model, &spec) {
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

fn wait_response(output: &mut impl BufRead, id: u64, _status: &str) -> Result<Value, String> {
    loop {
        let value = read_event(output)?;
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            return value
                .get("result")
                .cloned()
                .ok_or_else(|| value.to_string());
        }
    }
}

fn wait_turn(
    output: &mut impl BufRead,
    id: u64,
    status: &str,
    model: &str,
    spec: &Value,
) -> Result<(), String> {
    let mut started = false;
    loop {
        let value = read_event(output)?;
        update_status(&value, status, model, spec);
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            started = true
        }
        if started && value.get("method").and_then(Value::as_str) == spec["turn_complete"].as_str()
        {
            return Ok(());
        }
        if let Some(text) = spec["output_text"]
            .as_str()
            .and_then(|path| value.pointer(path))
            .and_then(Value::as_str)
        {
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

fn update_status(value: &Value, path: &str, model: &str, spec: &Value) {
    if value.get("method").and_then(Value::as_str)
        != spec.pointer("/telemetry/method").and_then(Value::as_str)
    {
        return;
    }
    let Some(window) = spec
        .pointer("/telemetry/window")
        .and_then(Value::as_str)
        .and_then(|key| value.pointer(key))
        .and_then(Value::as_u64)
    else {
        return;
    };
    let used = spec
        .pointer("/telemetry/used")
        .and_then(Value::as_array)
        .map_or(0, |paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|key| value.pointer(key))
                .filter_map(Value::as_u64)
                .sum()
        });
    write_status(path, model, &used.to_string(), &window.to_string());
}

fn substitute(value: Value, thread_id: &str, input: &str) -> Value {
    match value {
        Value::String(value) if value == "$thread_id" => Value::String(thread_id.into()),
        Value::String(value) if value == "$input" => Value::String(input.into()),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| substitute(value, thread_id, input))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, substitute(value, thread_id, input)))
                .collect(),
        ),
        value => value,
    }
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
    eprintln!("remuda: JSON-RPC terminal: {message}");
    ExitCode::FAILURE
}
