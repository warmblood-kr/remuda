//! Host layer: everything that touches the operating system.
//!
//! The pty backend, process spawning, and terminal emulation land here, behind
//! [`remuda_core::AgentProcess`], so none of them can disturb what the pure
//! crate already proves. Cargo enforces the direction: `remuda-core` does not
//! depend on this crate and cannot be made to.

pub mod client;
pub mod daemon;
pub mod pty;

pub use portable_pty::CommandBuilder;
pub use pty::PtyAgent;

use remuda_core::Clock;
use std::time::{Duration, Instant};

/// The real clock.
///
/// It lives here and not in `remuda-core` because `Instant` is a host facility.
/// A policy layer has no business reading a clock directly anyway — it receives
/// one, which is what makes idle-timeout behaviour testable without sleeping.
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
