//! Deferred extension-command replies over a private daemon.

use std::fs;
use std::process::{Command, Output};

struct PrivateDaemonCleanup(std::path::PathBuf);

impl Drop for PrivateDaemonCleanup {
    fn drop(&mut self) {
        let socket = remuda_native::daemon::socket_path_in(&self.0, "s");
        if socket.exists() || cfg!(windows) {
            let _ = Command::new(env!("CARGO_BIN_EXE_remuda"))
                .args(["-s", "s", "stop", "-f"])
                .env("REMUDA_RUNTIME_DIR", &self.0)
                .env("XDG_DATA_HOME", self.0.join("data"))
                .env("HOME", &self.0)
                .output();
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn pending_handle_count_is_bounded_and_overflow_fails_immediately() {
    let pending = remuda_native::pending::PendingReplies::default();
    let mut handles = Vec::new();
    for _ in 0..remuda_native::pending::MAX_PENDING_REPLIES {
        handles.push(pending.create(std::time::Duration::from_secs(1)).unwrap());
    }
    assert!(matches!(
        pending.create(std::time::Duration::from_secs(1)),
        Err(error) if error.contains("limit 64 per daemon")
    ));
}

fn fixture(tag: &str) -> (std::path::PathBuf, impl Fn(&[&str]) -> Output) {
    let root = if cfg!(target_os = "macos") {
        std::path::PathBuf::from("/private/tmp")
    } else {
        std::env::temp_dir()
    };
    let dir = root.join(format!("r-dr-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let package = dir.join("data/remuda/mods/deferred");
    fs::create_dir_all(package.join("packages/deferred")).unwrap();
    fs::write(
        package.join("extension.toml"),
        "name = \"deferred\"\nentry = \"packages/deferred/init.lua\"\napi = \"remuda-lua-v1\"\ncommand = \"deferred\"\n",
    )
    .unwrap();
    fs::write(
        package.join("packages/deferred/init.lua"),
        r#"
remuda.extension_command("deferred", function(args)
  if args[1] == "later" then
    local reply = remuda.pending { timeout = 5, on_cancel = function(reason) _G.cancel_reason = reason end }
    local schedule
    schedule = remuda.schedule { every = 0.1, run = function()
      remuda.cancel(schedule)
      reply:resolve(7, "out\n" .. string.char(255), "err\n" .. string.char(0))
    end }
    return reply
  elseif args[1] == "timeout" then
    return remuda.pending { timeout = 0.1, on_cancel = function(reason) _G.cancel_reason = reason end }
  elseif args[1] == "timeout_reason" or args[1] == "cancel_reason" or args[1] == "double_status" then
    return _G.cancel_reason or tostring(_G.double_second_error or "")
  elseif args[1] == "cancel_status" then
    return _G.cancel_reason or "none"
  elseif args[1] == "double" then
    local reply = remuda.pending { timeout = 5 }
    local schedule
    schedule = remuda.schedule { every = 0.1, run = function()
      remuda.cancel(schedule)
      reply:resolve(0, "first", "")
      local ok = pcall(function() reply:resolve(0, "second", "") end)
      _G.double_second_error = not ok
    end }
    return reply
  elseif args[1] == "disconnect" then
    local reply = remuda.pending { timeout = 5,
      on_cancel = function(reason) _G.cancel_reason = reason end }
    local schedule = remuda.schedule { every = 3, run = function() reply:resolve(0, "late", "") end }
    _G.disconnect_schedule = schedule
    return reply
  elseif args[1] == "oversize" then
    local reply = remuda.pending { timeout = 5 }
    local schedule
    schedule = remuda.schedule { every = 0.1, run = function()
      remuda.cancel(schedule)
      reply:resolve(0, string.rep("x", 16 * 1024 * 1024 + 1), "")
    end }
    return reply
  elseif args[1] == "max_output" then
    local reply = remuda.pending { timeout = 5 }
    local schedule
    schedule = remuda.schedule { every = 0.1, run = function()
      remuda.cancel(schedule)
      reply:resolve(0, string.rep("x", 16 * 1024 * 1024), "")
    end }
    return reply
  elseif args[1] == "secret" or args[1] == "secret_session" then
    local reply = remuda.pending { timeout = 5 }
    local label = "deferred test secret"
    if args[1] == "secret_session" then
      label = "deferred " .. string.char(27) .. "test secret"
    end
    reply:prompt_secret { label = label, callback = function(secret, err)
      if err then
        reply:reject("secret prompt " .. err .. "\nNext: remuda deferred --password-file PATH")
      else
        reply:resolve(0, "secret accepted", "")
      end
    end }
    return reply
  elseif args[1] == "secret_cap" or args[1] == "secret_over_cap" then
    local reply = remuda.pending { timeout = 5 }
    reply:prompt_secret { label = "secret cap test", callback = function(secret, err)
      if err then
        reply:reject(err)
      elseif #secret == 4096 and secret == string.rep("x", 4096) then
        reply:resolve(0, "exact 4 KiB", "")
      else
        reply:reject("unexpected secret length")
      end
    end }
    return reply
  elseif args[1] == "shutdown_wait" then
    local path = args[2]
    return remuda.pending { timeout = 30, on_cancel = function(reason)
      local file = io.open(path, "w")
      if file then file:write(reason); file:close() end
    end }
  end
  return "plain"
end)
"#,
    )
    .unwrap();
    let run_dir = dir.clone();
    let run = move |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &run_dir)
            .env("XDG_DATA_HOME", run_dir.join("data"))
            .env("HOME", &run_dir)
            .output()
            .expect("run remuda")
    };
    (dir, run)
}

#[test]
fn cli_waits_for_deferred_result_and_preserves_plain_return_behavior() {
    let (dir, remuda) = fixture("result");
    let _cleanup = PrivateDaemonCleanup(dir.clone());
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );
    let later = remuda(&["deferred", "later"]);
    let plain = remuda(&["deferred", "plain"]);
    let cancel_status = remuda(&["deferred", "cancel_status"]);
    let _ = remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(later.status.code(), Some(7), "{later:?}");
    assert_eq!(later.stdout, b"out\n\xff", "{later:?}");
    assert_eq!(later.stderr, b"err\n\0", "{later:?}");
    assert_eq!(
        cancel_status.stdout, b"none\n",
        "resolved replies must not cancel: {cancel_status:?}"
    );
    assert!(plain.status.success(), "{plain:?}");
    assert_eq!(plain.stdout, b"plain\n", "{plain:?}");
    assert!(plain.stderr.is_empty(), "{plain:?}");
}

#[test]
fn cli_reports_pending_timeout() {
    let (dir, remuda) = fixture("timeout");
    let _cleanup = PrivateDaemonCleanup(dir.clone());
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );
    let output = remuda(&["deferred", "timeout"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("timed out"),
        "{output:?}"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let reason = loop {
        let reason = remuda(&["deferred", "timeout_reason"]);
        if reason.stdout == b"timeout\n" || std::time::Instant::now() >= deadline {
            break reason;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let _ = remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(reason.stdout, b"timeout\n", "{reason:?}");
}

#[test]
fn double_resolution_fails_without_replacing_the_first_result() {
    let (dir, remuda) = fixture("double");
    let _cleanup = PrivateDaemonCleanup(dir.clone());
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );
    let first = remuda(&["deferred", "double"]);
    let second_failed = remuda(&["deferred", "double_status"]);
    let _ = remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);

    assert!(first.status.success(), "{first:?}");
    assert_eq!(first.stdout, b"first", "{first:?}");
    assert_eq!(second_failed.stdout, b"true\n", "{second_failed:?}");
}

#[test]
fn disconnect_notifies_on_cancel_and_drops_the_pending_reply() {
    use remuda_core::protocol::Request;
    use std::io::Write;
    use std::time::{Duration, Instant};

    let (dir, remuda) = fixture("disconnect");
    let _cleanup = PrivateDaemonCleanup(dir.clone());
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );
    let socket = remuda_native::daemon::socket_path_in(&dir, "s");
    let mut stream = remuda_native::ipc::connect(&socket).expect("private daemon socket");
    let request = Request::Eval {
        code: "return remuda._dispatch_extension_command('deferred', {'disconnect'}, {})".into(),
        name: None,
    };
    let mut wire = serde_json::to_vec(&request).unwrap();
    wire.push(b'\n');
    stream.write_all(&wire).unwrap();
    drop(stream);

    let deadline = Instant::now() + Duration::from_secs(4);
    let notified = loop {
        let output = remuda(&["deferred", "cancel_reason"]);
        if output.stdout == b"client_disconnected\n" {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);
    assert!(notified, "on_cancel did not report the client disconnect");
}

#[test]
fn deferred_output_over_limit_is_an_error_without_truncation() {
    let (dir, remuda) = fixture("oversize");
    let _cleanup = PrivateDaemonCleanup(dir.clone());
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );
    let output = remuda(&["deferred", "oversize"]);
    let _ = remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        output.stdout.is_empty(),
        "oversized output must not be truncated and printed"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("16 MiB output limit"),
        "{output:?}"
    );
}

#[test]
fn synchronous_value_over_limit_is_an_error_and_daemon_stays_healthy() {
    let (dir, remuda) = fixture("oversize_sync");
    let _cleanup = PrivateDaemonCleanup(dir.clone());
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );

    let oversized = remuda(&["-e", "return string.rep('x', 16 * 1024 * 1024 + 1)"]);
    let healthy = remuda(&["-e", "return 'still healthy'"]);
    let _ = remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        oversized.status.code(),
        Some(1),
        "status: {:?}",
        oversized.status
    );
    assert!(
        oversized.stdout.is_empty(),
        "unexpected stdout length: {}",
        oversized.stdout.len()
    );
    assert!(
        String::from_utf8_lossy(&oversized.stderr).contains("16 MiB"),
        "oversized synchronous value should report the reply limit: {}",
        String::from_utf8_lossy(&oversized.stderr)
    );
    assert!(
        healthy.status.success(),
        "daemon did not recover: {healthy:?}"
    );
    assert_eq!(healthy.stdout, b"still healthy\n", "{healthy:?}");
}

#[test]
fn maximum_deferred_output_round_trips_with_base64_wire_encoding() {
    let (dir, remuda) = fixture("max_output");
    let _cleanup = PrivateDaemonCleanup(dir.clone());
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );
    let started = std::time::Instant::now();
    let output = remuda(&["deferred", "max_output"]);
    let elapsed = started.elapsed();
    let _ = remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout.len(), 16 * 1024 * 1024);
    assert!(output.stderr.is_empty(), "{output:?}");
    eprintln!("16 MiB deferred reply round trip: {elapsed:.2?}");
}

#[test]
fn secret_prompt_non_tty_fallback_and_answer_do_not_leak() {
    use remuda_core::protocol::{Request, Response, SecretBytes};
    use remuda_native::ipc::TryClone;
    use std::io::{BufRead, BufReader, Write};

    let (dir, remuda) = fixture("secret_prompt");
    let _cleanup = PrivateDaemonCleanup(dir.clone());
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );

    let sentinel = b"S3CRET-probe";
    let socket = remuda_native::daemon::socket_path_in(&dir, "s");
    let mut stream = remuda_native::ipc::connect(&socket).expect("private daemon socket");
    let mut reader = BufReader::new(stream.try_clone().expect("clone private socket"));
    let request = Request::Eval {
        code: "return remuda._dispatch_extension_command('deferred', {'secret'}, {})".into(),
        name: None,
    };
    let mut request_frame = serde_json::to_vec(&request).unwrap();
    request_frame.push(b'\n');
    stream.write_all(&request_frame).unwrap();

    let mut prompt_frame = Vec::new();
    reader.read_until(b'\n', &mut prompt_frame).unwrap();
    let prompt: Response = serde_json::from_slice(&prompt_frame).expect("secret prompt frame");
    let prompt_id = match prompt {
        Response::PromptSecret { id, label } => {
            assert_eq!(
                label, "deferred test secret",
                "outside caller gets no prefix"
            );
            id
        }
        response => panic!("expected secret prompt, got {response:?}"),
    };
    let answer = Request::SecretAnswer {
        id: prompt_id,
        secret: Some(SecretBytes::new(sentinel.to_vec())),
        refusal: None,
    };
    let mut answer_frame = serde_json::to_vec(&answer).unwrap();
    answer_frame.push(b'\n');
    stream.write_all(&answer_frame).unwrap();

    let mut reply_frame = Vec::new();
    reader.read_until(b'\n', &mut reply_frame).unwrap();
    assert!(
        !prompt_frame
            .windows(sentinel.len())
            .any(|window| window == sentinel)
            && !reply_frame
                .windows(sentinel.len())
                .any(|window| window == sentinel),
        "secret appeared in a daemon reply frame"
    );
    let reply: Response = serde_json::from_slice(&reply_frame).expect("deferred reply frame");
    assert!(matches!(reply, Response::CommandResult { .. }), "{reply:?}");
    drop(reader);
    drop(stream);

    // Command::output gives this CLI no stdin or stderr TTY.
    let non_tty = remuda(&["deferred", "secret"]);
    let stop = remuda(&["stop", "-f"]);
    let log = socket.with_extension("log");
    let log_bytes = fs::read(&log).unwrap_or_default();
    assert!(
        !log_bytes
            .windows(sentinel.len())
            .any(|window| window == sentinel),
        "sentinel appeared in daemon log {}",
        log.display()
    );
    assert_no_secret_in_files(&dir, sentinel);
    let _ = fs::remove_dir_all(&dir);

    assert!(stop.status.success(), "{stop:?}");
    assert_eq!(non_tty.status.code(), Some(1), "{non_tty:?}");
    assert!(non_tty.stdout.is_empty(), "{non_tty:?}");
    let stderr = String::from_utf8_lossy(&non_tty.stderr);
    assert!(stderr.contains("not_a_terminal"), "{stderr}");
    assert_eq!(
        stderr.lines().last(),
        Some("Next: remuda deferred --password-file PATH"),
        "non-TTY error must end with a concrete password-file Next line: {stderr}"
    );
}

#[test]
fn secret_answer_frame_round_trips_at_four_kib_and_reports_too_long() {
    use remuda_core::protocol::{Request, Response, SecretAnswerRefusal, SecretBytes};
    use remuda_native::ipc::TryClone;
    use std::io::{BufRead, BufReader, Write};

    let (dir, remuda) = fixture("secret_cap");
    let _cleanup = PrivateDaemonCleanup(dir.clone());
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );
    let socket = remuda_native::daemon::socket_path_in(&dir, "s");

    let answer_over_socket = |word: &str, secret: Option<SecretBytes>, refusal| {
        let mut stream = remuda_native::ipc::connect(&socket).expect("private daemon socket");
        let mut reader = BufReader::new(stream.try_clone().expect("clone private socket"));
        let request = Request::Eval {
            code: format!(
                "return remuda._dispatch_extension_command('deferred', {{'{word}'}}, {{}})"
            ),
            name: None,
        };
        let mut request_frame = serde_json::to_vec(&request).unwrap();
        request_frame.push(b'\n');
        stream.write_all(&request_frame).unwrap();

        let mut prompt_frame = Vec::new();
        reader.read_until(b'\n', &mut prompt_frame).unwrap();
        let prompt: Response = serde_json::from_slice(&prompt_frame).expect("secret prompt frame");
        let id = match prompt {
            Response::PromptSecret { id, .. } => id,
            response => panic!("expected secret prompt, got {response:?}"),
        };
        let answer = Request::SecretAnswer {
            id,
            secret,
            refusal,
        };
        let mut answer_frame = serde_json::to_vec(&answer).unwrap();
        answer_frame.push(b'\n');
        stream.write_all(&answer_frame).unwrap();
        let mut reply_frame = Vec::new();
        reader.read_until(b'\n', &mut reply_frame).unwrap();
        (answer_frame, reply_frame)
    };

    let (answer_frame, exact_reply_frame) = answer_over_socket(
        "secret_cap",
        Some(SecretBytes::new(vec![b'x'; 4 * 1024])),
        None,
    );
    assert!(
        answer_frame.len() <= remuda_core::protocol::SECRET_ANSWER_MAX_FRAME_BYTES,
        "serialized 4 KiB SecretAnswer frame was too large: {} bytes",
        answer_frame.len()
    );
    let decoded_answer: Request = serde_json::from_slice(&answer_frame).unwrap();
    assert!(matches!(
        decoded_answer,
        Request::SecretAnswer {
            secret: Some(secret),
            refusal: None,
            ..
        } if secret.as_bytes().len() == 4 * 1024
            && secret.as_bytes().iter().all(|byte| *byte == b'x')
    ));
    let exact_reply: Response = serde_json::from_slice(&exact_reply_frame).unwrap();
    match exact_reply {
        Response::CommandResult {
            exit_code,
            stdout_base64,
            stderr_base64,
        } => {
            assert_eq!(exit_code, 0);
            assert_eq!(
                remuda_native::cluster::encoding::decode_base64(&stdout_base64).unwrap(),
                b"exact 4 KiB"
            );
            assert!(stderr_base64.is_empty());
        }
        response => panic!("expected successful 4 KiB callback, got {response:?}"),
    }

    let (_, too_long_reply_frame) =
        answer_over_socket("secret_over_cap", None, Some(SecretAnswerRefusal::TooLong));
    let too_long_reply: Response = serde_json::from_slice(&too_long_reply_frame).unwrap();
    assert_eq!(too_long_reply, Response::Error("too_long".into()));

    let stop = remuda(&["stop", "-f"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(stop.status.success(), "{stop:?}");
}

#[cfg(unix)]
#[test]
fn session_secret_prompt_label_names_the_session_and_strips_controls() {
    use remuda_core::protocol::{Request, Response, Step};
    use remuda_core::Size;
    use remuda_native::{client, daemon};
    use std::time::{Duration, Instant};

    let (dir, remuda) = fixture("secret_session_label");
    let _cleanup = PrivateDaemonCleanup(dir.clone());
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );

    let socket = daemon::socket_path_in(&dir, "s");
    let session_name = "secret-label-session";
    let binary = env!("CARGO_BIN_EXE_remuda").replace('\\', "/");
    let command = format!("sleep 0.2; \"{binary}\" -s s deferred secret_session");
    assert_eq!(
        client::request(
            &socket,
            &Request::New {
                name: Some(session_name.into()),
                command: vec!["sh".into(), "-c".into(), command],
                size: Size::new(80, 24),
                cwd: None,
                env: None,
            },
        )
        .expect("start secret-prompt session"),
        Response::Value(session_name.into())
    );

    let deadline = Instant::now() + Duration::from_secs(8);
    let prompt_screen = loop {
        let screen = match client::request(
            &socket,
            &Request::Capture {
                name: session_name.into(),
            },
        ) {
            Ok(Response::Screen(screen)) => screen,
            other => panic!("capture failed: {other:?}"),
        };
        if screen.contains("deferred test secret") {
            break screen;
        }
        assert!(
            Instant::now() < deadline,
            "secret prompt did not appear:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    client::request(
        &socket,
        &Request::Feed {
            name: session_name.into(),
            steps: vec![Step::Burst(vec![b'\r'])],
        },
    )
    .expect("submit empty secret answer");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let screen = match client::request(
            &socket,
            &Request::Capture {
                name: session_name.into(),
            },
        ) {
            Ok(Response::Screen(screen)) => screen,
            other => panic!("capture failed: {other:?}"),
        };
        if screen.contains("secret accepted") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "secret reply did not finish:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = remuda(&["stop", "-f"]);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        prompt_screen.contains("secret-label-session: deferred test secret"),
        "session prompt label omitted its caller name or retained ESC:\n{prompt_screen}"
    );
    assert!(
        !prompt_screen.contains('\x1b'),
        "ESC remained in the terminal screen:\n{prompt_screen}"
    );
}

fn assert_no_secret_in_files(root: &std::path::Path, secret: &[u8]) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            assert_no_secret_in_files(&path, secret);
        } else if path.is_file() {
            let contents = fs::read(&path).unwrap();
            assert!(
                !contents
                    .windows(secret.len())
                    .any(|window| window == secret),
                "sentinel appeared in {}",
                path.display()
            );
        }
    }
}

#[test]
fn shutdown_answers_waiters_and_runs_shutdown_cancellation_callback() {
    use std::time::{Duration, Instant};

    let (dir, remuda) = fixture("shutdown");
    let _cleanup = PrivateDaemonCleanup(dir.clone());
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );
    let cancellation_file = dir.join("cancelled");
    let mut cli = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s", "deferred", "shutdown_wait"])
        .arg(&cancellation_file)
        .env("REMUDA_RUNTIME_DIR", &dir)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("HOME", &dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start pending CLI caller");
    std::thread::sleep(Duration::from_millis(150));
    let stop = remuda(&["stop", "-f"]);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut exited = cli.try_wait().unwrap().is_some();
    while !exited && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
        exited = cli.try_wait().unwrap().is_some();
    }
    if !exited {
        let _ = cli.kill();
    }
    let output = cli.wait_with_output().unwrap();
    let cancellation = fs::read_to_string(&cancellation_file).unwrap_or_default();
    let _ = fs::remove_dir_all(&dir);

    assert!(stop.status.success(), "{stop:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("daemon stopping"),
        "{output:?}"
    );
    assert_eq!(cancellation, "shutdown");
}
