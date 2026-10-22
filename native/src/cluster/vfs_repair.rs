//! Which missing copies a repair pass fetches (#778, design step 6).
//!
//! A copy that failed waits before its next try, doubling up to an hour, so
//! a backlog of unreachable blobs cannot use up every pass while others wait.
//! Never-tried copies go first. The queue lives in memory: a restarted
//! daemon simply tries everything once more.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const FIRST_RETRY: Duration = Duration::from_secs(30);
const MAX_RETRY: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Default)]
pub(super) struct RepairQueue {
    /// sha256 -> (next try, failures so far)
    retry: HashMap<String, (Instant, u32)>,
}

impl RepairQueue {
    /// Up to `limit` of `wanted` to try now: new ones first, then the longest
    /// waiting. `force` ignores the wait, as `remuda vfs sync` does.
    pub(super) fn due(
        &mut self,
        wanted: &[String],
        now: Instant,
        force: bool,
        limit: usize,
    ) -> Vec<String> {
        let wanted_set: HashSet<&String> = wanted.iter().collect();
        self.retry.retain(|sha, _| wanted_set.contains(sha));
        let mut due: Vec<String> = wanted
            .iter()
            .filter(|sha| force || self.retry.get(*sha).is_none_or(|(at, _)| *at <= now))
            .cloned()
            .collect();
        due.sort_by_key(|sha| self.retry.get(sha).map(|(at, _)| *at));
        due.truncate(limit);
        due
    }

    /// Record a try: success forgets the copy, failure delays its next try.
    pub(super) fn tried(&mut self, sha: &str, fetched: bool, now: Instant) {
        if fetched {
            self.retry.remove(sha);
            return;
        }
        let (at, errors) = self.retry.entry(sha.to_owned()).or_insert((now, 0));
        *errors += 1;
        let wait = FIRST_RETRY.saturating_mul(1 << (*errors - 1).min(16));
        *at = now + wait.min(MAX_RETRY);
    }

    /// Copies waiting for a later try.
    pub(super) fn waiting(&self, now: Instant) -> usize {
        self.retry.values().filter(|(at, _)| *at > now).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashes(count: usize) -> Vec<String> {
        (0..count).map(|n| format!("{n:064x}")).collect()
    }

    #[test]
    fn a_failing_backlog_cannot_starve_the_rest() {
        let wanted = hashes(300);
        let mut queue = RepairQueue::default();
        let now = Instant::now();
        let first = queue.due(&wanted, now, false, 256);
        assert_eq!(first.len(), 256);
        for sha in &first {
            queue.tried(sha, false, now);
        }
        // The next pass takes the 44 never tried; the failed ones wait.
        let second = queue.due(&wanted, now + Duration::from_secs(1), false, 256);
        assert_eq!(second, wanted[256..].to_vec());
        assert_eq!(queue.waiting(now), 256);
        // Once their wait is over they come back.
        let later = now + FIRST_RETRY;
        assert_eq!(queue.due(&wanted, later, false, 256).len(), 256);
    }

    #[test]
    fn waits_double_up_to_an_hour_and_success_or_removal_forgets() {
        let wanted = hashes(1);
        let sha = &wanted[0];
        let mut queue = RepairQueue::default();
        let now = Instant::now();
        queue.tried(sha, false, now);
        queue.tried(sha, false, now);
        assert_eq!(queue.retry[sha].0, now + FIRST_RETRY * 2);
        for _ in 0..40 {
            queue.tried(sha, false, now);
        }
        assert_eq!(queue.retry[sha].0, now + MAX_RETRY);
        assert!(queue.due(&wanted, now, false, 256).is_empty());
        assert_eq!(queue.due(&wanted, now, true, 256), wanted);

        queue.tried(sha, true, now);
        assert_eq!(queue.waiting(now), 0);
        queue.tried(sha, false, now);
        // A copy no longer wanted (fetched elsewhere, deleted) is dropped.
        assert!(queue.due(&[], now, false, 256).is_empty());
        assert_eq!(queue.waiting(now), 0);
    }
}
