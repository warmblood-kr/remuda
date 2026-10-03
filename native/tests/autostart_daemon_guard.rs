//! Cleanup for daemons autostarted by a fixture's private runtime directory.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub struct AutostartDaemonGuard {
    runtime: PathBuf,
}

impl AutostartDaemonGuard {
    pub fn new(runtime: impl Into<PathBuf>) -> Self {
        let runtime = runtime.into();
        Self { runtime }
    }
}

impl Drop for AutostartDaemonGuard {
    fn drop(&mut self) {
        let pids = daemon_pids_for_runtime(&self.runtime);

        let _ = Command::new(env!("CARGO_BIN_EXE_remuda"))
            .args(["-s", "s", "stop", "-f", "--yes"])
            .env("REMUDA_RUNTIME_DIR", &self.runtime)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("XDG_DATA_HOME", self.runtime.join("data"))
            .env("XDG_CACHE_HOME", self.runtime.join("cache"))
            .env("HOME", &self.runtime)
            .output();

        for pid in &pids {
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && pids.iter().any(|pid| process_exists(*pid)) {
            std::thread::sleep(Duration::from_millis(20));
        }
        for pid in pids {
            if process_exists(pid) {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && daemon_pids_for_runtime(&self.runtime)
                .into_iter()
                .any(process_exists)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

/// Follow the cross-platform macOS/Linux `ps` scan used by cli_no_daemon.rs.
#[cfg(unix)]
fn daemon_pids_for_runtime(runtime: &Path) -> Vec<u32> {
    let output = Command::new("ps").args(["-axo", "pid=,comm="]).output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let runtime_marker = format!("REMUDA_RUNTIME_DIR={}", runtime.display());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let executable = fields.next()?;
            (Path::new(executable)
                .file_name()
                .and_then(|name| name.to_str())
                == Some("remuda"))
            .then_some(pid)
        })
        .filter(|pid| {
            let pid = pid.to_string();
            let output = Command::new("ps")
                .args(["eww", "-p", &pid, "-o", "command="])
                .output();
            output.is_ok_and(|output| {
                String::from_utf8_lossy(&output.stdout).contains(&runtime_marker)
            })
        })
        .collect()
}

#[cfg(not(unix))]
fn daemon_pids_for_runtime(_runtime: &Path) -> Vec<u32> {
    Vec::new()
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    false
}
