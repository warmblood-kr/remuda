//! Standalone synthetic process tests. Never contact a real Remuda daemon.
#![cfg(any(target_os = "linux", target_os = "macos"))]
use base64::Engine;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn command(operation: &str, root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_remuda-session-host"));
    command
        .args([operation])
        .arg(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}
fn frame(writer: &mut impl Write, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    writer
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    writer.write_all(&bytes).unwrap();
    writer.flush().unwrap();
}
fn rpc(root: &Path, control: bool, request: Value) -> Value {
    let mut child = command(if control { "rpc-control" } else { "rpc-view" }, root)
        .spawn()
        .unwrap();
    frame(&mut child.stdin.take().unwrap(), &request);
    wait_client(&mut child);
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes.len() <= 32 * 1024);
    serde_json::from_slice(&bytes).unwrap()
}
fn wait_client(child: &mut Child) {
    let status = wait_status(child);
    assert!(status.success(), "isolated client failed: {status}");
}
fn wait_status(child: &mut Child) -> std::process::ExitStatus {
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("isolated client timeout");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
struct Host {
    process: Child,
    parent: PathBuf,
    root: PathBuf,
    ready: Value,
}
impl Drop for Host {
    fn drop(&mut self) {
        let _ = fs::write(self.parent.join("release-descendant"), b"release");
        let _ = self.process.kill();
        let _ = self.process.wait();
        let _ = fs::remove_dir_all(&self.parent);
    }
}
impl Host {
    fn start() -> Self {
        Self::start_with(
            r#"stty -echo; count=0; printf '\033[?2004h\033[31mhost-ready\033[0m\n'; while IFS= read -r n; do count=$((count+1)); printf 'effect:%s:%s\n' "$count" "$((n+7))"; done"#,
        )
    }
    fn start_with(script: &str) -> Self {
        Self::start_argv(vec!["/bin/sh".into(), "-c".into(), script.into()])
    }
    fn start_argv(argv: Vec<String>) -> Self {
        Self::start_ack(argv, true)
    }
    fn start_ack(argv: Vec<String>, read_ack: bool) -> Self {
        #[cfg(target_os = "macos")]
        let parent = PathBuf::from("/private/tmp");
        #[cfg(not(target_os = "macos"))]
        let parent = PathBuf::from("/tmp");
        let parent = parent.join(format!(
            "rh-process-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&parent).unwrap();
        let root = parent.join("s");
        let mut process = command("run", &root).spawn().unwrap();
        if !read_ack {
            drop(process.stdout.take());
        }
        let launch = json!({"name":"isolated-host", "argv":argv, "cwd":parent, "agent":"synthetic", "rows":24, "cols":80, "env":[["PATH","/usr/bin:/bin"]]});
        frame(&mut process.stdin.take().unwrap(), &launch);
        let mut host = Self {
            process,
            parent,
            root,
            ready: Value::Null,
        };
        if !read_ack {
            let until = Instant::now() + Duration::from_secs(4);
            while !host.root.join("session.json").exists() {
                assert!(Instant::now() < until, "committed host manifest missing");
                std::thread::sleep(Duration::from_millis(5));
            }
            host.ready =
                serde_json::from_slice(&fs::read(host.root.join("session.json")).unwrap()).unwrap();
            host.wait_screen("host-ready");
            return host;
        }
        let mut stdout = host.process.stdout.take().unwrap();
        let (sent, received) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = (|| {
                let mut header = [0; 4];
                stdout.read_exact(&mut header)?;
                let len = u32::from_be_bytes(header) as usize;
                if len > 8192 {
                    return Err(std::io::Error::other("startup manifest budget"));
                }
                let mut data = vec![0; len];
                stdout.read_exact(&mut data)?;
                Ok(data)
            })();
            let _ = sent.send(result);
        });
        let bytes = received
            .recv_timeout(Duration::from_secs(4))
            .expect("host startup timeout")
            .expect("host startup failed");
        host.ready = serde_json::from_slice(&bytes).unwrap();
        host.wait_screen("host-ready");
        host
    }
    fn inspect(&self) -> Value {
        let value = rpc(&self.root, false, json!({"op":"Inspect"}));
        assert_eq!(value["result"], "Info");
        value["info"].clone()
    }
    fn screen_state(&self) -> vt100::Parser {
        let mut child = command("screen", &self.root).spawn().unwrap();
        drop(child.stdin.take());
        wait_client(&mut child);
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        let data = base64::engine::general_purpose::STANDARD
            .decode(value["screen_base64"].as_str().unwrap())
            .unwrap();
        let mut parser = vt100::Parser::new(
            value["rows"].as_u64().unwrap() as u16,
            value["cols"].as_u64().unwrap() as u16,
            0,
        );
        parser.process(&data);
        parser
    }
    fn screen(&self) -> String {
        self.screen_state().screen().contents()
    }
    fn wait_screen(&self, expected: &str) {
        let until = Instant::now() + Duration::from_secs(4);
        while !self.screen().contains(expected) {
            assert!(Instant::now() < until, "synthetic screen timeout");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn stop(&mut self) {
        assert_eq!(rpc(&self.root, true, json!({"op":"Close"}))["result"], "Ok");
        let until = Instant::now() + Duration::from_secs(3);
        while self.inspect()["output_eof"] != true {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(rpc(&self.root, true, json!({"op":"Stop"}))["result"], "Ok");
        wait_client(&mut self.process);
    }
}
fn outcome_request(act: &Value, op: &str) -> Value {
    json!({"op":op,"client":vec![7;16],"sequence":1,"act":act})
}
#[test]
fn controller_sigkill_preserves_child_identity_terminal_and_admitted_input_result() {
    let mut host = Host::start();
    let initial = host.inspect();
    assert_eq!(
        host.screen_state().screen().cell(0, 0).unwrap().fgcolor(),
        vt100::Color::Idx(1)
    );
    assert!(host.screen_state().screen().bracketed_paste());
    let act = json!({"kind":"Feed","value":[{"kind":"Bytes","value":[51,53]},{"kind":"Pause","value":1000},{"kind":"Bytes","value":[10]}]});
    let mut controller = command("hold-input", &host.root).spawn().unwrap();
    frame(
        &mut controller.stdin.take().unwrap(),
        &outcome_request(&act, "Input"),
    );
    let mut output = std::io::BufReader::new(controller.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["result"],
        "Admitted"
    );
    controller.kill().unwrap();
    controller.wait().unwrap();
    let recovering = rpc(&host.root, false, outcome_request(&act, "Recover"));
    assert_eq!(recovering["result"], "Outcome");
    host.wait_screen("effect:1:42");
    let until = Instant::now() + Duration::from_secs(3);
    let complete = loop {
        let value = rpc(&host.root, false, outcome_request(&act, "Recover"));
        if !value["completion"].is_null() {
            break value;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(complete["completion"]["delivery"], "Written");
    let current = host.inspect();
    assert_eq!(current["identity"], initial["identity"]);
    assert_eq!(current["pid"], initial["pid"]);
    assert_eq!(current["lifecycle"], "Running");
    assert_eq!(
        host.screen_state().screen().cell(0, 0).unwrap().fgcolor(),
        vt100::Color::Idx(1)
    );
    assert!(host.screen_state().screen().bracketed_paste());
    let duplicate = rpc(&host.root, true, outcome_request(&act, "Input"));
    assert_eq!(duplicate["completion"], complete["completion"]);
    assert!(!host.screen().contains("effect:2:"));
    assert_eq!(
        rpc(&host.root, true, json!({"op":"Resize","rows":25,"cols":90}))["result"],
        "Ok"
    );
    assert_eq!(host.inspect()["cols"], 90);
    assert_eq!(host.screen_state().screen().size(), (25, 90));
    host.stop();
}

struct SessionClient {
    child: Child,
    input: std::process::ChildStdin,
    responses: std::sync::mpsc::Receiver<Vec<u8>>,
}
impl Drop for SessionClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl SessionClient {
    fn view(root: &Path) -> Self {
        let mut child = command("rpc-session-view", root).spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (sent, responses) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut output = std::io::BufReader::new(output);
            loop {
                let mut bytes = Vec::new();
                if output
                    .read_until(b'\n', &mut bytes)
                    .ok()
                    .is_none_or(|n| n == 0)
                {
                    break;
                }
                if sent.send(bytes).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            responses,
        }
    }
    fn request(&mut self, value: Value) -> Value {
        frame(&mut self.input, &value);
        let bytes = self
            .responses
            .recv_timeout(Duration::from_secs(4))
            .expect("synthetic RPC response timeout");
        serde_json::from_slice(&bytes).unwrap()
    }
}
#[test]
fn view_role_wrong_manifest_and_incomplete_snapshot_ack_do_not_mutate_host() {
    let mut host = Host::start();
    let original = host.inspect();
    assert_eq!(
        rpc(
            &host.root,
            false,
            json!({"op":"Resize","rows":30,"cols":100})
        )["result"],
        "Error"
    );
    assert_eq!(
        rpc(&host.root, false, json!({"op":"Close"}))["result"],
        "Error"
    );
    assert_eq!(host.inspect()["cols"], original["cols"]);
    assert_eq!(host.inspect()["lifecycle"], "Running");
    let path = host.root.join("session.json");
    let bytes = fs::read(&path).unwrap();
    let mut wrong: Value = serde_json::from_slice(&bytes).unwrap();
    wrong["identity"]["instance"] = json!("wrong-instance");
    fs::write(&path, serde_json::to_vec(&wrong).unwrap()).unwrap();
    let mut refused = command("inspect", &host.root).spawn().unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    let status = loop {
        if let Some(status) = refused.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(!status.success());
    fs::write(&path, bytes).unwrap();
    let mut viewer = SessionClient::view(&host.root);
    let snapshot = viewer.request(json!({"op":"Snapshot"}));
    assert_eq!(snapshot["result"], "Snapshot");
    let id = snapshot["snapshot"].clone();
    let generation = snapshot["generation"].clone();
    assert_eq!(
        viewer.request(json!({"op":"Acknowledge","snapshot":id,"generation":generation}))["result"],
        "Error"
    );
    for index in 0..snapshot["pages"].as_u64().unwrap() {
        assert_eq!(
            viewer.request(json!({"op":"Page","snapshot":id,"index":index}))["result"],
            "Page"
        );
    }
    assert_eq!(viewer.request(json!({"op":"Snapshot"}))["result"], "Error"); // sent pages are not baseline ACK
    assert_eq!(
        viewer.request(json!({"op":"Acknowledge","snapshot":id,"generation":generation}))["result"],
        "Ok"
    );
    assert_eq!(
        viewer.request(json!({"op":"Acknowledge","snapshot":id,"generation":generation}))["result"],
        "Error"
    );
    drop(viewer);
    assert_eq!(host.inspect()["identity"], original["identity"]);
    host.stop();
}

#[test]
fn preauthentication_admission_is_bounded_without_stalling_pty_parser() {
    use std::os::unix::net::UnixStream;
    let mut host = Host::start_with(
        r#"stty -echo; printf 'host-ready\n'; read start; i=0; while :; do i=$((i+1)); printf 'progress:%s xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n' "$i"; sleep 0.01; done; read ignored"#,
    );
    let mut trusted = SessionClient::view(&host.root);
    let setup = Instant::now() + Duration::from_secs(1);
    while trusted.request(json!({"op":"Inspect"}))["info"]["active_connections"] != 1 {
        assert!(
            Instant::now() < setup,
            "prior fixture connections not reclaimed"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let start = json!({"kind":"Bytes","value":[10]});
    assert_eq!(
        rpc(&host.root, true, outcome_request(&start, "Input"))["result"],
        "Admitted"
    );
    while trusted.request(json!({"op":"Inspect"}))["info"]["active_connections"] != 1 {
        assert!(Instant::now() < setup);
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut attackers = Vec::new();
    for _ in 0..15 {
        attackers.push(UnixStream::connect(host.root.join("socket")).unwrap());
    }
    let until = Instant::now() + Duration::from_secs(1);
    let before = loop {
        let info = trusted.request(json!({"op":"Inspect"}));
        if info["info"]["active_connections"] == 16 {
            break info["info"]["generation"].as_u64().unwrap();
        }
        assert!(Instant::now() < until, "preauth workers did not reach cap");
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut rejected = UnixStream::connect(host.root.join("socket")).unwrap();
    rejected.set_nonblocking(true).unwrap();
    loop {
        match rejected.read(&mut [0; 1]) {
            Ok(0) => break,
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("unexpected capped unauthenticated connection: {other:?}"),
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    loop {
        let info = trusted.request(json!({"op":"Inspect"}));
        assert!(info["info"]["active_connections"].as_u64().unwrap() <= 16);
        if info["info"]["generation"].as_u64().unwrap() > before {
            break;
        }
        assert!(
            Instant::now() < until,
            "PTY parser stalled behind handshake workers"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(attackers);
    drop(rejected);
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let info = trusted.request(json!({"op":"Inspect"}));
        if info["info"]["active_connections"].as_u64().unwrap() == 1 {
            break;
        }
        assert!(Instant::now() < until, "preauth permits leaked");
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(trusted);
    host.stop();
}

#[test]
fn controller_sigkill_after_first_effect_preserves_remainder_and_dedup() {
    let mut host = Host::start();
    let act = json!({"kind":"Feed","value":[{"kind":"Bytes","value":[51,53,10]},{"kind":"Pause","value":3000},{"kind":"Bytes","value":[52,52,10]}]});
    let mut controller = command("hold-input", &host.root).spawn().unwrap();
    frame(
        &mut controller.stdin.take().unwrap(),
        &outcome_request(&act, "Input"),
    );
    let mut output = std::io::BufReader::new(controller.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["result"],
        "Admitted"
    );
    host.wait_screen("effect:1:42");
    assert!(!host.screen().contains("effect:2:"));
    controller.kill().unwrap();
    controller.wait().unwrap();
    let pending = rpc(&host.root, false, outcome_request(&act, "Recover"));
    assert_eq!(pending["state"], "Pending");
    let mut refused = command("rpc-control", &host.root).spawn().unwrap();
    frame(
        &mut refused.stdin.take().unwrap(),
        &json!({"op":"Resize","rows":25,"cols":90}),
    );
    assert!(!wait_status(&mut refused).success());
    assert_eq!(host.inspect()["cols"], 80);
    host.wait_screen("effect:2:51");
    let until = Instant::now() + Duration::from_secs(2);
    let complete = loop {
        let value = rpc(&host.root, false, outcome_request(&act, "Recover"));
        if value["state"] == "Complete" {
            break value;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(complete["completion"]["delivery"], "Written");
    assert_eq!(
        rpc(&host.root, true, outcome_request(&act, "Input"))["completion"],
        complete["completion"]
    );
    assert!(!host.screen().contains("effect:3:"));
    host.stop();
}

#[test]
fn descendant_lifetime_and_terminal_end_are_distinct_from_direct_child_exit() {
    let script = r#"stty -echo; (trap '' HUP; : > descendant-ready; n=0; while [ ! -e release-descendant ] && [ "$n" -lt 500 ]; do n=$((n+1)); printf '%s\n' "$n" > descendant-heartbeat.tmp; mv descendant-heartbeat.tmp descendant-heartbeat; sleep 0.01; done; printf 'descendant-final:%s\n' "$((6*7))"; : > descendant-done) & n=0; while [ ! -e descendant-ready ] && [ "$n" -lt 200 ]; do n=$((n+1)); sleep 0.01; done; printf 'host-ready\n'; exit 7"#;
    let mut host = Host::start_with(script);
    let until = Instant::now() + Duration::from_secs(3);
    let exited = loop {
        let info = host.inspect();
        if info["lifecycle"] == "Exited" {
            break info;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(exited["exit_code"], 7);
    let heartbeat = || {
        fs::read_to_string(host.parent.join("descendant-heartbeat"))
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
    };
    let before = loop {
        if let Some(value) = heartbeat() {
            break value;
        }
        assert!(Instant::now() < until, "missing descendant heartbeat");
        std::thread::sleep(Duration::from_millis(5));
    };
    while !heartbeat().is_some_and(|value| value >= before + 2) {
        assert!(
            Instant::now() < until,
            "descendant did not remain alive after direct child exit"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    fs::write(host.parent.join("release-descendant"), b"release").unwrap();
    while !host.parent.join("descendant-done").exists() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    let drained = loop {
        let info = host.inspect();
        if info["output_eof"] == true {
            break info;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(matches!(
        drained["output_end"].as_str(),
        Some("Eof" | "Hangup")
    ));
    assert_eq!(drained["exit_code"], 7);
    assert!(drained["drained_generation"].as_u64().is_some());
    host.stop();
}

#[test]
fn closing_terminal_handles_does_not_invent_process_exit_or_allow_stop() {
    let mut host = Host::start_argv(vec![
        env!("CARGO_BIN_EXE_session_host_fixture").into(),
        "close-terminal-handles".into(),
    ]);
    let until = Instant::now() + Duration::from_secs(3);
    while !host.parent.join("terminal-handles-closed").exists() {
        assert!(
            Instant::now() < until,
            "native stdio closure did not complete"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let ended = host.inspect();
    assert_eq!(ended["lifecycle"], "Running", "{ended}");
    assert!(ended["exit_code"].is_null());
    // Native PTYs may defer EOF even after user descriptors/ctty are closed;
    // only the sole wait result, never this terminal condition, establishes exit.
    assert!(matches!(
        ended["output_end"].as_str(),
        Some("Open" | "Eof" | "Hangup")
    ));
    assert_eq!(
        rpc(&host.root, true, json!({"op":"Stop"}))["result"],
        "Error"
    );
    fs::write(host.parent.join("finish-main"), b"finish").unwrap();
    while host.inspect()["lifecycle"] != "Exited" {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(host.inspect()["exit_code"], 7);
    host.stop();
}

#[test]
fn lost_launch_ack_preserves_committed_host_and_same_root_retry_cannot_duplicate() {
    let script = "stty -echo; printf 'host-ready\\n'; read ignored";
    let mut host = Host::start_ack(vec!["/bin/sh".into(), "-c".into(), script.into()], false);
    let before = host.inspect();
    assert_eq!(before["identity"], host.ready["identity"]);
    assert_eq!(before["lifecycle"], "Running");
    let mut duplicate = command("run", &host.root).spawn().unwrap();
    frame(
        &mut duplicate.stdin.take().unwrap(),
        &json!({"name":"isolated-host","argv":["/bin/sh","-c",script],"cwd":host.parent,"agent":"synthetic","rows":24,"cols":80,"env":[["PATH","/usr/bin:/bin"]]}),
    );
    assert!(!wait_status(&mut duplicate).success());
    assert_eq!(host.inspect()["identity"], before["identity"]);
    assert_eq!(host.inspect()["pid"], before["pid"]);
    host.stop();
}
