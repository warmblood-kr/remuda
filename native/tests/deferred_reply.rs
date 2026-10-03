//! Deferred extension-command replies over a private daemon.

use std::fs;
use std::process::{Command, Output};

#[path = "autostart_daemon_guard.rs"]
mod autostart_daemon_guard;
use autostart_daemon_guard::AutostartDaemonGuard;

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

#[allow(clippy::too_many_lines)] // The fixture is one Lua module used by the integration cases below.
fn fixture(
    tag: &str,
) -> (
    std::path::PathBuf,
    impl Fn(&[&str]) -> Output,
    AutostartDaemonGuard,
) {
    let root = if cfg!(target_os = "macos") {
        std::path::PathBuf::from("/private/tmp")
    } else {
        std::env::temp_dir()
    };
    let dir = root.join(format!("r-dr-{}-{tag}", std::process::id()));
    let daemon_guard = AutostartDaemonGuard::new(&dir);
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
  elseif args[1] == "secret" or args[1] == "secret_session" or args[1] == "secret_unicode" or args[1] == "secret_long" then
    local reply = remuda.pending { timeout = 5 }
    local label = "deferred test secret"
    if args[1] == "secret_session" then
      label = "deferred " .. string.char(27) .. "test secret"
    elseif args[1] == "secret_unicode" then
      label = "before" .. string.char(226, 128, 139, 226, 128, 174, 226, 128, 168, 226, 128, 169) .. "after"
    elseif args[1] == "secret_long" then
      label = string.rep("x", 300)
    end
    reply:prompt_secret { label = label, callback = function(secret, err)
      if err then
        reply:reject("secret prompt " .. err .. "\nNext: remuda deferred --password-file PATH")
      else
        reply:resolve(0, "secret length: " .. #secret, "")
      end
    end }
    return reply
  elseif args[1] == "secret_short" then
    local reply = remuda.pending { timeout = 0.5 }
    reply:prompt_secret { label = "short secret", callback = function(secret, err)
      if err then reply:reject(err) else reply:resolve(0, "secret accepted", "") end
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
  elseif args[1] == "line" or args[1] == "line_session" then
    local reply = remuda.pending { timeout = 5 }
    local label = "wizard label"
    local default = "de" .. string.char(27) .. "fa" .. string.char(226, 128, 174) .. "ult"
    reply:prompt_line { label = label, default = default, callback = function(line, err)
      if err then reply:reject(err) else reply:resolve(0, "line: " .. line, "") end
    end }
    return reply
  elseif args[1] == "line_answers" then
    local reply = remuda.pending { timeout = 5 }
    reply:prompt_line { label = "owner ID", default = "owner", callback = function(line, err)
      if err then reply:reject(err) else reply:resolve(0, line, "") end
    end }
    return reply
  elseif args[1] == "line_cross" then
    _G.line_cross_calls = 0
    local reply = remuda.pending { timeout = 5 }
    reply:prompt_line { label = "line cross", callback = function(line, err)
      _G.line_cross_calls = _G.line_cross_calls + 1
      if err then reply:reject(err) else reply:resolve(0, line or "", "") end
    end }
    return reply
  elseif args[1] == "secret_cross" then
    _G.secret_cross_calls = 0
    local reply = remuda.pending { timeout = 5 }
    reply:prompt_secret { label = "secret cross", callback = function(secret, err)
      _G.secret_cross_calls = _G.secret_cross_calls + 1
      if err then reply:reject(err) else reply:resolve(0, "accepted", "") end
    end }
    return reply
  elseif args[1] == "line_long_default" then
    local reply = remuda.pending { timeout = 5 }
    reply:prompt_line { label = "owner ID", default = string.rep("x", 1100), callback = function(line, err)
      if err then reply:reject(err) else reply:resolve(0, line, "") end
    end }
    return reply
  elseif args[1] == "line_label_long" then
    local reply = remuda.pending { timeout = 5 }
    local label = string.rep("A", 150) .. "\n" .. string.rep("B", 150) .. "\n" .. string.rep("C", 98)
    reply:prompt_line { label = label, default = "N", callback = function(line, err)
      if err then reply:reject(err) else reply:resolve(0, "line: " .. line, "") end
    end }
    return reply
  elseif args[1] == "line_preface" or args[1] == "line_preface_hostile" then
    local reply = remuda.pending { timeout = 5 }
    local preface = "Matrix setup will:\n- join the room\nSave to /tmp/example\n"
    if args[1] == "line_preface_hostile" then
      preface = "safe" .. string.char(27) .. "[31mred" .. string.char(13) .. "spoof"
        .. string.char(226, 128, 174) .. "end\n" .. string.char(27) .. "\n   \nremuda[outside] fake:"
    end
    reply:prompt_line { label = "Continue?", default = "N", preface = preface, callback = function(line, err)
      if err then reply:reject(err) else reply:resolve(0, "line: " .. line, "") end
    end }
    return reply
  elseif args[1] == "line_preface_lines" or args[1] == "line_preface_width" then
    local reply = remuda.pending { timeout = 5 }
    local preface = args[1] == "line_preface_lines" and string.rep("x\n", 33) or string.rep("x", 257)
    local ok, err = pcall(function()
      reply:prompt_line { label = "Continue?", preface = preface, callback = function() end }
    end)
    return ok and "accepted" or ("refused: " .. tostring(err))
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
            .env("XDG_CACHE_HOME", run_dir.join("cache"))
            .env("HOME", &run_dir)
            .output()
            .expect("run remuda")
    };
    (dir, run, daemon_guard)
}

#[cfg(unix)]
fn secret_prompt_client_exits_and_restores_tty(
    runtime: &std::path::Path,
    signal: Option<libc::c_int>,
) -> (bool, bool, bool) {
    use std::io::Read as _;

    let pty = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open secret prompt pty");
    let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_remuda"));
    command.args(["-s", "s", "deferred", "secret_short"]);
    command.env("REMUDA_RUNTIME_DIR", runtime);
    command.env("XDG_DATA_HOME", runtime.join("data"));
    command.env("XDG_CACHE_HOME", runtime.join("cache"));
    command.env("HOME", runtime);
    let mut child = pty
        .slave
        .spawn_command(command)
        .expect("spawn secret client");
    let pid = child.process_id().expect("secret client pid");
    drop(pty.slave);
    let mut reader = pty
        .master
        .try_clone_reader()
        .expect("clone secret pty reader");
    let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let reader_output = output.clone();
    std::thread::spawn(move || {
        let mut buffer = [0; 1024];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 {
                break;
            }
            reader_output
                .lock()
                .unwrap()
                .extend_from_slice(&buffer[..count]);
        }
    });

    let flags = || {
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::tcgetattr(pty.master.as_raw_fd().unwrap(), &mut termios) };
        assert_eq!(result, 0, "read secret prompt terminal mode");
        termios.c_lflag & libc::ECHO != 0 && termios.c_lflag & libc::ICANON != 0
    };
    let prompt_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !output
        .lock()
        .unwrap()
        .windows(b"short secret".len())
        .any(|w| w == b"short secret")
        && std::time::Instant::now() < prompt_deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let prompt_shown = output
        .lock()
        .unwrap()
        .windows(b"short secret".len())
        .any(|w| w == b"short secret");
    if let (Some(signal), true) = (signal, prompt_shown) {
        unsafe { libc::kill(pid as libc::pid_t, signal) };
    }

    let exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut exited = false;
    while std::time::Instant::now() < exit_deadline {
        if child.try_wait().expect("poll secret client").is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
    }
    (prompt_shown, exited, flags())
}

#[cfg(unix)]
#[test]
fn secret_prompt_deadline_and_termination_signals_restore_the_tty() {
    let (dir, remuda, _daemon_guard) = fixture("secret-tty-lifecycle");
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );

    let deadline_case = secret_prompt_client_exits_and_restores_tty(&dir, None);
    let term_case = secret_prompt_client_exits_and_restores_tty(&dir, Some(libc::SIGTERM));
    let hup_case = secret_prompt_client_exits_and_restores_tty(&dir, Some(libc::SIGHUP));
    let _ = remuda(&["stop", "-f"]);

    assert!(deadline_case.0, "client never displayed its prompt");
    assert!(
        deadline_case.1,
        "client did not exit at its pending deadline"
    );
    assert!(deadline_case.2, "deadline left terminal in raw mode");
    assert!(
        term_case.0 && term_case.1 && term_case.2,
        "SIGTERM did not restore terminal: {term_case:?}"
    );
    assert!(
        hup_case.0 && hup_case.1 && hup_case.2,
        "SIGHUP did not restore terminal: {hup_case:?}"
    );
}

#[test]
fn cli_waits_for_deferred_result_and_preserves_plain_return_behavior() {
    let (dir, remuda, _daemon_guard) = fixture("result");
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
    let (dir, remuda, _daemon_guard) = fixture("timeout");
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
    let (dir, remuda, _daemon_guard) = fixture("double");
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

    let (dir, remuda, _daemon_guard) = fixture("disconnect");
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
    let (dir, remuda, _daemon_guard) = fixture("oversize");
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
    let (dir, remuda, _daemon_guard) = fixture("oversize_sync");
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
    let (dir, remuda, _daemon_guard) = fixture("max_output");
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

    let (dir, remuda, _daemon_guard) = fixture("secret_prompt");
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
        Response::PromptSecret { id, label, .. } => {
            assert_eq!(
                label, "remuda[outside] deferred test secret",
                "outside caller label must include its provenance tag"
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

    let (dir, remuda, _daemon_guard) = fixture("secret_cap");
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
fn secret_prompt_in_session(
    socket: &std::path::Path,
    session_name: &str,
    binary: &str,
    columns: u16,
) -> String {
    use remuda_core::protocol::{Request, Response, Step};
    use remuda_core::Size;
    use remuda_native::client;
    use std::time::{Duration, Instant};

    let command = format!("sleep 0.2; \"{binary}\" -s s deferred secret_session");
    assert_eq!(
        client::request(
            socket,
            &Request::New {
                name: Some(session_name.into()),
                command: vec!["sh".into(), "-c".into(), command],
                size: Size::new(columns, 24),
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
            socket,
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
        socket,
        &Request::Feed {
            name: session_name.into(),
            steps: vec![Step::Burst(vec![b'\r'])],
        },
    )
    .expect("submit empty secret answer");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let screen = match client::request(
            socket,
            &Request::Capture {
                name: session_name.into(),
            },
        ) {
            Ok(Response::Screen(screen)) => screen,
            other => panic!("capture failed: {other:?}"),
        };
        if screen.contains("secret length:") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "secret reply did not finish:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    prompt_screen
}

#[cfg(unix)]
#[test]
fn session_secret_prompt_label_names_the_session_and_strips_controls() {
    use remuda_native::daemon;

    let (dir, remuda, _daemon_guard) = fixture("secret_session_label");
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );

    let socket = daemon::socket_path_in(&dir, "s");
    let session_name = "secret-label-session";
    let binary = env!("CARGO_BIN_EXE_remuda").replace('\\', "/");
    let prompt_screen = secret_prompt_in_session(&socket, session_name, &binary, 80);
    let _ = remuda(&["stop", "-f"]);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        prompt_screen.contains("remuda[session secret-label-session] deferred test secret"),
        "session prompt label omitted its provenance tag or retained ESC:\n{prompt_screen}"
    );
    assert!(
        !prompt_screen.contains('\x1b'),
        "ESC remained in the terminal screen:\n{prompt_screen}"
    );
}

#[cfg(unix)]
#[test]
fn session_secret_prompt_tag_replaces_brackets_in_the_session_name() {
    use remuda_native::daemon;

    let (dir, remuda, _daemon_guard) = fixture("secret_session_tag_brackets");
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );

    let socket = daemon::socket_path_in(&dir, "s");
    let binary = env!("CARGO_BIN_EXE_remuda").replace('\\', "/");
    let prompt_screen = secret_prompt_in_session(&socket, "x] remuda[outside", &binary, 80);
    let _ = remuda(&["stop", "-f"]);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        prompt_screen.contains("remuda[session x? remuda?outside] deferred test secret"),
        "session name forged the caller tag:\n{prompt_screen}"
    );
}

#[cfg(unix)]
#[test]
fn session_secret_prompt_tag_caps_the_session_name_at_64_chars() {
    use remuda_native::daemon;

    let (dir, remuda, _daemon_guard) = fixture("secret_session_tag_cap");
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );

    let socket = daemon::socket_path_in(&dir, "s");
    let binary = env!("CARGO_BIN_EXE_remuda").replace('\\', "/");
    let session_name = "x".repeat(70);
    let prompt_screen = secret_prompt_in_session(&socket, &session_name, &binary, 110);
    let _ = remuda(&["stop", "-f"]);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        prompt_screen.contains(&format!("remuda[session {}]", "x".repeat(64))),
        "session name was not truncated to 64 chars:\n{prompt_screen}"
    );
}

#[cfg(unix)]
#[test]
fn session_secret_prompt_tag_preserves_non_ascii_session_names() {
    use remuda_native::daemon;

    let (dir, remuda, _daemon_guard) = fixture("secret_session_tag_unicode");
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );

    let socket = daemon::socket_path_in(&dir, "s");
    let binary = env!("CARGO_BIN_EXE_remuda").replace('\\', "/");
    let session_name = "東京🦀";
    let prompt_screen = secret_prompt_in_session(&socket, session_name, &binary, 80);
    let _ = remuda(&["stop", "-f"]);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        prompt_screen.contains("remuda[session 東京🦀] deferred test secret"),
        "non-ASCII session name changed in the caller tag:\n{prompt_screen}"
    );
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)] // Keep the pane-input-to-scrollback leak guard as one end-to-end case.
fn secret_entered_in_session_pane_is_absent_from_capture_and_scrollback() {
    use remuda_core::protocol::{Request, Response, Step};
    use remuda_core::Size;
    use remuda_native::{client, daemon};
    use std::time::{Duration, Instant};

    let (dir, remuda, _daemon_guard) = fixture("secret_pane_leak");
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );

    let socket = daemon::socket_path_in(&dir, "s");
    let session_name = "secret-pane-leak-session";
    let binary = env!("CARGO_BIN_EXE_remuda").replace('\\', "/");
    let command = format!(
        "for i in $(seq 1 40); do printf 'setup-%s\\n' \"$i\"; done; sleep 0.2; \"{binary}\" -s s deferred secret_session"
    );
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
        if screen.contains("remuda[session secret-pane-leak-session] deferred test secret") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "secret prompt did not appear:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    let sentinel = b"S3CRET-probe";
    assert_eq!(sentinel.len(), 12);
    client::request(
        &socket,
        &Request::Feed {
            name: session_name.into(),
            steps: vec![Step::Burst(sentinel.to_vec())],
        },
    )
    .expect("type secret into the session prompt");
    client::request(
        &socket,
        &Request::Feed {
            name: session_name.into(),
            steps: vec![Step::Burst(vec![b'\r'])],
        },
    )
    .expect("submit session prompt secret");

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
        if screen.contains("secret length: 12") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "secret reply did not finish:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    let capture = match client::request(
        &socket,
        &Request::Capture {
            name: session_name.into(),
        },
    ) {
        Ok(Response::Screen(screen)) => screen,
        other => panic!("final capture failed: {other:?}"),
    };
    let (visible_text, scrollback_len) = match client::request(
        &socket,
        &Request::CaptureStyled {
            name: session_name.into(),
            scrollback: 0,
        },
    ) {
        Ok(Response::StyledScreen {
            rows,
            scrollback_len,
            ..
        }) => (
            rows.into_iter()
                .map(|row| row.into_iter().map(|run| run.text).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n"),
            scrollback_len,
        ),
        other => panic!("styled capture failed: {other:?}"),
    };
    assert!(scrollback_len > 0, "test session did not create scrollback");
    assert!(
        capture.contains("secret length: 12"),
        "callback did not receive the 12-byte sentinel:\n{capture}"
    );
    assert!(
        !capture.contains(std::str::from_utf8(sentinel).unwrap())
            && !visible_text.contains(std::str::from_utf8(sentinel).unwrap()),
        "secret appeared in current pane capture:\n{capture}"
    );
    for offset in 1..=scrollback_len {
        let history = match client::request(
            &socket,
            &Request::CaptureStyled {
                name: session_name.into(),
                scrollback: offset,
            },
        ) {
            Ok(Response::StyledScreen { rows, .. }) => rows
                .into_iter()
                .map(|row| row.into_iter().map(|run| run.text).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n"),
            other => panic!("scrollback capture failed: {other:?}"),
        };
        assert!(
            !history.contains(std::str::from_utf8(sentinel).unwrap()),
            "secret appeared in scrollback at offset {offset}:\n{history}"
        );
    }

    let _ = remuda(&["stop", "-f"]);
    assert_no_secret_in_files(&dir, sentinel);
    let _ = fs::remove_dir_all(&dir);
}

fn secret_prompt_label_for_word(tag: &str, word: &str) -> String {
    use remuda_core::protocol::{Request, Response};
    use remuda_native::ipc::TryClone;
    use std::io::{BufRead, BufReader, Write};

    let (dir, remuda, _daemon_guard) = fixture(tag);
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );
    let socket = remuda_native::daemon::socket_path_in(&dir, "s");
    let mut stream = remuda_native::ipc::connect(&socket).expect("private daemon socket");
    let mut reader = BufReader::new(stream.try_clone().expect("clone private socket"));
    let request = Request::Eval {
        code: format!("return remuda._dispatch_extension_command('deferred', {{'{word}'}}, {{}})"),
        name: None,
    };
    let mut frame = serde_json::to_vec(&request).unwrap();
    frame.push(b'\n');
    stream.write_all(&frame).unwrap();
    let mut prompt_frame = Vec::new();
    reader.read_until(b'\n', &mut prompt_frame).unwrap();
    let prompt: Response = serde_json::from_slice(&prompt_frame).unwrap();
    let label = match prompt {
        Response::PromptSecret { label, .. } => label,
        response => panic!("expected secret prompt, got {response:?}"),
    };
    drop(reader);
    drop(stream);
    label
}

#[test]
fn secret_prompt_label_strips_format_and_separator_characters() {
    let label = secret_prompt_label_for_word("secret_label_unicode", "secret_unicode");
    assert!(
        !label
            .chars()
            .any(|ch| matches!(ch, '\u{200b}' | '\u{202e}' | '\u{2028}' | '\u{2029}')),
        "format and line separator characters remained in label: {label:?}"
    );
    assert_eq!(label, "remuda[outside] beforeafter");
}

#[test]
fn secret_prompt_label_caps_caller_text_at_256_chars() {
    let label = secret_prompt_label_for_word("secret_label_long", "secret_long");
    let caller_label = label.strip_prefix("remuda[outside] ").expect("outside tag");
    assert_eq!(caller_label.chars().count(), 256);
    assert!(caller_label.chars().all(|ch| ch == 'x'));
}

#[test]
fn prompt_line_from_a_non_tty_reports_not_a_terminal() {
    let (_dir, remuda, _daemon_guard) = fixture("line_non_tty");
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );

    let output = remuda(&["deferred", "line"]);
    let _ = remuda(&["stop", "-f"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(stderr.contains("not_a_terminal"), "{stderr}");
}

#[test]
fn prompt_line_value_round_trips_over_the_daemon_socket() {
    use remuda_core::protocol::{Request, Response};
    use remuda_native::ipc::TryClone;
    use std::io::{BufRead, BufReader, Write};

    let (dir, remuda, _daemon_guard) = fixture("line_answers");
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );
    let socket = remuda_native::daemon::socket_path_in(&dir, "s");

    let answer_over_socket = |word: &str, line: &str, expected_default: &str| {
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
        let prompt: serde_json::Value = serde_json::from_slice(&prompt_frame).unwrap();
        let prompt_line = prompt.get("PromptLine").expect("PromptLine response");
        assert_eq!(prompt_line["label"], "remuda[outside] owner ID");
        assert_eq!(prompt_line["default"], expected_default);
        let id = prompt_line["id"].as_u64().expect("prompt id");
        let answer = serde_json::json!({
            "LineAnswer": { "id": id, "line": line, "refusal": null }
        });
        let mut answer_frame = serde_json::to_vec(&answer).unwrap();
        answer_frame.push(b'\n');
        stream.write_all(&answer_frame).unwrap();

        let mut reply_frame = Vec::new();
        reader.read_until(b'\n', &mut reply_frame).unwrap();
        let reply: Response = serde_json::from_slice(&reply_frame).unwrap();
        match reply {
            Response::CommandResult {
                exit_code,
                stdout_base64,
                stderr_base64,
            } => {
                assert_eq!(exit_code, 0);
                assert!(stderr_base64.is_empty());
                remuda_native::cluster::encoding::decode_base64(&stdout_base64).unwrap()
            }
            response => panic!("expected successful line callback, got {response:?}"),
        }
    };

    assert_eq!(
        answer_over_socket("line_answers", "https://homeserver.example", "owner"),
        b"https://homeserver.example"
    );
    let capped_default = "x".repeat(1024);
    assert_eq!(
        answer_over_socket("line_long_default", "selected", &capped_default),
        b"selected"
    );

    let stop = remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);
    assert!(stop.status.success(), "{stop:?}");
}

#[test]
fn prompt_line_sanitizes_raw_socket_answer() {
    use remuda_core::protocol::{Request, Response};
    use remuda_native::ipc::TryClone;
    use std::io::{BufRead, BufReader, Write};

    let (dir, remuda, _daemon_guard) = fixture("line_sanitize");
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );
    let socket = remuda_native::daemon::socket_path_in(&dir, "s");
    let mut stream = remuda_native::ipc::connect(&socket).expect("private daemon socket");
    let mut reader = BufReader::new(stream.try_clone().expect("clone private socket"));
    let request = Request::Eval {
        code: "return remuda._dispatch_extension_command('deferred', {'line_answers'}, {})".into(),
        name: None,
    };
    let mut frame = serde_json::to_vec(&request).unwrap();
    frame.push(b'\n');
    stream.write_all(&frame).unwrap();

    let mut prompt_frame = Vec::new();
    reader.read_until(b'\n', &mut prompt_frame).unwrap();
    let prompt: Response = serde_json::from_slice(&prompt_frame).unwrap();
    let id = match prompt {
        Response::PromptLine { id, .. } => id,
        response => panic!("expected line prompt, got {response:?}"),
    };
    let answer = Request::LineAnswer {
        id,
        line: Some("ok\u{1b}\r\n\u{202e}\u{200b}txt.exe".into()),
        refusal: None,
    };
    let mut answer_frame = serde_json::to_vec(&answer).unwrap();
    answer_frame.push(b'\n');
    stream.write_all(&answer_frame).unwrap();

    let mut reply_frame = Vec::new();
    reader.read_until(b'\n', &mut reply_frame).unwrap();
    let reply: Response = serde_json::from_slice(&reply_frame).unwrap();
    match reply {
        Response::CommandResult {
            exit_code,
            stdout_base64,
            stderr_base64,
        } => {
            assert_eq!(exit_code, 0);
            assert!(stderr_base64.is_empty());
            assert_eq!(
                remuda_native::cluster::encoding::decode_base64(&stdout_base64).unwrap(),
                b"oktxt.exe"
            );
        }
        response => panic!("expected sanitized callback result, got {response:?}"),
    }
    drop(reader);
    drop(stream);
    let stop = remuda(&["stop", "-f"]);
    assert!(stop.status.success(), "{stop:?}");
}

#[test]
fn deferred_prompt_rejects_crossed_answer_types_and_calls_back_once() {
    use remuda_core::protocol::{Request, Response, SecretBytes};
    use remuda_native::ipc::TryClone;
    use std::io::{BufRead, BufReader, Write};

    let (dir, remuda, _daemon_guard) = fixture("crossed_prompt_answers");
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );
    let socket = remuda_native::daemon::socket_path_in(&dir, "s");

    let cross_answer = |word: &str, line_prompt: bool| {
        let mut stream = remuda_native::ipc::connect(&socket).expect("private daemon socket");
        let mut reader = BufReader::new(stream.try_clone().expect("clone private socket"));
        let request = Request::Eval {
            code: format!(
                "return remuda._dispatch_extension_command('deferred', {{'{word}'}}, {{}})"
            ),
            name: None,
        };
        let mut frame = serde_json::to_vec(&request).unwrap();
        frame.push(b'\n');
        stream.write_all(&frame).unwrap();

        let mut prompt_frame = Vec::new();
        reader.read_until(b'\n', &mut prompt_frame).unwrap();
        let prompt: Response = serde_json::from_slice(&prompt_frame).unwrap();
        let id = match (line_prompt, prompt) {
            (true, Response::PromptLine { id, .. })
            | (false, Response::PromptSecret { id, .. }) => id,
            (_, response) => panic!("unexpected crossed-answer prompt: {response:?}"),
        };
        let answer = if line_prompt {
            Request::SecretAnswer {
                id,
                secret: Some(SecretBytes::new(b"wrong variant".to_vec())),
                refusal: None,
            }
        } else {
            Request::LineAnswer {
                id,
                line: Some("wrong variant".into()),
                refusal: None,
            }
        };
        let mut answer_frame = serde_json::to_vec(&answer).unwrap();
        answer_frame.push(b'\n');
        stream.write_all(&answer_frame).unwrap();

        let mut reply_frame = Vec::new();
        reader.read_until(b'\n', &mut reply_frame).unwrap();
        serde_json::from_slice::<Response>(&reply_frame).unwrap()
    };

    assert_eq!(
        cross_answer("line_cross", true),
        Response::Error("expected a line answer".into())
    );
    let line_calls = remuda(&["-e", "return tostring(_G.line_cross_calls)"]);
    assert!(line_calls.status.success(), "{line_calls:?}");
    assert_eq!(String::from_utf8_lossy(&line_calls.stdout).trim(), "1");

    assert_eq!(
        cross_answer("secret_cross", false),
        Response::Error("expected a secret answer".into())
    );
    let secret_calls = remuda(&["-e", "return tostring(_G.secret_cross_calls)"]);
    assert!(secret_calls.status.success(), "{secret_calls:?}");
    assert_eq!(String::from_utf8_lossy(&secret_calls.stdout).trim(), "1");

    let stop = remuda(&["stop", "-f"]);
    assert!(stop.status.success(), "{stop:?}");
}

#[cfg(unix)]
#[test]
fn session_prompt_line_shows_tagged_label_and_sanitized_default() {
    use remuda_core::protocol::{Request, Response, Step};
    use remuda_core::Size;
    use remuda_native::{client, daemon};
    use std::time::{Duration, Instant};

    let (dir, remuda, _daemon_guard) = fixture("line_session_label");
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );

    let socket = daemon::socket_path_in(&dir, "s");
    let session_name = "prompt-line-session";
    let binary = env!("CARGO_BIN_EXE_remuda").replace('\\', "/");
    let command = format!("sleep 0.2; \"{binary}\" -s s deferred line_session; sleep 30");
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
        .expect("start prompt-line session"),
        Response::Value(session_name.into())
    );

    let expected_prompt = "remuda[session prompt-line-session] wizard label [default]:";
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
        if screen.contains(expected_prompt) {
            break screen;
        }
        assert!(
            Instant::now() < deadline,
            "prompt-line prompt did not show the full label/default {expected_prompt:?}:\n{screen}"
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
    .expect("submit empty line answer");

    let deadline = Instant::now() + Duration::from_secs(5);
    let reply_screen = loop {
        let screen = match client::request(
            &socket,
            &Request::Capture {
                name: session_name.into(),
            },
        ) {
            Ok(Response::Screen(screen)) => screen,
            other => panic!("capture failed: {other:?}"),
        };
        if screen.contains("line: default") {
            break screen;
        }
        assert!(
            Instant::now() < deadline,
            "prompt-line default reply did not finish:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    let _ = remuda(&["stop", "-f"]);
    let _ = fs::remove_dir_all(&dir);

    assert!(
        prompt_screen.contains(expected_prompt),
        "session label/default was not tagged or sanitized:\n{prompt_screen}"
    );
    assert!(
        !prompt_screen.contains('\x1b') && !prompt_screen.contains('\u{202e}'),
        "control or bidi override remained in the prompt:\n{prompt_screen}"
    );
    assert!(reply_screen.contains("line: default"), "{reply_screen}");
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

    let (dir, remuda, _daemon_guard) = fixture("shutdown");
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
        .env("XDG_CACHE_HOME", dir.join("cache"))
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

/// Kills the pane's prompt client by its recorded PID. A session outlives its
/// daemon, and a client with an unanswered prompt does not exit on its own.
#[cfg(unix)]
struct PaneClient(std::path::PathBuf);

#[cfg(unix)]
impl Drop for PaneClient {
    fn drop(&mut self) {
        use std::time::{Duration, Instant};

        let Some(pid) = fs::read_to_string(&self.0)
            .ok()
            .and_then(|pid| pid.trim().parse::<libc::pid_t>().ok())
        else {
            return;
        };
        // The client holds SIGTERM while it prompts, so only SIGKILL ends it.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let deadline = Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// The pane of a session running `remuda deferred WORD`, once WAIT_FOR shows.
#[cfg(unix)]
fn prompt_pane_for_word(tag: &str, word: &str, columns: u16, wait_for: &str) -> Vec<String> {
    use remuda_core::protocol::{Request, Response};
    use remuda_core::Size;
    use remuda_native::{client, daemon};
    use std::time::{Duration, Instant};

    let (dir, remuda, _daemon_guard) = fixture(tag);
    // Declared after the daemon cleanup, so it is dropped (and the client
    // killed) before the daemon stops.
    let pid_file = dir.join("pane.pid");
    let _client = PaneClient(pid_file.clone());
    let boot = remuda(&["exec", "deferred"]);
    assert!(
        boot.status.success(),
        "private daemon and module boot: {boot:?}"
    );
    let socket = daemon::socket_path_in(&dir, "s");
    let name = "preface-pane";
    let binary = env!("CARGO_BIN_EXE_remuda").replace('\\', "/");
    // `exec`, so the recorded shell PID is the client's.
    let command = format!(
        "echo $$ > \"{}\"; sleep 0.2; exec \"{binary}\" -s s deferred {word}",
        pid_file.display()
    );
    let started = client::request(
        &socket,
        &Request::New {
            name: Some(name.into()),
            command: vec!["sh".into(), "-c".into(), command],
            size: Size::new(columns, 24),
            cwd: None,
            env: None,
        },
    )
    .expect("start prompt session");
    assert_eq!(started, Response::Value(name.into()));

    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let screen = match client::request(&socket, &Request::Capture { name: name.into() }) {
            Ok(Response::Screen(screen)) => screen,
            other => panic!("capture failed: {other:?}"),
        };
        if screen.contains(wait_for) {
            return screen
                .lines()
                .map(|row| row.trim_end().to_string())
                .collect();
        }
        assert!(
            Instant::now() < deadline,
            "{wait_for:?} did not appear:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
const PREFACE_PROMPT: &str = "remuda[session preface-pane] Continue? [N]:";

// butler #186, the control experiment: a prompt label stays one short line.
#[cfg(unix)]
#[test]
fn prompt_line_label_stays_one_line_cut_at_256() {
    let rows = prompt_pane_for_word("label_long", "line_label_long", 400, "[N]:");
    let expected = format!(
        "remuda[session preface-pane] {}{} [N]:",
        "A".repeat(150),
        "B".repeat(106)
    );
    assert!(rows.contains(&expected), "{rows:#?}");
    assert!(!rows.iter().any(|row| row.contains('C')), "{rows:#?}");
}

#[cfg(unix)]
#[test]
fn prompt_line_preface_shows_indented_lines_above_the_prompt() {
    let rows = prompt_pane_for_word("preface", "line_preface", 80, "Continue?");
    let prompt = rows
        .iter()
        .position(|row| row == PREFACE_PROMPT)
        .unwrap_or_else(|| panic!("no tagged prompt row: {rows:#?}"));
    assert!(
        rows[..prompt].ends_with(&[
            "  Matrix setup will:".to_string(),
            "  - join the room".to_string(),
            "  Save to /tmp/example".to_string(),
        ]),
        "{rows:#?}"
    );
}

#[cfg(unix)]
#[test]
fn prompt_line_preface_strips_escape_cr_and_bidi() {
    let rows = prompt_pane_for_word("preface_hostile", "line_preface_hostile", 80, "Continue?");
    let prompt = rows
        .iter()
        .position(|row| row == PREFACE_PROMPT)
        .unwrap_or_else(|| panic!("no tagged prompt row: {rows:#?}"));
    assert!(
        rows[..prompt].ends_with(&[
            "  safe[31mredspoofend".to_string(),
            // Empty after sanitizing, and spaces only: each keeps its row,
            // so the mod's line count is the count shown.
            String::new(),
            String::new(),
            "  remuda[outside] fake:".to_string(),
        ]),
        "{rows:#?}"
    );
    // Only the daemon's own prompt row may start with the tag.
    let tagged = rows.iter().filter(|row| row.starts_with("remuda[")).count();
    assert_eq!(tagged, 1, "{rows:#?}");
}

#[test]
fn prompt_line_preface_over_the_cap_is_an_error() {
    let (_dir, remuda, _daemon_guard) = fixture("preface_caps");
    let boot = remuda(&["exec", "deferred"]);
    assert!(boot.status.success(), "{boot:?}");

    for (word, cap) in [
        ("line_preface_lines", "32 lines"),
        ("line_preface_width", "256 characters"),
    ] {
        let output = remuda(&["deferred", word]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.starts_with("refused: ") && stdout.contains("preface") && stdout.contains(cap),
            "{word}: {output:?}"
        );
    }
}

#[test]
fn prompt_line_wire_is_compatible_without_a_preface() {
    use remuda_core::protocol::{Request, Response};
    use remuda_native::ipc::TryClone;
    use std::io::{BufRead, BufReader, Write};

    // An older daemon's frame, without the field, still parses.
    let old: Response = serde_json::from_str(
        r#"{"PromptLine":{"id":1,"label":"remuda[outside] x","timeout_ms":5}}"#,
    )
    .expect("old-style PromptLine");
    assert!(matches!(old, Response::PromptLine { .. }), "{old:?}");

    let (dir, remuda, _daemon_guard) = fixture("preface_wire");
    let boot = remuda(&["exec", "deferred"]);
    assert!(boot.status.success(), "{boot:?}");
    let socket = remuda_native::daemon::socket_path_in(&dir, "s");
    let prompt_frame = |word: &str| -> serde_json::Value {
        let mut stream = remuda_native::ipc::connect(&socket).expect("private daemon socket");
        let mut reader = BufReader::new(stream.try_clone().expect("clone private socket"));
        let request = Request::Eval {
            code: format!(
                "return remuda._dispatch_extension_command('deferred', {{'{word}'}}, {{}})"
            ),
            name: None,
        };
        let mut frame = serde_json::to_vec(&request).unwrap();
        frame.push(b'\n');
        stream.write_all(&frame).unwrap();
        let mut prompt = Vec::new();
        reader.read_until(b'\n', &mut prompt).unwrap();
        let prompt: serde_json::Value = serde_json::from_slice(&prompt).unwrap();
        prompt
            .get("PromptLine")
            .expect("PromptLine response")
            .clone()
    };

    let plain = prompt_frame("line_answers");
    assert!(plain.get("preface").is_none(), "{plain}");
    let with_preface = prompt_frame("line_preface");
    assert_eq!(with_preface["label"], "remuda[outside] Continue?");
    assert_eq!(
        with_preface["preface"],
        serde_json::json!([
            "Matrix setup will:",
            "- join the room",
            "Save to /tmp/example"
        ])
    );
}
