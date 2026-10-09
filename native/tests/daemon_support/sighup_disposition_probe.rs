//! Test child for `daemon_signals.rs`: writes its own SIGHUP disposition to the
//! file named by its argument (0 = SIG_DFL, 1 = SIG_IGN, 2 = caught) and its pid
//! to FILE.pid, then waits until its parent exits.

#[cfg(unix)]
fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: sighup_disposition_probe FILE");
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut old) };
    assert_eq!(rc, 0, "sigaction: {}", std::io::Error::last_os_error());
    let disposition = match old.sa_sigaction {
        libc::SIG_DFL => "0",
        libc::SIG_IGN => "1",
        _ => "2",
    };
    let pid_path = std::path::Path::new(&path).with_extension("pid");
    std::fs::write(pid_path, std::process::id().to_string()).expect("write pid");
    std::fs::write(&path, disposition).expect("write disposition");
    // Stay alive while the daemon does, but no longer: an orphaned probe would
    // write its coverage profile after the suite ends (#624).
    let parent = unsafe { libc::getppid() };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while unsafe { libc::getppid() } == parent && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(not(unix))]
fn main() {}
