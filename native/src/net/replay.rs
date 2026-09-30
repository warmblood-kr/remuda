//! Timestamp window and bounded ephemeral cache for one-shot frames.

use std::collections::HashMap;
use std::time::{Duration, Instant};

const REPLAY_WINDOW_SECONDS: i64 = 60;
const MAX_REPLAY_RETENTION: Duration = Duration::from_secs(120);
const MAX_PEER_CACHE_ENTRIES: usize = 4096;

/// Why a frame did not pass the replay check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayError {
    OutsideWindow,
    AlreadySeen,
    Capacity,
    PeerCapacity,
}

struct SeenFrame {
    peer: String,
    inserted_at: Instant,
    timestamp_seconds: i64,
}

/// Bounded cache of Noise ephemerals accepted inside the timestamp window.
pub struct ReplayWindow {
    capacity: usize,
    peer_capacity: usize,
    seen: HashMap<[u8; 32], SeenFrame>,
    peer_counts: HashMap<String, usize>,
}

impl ReplayWindow {
    /// Create a fixed-size global cache with a per-peer ceiling.
    pub fn new(capacity: usize) -> Self {
        Self::with_peer_capacity(capacity, MAX_PEER_CACHE_ENTRIES)
    }

    /// Create a cache with a smaller per-peer ceiling; useful for policy tests.
    pub fn with_peer_capacity(capacity: usize, peer_capacity: usize) -> Self {
        Self {
            capacity,
            peer_capacity: peer_capacity.min(capacity),
            seen: HashMap::with_capacity(capacity),
            peer_counts: HashMap::new(),
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Accept a fresh frame timestamped within ±60 seconds of `now_seconds`.
    pub fn check_and_insert(
        &mut self,
        peer: &str,
        ephemeral: [u8; 32],
        timestamp_seconds: i64,
        now_seconds: i64,
    ) -> Result<(), ReplayError> {
        self.check_and_insert_at(
            peer,
            ephemeral,
            timestamp_seconds,
            now_seconds,
            Instant::now(),
        )
    }

    /// Variant with an injected monotonic clock for deterministic eviction tests.
    pub fn check_and_insert_at(
        &mut self,
        peer: &str,
        ephemeral: [u8; 32],
        timestamp_seconds: i64,
        now_seconds: i64,
        monotonic_now: Instant,
    ) -> Result<(), ReplayError> {
        let distance = i128::from(timestamp_seconds) - i128::from(now_seconds);
        if distance.abs() > i128::from(REPLAY_WINDOW_SECONDS) {
            return Err(ReplayError::OutsideWindow);
        }
        self.evict_expired(monotonic_now, now_seconds);
        if self.seen.contains_key(&ephemeral) {
            return Err(ReplayError::AlreadySeen);
        }
        if self.seen.len() >= self.capacity {
            return Err(ReplayError::Capacity);
        }
        if self.peer_counts.get(peer).copied().unwrap_or_default() >= self.peer_capacity {
            return Err(ReplayError::PeerCapacity);
        }
        let peer = peer.to_owned();
        self.seen.insert(
            ephemeral,
            SeenFrame {
                peer: peer.clone(),
                inserted_at: monotonic_now,
                timestamp_seconds,
            },
        );
        *self.peer_counts.entry(peer).or_default() += 1;
        Ok(())
    }

    fn evict_expired(&mut self, monotonic_now: Instant, now_seconds: i64) {
        let expired = self
            .seen
            .iter()
            .filter_map(|(ephemeral, entry)| {
                let timestamp_distance =
                    i128::from(entry.timestamp_seconds) - i128::from(now_seconds);
                let monotonic_expired = monotonic_now.saturating_duration_since(entry.inserted_at)
                    >= MAX_REPLAY_RETENTION;
                let timestamp_expired =
                    timestamp_distance.abs() > i128::from(REPLAY_WINDOW_SECONDS);
                (monotonic_expired && timestamp_expired).then_some(*ephemeral)
            })
            .collect::<Vec<_>>();
        for ephemeral in expired {
            if let Some(entry) = self.seen.remove(&ephemeral) {
                if let Some(count) = self.peer_counts.get_mut(&entry.peer) {
                    *count -= 1;
                    if *count == 0 {
                        self.peer_counts.remove(&entry.peer);
                    }
                }
            }
        }
    }
}
