use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

pub fn run(args: &[&str]) -> ExitCode {
    let Some((&"--status", status)) = args.first().zip(args.get(1)) else {
        eprintln!("remuda: Codex TUI needs --status PATH");
        return ExitCode::FAILURE;
    };
    let socket = std::env::temp_dir().join(format!("remuda-codex-{}.sock", std::process::id()));
    let address = format!("unix://{}", socket.display());
    let mut server = match Command::new("codex")
        .args(["app-server", "--listen", &address])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return fail(error),
    };
    if !wait_for_socket(&socket) {
        let _ = server.kill();
        return fail("Codex app-server did not bind its local socket");
    }
    let monitor_socket = socket.clone();
    let monitor_status = status.to_string();
    std::thread::spawn(move || monitor(&monitor_socket, &monitor_status));
    let result = Command::new("codex")
        .args(["--remote", &address, "--approve-for-me"])
        .status();
    let _ = server.kill();
    let _ = std::fs::remove_file(socket);
    match result {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => fail(error),
    }
}

fn wait_for_socket(socket: &std::path::Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if UnixStream::connect(socket).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

fn monitor(socket: &std::path::Path, status: &str) {
    let Ok(stream) = UnixStream::connect(socket) else {
        return;
    };
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let mut writer = writer;
    let _ = writeln!(
        writer,
        "{}",
        json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"remuda-butler","version":env!("CARGO_PKG_VERSION")}}})
    );
    let _ = writer.flush();
    let mut model = "?".to_string();
    for line in BufReader::new(stream).lines().flatten() {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if event.get("id").and_then(Value::as_u64) == Some(1) {
            let _ = writeln!(writer, "{}", json!({"method":"initialized","params":{}}));
            let _ = writer.flush();
        }
        if event.get("method").and_then(Value::as_str) == Some("thread/started") {
            if let Some(value) = event
                .pointer("/params/thread/model")
                .and_then(Value::as_str)
            {
                model = value.into();
            }
        }
        if event.get("method").and_then(Value::as_str) != Some("thread/tokenUsage/updated") {
            continue;
        }
        let usage = &event["params"]["tokenUsage"];
        let Some(window) = usage["modelContextWindow"].as_u64() else {
            continue;
        };
        let last = &usage["last"];
        let used = ["inputTokens", "cachedInputTokens", "cacheWriteInputTokens"]
            .iter()
            .map(|key| last[*key].as_u64().unwrap_or(0))
            .sum::<u64>();
        let percent = used * 100 / window;
        let _ = std::fs::write(
            status,
            format!("MODEL:{model} CTX:{used} CTXWIN:{window} CTXPCT:{percent}\n"),
        );
    }
}

fn fail(error: impl std::fmt::Display) -> ExitCode {
    eprintln!("remuda: Codex TUI: {error}");
    ExitCode::FAILURE
}
