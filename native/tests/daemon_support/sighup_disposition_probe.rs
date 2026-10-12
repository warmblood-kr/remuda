//! Test child for `daemon_signals.rs`: holds a lock on FILE.alive while it runs,
//! writes its SIGHUP disposition to FILE (0 = SIG_DFL, 1 = SIG_IGN, 2 = caught),
//! then waits while DAEMON_PID is still its parent (at most 30s).

#[cfg(unix)]
fn main() {
    let usage = "usage: sighup_disposition_probe FILE DAEMON_PID";
    let path = std::env::args().nth(1).expect(usage);
    let daemon: libc::pid_t = std::env::args().nth(2).expect(usage).parse().expect(usage);
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut old) };
    assert_eq!(rc, 0, "sigaction: {}", std::io::Error::last_os_error());
    let disposition = match old.sa_sigaction {
        libc::SIG_DFL => "0",
        libc::SIG_IGN => "1",
        _ => "2",
    };
    // The kernel drops this lock at exit, even while the probe is an unreaped zombie.
    let alive = std::fs::File::create(format!("{path}.alive")).expect("create FILE.alive");
    let rc = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&alive), libc::LOCK_EX) };
    assert_eq!(rc, 0, "flock: {}", std::io::Error::last_os_error());
    std::fs::write(&path, disposition).expect("write disposition");
    // Stay alive while the daemon does, but no longer: an orphaned probe would
    // write its coverage profile after the suite ends (#624).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while unsafe { libc::getppid() } == daemon && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(not(unix))]
fn main() {}
