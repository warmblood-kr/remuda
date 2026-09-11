//! Time, as an injected dependency.
//!
//! Nothing in this crate calls a global clock. That is not a style preference:
//! interleaving and timeout bugs are only testable when a test can advance time
//! deliberately, and retrofitting injection after the fact means rewriting every
//! call site. So it is here in the first slice, before there are call sites.

use core::time::Duration;

/// A source of monotonic elapsed time. Deliberately NOT wall-clock: every
/// question here is "how long since X", never "what date is it".
pub trait Clock: Send + Sync {
    /// Time elapsed since this clock's own arbitrary origin. Only differences
    /// between two readings are meaningful, and never across two clocks.
    fn now(&self) -> Duration;

    /// Block until `duration` has passed on *this* clock — never
    /// `std::thread::sleep` directly, so a `ManualClock` can make a pause
    /// deterministic instead of a flaky real-time wait.
    fn sleep(&self, duration: Duration);
}

// The real clock lives in `remuda-native`, not here. It sat behind a `native`
// feature in the first cut — but a feature you can forget to disable is a
// weaker wall than a crate that cannot name `std::time::Instant` at all.
// `clippy.toml` beside this crate's manifest denies that path by name.

/// A clock that only moves when a caller moves it. `Mutex` + `Condvar`, not a
/// lock-free counter, so `sleep` can block on it rather than poll.
pub struct ManualClock {
    elapsed: std::sync::Mutex<u64>,
    advanced: std::sync::Condvar,
}

impl ManualClock {
    pub fn new() -> Self {
        Self {
            elapsed: std::sync::Mutex::new(0),
            advanced: std::sync::Condvar::new(),
        }
    }

    /// Move time forward. Time never moves backwards; there is no setter.
    /// Wakes every `sleep` whose deadline this reaches or passes.
    pub fn advance(&self, by: Duration) {
        let mut elapsed = self.elapsed.lock().unwrap_or_else(|p| p.into_inner());
        *elapsed += by.as_millis() as u64;
        self.advanced.notify_all();
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        Duration::from_millis(*self.elapsed.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Blocks for real until a test `advance`s this clock far enough — a test
    /// that forgets to drive time hangs, on purpose, rather than passing.
    fn sleep(&self, duration: Duration) {
        let target = self.now().as_millis() as u64 + duration.as_millis() as u64;
        let guard = self.elapsed.lock().unwrap_or_else(|p| p.into_inner());
        let _done = self
            .advanced
            .wait_while(guard, |elapsed| *elapsed < target)
            .unwrap_or_else(|p| p.into_inner());
    }
}
