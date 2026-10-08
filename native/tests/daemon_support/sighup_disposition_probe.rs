//! Test child for `daemon_signals.rs`: writes its own SIGHUP disposition to the
//! file named by its argument (0 = SIG_DFL, 1 = SIG_IGN, 2 = caught), then waits.

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
    std::fs::write(&path, disposition).expect("write disposition");
    std::thread::sleep(std::time::Duration::from_secs(30));
}

#[cfg(not(unix))]
fn main() {}
