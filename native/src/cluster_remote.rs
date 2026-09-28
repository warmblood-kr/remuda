//! Bounded remote-node polling state, separate from the cluster tree renderer.

use remuda_core::agent::ScreenSnapshot;
use std::collections::{BTreeMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_REMOTE_WORKERS: usize = 8;
pub const REMOTE_SYNC_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteState {
    Reachable,
    Stale,
    Reconnecting,
    Unreachable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSessionSnapshot {
    pub name: String,
    pub instance_id: String,
    pub alive: bool,
    pub output_version: u64,
    pub screen: Option<ScreenSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteNodeSnapshot {
    pub name: String,
    pub state: RemoteState,
    pub last_sync_age: Option<Duration>,
    pub sessions: Vec<RemoteSessionSnapshot>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteSnapshot {
    pub nodes: Vec<RemoteNodeSnapshot>,
}

/// Read-only view consumed by the cluster tree and replaceable by a test fake.
pub trait RemoteSource: Send + Sync {
    fn snapshot(&self) -> RemoteSnapshot;
}

pub struct RemoteNode {
    name: String,
    state: RemoteState,
    sessions: Vec<RemoteSessionSnapshot>,
    last_sync: Option<Instant>,
    failure_count: u32,
    next_poll: Instant,
    polling: bool,
}

impl RemoteNode {
    pub fn new(name: impl Into<String>, now: Instant) -> Self {
        Self {
            name: name.into(),
            state: RemoteState::Unreachable,
            sessions: Vec::new(),
            last_sync: None,
            failure_count: 0,
            next_poll: now,
            polling: false,
        }
    }

    pub fn begin_poll(&mut self, now: Instant) -> bool {
        if self.polling || now < self.next_poll {
            return false;
        }
        self.polling = true;
        self.state = RemoteState::Reconnecting;
        true
    }

    pub fn poll_succeeded(&mut self, sessions: Vec<RemoteSessionSnapshot>, now: Instant) {
        self.sessions = sessions;
        self.last_sync = Some(now);
        self.failure_count = 0;
        self.next_poll = now + POLL_INTERVAL;
        self.polling = false;
        self.state = RemoteState::Reachable;
    }

    pub fn poll_failed(&mut self, now: Instant) {
        self.failure_count = self.failure_count.saturating_add(1);
        self.next_poll = now + retry_backoff(self.failure_count);
        self.polling = false;
        self.state = if self.last_sync.is_some() {
            RemoteState::Stale
        } else {
            RemoteState::Unreachable
        };
    }

    pub fn retry_at(&self) -> Instant {
        self.next_poll
    }

    fn snapshot_at(&self, now: Instant) -> RemoteNodeSnapshot {
        RemoteNodeSnapshot {
            name: self.name.clone(),
            state: self.state,
            last_sync_age: self
                .last_sync
                .map(|last| now.saturating_duration_since(last)),
            sessions: self.sessions.clone(),
        }
    }
}

fn retry_backoff(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(5);
    BACKOFF_BASE.saturating_mul(1_u32 << shift).min(BACKOFF_MAX)
}

#[derive(Default)]
pub struct RemoteTreeState {
    nodes: Mutex<BTreeMap<String, RemoteNode>>,
}

impl RemoteTreeState {
    pub fn insert_node(&self, node: RemoteNode) {
        self.nodes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(node.name.clone(), node);
    }

    pub fn remove_node(&self, name: &str) -> Option<RemoteNode> {
        self.nodes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(name)
    }

    pub fn with_node<R>(&self, name: &str, f: impl FnOnce(&mut RemoteNode) -> R) -> Option<R> {
        self.nodes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(name)
            .map(f)
    }

    pub fn snapshot_at(&self, now: Instant) -> RemoteSnapshot {
        let nodes = self
            .nodes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|node| node.snapshot_at(now))
            .collect();
        RemoteSnapshot { nodes }
    }
}

impl RemoteSource for RemoteTreeState {
    fn snapshot(&self) -> RemoteSnapshot {
        self.snapshot_at(Instant::now())
    }
}

#[derive(Default)]
struct WorkerState {
    nodes: HashSet<String>,
}

/// Owns at most one polling worker per node and eight workers in total.
#[derive(Default)]
pub struct RemoteWorkerCap {
    state: Mutex<WorkerState>,
}

impl RemoteWorkerCap {
    pub fn acquire(self: &Arc<Self>, node: impl Into<String>) -> Option<RemoteWorkerPermit> {
        let node = node.into();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.nodes.len() >= MAX_REMOTE_WORKERS || !state.nodes.insert(node.clone()) {
            return None;
        }
        Some(RemoteWorkerPermit {
            cap: Arc::clone(self),
            node,
        })
    }

    pub fn spawn(
        self: &Arc<Self>,
        node: impl Into<String>,
        work: impl FnOnce() + Send + 'static,
    ) -> io::Result<bool> {
        let Some(permit) = self.acquire(node) else {
            return Ok(false);
        };
        std::thread::Builder::new()
            .name("cluster-remote-poll".into())
            .spawn(move || {
                let _permit = permit;
                work();
            })?;
        Ok(true)
    }
}

pub struct RemoteWorkerPermit {
    cap: Arc<RemoteWorkerCap>,
    node: String,
}

impl Drop for RemoteWorkerPermit {
    fn drop(&mut self) {
        self.cap
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .nodes
            .remove(&self.node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_core::agent::{Color, Cursor, StyledCell};

    fn screen(text: &str) -> ScreenSnapshot {
        ScreenSnapshot {
            cells: vec![vec![StyledCell {
                text: text.into(),
                fg: Color::Default,
                bg: Color::Default,
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                inverse: false,
                wide: false,
            }]],
            wrapped: vec![false],
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: true,
            },
            scrollback_len: 0,
            scrollback_total: 0,
        }
    }

    fn session(name: &str, text: &str) -> RemoteSessionSnapshot {
        RemoteSessionSnapshot {
            name: name.into(),
            instance_id: "instance-1".into(),
            alive: true,
            output_version: 4,
            screen: Some(screen(text)),
        }
    }

    #[test]
    fn failed_poll_keeps_last_good_snapshot_and_applies_bounded_backoff() {
        let start = Instant::now();
        let mut node = RemoteNode::new("field-laptop", start);
        assert!(node.begin_poll(start));
        node.poll_succeeded(vec![session("dev", "last frame")], start);
        let failed_at = start + Duration::from_secs(2);
        assert!(node.begin_poll(failed_at));
        node.poll_failed(failed_at);

        let snapshot = node.snapshot_at(failed_at + Duration::from_secs(1));
        assert_eq!(snapshot.state, RemoteState::Stale);
        assert_eq!(snapshot.last_sync_age, Some(Duration::from_secs(3)));
        assert_eq!(snapshot.sessions[0].screen, Some(screen("last frame")));
        assert_eq!(node.retry_at(), failed_at + BACKOFF_BASE);
        assert!(!node.begin_poll(failed_at + Duration::from_millis(999)));
        assert!(node.begin_poll(node.retry_at()));
        assert_eq!(node.snapshot_at(failed_at).state, RemoteState::Reconnecting);
        node.poll_failed(node.retry_at());
        let mut retry_at = node.retry_at();
        let mut last_backoff = Duration::ZERO;
        for _ in 0..8 {
            assert!(node.begin_poll(retry_at));
            node.poll_failed(retry_at);
            last_backoff = node.retry_at().duration_since(retry_at);
            assert!(last_backoff <= BACKOFF_MAX);
            retry_at = node.retry_at();
        }
        assert_eq!(last_backoff, BACKOFF_MAX);
    }

    #[test]
    fn first_failure_is_unreachable_then_reconnects_on_retry() {
        let start = Instant::now();
        let mut node = RemoteNode::new("field-laptop", start);
        assert!(node.begin_poll(start));
        node.poll_failed(start);
        assert_eq!(node.snapshot_at(start).state, RemoteState::Unreachable);
        assert!(!node.begin_poll(start + Duration::from_millis(999)));
        assert!(node.begin_poll(start + BACKOFF_BASE));
        assert_eq!(node.snapshot_at(start).state, RemoteState::Reconnecting);
    }

    #[test]
    fn worker_cap_allows_one_per_node_and_eight_total() {
        let cap = Arc::new(RemoteWorkerCap::default());
        let permits: Vec<_> = (0..MAX_REMOTE_WORKERS)
            .map(|index| cap.acquire(format!("node-{index}")).expect("within cap"))
            .collect();
        assert!(cap.acquire("node-extra").is_none());
        assert!(cap.acquire("node-0").is_none());
        drop(permits);
        assert!(cap.acquire("node-0").is_some());
    }

    struct FakeSource(RemoteSnapshot);

    impl RemoteSource for FakeSource {
        fn snapshot(&self) -> RemoteSnapshot {
            self.0.clone()
        }
    }

    #[test]
    fn fake_source_exposes_node_state_and_sync_age_to_the_tree() {
        let start = Instant::now();
        let mut node = RemoteNode::new("field-laptop", start);
        node.poll_succeeded(vec![session("dev", "screen")], start);
        node.poll_failed(start + Duration::from_secs(1));
        let fake = FakeSource(RemoteSnapshot {
            nodes: vec![node.snapshot_at(start + Duration::from_secs(3))],
        });
        let snapshot = fake.snapshot();
        assert_eq!(snapshot.nodes[0].state, RemoteState::Stale);
        assert_eq!(
            snapshot.nodes[0].last_sync_age,
            Some(Duration::from_secs(3))
        );
        assert_eq!(snapshot.nodes[0].sessions[0].name, "dev");
    }
}
