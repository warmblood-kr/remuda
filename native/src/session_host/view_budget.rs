//! One snapshot allocation, bounded FIFO turns and capture cooldowns. The
//! host-wide cooldown after a snapshot grows with its size, so small typed
//! views (most captures since compact rows) stay fast while large ones are
//! still spaced by the full cooldown.
use super::control::ConnectionId;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
const CONNECTIONS: usize = 16;
const TURN_TTL: Duration = Duration::from_millis(500);
const CONNECTION_COOLDOWN: Duration = Duration::from_millis(250);
const GLOBAL_COOLDOWN: Duration = Duration::from_millis(50);
/// Host-wide cooldown per snapshot page, up to `GLOBAL_COOLDOWN`.
const PAGE_COOLDOWN: Duration = Duration::from_millis(1);

pub(crate) struct Budget(Arc<Mutex<State>>);
struct State {
    owner: Option<ConnectionId>,
    connections: Vec<(ConnectionId, Instant)>,
    waiting: VecDeque<(ConnectionId, Instant)>,
    ready: Instant,
}
pub(crate) struct Lease {
    state: Arc<Mutex<State>>,
    id: ConnectionId,
    released: bool,
    cooldown: Duration,
}
impl Budget {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(State {
            owner: None,
            connections: Vec::new(),
            waiting: VecDeque::new(),
            ready: Instant::now(),
        })))
    }
    pub fn acquire(&self, id: ConnectionId, now: Instant) -> Option<Lease> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.waiting.retain(|(_, until)| *until > now);
        if !state.connections.iter().any(|(known, _)| *known == id) {
            if state.connections.len() == CONNECTIONS {
                return None;
            }
            state.connections.push((id, now));
        }
        let (_, ready) = state.connections.iter().find(|(known, _)| *known == id)?;
        if *ready > now || state.owner == Some(id) {
            return None;
        }
        if let Some((_, until)) = state.waiting.iter_mut().find(|(waiting, _)| *waiting == id) {
            *until = now + TURN_TTL;
        } else {
            state.waiting.push_back((id, now + TURN_TTL));
        }
        if state.owner.is_some() || state.ready > now || state.waiting.front()?.0 != id {
            return None;
        }
        state.waiting.pop_front();
        state.owner = Some(id);
        Some(Lease {
            state: self.0.clone(),
            id,
            released: false,
            cooldown: GLOBAL_COOLDOWN,
        })
    }
    /// Caller drops its snapshot lease before forgetting its connection.
    pub fn forget(&self, id: ConnectionId) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.connections.retain(|(known, _)| *known != id);
        state.waiting.retain(|(waiting, _)| *waiting != id);
    }
}
impl Lease {
    /// Sizes the host-wide cooldown to the snapshot this lease holds.
    pub fn charge(&mut self, pages: usize) {
        let pages = u32::try_from(pages.max(1)).unwrap_or(u32::MAX);
        self.cooldown = PAGE_COOLDOWN.saturating_mul(pages).min(GLOBAL_COOLDOWN);
    }
    fn release(&mut self, now: Instant) {
        if self.released {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.owner == Some(self.id) {
            state.owner = None;
            state.ready = now + self.cooldown;
            if let Some((_, ready)) = state.connections.iter_mut().find(|(id, _)| *id == self.id) {
                *ready = now + CONNECTION_COOLDOWN;
            }
        }
        self.released = true;
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.release(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn active_allocation_fifo_turn_and_both_cooldowns_are_independent() {
        let budget = Budget::new();
        let now = Instant::now();
        let mut first = budget.acquire(ConnectionId(1), now).unwrap();
        assert!(budget.acquire(ConnectionId(2), now).is_none());
        assert!(budget.acquire(ConnectionId(3), now).is_none());
        first.release(now);
        assert!(budget.acquire(ConnectionId(2), now).is_none());
        let after = now + GLOBAL_COOLDOWN;
        assert!(budget.acquire(ConnectionId(1), after).is_none());
        assert!(budget.acquire(ConnectionId(3), after).is_none());
        let mut second = budget.acquire(ConnectionId(2), after).unwrap();
        second.release(after);
        assert!(budget
            .acquire(ConnectionId(3), after + GLOBAL_COOLDOWN)
            .is_some());
    }
    #[test]
    fn abandoned_turn_expires_and_disconnects_do_not_fill_lifetime_slots() {
        let budget = Budget::new();
        let now = Instant::now();
        let mut active = budget.acquire(ConnectionId(1), now).unwrap();
        assert!(budget.acquire(ConnectionId(2), now).is_none());
        assert!(budget.acquire(ConnectionId(3), now).is_none());
        active.release(now);
        let later = now + TURN_TTL;
        assert!(budget.acquire(ConnectionId(3), later).is_some());
        for id in 1..=3 {
            budget.forget(ConnectionId(id));
        }
        for id in 4..100 {
            let time = later + Duration::from_secs(id as u64);
            let lease = budget.acquire(ConnectionId(id), time).unwrap();
            drop(lease);
            budget.forget(ConnectionId(id));
        }
        assert!(budget.0.lock().unwrap().connections.is_empty());
        assert!(budget.0.lock().unwrap().waiting.is_empty());
    }
    #[test]
    fn the_global_cooldown_follows_the_snapshot_size() {
        let budget = Budget::new();
        let now = Instant::now();
        let mut small = budget.acquire(ConnectionId(1), now).unwrap();
        small.charge(1);
        small.release(now);
        assert!(budget.acquire(ConnectionId(2), now).is_none());
        let after = now + PAGE_COOLDOWN;
        let mut large = budget.acquire(ConnectionId(2), after).unwrap();
        large.charge(10_000);
        large.release(after);
        assert!(budget
            .acquire(ConnectionId(3), after + GLOBAL_COOLDOWN - PAGE_COOLDOWN)
            .is_none());
        assert!(budget
            .acquire(ConnectionId(3), after + GLOBAL_COOLDOWN)
            .is_some());
    }
    #[test]
    fn connection_and_waiting_storage_refuse_at_fixed_count() {
        let budget = Budget::new();
        let now = Instant::now();
        let lease = budget.acquire(ConnectionId(1), now).unwrap();
        for id in 2..=16 {
            assert!(budget.acquire(ConnectionId(id), now).is_none());
        }
        assert!(budget.acquire(ConnectionId(17), now).is_none());
        let state = budget.0.lock().unwrap();
        assert_eq!(state.connections.len(), 16);
        assert_eq!(state.waiting.len(), 15);
        drop(state);
        drop(lease);
    }
}
