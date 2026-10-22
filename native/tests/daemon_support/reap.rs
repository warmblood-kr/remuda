//! Cleaning up a test's processes. Tests share their machine with a user's
//! live `remuda` daemon and with other test runs, so cleanup never kills by
//! pattern: no `pkill`, `killall` or `pgrep -f`. A pattern that looks
//! narrow still matches what it was not meant to; `root-1` also matches
//! `root-10`. Instead, list processes, keep the pids whose command line
//! names this test's own unique root followed by `/`, and kill those pids.
//! A daemon that took over in a reload runs as `remuda -s NAME daemon`, which
//! names no root; the pid in the root's socket lock names it instead.

// Each including binary uses a different subset.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

/// Pids whose command line names a path under `root`: its session hosts,
/// and a daemon that took the socket over in a reload.
pub fn processes_under(root: &Path) -> Vec<i32> {
    processes_naming(&[&format!("{}/", root.to_string_lossy())])
}

/// Pids whose command line contains every one of `parts`, read from `ps`.
pub fn processes_naming(parts: &[&str]) -> Vec<i32> {
    let output = match Command::new("/bin/ps")
        .args(["-A", "-o", "pid=", "-o", "args="])
        .output()
    {
        Ok(output) => output,
        Err(_) => return Vec::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| parts.iter().all(|part| line.contains(part)))
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .filter(|&pid: &i32| pid != std::process::id() as i32)
        .collect()
}

/// Pids named in `root`'s socket lock files whose command line is still a
/// remuda daemon. The check keeps a reused pid from being killed.
pub fn daemons_locked_under(root: &Path) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir(root.join("remuda")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".sock.lock"))
        .filter_map(|entry| {
            std::fs::read_to_string(entry.path())
                .ok()?
                .trim()
                .parse()
                .ok()
        })
        .filter(|&pid: &i32| {
            let Ok(output) = Command::new("/bin/ps")
                .args(["-o", "args=", "-p", &pid.to_string()])
                .output()
            else {
                return false;
            };
            let args = String::from_utf8_lossy(&output.stdout);
            let args = args.trim_end();
            args.contains("remuda") && args.ends_with(" daemon")
        })
        .collect()
}

/// SIGKILL every process under `root`; see [`processes_under`] and
/// [`daemons_locked_under`].
pub fn kill_processes_under(root: &Path) {
    let mut pids = processes_under(root);
    pids.extend(daemons_locked_under(root));
    for pid in pids {
        // SAFETY: kill only sends a signal to a pid we just listed.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}
