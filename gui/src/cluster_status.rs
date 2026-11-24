use remuda_native::{
    cluster,
    cluster_remote::{RemotePoller, RemoteSource, RemoteState},
};
use std::{
    collections::HashMap,
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, Thread},
    time::{Duration, SystemTime},
};

const SAMPLE_PERIOD: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Reachability {
    Reachable,
    Connecting,
    Offline,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PeerStatus {
    pub(super) state: Reachability,
    pub(super) last_updated_at: Option<SystemTime>,
}

pub(super) struct ClusterStatus {
    statuses: Arc<Mutex<HashMap<String, PeerStatus>>>,
    stopped: Arc<AtomicBool>,
    worker: Thread,
}

impl ClusterStatus {
    pub(super) fn start() -> io::Result<Self> {
        let statuses = Arc::new(Mutex::new(HashMap::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let shared_statuses = statuses.clone();
        let shared_stopped = stopped.clone();
        let worker = thread::Builder::new()
            .name("gui-cluster-status".into())
            .spawn(move || run(&shared_statuses, &shared_stopped))?;
        Ok(Self {
            statuses,
            stopped,
            worker: worker.thread().clone(),
        })
    }

    pub(super) fn snapshot(&self) -> HashMap<String, PeerStatus> {
        self.statuses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Drop for ClusterStatus {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        // Registry I/O and the poller's shutdown stay off the UI thread.
        self.worker.unpark();
    }
}

fn run(statuses: &Mutex<HashMap<String, PeerStatus>>, stopped: &AtomicBool) {
    let (poller, source) = loop {
        if stopped.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(Some((identity, registry))) = cluster::nodes() {
            if let Ok(poller) = RemotePoller::from_registry(&registry, &identity.node_fp) {
                if poller.start().is_ok() {
                    let source = poller.source();
                    break (poller, source);
                }
            }
        }
        thread::park_timeout(SAMPLE_PERIOD);
    };

    while !stopped.load(Ordering::Relaxed) {
        let sampled = source
            .snapshot()
            .nodes
            .into_iter()
            .map(|node| {
                let state = match node.state {
                    RemoteState::Reachable => Reachability::Reachable,
                    RemoteState::Stale | RemoteState::Reconnecting => Reachability::Connecting,
                    RemoteState::Unreachable
                        if node.last_updated_at.is_none() && node.last_error.is_none() =>
                    {
                        Reachability::Unknown
                    }
                    RemoteState::Unreachable => Reachability::Offline,
                };
                (
                    node.registry_key,
                    PeerStatus {
                        state,
                        last_updated_at: node.last_updated_at,
                    },
                )
            })
            .collect();
        *statuses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = sampled;
        thread::park_timeout(SAMPLE_PERIOD);
    }
    drop(poller);
}
