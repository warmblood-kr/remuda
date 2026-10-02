//! Host layer: everything that touches the operating system.
//!
//! The pty backend, process spawning, and terminal emulation land here, behind
//! [`remuda_core::AgentProcess`], so none of them can disturb what the pure
//! crate already proves. Cargo enforces the direction: `remuda-core` does not
//! depend on this crate and cannot be made to.

pub mod child_guard;
pub mod cli_parse;
pub mod client;
pub mod cluster;
pub mod cluster_remote;
pub mod cluster_tui;
pub(crate) mod credential;
pub mod daemon;
pub mod dist;
pub mod find_command;
pub(crate) mod fs_atomic;
pub mod fs_lock;
pub mod hostname;
pub mod image;
pub mod ipc;
pub mod json;
pub mod mcp;
pub mod mouse;
#[allow(clippy::disallowed_types)]
pub mod net;
pub mod packages;
pub mod pending;
pub mod process;
mod process_ancestry;
pub mod pty;
pub mod remote_front;
pub mod reply_limit;
pub mod script;
pub(crate) mod storage;
pub mod text;
pub mod tick;
pub mod tui;

pub use portable_pty::CommandBuilder;
pub use pty::PtyAgent;

use remuda_core::{Clock, Size, WallClock};
use std::time::{Duration, Instant};

/// The host implementation of persistent Unix wall time.
pub struct SystemWallClock;

impl SystemWallClock {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SystemWallClock {
    fn default() -> Self {
        Self::new()
    }
}

impl WallClock for SystemWallClock {
    fn unix_seconds(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}

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
    instance_id_seed: u128,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            instance_id_seed: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                ^ ((std::process::id() as u128) << 64),
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

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn instance_id_seed(&self) -> u128 {
        self.instance_id_seed
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
