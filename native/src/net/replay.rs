//! Timestamp window and bounded ephemeral cache for one-shot frames.

use std::collections::HashMap;

const REPLAY_WINDOW_SECONDS: i64 = 60;

/// Why a frame did not pass the replay check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayError {
    OutsideWindow,
    AlreadySeen,
    Capacity,
}

/// Bounded cache of Noise ephemerals accepted inside the timestamp window.
pub struct ReplayWindow {
    capacity: usize,
    seen: HashMap<[u8; 32], i64>,
}

impl ReplayWindow {
    /// Create a replay cache with a fixed maximum number of live ephemerals.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            seen: HashMap::with_capacity(capacity),
        }
    }

    /// Accept a fresh frame timestamped within ±60 seconds of `now_seconds`.
    pub fn check_and_insert(
        &mut self,
        ephemeral: [u8; 32],
        timestamp_seconds: i64,
        now_seconds: i64,
    ) -> Result<(), ReplayError> {
        let distance = i128::from(timestamp_seconds) - i128::from(now_seconds);
        if distance.abs() > i128::from(REPLAY_WINDOW_SECONDS) {
            return Err(ReplayError::OutsideWindow);
        }
        self.evict_expired(now_seconds);
        if self.seen.contains_key(&ephemeral) {
            return Err(ReplayError::AlreadySeen);
        }
        if self.seen.len() >= self.capacity {
            return Err(ReplayError::Capacity);
        }
        self.seen.insert(ephemeral, timestamp_seconds);
        Ok(())
    }

    fn evict_expired(&mut self, now_seconds: i64) {
        let oldest_valid = i128::from(now_seconds) - i128::from(REPLAY_WINDOW_SECONDS);
        self.seen
            .retain(|_, timestamp| i128::from(*timestamp) >= oldest_valid);
    }
}
