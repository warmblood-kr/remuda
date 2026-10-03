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
            if can_signal_pid(*pid) && process_matches_runtime(*pid, &self.runtime) {
                let _ = Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status();
            }
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && pids
                .iter()
                .any(|pid| process_matches_runtime(*pid, &self.runtime))
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        for pid in pids {
            if can_signal_pid(pid) && process_matches_runtime(pid, &self.runtime) {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && daemon_pids_for_runtime(&self.runtime)
                .into_iter()
                .any(|pid| process_matches_runtime(pid, &self.runtime))
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
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let executable = fields.next()?;
            (Path::new(executable)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("remuda")))
            .then_some(pid)
        })
        .filter(|pid| process_matches_runtime(*pid, runtime))
        .collect()
}

#[cfg(unix)]
/// Confirm the PID still belongs to a remuda process with this exact runtime.
/// The `remuda` prefix also survives `ps` implementations that truncate `comm`.
fn process_matches_runtime(pid: u32, runtime: &Path) -> bool {
    let pid_arg = pid.to_string();
    let comm = Command::new("ps")
        .args(["-p", &pid_arg, "-o", "comm="])
        .output();
    let Ok(comm) = comm else {
        return false;
    };
    if !comm.status.success()
        || !String::from_utf8_lossy(&comm.stdout)
            .lines()
            .next()
            .and_then(|name| Path::new(name.trim()).file_name())
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("remuda"))
    {
        return false;
    }

    let command = Command::new("ps")
        .args(["eww", "-p", &pid_arg, "-o", "command="])
        .output();
    let Ok(command) = command else {
        return false;
    };
    command.status.success()
        && runtime_env_token_matches(
            &String::from_utf8_lossy(&command.stdout),
            &format!("REMUDA_RUNTIME_DIR={}", runtime.display()),
        )
}

#[cfg(not(unix))]
fn process_matches_runtime(_pid: u32, _runtime: &Path) -> bool {
    false
}

fn can_signal_pid(pid: u32) -> bool {
    pid > 1 && pid != std::process::id()
}

#[cfg(unix)]
fn runtime_env_token_matches(command: &str, expected_token: &str) -> bool {
    command
        .split_whitespace()
        .any(|token| token == expected_token)
}

/// Windows has no `ps`-based PID discovery here; cleanup uses the graceful CLI stop only.
#[cfg(not(unix))]
fn daemon_pids_for_runtime(_runtime: &Path) -> Vec<u32> {
    Vec::new()
}

#[cfg(all(test, unix))]
mod tests {
    use super::runtime_env_token_matches;

    #[test]
    fn runtime_match_requires_an_exact_environment_token() {
        assert!(runtime_env_token_matches(
            "remuda daemon REMUDA_RUNTIME_DIR=/tmp/rcx-123 -s s",
            "REMUDA_RUNTIME_DIR=/tmp/rcx-123"
        ));
        assert!(!runtime_env_token_matches(
            "remuda daemon REMUDA_RUNTIME_DIR=/tmp/rcx-1234 -s s",
            "REMUDA_RUNTIME_DIR=/tmp/rcx-123"
        ));
        assert!(!runtime_env_token_matches(
            "remuda daemon X_REMUDA_RUNTIME_DIR=/tmp/rcx-123 -s s",
            "REMUDA_RUNTIME_DIR=/tmp/rcx-123"
        ));
        assert!(!runtime_env_token_matches(
            "remuda daemon REMUDA_RUNTIME_DIR=/tmp/rcx-123/sub -s s",
            "REMUDA_RUNTIME_DIR=/tmp/rcx-123"
        ));
    }
}
