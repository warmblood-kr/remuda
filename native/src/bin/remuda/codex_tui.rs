use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
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

#[cfg(unix)]
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

/// Codex integration needs a Unix domain socket; there's no Windows
/// equivalent wired up here, so `run` fails fast via this always-false
/// stub instead of hanging.
#[cfg(not(unix))]
fn wait_for_socket(_socket: &std::path::Path) -> bool {
    false
}

#[cfg(unix)]
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
        update_status(&event, &mut model, status);
    }
}

#[cfg(not(unix))]
fn monitor(_socket: &std::path::Path, _status: &str) {}

/// Publish a complete record as soon as Codex announces its thread.  Context
/// capacity is intentionally unknown until its first token-usage event: the
/// App Server does not include it in `thread/started`.
fn update_status(event: &Value, model: &mut String, status: &str) {
    match event.get("method").and_then(Value::as_str) {
        Some("thread/started") => {
            if let Some(value) = event
                .pointer("/params/thread/model")
                .and_then(Value::as_str)
            {
                *model = value.into();
            }
            write_status(status, model, "?", "?");
        }
        Some("thread/tokenUsage/updated") => {
            let usage = &event["params"]["tokenUsage"];
            let Some(window) = usage["modelContextWindow"].as_u64() else {
                return;
            };
            let last = &usage["last"];
            let used = ["inputTokens", "cachedInputTokens", "cacheWriteInputTokens"]
                .iter()
                .map(|key| last[*key].as_u64().unwrap_or(0))
                .sum::<u64>();
            write_status(status, model, &used.to_string(), &window.to_string());
        }
        _ => {}
    }
}

fn write_status(status: &str, model: &str, used: &str, window: &str) {
    let percent = match (used.parse::<u64>(), window.parse::<u64>()) {
        (Ok(used), Ok(window)) if window > 0 => (used * 100 / window).to_string(),
        _ => "?".into(),
    };
    let _ = std::fs::write(
        status,
        format!("MODEL:{model} CTX:{used} CTXWIN:{window} CTXPCT:{percent}\n"),
    );
}

fn fail(error: impl std::fmt::Display) -> ExitCode {
    eprintln!("remuda: Codex TUI: {error}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_path(test: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("remuda-codex-{test}-{}", std::process::id()))
    }

    #[test]
    fn thread_start_publishes_the_model_before_any_usage_event() {
        let path = status_path("thread-start");
        let _ = std::fs::remove_file(&path);
        let mut model = "?".to_string();
        update_status(
            &json!({"method":"thread/started","params":{"thread":{"model":"gpt-5.4"}}}),
            &mut model,
            &path.to_string_lossy(),
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "MODEL:gpt-5.4 CTX:? CTXWIN:? CTXPCT:?\n"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn token_usage_replaces_unknown_context_with_real_capacity() {
        let path = status_path("token-usage");
        let _ = std::fs::remove_file(&path);
        let mut model = "gpt-5.4".to_string();
        update_status(
            &json!({"method":"thread/tokenUsage/updated","params":{"tokenUsage":{
                "modelContextWindow":200000,
                "last":{"inputTokens":12000,"cachedInputTokens":3000,"cacheWriteInputTokens":0}
            }}}),
            &mut model,
            &path.to_string_lossy(),
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "MODEL:gpt-5.4 CTX:15000 CTXWIN:200000 CTXPCT:7\n"
        );
        let _ = std::fs::remove_file(path);
    }
}
