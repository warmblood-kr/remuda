//! Spawn helpers never read the real developer's `~/.config/remuda/init.lua`.
//!
//! Its own test binary on purpose: the test pins the process-wide `HOME`, and
//! any sibling test that starts a daemon in-process would load the sentinel
//! config and write the marker, failing this test for the wrong reason.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[path = "daemon_support/spawn.rs"]
mod spawn;
use spawn::Daemon;

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remuda-h{}-{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Guards `HOME`/`XDG_CONFIG_HOME` mutation below -- `std::env::set_var` is
/// process-wide, and any daemon started in-process reads it. That is why this
/// test has a binary to itself; the lock covers a second test added here.
static REAL_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII: pins the parent test PROCESS's own `HOME` (and clears
/// `XDG_CONFIG_HOME`) to simulate "a real developer's env", restoring both on
/// drop -- including on panic -- so a failing assertion below can't leave the
/// process env corrupted for whatever test runs next in this binary.
struct RealHomeGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    old_home: Option<std::ffi::OsString>,
    old_xdg: Option<std::ffi::OsString>,
}

impl RealHomeGuard {
    fn set_to(home: &Path) -> Self {
        let lock = REAL_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("HOME", home);
        std::env::remove_var("XDG_CONFIG_HOME");
        Self {
            _lock: lock,
            old_home,
            old_xdg,
        }
    }
}

impl Drop for RealHomeGuard {
    fn drop(&mut self) {
        match &self.old_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match &self.old_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }
}

/// Polls for `marker`'s appearance -- fails fast the moment it shows up,
/// rather than waiting out the whole bound -- then, if the bound passes
/// clean, treats that as proof `load_user_config` (daemon.rs:126) never ran
/// against this sentinel. 1200ms matches
/// `a_fresh_daemon_with_no_user_config_never_auto_registers_butler`'s own
/// margin for this identical background thread.
fn assert_marker_never_appears(marker: &Path, helper: &str) {
    let deadline = Instant::now() + Duration::from_millis(1200);
    loop {
        assert!(
            !marker.exists(),
            "{helper} spawned a daemon that read and eval'd a config from \
             outside its own scratch dir (marker present at {marker:?})"
        );
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The defect this guards: `load_user_config` (daemon.rs:126) reads and
/// `eval`s whatever `$XDG_CONFIG_HOME`/`$HOME/.config` + `remuda/init.lua`
/// resolves to, against a live, IPC-connected `Image` -- on EVERY daemon
/// boot, unconditionally. A test helper that builds its `Command` without
/// pinning `HOME`/`XDG_CONFIG_HOME` spawns a daemon that inherits the *real*
/// test-runner process's env, and will find and eval the *actual developer
/// machine's* own `~/.config/remuda/init.lua`, if one exists.
///
/// `RealHomeGuard` simulates exactly that "real developer env" without ever
/// touching the actual real `$HOME`: it pins the parent test PROCESS's own
/// `HOME` to a fake developer home (with a sentinel `init.lua` planted in
/// it) for the test's duration. Each spawn helper is then
/// exercised in turn, each against its own fresh marker path, and none of
/// them may ever cause that marker to appear.
#[test]
#[cfg(unix)]
fn daemon_spawn_helpers_never_read_the_real_developer_home() {
    let fake_home = scratch_dir("home-isolation-fake-developer-home");
    let config_dir = fake_home.join(".config").join("remuda");
    let marker_dir = scratch_dir("home-isolation-markers");
    std::fs::create_dir_all(&marker_dir).expect("mkdir marker dir");

    let _env = RealHomeGuard::set_to(&fake_home);

    // spawn(): the plain helper -- inherits ambient HOME/XDG_CONFIG_HOME.
    let marker = marker_dir.join("spawn.marker");
    write_home_sentinel(&config_dir, &marker);
    {
        let _daemon = Daemon::spawn(&scratch_dir("home-isolation-spawn"));
        assert_marker_never_appears(&marker, "Daemon::spawn");
    }

    // spawn_with_home(): existing-good baseline -- an explicit `home` wins
    // over whatever ambient HOME the parent process has. The fake-home
    // sentinel is left primed (the same one `spawn` just used)
    // specifically so this proves the helper ignores it, not merely that
    // nothing was there to find.
    let marker = marker_dir.join("spawn_with_home.marker");
    write_home_sentinel(&config_dir, &marker);
    let clean_home = scratch_dir("home-isolation-clean-home");
    {
        let _daemon =
            Daemon::spawn_with_home(&scratch_dir("home-isolation-spawn-home"), &clean_home);
        assert_marker_never_appears(&marker, "Daemon::spawn_with_home");
    }
}

/// (Re)writes the sentinel `init.lua` under `config_dir` so that, if eval'd,
/// it writes a marker to `marker_path` -- baked directly into the Lua source
/// as an absolute path, so the marker's presence is unambiguous evidence the
/// sentinel ran, regardless of what `HOME` the reading process sees.
fn write_home_sentinel(config_dir: &Path, marker_path: &Path) {
    std::fs::create_dir_all(config_dir).expect("mkdir sentinel config dir");
    let lua = format!(
        "local f = io.open({:?}, 'w') f:write('ran') f:close()",
        marker_path.display().to_string()
    );
    std::fs::write(config_dir.join("init.lua"), lua).expect("write sentinel init.lua");
}
