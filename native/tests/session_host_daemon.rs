//! Opt-in host-backed sessions through a real daemon process (#713).
//! The daemon is SIGKILLed and restarted, and it reattaches from its
//! private inventory. The host is a separate `remuda-session-host` process.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(15);
static NEXT: AtomicU64 = AtomicU64::new(0);

/// A short private root: host socket paths must fit `sun_path`.
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let base = if cfg!(target_os = "macos") {
            "/private/tmp"
        } else {
            "/tmp"
        };
        let path = PathBuf::from(base).join(format!(
            "rhd-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // Private regardless of umask: host directories refuse shared ancestors.
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
    fn socket(&self) -> PathBuf {
        daemon::socket_path_in(&self.0, "s")
    }
    fn hosts(&self) -> PathBuf {
        self.0.join("remuda").join("s.sock.hosts")
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        // Only processes whose argv names this unique root: our hosts, and
        // a daemon that took the socket over in a reload.
        for pid in processes_under(&self.0) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Daemon(Child);
impl Daemon {
    fn spawn(root: &Root, opt_in: bool) -> Self {
        Self::spawn_exe(root, Path::new(env!("CARGO_BIN_EXE_remuda")), opt_in)
    }
    /// The daemon launches the `remuda-session-host` beside `exe`.
    fn spawn_exe(root: &Root, exe: &Path, opt_in: bool) -> Self {
        let mut command = Command::new(exe);
        command
            .args(["-s", "s", "daemon"])
            .env("REMUDA_RUNTIME_DIR", &root.0)
            .env("HOME", &root.0)
            .env("XDG_STATE_HOME", root.0.join("state"))
            .env_remove("REMUDA_EXPERIMENTAL_SESSION_HOST")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if opt_in {
            command.env("REMUDA_EXPERIMENTAL_SESSION_HOST", "1");
        }
        let daemon = Self(command.spawn().expect("spawn daemon"));
        let deadline = Instant::now() + PATIENCE;
        while remuda_native::ipc::connect(&root.socket()).is_err() {
            assert!(Instant::now() < deadline, "daemon never bound");
            std::thread::sleep(Duration::from_millis(10));
        }
        daemon
    }
    fn sigkill(mut self) {
        let pid = self.0.id() as libc::pid_t;
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        let _ = self.0.wait();
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn request(path: &Path, request: &Request) -> Response {
    client::request(path, request).expect("daemon request")
}
fn new_session(path: &Path, name: &str) {
    let response = request(
        path,
        &Request::New {
            name: Some(name.into()),
            command: vec!["/bin/sh".into()],
            size: Size::new(80, 24),
            cwd: Some("/".into()),
            env: None,
        },
    );
    assert_eq!(response, Response::Value(name.into()));
}
fn send_line(path: &Path, name: &str, text: &str) -> Response {
    request(
        path,
        &Request::SendLine {
            name: name.into(),
            text: text.into(),
        },
    )
}
fn capture(path: &Path, name: &str) -> Response {
    request(path, &Request::Capture { name: name.into() })
}
fn wait_for(path: &Path, name: &str, needle: &str) -> String {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Response::Screen(screen) = capture(path, name) {
            if screen.contains(needle) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "{needle:?} never appeared:\n{screen}"
            );
        } else {
            assert!(Instant::now() < deadline, "capture kept failing");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn names(path: &Path) -> Vec<String> {
    match request(path, &Request::List) {
        Response::Sessions(list) => list.into_iter().map(|s| s.name).collect(),
        other => panic!("list failed: {other:?}"),
    }
}
fn pid_after(screen: &str, marker: &str) -> i32 {
    let rest = &screen[screen.find(marker).unwrap() + marker.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().expect("a pid")
}
fn alive(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}
fn wait_gone(pid: i32) {
    let deadline = Instant::now() + PATIENCE;
    while alive(pid) {
        assert!(Instant::now() < deadline, "{pid} stayed alive");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn processes_naming(root: &Path) -> Vec<i32> {
    processes_matching(root, "run-reserved")
}
fn processes_under(root: &Path) -> Vec<i32> {
    processes_matching(root, "")
}
fn processes_matching(root: &Path, also: &str) -> Vec<i32> {
    let output = Command::new("/bin/ps")
        .args(["-A", "-o", "pid=", "-o", "args="])
        .output()
        .expect("ps");
    // The trailing slash keeps root `…-1` from matching `…-10`.
    let needle = format!("{}/", root.to_string_lossy());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(&needle) && line.contains(also))
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}
fn host_pid(root: &Root) -> i32 {
    let hosts = processes_naming(&root.0);
    assert_eq!(hosts.len(), 1, "exactly one host process: {hosts:?}");
    hosts[0]
}

#[test]
fn without_the_opt_in_new_sessions_stay_in_process() {
    let root = Root::new();
    let daemon = Daemon::spawn(&root, false);
    let path = root.socket();
    new_session(&path, "plain");
    send_line(&path, "plain", "echo IN:$((40+2))");
    wait_for(&path, "plain", "IN:42");
    assert!(processes_naming(&root.0).is_empty(), "no host process");
    assert!(!root.hosts().exists(), "no host inventory is created");
    drop(daemon);
}

#[test]
fn a_hosted_session_survives_daemon_sigkill_and_is_rediscovered() {
    let root = Root::new();
    let path = root.socket();
    let first = Daemon::spawn(&root, true);
    new_session(&path, "keeper");
    assert_eq!(
        send_line(&path, "keeper", "echo PID:$((6*7-42))$$"),
        Response::Ok
    );
    let screen = wait_for(&path, "keeper", "PID:0");
    let child = pid_after(&screen, "PID:0");
    let host = host_pid(&root);
    first.sigkill();

    // The child and its host outlive the daemon.
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(child), "child died with the daemon");
    assert!(alive(host), "host died with the daemon");

    // A restarted daemon, even without the opt-in, reattaches.
    let second = Daemon::spawn(&root, false);
    assert_eq!(names(&path), vec!["keeper".to_owned()]);
    let screen = wait_for(&path, "keeper", "PID:0");
    assert_eq!(pid_after(&screen, "PID:0"), child, "same child");
    assert_eq!(
        send_line(&path, "keeper", "echo AGAIN:$((1+1))"),
        Response::Ok
    );
    wait_for(&path, "keeper", "AGAIN:2");
    // Re-read after a quiet period, so a late duplicate would show.
    std::thread::sleep(Duration::from_millis(500));
    let screen = wait_for(&path, "keeper", "AGAIN:2");
    assert_eq!(screen.matches("AGAIN:2").count(), 1, "sent exactly once");

    // The host dies: the daemon keeps the name and answers Unavailable.
    assert_eq!(unsafe { libc::kill(host, libc::SIGKILL) }, 0);
    wait_gone(host);
    let unavailable = |response: Response| match response {
        Response::Error(message) => assert!(
            message.contains("unavailable"),
            "not an Unavailable answer: {message}"
        ),
        other => panic!("expected Unavailable, got {other:?}"),
    };
    unavailable(capture(&path, "keeper"));
    unavailable(send_line(&path, "keeper", "echo LOST"));
    // Listed, not exited, and marked: its size and idle time are unknown.
    let Response::Sessions(sessions) = request(&path, &Request::List) else {
        panic!("ls failed");
    };
    assert!(sessions[0].unavailable && sessions[0].alive, "{sessions:?}");
    let listed = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s", "ls"])
        .env("REMUDA_RUNTIME_DIR", &root.0)
        .env("HOME", &root.0)
        .output()
        .expect("remuda ls");
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.contains("keeper") && listed.contains("unreachable"),
        "{listed}"
    );
    second.sigkill();

    // After another restart the entry is still listed, and still Unavailable.
    let third = Daemon::spawn(&root, false);
    assert_eq!(names(&path), vec!["keeper".to_owned()]);
    unavailable(capture(&path, "keeper"));
    drop(third);
}

fn close(path: &Path, name: &str) -> Response {
    request(
        path,
        &Request::Close {
            name: name.into(),
            instance_id: None,
            confirm: None,
        },
    )
}

#[test]
fn closing_a_hosted_session_stops_its_host_and_retires_it() {
    let root = Root::new();
    let path = root.socket();
    let first = Daemon::spawn(&root, true);
    new_session(&path, "brief");
    send_line(&path, "brief", "echo UP:$((2+3))");
    wait_for(&path, "brief", "UP:5");
    let host = host_pid(&root);
    assert_eq!(close(&path, "brief"), Response::Ok);
    assert!(names(&path).is_empty());
    wait_gone(host);
    // Retired: the name can be used again once the teardown finished.
    let deadline = Instant::now() + PATIENCE;
    loop {
        let response = request(
            &path,
            &Request::New {
                name: Some("brief".into()),
                command: vec!["/bin/sh".into()],
                size: Size::new(80, 24),
                cwd: Some("/".into()),
                env: None,
            },
        );
        if response == Response::Value("brief".into()) {
            break;
        }
        assert!(Instant::now() < deadline, "name never freed: {response:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let second_host = host_pid(&root);
    assert_eq!(close(&path, "brief"), Response::Ok);
    first.sigkill();

    // Closed: a restarted daemon does not bring it back, even mid-teardown.
    let second = Daemon::spawn(&root, false);
    assert!(names(&path).is_empty());
    wait_gone(second_host);
    drop(second);
}

#[test]
fn a_session_whose_host_died_can_still_be_closed_and_stays_closed() {
    let root = Root::new();
    let path = root.socket();
    let first = Daemon::spawn(&root, true);
    new_session(&path, "orphan");
    send_line(&path, "orphan", "echo UP:$((2+3))");
    wait_for(&path, "orphan", "UP:5");
    let host = host_pid(&root);
    assert_eq!(unsafe { libc::kill(host, libc::SIGKILL) }, 0);
    wait_gone(host);
    assert_eq!(close(&path, "orphan"), Response::Ok);
    assert!(names(&path).is_empty());
    first.sigkill();
    let second = Daemon::spawn(&root, false);
    assert!(names(&path).is_empty(), "a closed session came back");
    drop(second);
}

/// The previous build N: a directory with its `remuda` and `remuda-session-host`.
/// CI builds it from the base commit; there is no default, so it cannot pass unmeasured.
fn previous() -> PathBuf {
    let dir = std::env::var_os("REMUDA_COMPAT_PREVIOUS")
        .expect("REMUDA_COMPAT_PREVIOUS names the previous build's binary directory");
    let dir = PathBuf::from(dir);
    for bin in ["remuda", "remuda-session-host"] {
        assert!(
            dir.join(bin).is_file(),
            "{bin} missing in {}",
            dir.display()
        );
    }
    dir.join("remuda")
}
/// One daemon creates a hosted session and dies; the other adopts its
/// inventory and host, then sends input and closes through it.
fn hand_over(first: &Path, second: &Path) {
    let root = Root::new();
    let path = root.socket();
    let creator = Daemon::spawn_exe(&root, first, true);
    new_session(&path, "compat");
    send_line(&path, "compat", "echo BEFORE:$((6*7))");
    wait_for(&path, "compat", "BEFORE:42");
    let host = host_pid(&root);
    creator.sigkill();
    let adopter = Daemon::spawn_exe(&root, second, false);
    assert_eq!(names(&path), vec!["compat".to_owned()]);
    assert_eq!(
        send_line(&path, "compat", "echo AFTER:$((7*8))"),
        Response::Ok
    );
    wait_for(&path, "compat", "AFTER:56");
    assert_eq!(close(&path, "compat"), Response::Ok);
    wait_gone(host);
    assert!(names(&path).is_empty());
    drop(adopter);
}

#[test]
#[ignore = "N/N+1: needs REMUDA_COMPAT_PREVIOUS; run by CI job host-compat-n-plus-one"]
fn n_plus_one_daemon_adopts_an_older_host_and_inventory() {
    hand_over(&previous(), Path::new(env!("CARGO_BIN_EXE_remuda")));
}

#[test]
#[ignore = "N/N+1: needs REMUDA_COMPAT_PREVIOUS; run by CI job host-compat-n-plus-one"]
fn n_plus_one_older_daemon_adopts_a_newer_host_and_inventory() {
    hand_over(Path::new(env!("CARGO_BIN_EXE_remuda")), &previous());
}

fn daemon_id_in(path: &Path, name: &str) -> String {
    // The marker is computed, so the echoed command line cannot match it.
    assert_eq!(
        send_line(path, name, "echo \"D$((1+1))=$REMUDA_DAEMON_ID.\""),
        Response::Ok
    );
    let screen = wait_for(path, name, "D2=");
    let rest = &screen[screen.find("D2=").unwrap() + 3..];
    rest[..rest.find('.').expect("terminated id")].to_owned()
}

fn run_in(path: &Path, name: &str, command: &str, done: &str) -> String {
    let line = format!("printf '\\033[2J\\033[H'; {command}; echo {done}$((1+1))");
    assert_eq!(send_line(path, name, &line), Response::Ok);
    wait_for(path, name, &format!("{done}2"))
}

#[test]
fn an_agent_hosted_by_an_earlier_daemon_cannot_stop_its_replacement() {
    let root = Root::new();
    let path = root.socket();
    let binary = env!("CARGO_BIN_EXE_remuda");
    let first = Daemon::spawn(&root, true);
    new_session(&path, "agent");
    let stable = daemon_id_in(&path, "agent");
    let stored = std::fs::read_to_string(root.0.join("remuda").join("s.sock.daemon-id")).unwrap();
    assert_eq!(stable, stored.trim_end(), "sessions carry the stored id");
    assert_ne!(stable, first.0.id().to_string(), "not the process ID");
    first.sigkill();

    // The replacement daemon has a new pid but the same id, for every session.
    let second = Daemon::spawn(&root, false);
    new_session(&path, "fresh");
    assert_eq!(daemon_id_in(&path, "fresh"), stable);

    // Inherited identity: the hosted agent's REMUDA_* environment is intact.
    let screen = run_in(
        &path,
        "agent",
        &format!("'{binary}' -s s stop -f --yes"),
        "A",
    );
    assert!(screen.contains("cannot stop this daemon"), "{screen}");
    // Stripped identity: only the host child's pid in its ancestry refuses it.
    let stripped = format!(
        "/usr/bin/env -i PATH=\"$PATH\" REMUDA_RUNTIME_DIR=\"$REMUDA_RUNTIME_DIR\" '{binary}' -s s stop -f --yes"
    );
    let screen = run_in(&path, "agent", &stripped, "B");
    assert!(screen.contains("cannot stop this daemon"), "{screen}");
    assert!(
        remuda_native::ipc::connect(&path).is_ok(),
        "the daemon was stopped"
    );

    // The stored id alone, with a session claim, names this daemon too.
    let claimed = request(
        &path,
        &Request::Shutdown {
            requester_daemon_id: Some(stable.clone()),
            requester_session_id: Some("stale-session-id".into()),
            requester_session_name: None,
            override_hosted: false,
        },
    );
    assert!(
        matches!(&claimed, Response::Error(reason) if reason.contains("one of its own sessions")),
        "{claimed:?}"
    );

    // A caller outside every session, legacy or hosted, still stops it.
    let stopped = request(
        &path,
        &Request::Shutdown {
            requester_daemon_id: None,
            requester_session_id: None,
            requester_session_name: None,
            override_hosted: false,
        },
    );
    assert_eq!(stopped, Response::Ok);
    drop(second);
}

/// `remuda reload [--check]` run by the CLI build `cli`, against the daemon
/// at `root`, into `target`; answers the exit status, stdout and stderr.
fn reload_check(
    root: &Root,
    cli: &Path,
    target: &Path,
    check: bool,
) -> (Option<i32>, String, String) {
    let mut command = Command::new(cli);
    command.args(["-s", "s", "reload"]);
    if check {
        command.arg("--check");
    }
    let out = command
        .arg("--binary")
        .arg(target)
        .env("REMUDA_RUNTIME_DIR", &root.0)
        .env("HOME", &root.0)
        .env("XDG_STATE_HOME", root.0.join("state"))
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .output()
        .expect("reload check");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Whatever either build supports, a reload or its check across N and N+1
/// leaves the hosted session working, and a refusal says why.
fn check_leaves_the_session_alone(daemon_exe: &Path, cli: &Path, target: &Path, check: bool) {
    let root = Root::new();
    let path = root.socket();
    let daemon = Daemon::spawn_exe(&root, daemon_exe, true);
    new_session(&path, "compat");
    let (code, stdout, stderr) = reload_check(&root, cli, target, check);
    eprintln!("reload (check {check}): exit {code:?}\n{stdout}{stderr}");
    // Builds that speak different handoff versions refuse each other cleanly.
    let understood = stdout.starts_with("ready: ")
        || stdout.starts_with("reloaded: ")
        || stderr.contains("candidate exited before its hello")
        || stderr.contains("predates `remuda reload`")
        || stderr.contains(" speaks handoff version ")
        || stderr.contains("reload without --check is not implemented yet");
    assert!(understood, "exit {code:?}\n{stdout}\n{stderr}");
    // `remuda stop` would end every session: never the advice here.
    assert!(!stderr.contains("run `remuda stop`"), "{stderr}");
    assert_eq!(
        send_line(&path, "compat", "echo AFTER:$((7*8))"),
        Response::Ok
    );
    wait_for(&path, "compat", "AFTER:56");
    drop(daemon);
}

#[test]
#[ignore = "N/N+1: needs REMUDA_COMPAT_PREVIOUS; run by CI job host-compat-n-plus-one"]
fn n_plus_one_daemon_checks_a_reload_into_the_older_build() {
    let this = Path::new(env!("CARGO_BIN_EXE_remuda"));
    check_leaves_the_session_alone(this, this, &previous(), true);
}

#[test]
#[ignore = "N/N+1: needs REMUDA_COMPAT_PREVIOUS; run by CI job host-compat-n-plus-one"]
fn n_plus_one_older_daemon_is_asked_to_check_a_reload() {
    let this = Path::new(env!("CARGO_BIN_EXE_remuda"));
    check_leaves_the_session_alone(&previous(), this, this, true);
}

#[test]
#[ignore = "N/N+1: needs REMUDA_COMPAT_PREVIOUS; run by CI job host-compat-n-plus-one"]
fn n_plus_one_daemon_reloads_into_the_older_build() {
    let this = Path::new(env!("CARGO_BIN_EXE_remuda"));
    check_leaves_the_session_alone(this, this, &previous(), false);
}

#[test]
#[ignore = "N/N+1: needs REMUDA_COMPAT_PREVIOUS; run by CI job host-compat-n-plus-one"]
fn n_plus_one_older_daemon_is_asked_to_reload() {
    let this = Path::new(env!("CARGO_BIN_EXE_remuda"));
    check_leaves_the_session_alone(&previous(), this, this, false);
}

#[test]
fn a_rediscovered_hosted_session_still_wakes_output_hooks() {
    let root = Root::new();
    let path = root.socket();
    let first = Daemon::spawn(&root, true);
    new_session(&path, "keeper");
    wait_for(&path, "keeper", "$");
    first.sigkill();

    let _second = Daemon::spawn(&root, false);
    let log = root.0.join("output.log");
    let hook = format!(
        "remuda.on('session_output', function(name) \
           local f = assert(io.open('{}', 'a')); f:write(name, '\\n'); f:close() end, {{ group = 'test' }})",
        log.display()
    );
    assert!(matches!(
        request(
            &path,
            &Request::Eval {
                code: hook,
                name: None
            }
        ),
        Response::Value(_)
    ));
    // Only output after the hook counts: clear what the catch-up wake wrote.
    std::thread::sleep(Duration::from_millis(500));
    let _ = std::fs::remove_file(&log);
    assert_eq!(
        send_line(&path, "keeper", "echo AFTER:$((2+2))"),
        Response::Ok
    );
    wait_for(&path, "keeper", "AFTER:4");
    let deadline = Instant::now() + PATIENCE;
    while !std::fs::read_to_string(&log).is_ok_and(|text| text.contains("keeper")) {
        assert!(
            Instant::now() < deadline,
            "no session_output after rediscovery"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `remuda upgrade`'s reload across N and N+1: the daemon moves onto the
/// installed build, or keeps its own and says why; the session lives on.
fn upgrade_across_builds(daemon_exe: &Path, installed: &Path) {
    use remuda_native::reload::upgrade::{after_upgrade, AfterUpgrade};
    let root = Root::new();
    let path = root.socket();
    let daemon = Daemon::spawn_exe(&root, daemon_exe, true);
    new_session(&path, "compat");
    let outcome = after_upgrade(&path, installed);
    eprintln!("upgrade: {outcome:?}");
    let understood = match &outcome {
        AfterUpgrade::Reloaded(report) => report.starts_with("reloaded: "),
        AfterUpgrade::Current(_) => true,
        AfterUpgrade::Kept(why) => {
            why.contains("predates `remuda reload`")
                || why.contains("did not start as a reload candidate")
                || why.contains(" speaks handoff version ")
                || why.contains("reload without --check is not implemented yet")
                || why.starts_with("blocked: ")
        }
        AfterUpgrade::Failed(_) => false,
        AfterUpgrade::NoDaemon => false,
    };
    assert!(understood, "{outcome:?}");
    assert_eq!(
        send_line(&path, "compat", "echo AFTER:$((7*8))"),
        Response::Ok
    );
    wait_for(&path, "compat", "AFTER:56");
    // Whichever daemon serves now, stop it: it may not be `daemon`'s child.
    let _ = client::request(
        &path,
        &Request::Shutdown {
            requester_daemon_id: None,
            requester_session_id: None,
            requester_session_name: None,
            override_hosted: false,
        },
    );
    drop(daemon);
}

#[test]
#[ignore = "N/N+1: needs REMUDA_COMPAT_PREVIOUS; run by CI job host-compat-n-plus-one"]
fn n_plus_one_upgrade_moves_an_older_daemon_onto_this_build() {
    upgrade_across_builds(&previous(), Path::new(env!("CARGO_BIN_EXE_remuda")));
}

#[test]
#[ignore = "N/N+1: needs REMUDA_COMPAT_PREVIOUS; run by CI job host-compat-n-plus-one"]
fn n_plus_one_upgrade_to_the_older_build_moves_this_daemon() {
    upgrade_across_builds(Path::new(env!("CARGO_BIN_EXE_remuda")), &previous());
}
