//! Host layer: everything that touches the operating system.
//!
//! The pty backend, process spawning, and terminal emulation land here, behind
//! [`remuda_core::AgentProcess`], so none of them can disturb what the pure
//! crate already proves. Cargo enforces the direction: `remuda-core` does not
//! depend on this crate and cannot be made to.

pub mod client;
pub mod daemon;
pub mod dist;
pub mod image;
pub mod ipc;
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
    // This was a hand-written TIOCGWINSZ ioctl, and the only `unsafe` in the
    // crate. crossterm asks the console host on Windows and the same ioctl on
    // unix, so the one call covers both and the block goes away.
    match crossterm::terminal::size() {
        Ok((cols, rows)) if cols > 0 => Size::new(cols, rows),
        _ => Size::default(),
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
