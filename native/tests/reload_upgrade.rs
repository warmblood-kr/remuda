//! What `remuda upgrade` does with the running daemon after installing
//! (#713). The "installed build" is a wrapper that names another version and
//! runs this one, so a real reload happens without a second build.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::reload::upgrade::{after_upgrade, AfterUpgrade};
use remuda_native::{client, daemon};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(15);
const THIS: &str = env!("CARGO_BIN_EXE_remuda");

/// A short private root: host socket paths must fit `sun_path`.
struct Root(PathBuf);
impl Root {
    fn new(tag: &str) -> Self {
        let base = if cfg!(target_os = "macos") {
            "/private/tmp"
        } else {
            "/tmp"
        };
        let path = PathBuf::from(base).join(format!("rru-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
    fn socket(&self) -> PathBuf {
        daemon::socket_path_in(&self.0, "s")
    }
    /// A daemon of this build; `hosted` opts new sessions into hosts.
    fn daemon(&self, hosted: bool) -> Reaped {
        self.daemon_with(hosted, None)
    }
    /// `fault` is a `REMUDA_TEST_RELOAD_FAULT` its candidates act on.
    fn daemon_with(&self, hosted: bool, fault: Option<&str>) -> Reaped {
        let mut command = Command::new(THIS);
        command
            .args(["-s", "s", "daemon"])
            .env("REMUDA_RUNTIME_DIR", &self.0)
            .env("HOME", &self.0)
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("REMUDA_NO_UPDATE_CHECK", "1")
            .env_remove("REMUDA_EXPERIMENTAL_SESSION_HOST")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if hosted {
            command.env("REMUDA_EXPERIMENTAL_SESSION_HOST", "1");
        }
        if let Some(fault) = fault {
            command.env("REMUDA_TEST_RELOAD_FAULT", fault);
        }
        let child = Reaped(command.spawn().unwrap());
        let deadline = Instant::now() + PATIENCE;
        while remuda_native::ipc::connect(&self.socket()).is_err() {
            assert!(Instant::now() < deadline, "daemon never bound");
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    }
    /// An "installed build": names `version`, then runs `body`.
    fn installed(&self, version: &str, body: &str) -> PathBuf {
        let path = self.0.join("installed");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'remuda {version}'; exit 0; fi\n{body}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        // By checked pid, never by pattern. Hosts name the root in argv; a
        // daemon that took over may not, so the lock names it instead.
        let ps = Command::new("/bin/ps")
            .args(["-A", "-o", "pid=", "-o", "args="])
            .output()
            .unwrap();
        let needle = format!("{}/", self.0.display());
        let mut pids: Vec<i32> = String::from_utf8_lossy(&ps.stdout)
            .lines()
            .filter(|line| line.contains(&needle))
            .filter_map(|line| line.split_whitespace().next()?.parse().ok())
            .collect();
        if let Some(pid) = std::fs::read_to_string(format!("{}.lock", self.socket().display()))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
        {
            let args = Command::new("/bin/ps")
                .args(["-o", "args=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            let args = String::from_utf8_lossy(&args.stdout);
            if args.contains("remuda") && args.trim_end().ends_with(" daemon") {
                pids.push(pid);
            }
        }
        for pid in pids {
            // SAFETY: a signal to a pid just checked as under this root.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Reaped(Child);
impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn request(path: &Path, request: &Request) -> Response {
    client::request(path, request).expect("daemon request")
}
fn new_session(path: &Path, name: &str) {
    let made = request(
        path,
        &Request::New {
            name: Some(name.into()),
            command: vec!["/bin/sh".into()],
            size: Size::new(80, 24),
            cwd: Some("/".into()),
            // No history file lands in the root after it is removed.
            env: Some([("HISTFILE".into(), "/dev/null".into())].into()),
        },
    );
    assert_eq!(made, Response::Value(name.into()));
}
fn session_answers(path: &Path, name: &str, marker: &str) {
    let line = format!("echo {marker}-$((6*7))");
    assert_eq!(
        request(
            path,
            &Request::SendLine {
                name: name.into(),
                text: line
            }
        ),
        Response::Ok
    );
    let needle = format!("{marker}-42");
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Response::Screen(screen) = request(path, &Request::Capture { name: name.into() }) {
            if screen.contains(&needle) {
                return;
            }
        }
        assert!(Instant::now() < deadline, "{needle} never appeared");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn lock_pid(root: &Root) -> String {
    let mut lock = root.socket().into_os_string();
    lock.push(".lock");
    std::fs::read_to_string(lock).unwrap().trim().to_string()
}

#[test]
fn with_no_daemon_there_is_nothing_to_move() {
    let root = Root::new("none");
    let installed = root.installed("9.9.9", &format!("exec '{THIS}' \"$@\""));
    assert_eq!(
        after_upgrade(&root.socket(), &installed),
        AfterUpgrade::NoDaemon
    );
}

#[test]
fn a_daemon_already_on_the_installed_build_is_left_alone() {
    let root = Root::new("same");
    let _daemon = root.daemon(true);
    let before = lock_pid(&root);
    let build = remuda_native::dist::BUILD_VERSION;
    let installed = root.installed(build, &format!("exec '{THIS}' \"$@\""));
    assert_eq!(
        after_upgrade(&root.socket(), &installed),
        AfterUpgrade::Current(build.into())
    );
    assert_eq!(lock_pid(&root), before);
}

#[test]
fn a_daemon_on_another_build_is_reloaded_and_its_hosted_session_survives() {
    let root = Root::new("move");
    let path = root.socket();
    let _daemon = root.daemon(true);
    new_session(&path, "work");
    session_answers(&path, "work", "BEFORE");
    let before = lock_pid(&root);
    let installed = root.installed("9.9.9", &format!("exec '{THIS}' \"$@\""));

    let outcome = after_upgrade(&path, &installed);
    let AfterUpgrade::Reloaded(report) = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(report.starts_with("reloaded: "), "{report}");
    assert!(report.contains("\ndaemon now "), "{report}");
    assert_ne!(lock_pid(&root), before, "another daemon serves now");
    session_answers(&path, "work", "AFTER");
}

#[test]
fn an_in_process_session_keeps_the_daemon_on_its_build() {
    let root = Root::new("kept");
    let path = root.socket();
    let _daemon = root.daemon(false);
    new_session(&path, "plain");
    let before = lock_pid(&root);
    let installed = root.installed("9.9.9", &format!("exec '{THIS}' \"$@\""));

    let outcome = after_upgrade(&path, &installed);
    let AfterUpgrade::Kept(why) = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(why.starts_with("blocked: "), "{why}");
    assert!(
        why.contains("in-process sessions would end: plain"),
        "{why}"
    );
    assert_eq!(lock_pid(&root), before);
    session_answers(&path, "plain", "STILL");
}

#[test]
fn an_installed_build_that_cannot_be_a_candidate_keeps_the_daemon() {
    let root = Root::new("old");
    let path = root.socket();
    let _daemon = root.daemon(true);
    new_session(&path, "work");
    let before = lock_pid(&root);
    // What a build older than reload does with the candidate verb.
    let installed = root.installed("0.0.1", "exit 1");

    let outcome = after_upgrade(&path, &installed);
    let AfterUpgrade::Kept(why) = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(why.contains("did not start as a reload candidate"), "{why}");
    assert_eq!(lock_pid(&root), before);
    session_answers(&path, "work", "STILL");
}

#[test]
fn a_reload_that_fails_is_rolled_back_and_reported_as_failed() {
    let root = Root::new("fail");
    let path = root.socket();
    let _daemon = root.daemon_with(true, Some("exit-before-serving"));
    new_session(&path, "work");
    let before = lock_pid(&root);
    let installed = root.installed("9.9.9", &format!("exec '{THIS}' \"$@\""));

    let outcome = after_upgrade(&path, &installed);
    let AfterUpgrade::Failed(error) = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(error.contains("rolled back"), "{error}");
    assert_eq!(lock_pid(&root), before);
    session_answers(&path, "work", "STILL");
}

/// The CLI end to end, with an installer that writes nothing: the daemon
/// already runs this build, so `upgrade` leaves it and says so.
#[test]
fn the_upgrade_command_checks_the_selected_daemon_after_installing() {
    let root = Root::new("cli");
    let _daemon = root.daemon(true);
    let before = lock_pid(&root);
    let bin = root.0.join("bin");
    std::fs::create_dir(&bin).unwrap();
    // `curl -o FILE URL` lands an installer that succeeds and does nothing.
    std::fs::write(
        bin.join("curl"),
        "#!/bin/sh\nwhile [ $# -gt 0 ]; do if [ \"$1\" = -o ]; then printf '#!/bin/sh\\nexit 0\\n' > \"$2\"; exit 0; fi; shift; done; exit 2\n",
    )
    .unwrap();
    std::fs::set_permissions(bin.join("curl"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new(THIS)
        .args(["-s", "s", "upgrade", "--channel", "nightly"])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("REMUDA_RUNTIME_DIR", &root.0)
        .env("HOME", &root.0)
        .env("XDG_DATA_HOME", root.0.join("data"))
        .env("XDG_STATE_HOME", root.0.join("state"))
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let build = remuda_native::dist::BUILD_VERSION;
    assert!(
        stderr.contains(&format!("The daemon already runs {build}")),
        "{stderr}"
    );
    assert_eq!(lock_pid(&root), before);
}
