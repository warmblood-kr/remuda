//! Stopping the session hosts a test started (#713). A host outlives its
//! daemon by design, so a test that kills its daemon and removes its scratch
//! directory still leaves each host, and its PTY, running. With hosting forced
//! on for the whole suite that is hundreds of leaked PTYs per run.
//!
//! Every fixture that gives a daemon a scratch directory holds a [`HostSweep`]
//! for it; its Drop stops the hosts started under that directory. Hosts are
//! found by their argv (the host binary, `run-reserved`, and a private
//! directory under the scratch directory) and stopped by exact pid, never by
//! pattern. The directory is matched with its trailing separator, so `x-1`
//! never matches `x-12`.
//!
//! Included with `#[path]` from integration tests and from the lib's own
//! test modules.

// Each including binary uses a different subset.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Stops, on drop, every session host started under `dir`.
pub struct HostSweep(pub PathBuf);

impl HostSweep {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self(dir.into())
    }
}

impl Drop for HostSweep {
    fn drop(&mut self) {
        stop_hosts_under(&self.0);
    }
}

/// The pids of session hosts whose private directory is under `dir`.
pub fn hosts_under(dir: &Path) -> Vec<i32> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "args="])
        .output()
    else {
        return Vec::new();
    };
    // Both spellings: macOS reports /tmp paths as /private/tmp in some places.
    let roots: Vec<String> = [dir.to_path_buf(), dir.canonicalize().unwrap_or_default()]
        .iter()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| format!("{}/", path.to_string_lossy()))
        .collect();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse().ok()?;
            let program = words.next()?;
            let verb = words.next()?;
            let private = words.next()?;
            (program.ends_with("remuda-session-host")
                && verb == "run-reserved"
                && roots.iter().any(|root| private.starts_with(root.as_str())))
            .then_some(pid)
        })
        .collect()
}

/// Stops the session hosts under `dir`: TERM, then KILL for any that are
/// still the same hosts after a grace period.
#[cfg(unix)]
pub fn stop_hosts_under(dir: &Path) {
    let pids = hosts_under(dir);
    if pids.is_empty() {
        return;
    }
    for pid in &pids {
        unsafe { libc::kill(*pid, libc::SIGTERM) };
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        // Re-verified each time: a pid that ended may have been reused.
        let left = hosts_under(dir);
        if left.is_empty() {
            return;
        }
        if std::time::Instant::now() >= deadline {
            for pid in left {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(not(unix))]
pub fn stop_hosts_under(_dir: &Path) {}
