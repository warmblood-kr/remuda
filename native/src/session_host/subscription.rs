//! Bounded subscriber sequencing; parser capture/IPC integration comes later.
use std::collections::VecDeque;
use std::sync::Arc;
const MAX_QUEUE_BYTES: usize = 256 * 1024;
const MAX_QUEUE_ITEMS: usize = 32;
const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    NeedsResync,
    AwaitAck { id: u64, generation: u64 },
    Streaming(u64),
}
struct Update {
    generation: u64,
    bytes: Arc<[u8]>,
}
pub(crate) struct Subscription {
    state: State,
    last_snapshot: u64,
    snapshot: Option<Arc<[u8]>>,
    queue: VecDeque<Update>,
    queued_bytes: usize,
    tail: u64,
}
impl Default for Subscription {
    fn default() -> Self {
        Self {
            state: State::NeedsResync,
            last_snapshot: 0,
            snapshot: None,
            queue: VecDeque::new(),
            queued_bytes: 0,
            tail: 0,
        }
    }
}
impl Subscription {
    pub fn state(&self) -> State {
        self.state
    }
    /// Host must provide one immutable parser cut; this machine cannot prove
    /// capture atomicity. Snapshot IDs are monotonically allocated per subscriber.
    pub fn begin_snapshot(&mut self, generation: u64, bytes: Arc<[u8]>) -> Option<u64> {
        if bytes.len() > MAX_SNAPSHOT_BYTES || self.state != State::NeedsResync {
            return None;
        }
        let id = self.last_snapshot.checked_add(1)?;
        self.last_snapshot = id;
        self.tail = generation;
        self.snapshot = Some(bytes);
        self.state = State::AwaitAck { id, generation };
        Some(id)
    }
    pub fn acknowledge(&mut self, id: u64, generation: u64) -> bool {
        if self.state != (State::AwaitAck { id, generation }) {
            return false;
        }
        self.snapshot = None;
        self.state = State::Streaming(generation);
        true
    }
    /// Whole rendered delta with an explicit base generation. Overflow while
    /// snapshot pages/ACK are in flight invalidates that snapshot as well.
    pub fn publish(&mut self, base: u64, generation: u64, bytes: Arc<[u8]>) -> bool {
        if self.state == State::NeedsResync {
            return false;
        }
        if base != self.tail
            || generation <= base
            || self.queue.len() == MAX_QUEUE_ITEMS
            || bytes.len() > MAX_QUEUE_BYTES.saturating_sub(self.queued_bytes)
        {
            self.invalidate();
            return false;
        }
        self.queued_bytes += bytes.len();
        self.tail = generation;
        self.queue.push_back(Update { generation, bytes });
        true
    }
    pub fn pop(&mut self) -> Option<(u64, Arc<[u8]>)> {
        if !matches!(self.state, State::Streaming(_)) {
            return None;
        }
        let update = self.queue.pop_front()?;
        self.queued_bytes -= update.bytes.len();
        self.state = State::Streaming(update.generation);
        Some((update.generation, update.bytes))
    }
    pub fn invalidate(&mut self) {
        self.state = State::NeedsResync;
        self.queue.clear();
        self.queued_bytes = 0;
        self.snapshot = None;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overflow_while_awaiting_ack_invalidates_snapshot_and_stale_ack() {
        let mut subscriber = Subscription::default();
        let snapshot = Arc::from(&b"screen"[..]);
        let id = subscriber
            .begin_snapshot(10, Arc::clone(&snapshot))
            .unwrap();
        assert!(subscriber.publish(10, 11, Arc::from(vec![0; MAX_QUEUE_BYTES])));
        assert!(subscriber.pop().is_none());
        assert!(!subscriber.publish(11, 12, Arc::from(&b"x"[..])));
        assert_eq!(subscriber.state(), State::NeedsResync);
        assert_eq!(subscriber.queued_bytes, 0);
        assert!(subscriber.snapshot.is_none());
        assert!(!subscriber.acknowledge(id, 10));
        let fresh = subscriber.begin_snapshot(12, snapshot).unwrap();
        assert!(fresh > id);
        assert!(subscriber.acknowledge(fresh, 12));
        assert!(subscriber.publish(12, 13, Arc::from(&b"delta"[..])));
        assert_eq!(subscriber.pop().unwrap().0, 13);
    }
    #[test]
    fn count_gaps_and_snapshot_budget_are_independent_bounds() {
        let mut subscriber = Subscription::default();
        assert!(subscriber
            .begin_snapshot(1, Arc::from(vec![0; MAX_SNAPSHOT_BYTES + 1]))
            .is_none());
        let id = subscriber.begin_snapshot(1, Arc::from(&b""[..])).unwrap();
        assert!(!subscriber.acknowledge(id, 2));
        assert!(subscriber.acknowledge(id, 1));
        for n in 1..=MAX_QUEUE_ITEMS as u64 {
            assert!(subscriber.publish(n, n + 1, Arc::from(&b""[..])));
        }
        assert!(!subscriber.publish(33, 34, Arc::from(&b""[..])));
        let id = subscriber.begin_snapshot(100, Arc::from(&b""[..])).unwrap();
        assert!(subscriber.acknowledge(id, 100));
        assert!(!subscriber.publish(99, 101, Arc::from(&b"bad base"[..])));
        assert_eq!(subscriber.state(), State::NeedsResync);
    }
}
