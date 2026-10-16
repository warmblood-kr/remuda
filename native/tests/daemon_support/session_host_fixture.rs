//! Native synthetic child; no daemon/config/session discovery.
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::time::{Duration, Instant};
#[cfg(unix)]
fn main() {
    if std::env::args().nth(1).as_deref() != Some("close-terminal-handles") {
        std::process::exit(2);
    }
    detach_terminal();
}
#[cfg(unix)]
fn detach_terminal() {
    print!("host-ready\r\n");
    std::io::stdout().flush().unwrap();
    // Remain alive after closing stdio. Native PTYs may keep their stream
    // open through a controlling-session reference; no EOF assumption is made.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    for fd in 0..3 {
        unsafe {
            libc::close(fd);
        }
    }
    for fd in 0..3 {
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1 {
            std::process::exit(3);
        }
    }
    std::fs::write(
        "terminal-handles-closed",
        b"stdio descriptors verified closed",
    )
    .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while !std::path::Path::new("finish-main").exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    std::process::exit(7);
}
#[cfg(windows)]
#[path = "session_host_windows_fixture.rs"]
mod windows_fixture;
#[cfg(windows)]
fn main() {
    windows_fixture::run();
}
#[cfg(not(any(unix, windows)))]
fn main() {
    std::process::exit(4);
}
