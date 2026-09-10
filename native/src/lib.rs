//! Host layer: everything that touches the operating system.
//!
//! The pty backend, process spawning, and terminal emulation land here, behind
//! [`remuda_core::AgentProcess`], so none of them can disturb what the pure
//! crate already proves. Cargo enforces the direction: `remuda-core` does not
//! depend on this crate and cannot be made to.

pub mod client;
pub mod daemon;
pub mod image;
pub mod mcp;
pub mod pty;
pub mod script;

pub use portable_pty::CommandBuilder;
pub use pty::PtyAgent;

use remuda_core::{Clock, Size};
use std::time::{Duration, Instant};

/// This terminal's size, or the floor if it cannot be determined (no tty, a
/// pipe, a cron job). `Size::new` clamps anyway, so the worst case is a session
/// smaller than its window, never one that drops keystrokes.
pub fn terminal_size() -> Size {
    // TIOCGWINSZ has no safe wrapper in `nix`, and the alternative — shelling
    // out to `stty` — would put a subprocess on the startup path of every
    // command. Four lines of well-trodden ioctl instead.
    use std::os::fd::AsRawFd;
    let mut ws: nix::libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        nix::libc::ioctl(
            std::io::stdout().as_raw_fd(),
            nix::libc::TIOCGWINSZ,
            &mut ws,
        )
    };
    if rc == 0 && ws.ws_col > 0 {
        Size::new(ws.ws_col, ws.ws_row)
    } else {
        Size::default()
    }
}

/// The real clock. It lives here, not in `remuda-core`, because `Instant` is a
/// host facility and the policy layer receives a clock rather than reading one.
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_clock_moves_forward_on_its_own() {
        let clock = SystemClock::new();
        let first = clock.now();
        // Busy-wait rather than sleep: this asserts monotonicity, not duration.
        while clock.now() == first {}
        assert!(clock.now() > first);
    }
}
