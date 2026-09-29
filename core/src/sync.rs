//! Versioned screen synchronization, independent of the daemon transport.

use crate::agent::{AgentError, VersionedSnapshot};
use crate::session::Session;
use std::time::Duration;

/// Maximum time a Sync request can hold a daemon worker.
pub const MAX_TIMEOUT_MS: u64 = 20_000;

/// Clamp an untrusted wire timeout to the server-side Sync limit.
pub fn bounded_timeout_ms(timeout_ms: u64) -> u64 {
    timeout_ms.min(MAX_TIMEOUT_MS)
}

/// Wait for a screen generation newer than `since`; a timeout returns the
/// current frame even when its output version is unchanged.
pub fn wait(
    session: &Session,
    since: u64,
    timeout_ms: u64,
) -> Result<VersionedSnapshot, AgentError> {
    session.wait_for_output_after(since, Duration::from_millis(bounded_timeout_ms(timeout_ms)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_timeout_is_capped_at_20s() {
        assert_eq!(bounded_timeout_ms(0), 0);
        assert_eq!(bounded_timeout_ms(150), 150);
        assert_eq!(bounded_timeout_ms(u64::MAX), 20_000);
    }
}
