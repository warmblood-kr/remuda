//! Spawning the shipped `remuda` binary as a daemon process, shared by the
//! `daemon` and `home_isolation` test binaries so the isolation test guards
//! the very helpers the daemon tests use.

// Each including binary uses a different subset.
#![allow(dead_code)]

use remuda_native::daemon;
use std::path::Path;
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(10);

/// A daemon as its own PROCESS, with its streams pointed at nothing. Inheriting
/// the harness's stdout would let a leaked daemon hold cargo's pipe open, which
/// turns any failure below into a hung job instead of a red one.
pub struct Daemon(pub std::process::Child);

/// The one place every spawn helper below route through to build their
/// `Command`. Defaults `HOME` to a path under the test's own scratch `dir`
/// (created if needed) and removes `XDG_CONFIG_HOME` -- mirroring what only
/// `spawn_with_home` used to do, but now as the default every helper gets
/// for free, rather than something each has to opt into. `load_user_config`
/// (daemon.rs:126) reads and `eval`s `$XDG_CONFIG_HOME`/`$HOME/.config` +
/// `remuda/init.lua` on every daemon boot; without this, a helper that
/// forgot to redirect `HOME` would spawn a daemon that reads and executes
/// the REAL test-runner machine's own `~/.config/remuda/init.lua`, if one
/// exists. A future helper inherits this safety by construction --
/// it would have to deliberately override `HOME` to lose it.
pub fn base_command(dir: &Path) -> std::process::Command {
    let home = dir.join("home");
    let _ = std::fs::create_dir_all(&home);
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_remuda"));
    cmd.args(["-s", "s", "daemon"])
        .env("REMUDA_RUNTIME_DIR", dir)
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd
}

/// Spawns `cmd` and blocks until the daemon it starts actually answers on
/// its socket under `dir` -- the connect-poll every helper below used to
/// duplicate.
pub fn spawn_and_wait(mut cmd: std::process::Command, dir: &Path) -> Daemon {
    let child = cmd.spawn().expect("spawn daemon");
    let path = daemon::socket_path_in(dir, "s");
    let deadline = Instant::now() + PATIENCE;
    while remuda_native::ipc::connect(&path).is_err() {
        assert!(Instant::now() < deadline, "daemon never bound {path:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    Daemon(child)
}

impl Daemon {
    pub fn spawn(dir: &Path) -> Self {
        spawn_and_wait(base_command(dir), dir)
    }

    /// Like `spawn`, but for the birth-environment-poisoning
    /// scenario itself: a daemon born with `HOME` pinned to a caller-chosen
    /// directory (overriding `base_command`'s default) and NEITHER
    /// `REMUDA_BUTLER_TOKEN`/`REMUDA_BUTLER_CONFIG` nor `XDG_CONFIG_HOME`
    /// present at all (`env_remove`, not merely unset-by-omission --
    /// `cargo test`'s own process could otherwise leak either through,
    /// making the test non-deterministic on a machine where they happen to
    /// be set).
    pub fn spawn_with_home(dir: &Path, home: &Path) -> Self {
        let mut cmd = base_command(dir);
        cmd.env("HOME", home)
            .env_remove("REMUDA_BUTLER_TOKEN")
            .env_remove("REMUDA_BUTLER_CONFIG");
        spawn_and_wait(cmd, dir)
    }

    /// Bounded on purpose: an unbounded `wait` on a daemon that did not stop is
    /// the same hang this whole struct exists to avoid.
    pub fn left_on_its_own(&mut self) -> bool {
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.0.try_wait() {
                return status.success();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
