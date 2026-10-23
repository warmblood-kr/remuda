//! One scenario, two backends (#713): every parity test runs against a daemon
//! whose sessions are in-process and one whose sessions are host-backed, so a
//! difference between the two fails a test instead of a user.
#![allow(dead_code)]

use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::{client, daemon};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const PATIENCE: Duration = Duration::from_secs(15);
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    InProcess,
    Hosted,
}
pub const MODES: [Mode; 2] = [Mode::InProcess, Mode::Hosted];

/// A daemon of one mode in a short private root (host socket paths must fit
/// `sun_path`). Dropping it kills the daemon and every host under the root.
pub struct Parity {
    pub mode: Mode,
    root: PathBuf,
    env: Vec<(String, String)>,
    daemon: Child,
}

impl Parity {
    pub fn start(mode: Mode) -> Self {
        Self::start_with_env(mode, &[])
    }

    /// Start a daemon that also carries `env` in its own environment.
    pub fn start_with_env(mode: Mode, env: &[(String, String)]) -> Self {
        let base = if cfg!(target_os = "macos") {
            "/private/tmp"
        } else {
            "/tmp"
        };
        let root = PathBuf::from(base).join(format!(
            "rhp-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // Private regardless of umask: host directories refuse shared ancestors.
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let daemon = spawn_daemon(mode, &root, env);
        let parity = Self {
            mode,
            root,
            env: env.to_vec(),
            daemon,
        };
        parity.wait_bound();
        parity
    }

    /// Start a fresh daemon in the same root, after this one has stopped.
    pub fn restart(&mut self) {
        let _ = self.daemon.wait();
        self.daemon = spawn_daemon(self.mode, &self.root, &self.env);
        self.wait_bound();
    }

    /// SIGKILL this daemon, as a crash would; its hosts keep running.
    pub fn kill_daemon(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }

    fn wait_bound(&self) {
        let deadline = Instant::now() + PATIENCE;
        while remuda_native::ipc::connect(&self.socket()).is_err() {
            assert!(
                Instant::now() < deadline,
                "{:?}: daemon never bound",
                self.mode
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Run the `remuda` CLI against this daemon.
    pub fn cli(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s"])
            .args(args)
            .env("REMUDA_RUNTIME_DIR", &self.root)
            .env("HOME", &self.root)
            .env("XDG_STATE_HOME", self.root.join("state"))
            .stdin(Stdio::null())
            .output()
            .expect("run remuda")
    }

    /// Session hosts still running under this root.
    pub fn hosts_running(&self) -> usize {
        processes_naming(&self.root).len()
    }

    pub fn socket(&self) -> PathBuf {
        daemon::socket_path_in(&self.root, "s")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn request(&self, request: &Request) -> Response {
        client::request(&self.socket(), request).expect("daemon request")
    }

    /// Start `/bin/sh` and return its instance id.
    pub fn shell(&self, name: &str) -> String {
        self.start_command(name, vec!["/bin/sh".into()])
    }

    pub fn start_command(&self, name: &str, command: Vec<String>) -> String {
        match self.request(&Request::NewWithInstance {
            name: Some(name.into()),
            command,
            size: Size::new(80, 24),
            cwd: Some("/".into()),
            env: None,
        }) {
            Response::SessionStarted { instance_id, .. } => instance_id,
            other => panic!("{:?}: new {name}: {other:?}", self.mode),
        }
    }

    /// Run Lua in the daemon's image; `Err` carries the error text.
    pub fn eval(&self, code: &str) -> Result<String, String> {
        match self.request(&Request::Eval {
            code: code.into(),
            name: None,
        }) {
            Response::Value(value) => Ok(value),
            Response::Error(error) => Err(error),
            other => panic!("{:?}: eval: {other:?}", self.mode),
        }
    }

    pub fn screen(&self, name: &str) -> String {
        match self.request(&Request::Capture { name: name.into() }) {
            Response::Screen(screen) => screen,
            other => panic!("{:?}: capture {name}: {other:?}", self.mode),
        }
    }

    pub fn wait_for(&self, name: &str, needle: &str) -> String {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let screen = self.screen(name);
            if screen.contains(needle) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "{:?}: {needle:?} never appeared:\n{screen}",
                self.mode
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Parity {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        // Only processes whose argv names this unique root: our hosts.
        for pid in processes_naming(&self.root) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn spawn_daemon(mode: Mode, root: &Path, env: &[(String, String)]) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
    command
        .args(["-s", "s", "daemon"])
        .env("REMUDA_RUNTIME_DIR", root)
        .env("HOME", root)
        .env("XDG_STATE_HOME", root.join("state"))
        // Named either way, so a change of default never changes the mode.
        .env("REMUDA_EXPERIMENTAL_SESSION_HOST", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.envs(env.iter().map(|(key, value)| (key, value)));
    if mode == Mode::Hosted {
        command.env("REMUDA_EXPERIMENTAL_SESSION_HOST", "1");
    }
    command.spawn().expect("spawn daemon")
}

fn processes_naming(root: &Path) -> Vec<i32> {
    let Ok(output) = Command::new("/bin/ps")
        .args(["-A", "-o", "pid=", "-o", "args="])
        .output()
    else {
        return Vec::new();
    };
    // With the separator: root `rhp-1-1` must not match `rhp-1-12`'s hosts,
    // which belong to another test still running.
    let root = format!("{}/", root.to_string_lossy());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(&root) && line.contains("run-reserved"))
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}
