//! Time, as an injected dependency.
//!
//! Nothing in this crate calls a global clock. That is not a style preference:
//! interleaving and timeout bugs are only testable when a test can advance time
//! deliberately, and retrofitting injection after the fact means rewriting every
//! call site. So it is here in the first slice, before there are call sites.

use core::time::Duration;

/// A source of monotonic elapsed time.
///
/// Deliberately NOT wall-clock: every question this crate asks of time is
/// "how long since X", never "what date is it". Monotonic time cannot jump
/// backwards when the host adjusts its clock, and it is trivially fakeable.
pub trait Clock: Send + Sync {
    /// Time elapsed since this clock's own arbitrary origin.
    ///
    /// Only differences between two readings are meaningful. The origin
    /// itself carries no information and must not be compared across clocks.
    fn now(&self) -> Duration;
}

// The real clock lives in `remuda-native`, not here. It sat behind a `native`
// feature in the first cut — but a feature you can forget to disable is a
// weaker wall than a crate that cannot name `std::time::Instant` at all.
// `clippy.toml` beside this crate's manifest denies that path by name.

/// A clock that only moves when a test moves it.
///
/// Available in every build, not just tests: the whole point of the seam is
/// that a caller can drive time, and a WASM host has no `Instant` to fall
/// back on.
pub struct ManualClock {
    elapsed: core::sync::atomic::AtomicU64,
}

impl ManualClock {
    pub fn new() -> Self {
        Self {
            elapsed: core::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Move time forward. Time never moves backwards; there is no setter.
    pub fn advance(&self, by: Duration) {
        self.elapsed
            .fetch_add(by.as_millis() as u64, core::sync::atomic::Ordering::SeqCst);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.elapsed.load(core::sync::atomic::Ordering::SeqCst))
    }
}
