//! Bounded remote-node polling state, separate from the cluster tree renderer.

use remuda_core::agent::ScreenSnapshot;
use remuda_core::protocol::{expand_runs, Request, Response};
use std::collections::{BTreeMap, HashSet};
use std::io;
use std::net::SocketAddr;
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
    /// Sanitized label used only for display and UI selection.
    pub name: String,
    /// Original protocol name used when sending requests to this session.
    pub wire_name: String,
    pub instance_id: String,
    pub alive: bool,
    pub output_version: Option<u64>,
    pub screen: Option<ScreenSnapshot>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteNodeSnapshot {
    pub name: String,
    pub state: RemoteState,
    pub last_sync_age: Option<Duration>,
    pub last_error: Option<String>,
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

/// Sends idempotent Input batches to a remote member. Callers own retry policy.
pub trait RemoteInputTransport: Send + Sync {
    fn send_input(&self, node: &str, request: &Request) -> io::Result<Response>;
}

/// One-shot Noise client used by the remote composer.
type RemoteTargetResolver =
    dyn Fn(&str) -> io::Result<crate::cluster::ResolvedTarget> + Send + Sync;

pub struct ClusterRemoteInput {
    client: crate::net::cluster_client::ClusterClient,
    local_static_private: zeroize::Zeroizing<Vec<u8>>,
    resolve: Box<RemoteTargetResolver>,
}

impl ClusterRemoteInput {
    pub fn system() -> io::Result<Self> {
        let local_static_private = crate::cluster::identity::load_static_private_key()?;
        let timeout = Duration::from_secs(1);
        let client = crate::net::cluster_client::ClusterClient::with_timeouts(
            std::sync::Arc::new(crate::SystemWallClock::new()),
            crate::net::cluster_client::ClientTimeouts {
                connect: timeout,
                read: timeout,
                total: timeout,
            },
        );
        Ok(Self::with_client(local_static_private, client, |node| {
            crate::cluster::resolve_target(node, None)
        }))
    }

    pub fn with_client(
        local_static_private: zeroize::Zeroizing<Vec<u8>>,
        client: crate::net::cluster_client::ClusterClient,
        resolve: impl Fn(&str) -> io::Result<crate::cluster::ResolvedTarget> + Send + Sync + 'static,
    ) -> Self {
        Self {
            client,
            local_static_private,
            resolve: Box::new(resolve),
        }
    }
}

impl RemoteInputTransport for ClusterRemoteInput {
    fn send_input(&self, node: &str, request: &Request) -> io::Result<Response> {
        let target = (self.resolve)(node)?;
        self.client
            .request(
                target.address,
                &target.pinned_static_key,
                &self.local_static_private,
                request,
            )
            .map_err(io::Error::other)
    }
}

/// A caller-selected registry target. Addresses are transient overrides; the
/// registry resolver remains the source of the peer's pinned identity.
#[derive(Clone, Debug)]
pub struct RemoteTarget {
    pub name: String,
    /// Exact registry fingerprint, so colliding short node labels stay distinct.
    pub registry_key: String,
    pub addr_override: Option<SocketAddr>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SelectedSession {
    node: String,
    session: String,
}

/// Mutable selection signal kept separate from the read-only snapshot source.
#[derive(Clone, Default)]
pub struct RemoteSelection {
    selected: Arc<Mutex<Option<SelectedSession>>>,
}

impl RemoteSelection {
    pub fn select(&self, node: impl Into<String>, session: impl Into<String>) {
        *self
            .selected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(SelectedSession {
            node: node.into(),
            session: session.into(),
        });
    }

    pub fn clear(&self) {
        *self
            .selected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    fn selected_session(&self, node: &str) -> Option<String> {
        self.selected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .filter(|selected| selected.node == node)
            .map(|selected| selected.session.clone())
    }
}

/// Build poll targets from the cluster registry. PR9 adds the registry endpoint
/// field; until then these targets have no route and the TUI reports that fact.
pub fn registry_targets(
    registry: &crate::cluster::Registry,
    local_fingerprint: &str,
) -> Vec<RemoteTarget> {
    registry
        .authorized_nodes
        .iter()
        .filter(|entry| {
            entry.state == crate::cluster::NodeState::Admitted && entry.node_fp != local_fingerprint
        })
        .map(|entry| RemoteTarget {
            name: crate::cluster::node_label(&entry.node_fp),
            registry_key: entry.node_fp.clone(),
            addr_override: None,
        })
        .collect()
}

/// One read-only node poll. Implementations make at most one Sync call at a
/// time for this node and preserve the previous frame when nothing changed.
pub trait RemoteTransport: Send + Sync + 'static {
    fn poll_node(
        &self,
        target: &RemoteTarget,
        selected_session: Option<&str>,
        previous: &[RemoteSessionSnapshot],
    ) -> io::Result<Vec<RemoteSessionSnapshot>>;
}

pub struct RemoteNode {
    key: String,
    name: String,
    state: RemoteState,
    sessions: Vec<RemoteSessionSnapshot>,
    last_sync: Option<Instant>,
    failure_count: u32,
    last_error: Option<String>,
    next_poll: Instant,
    polling: bool,
}

impl RemoteNode {
    pub fn new(name: impl Into<String>, now: Instant) -> Self {
        let name = name.into();
        Self::with_key(name.clone(), name, now)
    }

    fn with_key(key: String, name: String, now: Instant) -> Self {
        Self {
            key,
            name,
            state: RemoteState::Unreachable,
            sessions: Vec::new(),
            last_sync: None,
            failure_count: 0,
            last_error: None,
            next_poll: now,
            polling: false,
        }
    }

    pub fn begin_poll(&mut self, now: Instant) -> bool {
        if self.polling || now < self.next_poll {
            return false;
        }
        self.polling = true;
        if self.last_sync.is_none() {
            self.state = RemoteState::Reconnecting;
        }
        true
    }

    pub fn poll_succeeded(&mut self, sessions: Vec<RemoteSessionSnapshot>, now: Instant) {
        self.sessions = sessions;
        self.last_sync = Some(now);
        self.failure_count = 0;
        self.last_error = None;
        self.next_poll = now + POLL_INTERVAL;
        self.polling = false;
        self.state = RemoteState::Reachable;
    }

    pub fn poll_failed(&mut self, now: Instant) {
        self.poll_failed_with_reason(now, None);
    }

    pub fn poll_failed_with_reason(&mut self, now: Instant, reason: Option<String>) {
        self.failure_count = self.failure_count.saturating_add(1);
        self.last_error = reason;
        self.next_poll = now + retry_backoff(self.failure_count);
        self.polling = false;
        self.state = if self.last_sync.is_some() {
            if self.failure_count >= 2 {
                RemoteState::Unreachable
            } else {
                RemoteState::Stale
            }
        } else {
            RemoteState::Unreachable
        };
    }

    pub fn retry_at(&self) -> Instant {
        self.next_poll
    }

    fn sessions(&self) -> Vec<RemoteSessionSnapshot> {
        self.sessions.clone()
    }

    fn snapshot_at(&self, now: Instant) -> RemoteNodeSnapshot {
        RemoteNodeSnapshot {
            name: self.name.clone(),
            state: self.state,
            last_sync_age: self
                .last_sync
                .map(|last| now.saturating_duration_since(last)),
            last_error: self.last_error.clone(),
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
            .insert(node.key.clone(), node);
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

/// Remote protocol implementation using the cluster's shared target resolver
/// and pinned one-shot Noise client.
type TargetResolver =
    dyn Fn(&RemoteTarget) -> io::Result<crate::cluster::ResolvedTarget> + Send + Sync;

pub struct ClusterRemoteTransport {
    client: crate::net::cluster_client::ClusterClient,
    local_static_private: zeroize::Zeroizing<Vec<u8>>,
    resolve: Box<TargetResolver>,
}

impl ClusterRemoteTransport {
    pub fn system() -> io::Result<Self> {
        let local_static_private = crate::cluster::identity::load_static_private_key()?;
        let timeout = REMOTE_SYNC_TIMEOUT + Duration::from_secs(5);
        let client = crate::net::cluster_client::ClusterClient::with_timeouts(
            Arc::new(crate::SystemWallClock::new()),
            crate::net::cluster_client::ClientTimeouts {
                connect: Duration::from_secs(5),
                read: timeout,
                total: timeout,
            },
        );
        Ok(Self::with_client(local_static_private, client, |target| {
            crate::cluster::resolve_target(&target.registry_key, target.addr_override)
        }))
    }

    /// Construct with injected pinned identity resolution, primarily for
    /// isolated two-node tests. Production uses [`Self::system`].
    pub fn with_client(
        local_static_private: zeroize::Zeroizing<Vec<u8>>,
        client: crate::net::cluster_client::ClusterClient,
        resolve: impl Fn(&RemoteTarget) -> io::Result<crate::cluster::ResolvedTarget>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            client,
            local_static_private,
            resolve: Box::new(resolve),
        }
    }

    fn request(
        &self,
        target: &crate::cluster::ResolvedTarget,
        request: &Request,
    ) -> io::Result<Response> {
        self.client
            .request(
                target.address,
                &target.pinned_static_key,
                &self.local_static_private,
                request,
            )
            .map_err(io::Error::other)
    }

    fn list_sessions(
        &self,
        target: &crate::cluster::ResolvedTarget,
    ) -> io::Result<Vec<remuda_core::registry::SessionSummary>> {
        match self.request(target, &Request::List)? {
            Response::Sessions(sessions) => Ok(sessions),
            Response::Error(error) => Err(io::Error::other(error)),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected remote List response: {other:?}"),
            )),
        }
    }

    fn selected_session_snapshot(
        &self,
        target: &crate::cluster::ResolvedTarget,
        session: remuda_core::registry::SessionSummary,
        prior: Option<&RemoteSessionSnapshot>,
    ) -> io::Result<Option<RemoteSessionSnapshot>> {
        let listed_instance_id = listed_instance_id(&session);
        let response = if should_sync(prior) {
            let prior = prior.expect("should_sync requires a prior screen");
            self.request(
                target,
                &Request::Sync {
                    name: prior.wire_name.clone(),
                    instance_id: Some(prior.instance_id.clone()),
                    since: prior.output_version.unwrap_or_default(),
                    timeout_ms: REMOTE_SYNC_TIMEOUT.as_millis() as u64,
                },
            )?
        } else {
            self.request(
                target,
                &Request::CaptureStyled {
                    name: session.name.clone(),
                    scrollback: 0,
                },
            )?
        };

        match response {
            Response::Sync {
                instance_id,
                output_version,
                snapshot,
            } => Ok(Some(RemoteSessionSnapshot {
                name: display_name(&session.name),
                wire_name: session.name,
                instance_id,
                alive: true,
                output_version: Some(output_version),
                screen: Some(to_screen_snapshot(snapshot)),
                last_error: None,
            })),
            Response::StyledScreen {
                rows,
                instance_id,
                output_version,
                wrapped,
                scrollback_len,
                scrollback_total,
                cursor,
            } => Ok(instance_id
                .or(listed_instance_id)
                .map(|instance_id| RemoteSessionSnapshot {
                    name: display_name(&session.name),
                    wire_name: session.name,
                    instance_id,
                    alive: true,
                    output_version,
                    screen: Some(to_screen_snapshot(remuda_core::protocol::StyledScreen {
                        rows,
                        wrapped,
                        scrollback_len,
                        scrollback_total,
                        cursor,
                    })),
                    last_error: None,
                })),
            Response::WrongInstance => Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "remote session instance changed during poll",
            )),
            Response::SyncAtCapacity => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote Sync is at capacity",
            )),
            Response::Error(error) => Err(io::Error::other(error)),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected remote screen response: {other:?}"),
            )),
        }
    }
}

impl RemoteTransport for ClusterRemoteTransport {
    fn poll_node(
        &self,
        target: &RemoteTarget,
        selected_session: Option<&str>,
        previous: &[RemoteSessionSnapshot],
    ) -> io::Result<Vec<RemoteSessionSnapshot>> {
        let resolved = (self.resolve)(target)?;
        let sessions = self.list_sessions(&resolved)?;
        let mut snapshots = Vec::with_capacity(sessions.len());
        for session in sessions {
            let prior = previous_session(&session, previous);
            if !session.alive || selected_session != Some(display_name(&session.name).as_str()) {
                if let Some(snapshot) = unpolled_session_snapshot(session, prior) {
                    snapshots.push(snapshot);
                }
                continue;
            }
            match self.selected_session_snapshot(&resolved, session.clone(), prior) {
                Ok(Some(snapshot)) => snapshots.push(snapshot),
                Ok(None) => {}
                Err(error) => {
                    if let Some(snapshot) = session_after_poll_error(session, prior, error) {
                        snapshots.push(snapshot);
                    }
                }
            }
        }
        Ok(snapshots)
    }
}

fn listed_instance_id(session: &remuda_core::registry::SessionSummary) -> Option<String> {
    session
        .instance_id
        .clone()
        .or_else(|| (!session.id.is_empty()).then(|| session.id.clone()))
}

fn display_name(name: &str) -> String {
    crate::text::strip_terminal_controls(name).into_owned()
}

fn previous_session<'a>(
    session: &remuda_core::registry::SessionSummary,
    previous: &'a [RemoteSessionSnapshot],
) -> Option<&'a RemoteSessionSnapshot> {
    let instance_id = listed_instance_id(session);
    previous.iter().find(|old| {
        old.name == display_name(&session.name) && instance_id.as_ref() == Some(&old.instance_id)
    })
}

fn should_sync(prior: Option<&RemoteSessionSnapshot>) -> bool {
    prior.is_some_and(|old| old.screen.is_some() && old.output_version.is_some())
}

fn unpolled_session_snapshot(
    session: remuda_core::registry::SessionSummary,
    prior: Option<&RemoteSessionSnapshot>,
) -> Option<RemoteSessionSnapshot> {
    let wire_name = session.name.clone();
    let instance_id = prior
        .map(|old| old.instance_id.clone())
        .or_else(|| listed_instance_id(&session))?;
    Some(RemoteSessionSnapshot {
        name: display_name(&session.name),
        wire_name,
        instance_id,
        alive: session.alive,
        output_version: session.output_version,
        screen: prior.and_then(|old| old.screen.clone()),
        last_error: prior.and_then(|old| old.last_error.clone()),
    })
}

fn session_after_poll_error(
    session: remuda_core::registry::SessionSummary,
    prior: Option<&RemoteSessionSnapshot>,
    error: io::Error,
) -> Option<RemoteSessionSnapshot> {
    let mut snapshot = unpolled_session_snapshot(session, prior)?;
    snapshot.last_error = Some(error.to_string());
    Some(snapshot)
}

fn to_screen_snapshot(screen: remuda_core::protocol::StyledScreen) -> ScreenSnapshot {
    ScreenSnapshot {
        cells: screen
            .rows
            .iter()
            .map(|row| {
                expand_runs(row)
                    .into_iter()
                    .map(|mut cell| {
                        cell.text = crate::text::strip_terminal_controls(&cell.text).into_owned();
                        cell
                    })
                    .collect()
            })
            .collect(),
        wrapped: screen.wrapped,
        cursor: screen.cursor,
        scrollback_len: screen.scrollback_len,
        scrollback_total: screen.scrollback_total,
    }
}

/// Starts bounded background polling while exposing cloneable snapshots to the
/// tree. The TUI schedules a node again after its current worker completes.
pub struct RemotePoller {
    state: Arc<RemoteTreeState>,
    transport: Arc<dyn RemoteTransport>,
    workers: Arc<RemoteWorkerCap>,
    targets: Vec<RemoteTarget>,
    registry_fingerprint: Option<String>,
    selection: RemoteSelection,
    stop: Arc<std::sync::atomic::AtomicBool>,
    scheduler: Mutex<Option<std::thread::JoinHandle<()>>>,
}

struct UnavailableRemoteTransport(String);

impl RemoteTransport for UnavailableRemoteTransport {
    fn poll_node(
        &self,
        _target: &RemoteTarget,
        _selected_session: Option<&str>,
        _previous: &[RemoteSessionSnapshot],
    ) -> io::Result<Vec<RemoteSessionSnapshot>> {
        Err(io::Error::other(self.0.clone()))
    }
}

impl RemotePoller {
    pub fn new(targets: Vec<RemoteTarget>, transport: Arc<dyn RemoteTransport>) -> Self {
        let poller = Self {
            state: Arc::new(RemoteTreeState::default()),
            transport,
            workers: Arc::new(RemoteWorkerCap::default()),
            targets,
            registry_fingerprint: None,
            selection: RemoteSelection::default(),
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            scheduler: Mutex::new(None),
        };
        for target in &poller.targets {
            poller.add_node(target);
        }
        poller
    }

    pub fn from_registry(
        registry: &crate::cluster::Registry,
        local_fingerprint: &str,
    ) -> io::Result<Self> {
        let targets = registry_targets(registry, local_fingerprint);
        let transport: Arc<dyn RemoteTransport> = match ClusterRemoteTransport::system() {
            Ok(transport) => Arc::new(transport),
            Err(error) => Arc::new(UnavailableRemoteTransport(format!(
                "remote transport unavailable: {error}"
            ))),
        };
        let mut poller = Self::new(targets, transport);
        poller.registry_fingerprint = Some(local_fingerprint.to_owned());
        Ok(poller)
    }

    pub fn source(&self) -> Arc<RemoteTreeState> {
        Arc::clone(&self.state)
    }

    pub fn selection(&self) -> RemoteSelection {
        self.selection.clone()
    }

    /// Start the single scheduler thread that drives bounded node workers.
    pub fn start(&self) -> io::Result<()> {
        let mut scheduler = self
            .scheduler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if scheduler.is_some() {
            return Ok(());
        }
        let mut targets = self.targets.clone();
        let registry_fingerprint = self.registry_fingerprint.clone();
        let state = Arc::clone(&self.state);
        let transport = Arc::clone(&self.transport);
        let workers = Arc::clone(&self.workers);
        let selection = self.selection.clone();
        let stop = Arc::clone(&self.stop);
        *scheduler = Some(
            std::thread::Builder::new()
                .name("cluster-remote-scheduler".into())
                .spawn(move || {
                    if targets.is_empty() && registry_fingerprint.is_none() {
                        return;
                    }
                    let mut cursor = 0;
                    let mut last_registry_refresh = Instant::now();
                    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                        if let Some(local_fingerprint) = &registry_fingerprint {
                            if last_registry_refresh.elapsed() >= Duration::from_secs(1) {
                                if let Ok(Some((_, registry))) = crate::cluster::nodes() {
                                    reconcile_registry_targets(
                                        &state,
                                        &mut targets,
                                        registry_targets(&registry, local_fingerprint),
                                    );
                                    cursor = cursor.min(targets.len().saturating_sub(1));
                                }
                                last_registry_refresh = Instant::now();
                            }
                        }
                        if targets.is_empty() {
                            std::thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                        let len = targets.len();
                        let start = cursor;
                        cursor = (cursor + 1) % len;
                        for offset in 0..len {
                            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                                break;
                            }
                            let target = targets[(start + offset) % len].clone();
                            let selected = selection.selected_session(&target.name);
                            let _ = schedule_remote_poll(
                                &state, &transport, &workers, target, selected,
                            );
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                })?,
        );
        Ok(())
    }

    pub fn add_node(&self, target: &RemoteTarget) {
        let exists = self
            .state
            .nodes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&target.registry_key);
        if !exists {
            self.state.insert_node(RemoteNode::with_key(
                target.registry_key.clone(),
                target.name.clone(),
                Instant::now(),
            ));
        }
    }

    /// Schedule one poll when backoff allows it and capacity is available.
    pub fn poll_node(
        &self,
        target: RemoteTarget,
        selected_session: Option<String>,
    ) -> io::Result<bool> {
        self.add_node(&target);
        schedule_remote_poll(
            &self.state,
            &self.transport,
            &self.workers,
            target,
            selected_session,
        )
    }
}

fn reconcile_registry_targets(
    state: &RemoteTreeState,
    current: &mut Vec<RemoteTarget>,
    refreshed: Vec<RemoteTarget>,
) {
    let refreshed_keys: HashSet<_> = refreshed
        .iter()
        .map(|target| target.registry_key.clone())
        .collect();
    let mut nodes = state
        .nodes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    nodes.retain(|key, _| refreshed_keys.contains(key));
    for target in &refreshed {
        nodes.entry(target.registry_key.clone()).or_insert_with(|| {
            RemoteNode::with_key(
                target.registry_key.clone(),
                target.name.clone(),
                Instant::now(),
            )
        });
    }
    *current = refreshed;
}

impl Drop for RemotePoller {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(scheduler) = self
            .scheduler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = scheduler.join();
        }
    }
}

fn schedule_remote_poll(
    state: &Arc<RemoteTreeState>,
    transport: &Arc<dyn RemoteTransport>,
    workers: &Arc<RemoteWorkerCap>,
    target: RemoteTarget,
    selected_session: Option<String>,
) -> io::Result<bool> {
    let Some(permit) = workers.acquire(target.registry_key.clone()) else {
        return Ok(false);
    };
    let Some(previous) = state
        .with_node(&target.registry_key, |node| {
            node.begin_poll(Instant::now()).then(|| node.sessions())
        })
        .flatten()
    else {
        return Ok(false);
    };

    let state = Arc::clone(state);
    let transport = Arc::clone(transport);
    let failed_state = Arc::clone(&state);
    let failed_name = target.registry_key.clone();
    let worker = std::thread::Builder::new()
        .name(format!("remote-sync-{}", target.name))
        .spawn(move || {
            let _permit = permit;
            let result = transport.poll_node(&target, selected_session.as_deref(), &previous);
            let now = Instant::now();
            let _ = state.with_node(&target.registry_key, |node| match result {
                Ok(sessions) => node.poll_succeeded(sessions, now),
                Err(error) => node.poll_failed_with_reason(now, Some(error.to_string())),
            });
        });
    if let Err(error) = worker {
        let now = Instant::now();
        let _ = failed_state.with_node(&failed_name, |node| {
            node.poll_failed_with_reason(now, Some(error.to_string()))
        });
        return Err(error);
    }
    Ok(true)
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
            wire_name: name.into(),
            instance_id: "instance-1".into(),
            alive: true,
            output_version: Some(4),
            screen: Some(screen(text)),
            last_error: None,
        }
    }

    #[test]
    fn remote_names_and_screen_cells_strip_terminal_injection_controls() {
        let hostile_name = "x\u{1b}]0;pwn\u{7}";
        assert_eq!(display_name(hostile_name), "x]0;pwn");

        let snapshot = to_screen_snapshot(remuda_core::protocol::StyledScreen {
            rows: vec![vec![remuda_core::protocol::StyledRun {
                text: "\u{1b}]52;c;aGk=\u{7}safe\u{009b}31m".into(),
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
            scrollback_len: 0,
            scrollback_total: 0,
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: true,
            },
        });
        let text: String = snapshot.cells[0]
            .iter()
            .map(|cell| cell.text.as_str())
            .collect();
        assert_eq!(text, "]52;c;aGk=safe31m");
    }

    #[test]
    fn selected_session_without_a_fetched_screen_uses_capture_first() {
        let listed = session("build", "");
        let listed_only = RemoteSessionSnapshot {
            output_version: Some(12),
            screen: None,
            ..listed
        };
        assert!(!should_sync(Some(&listed_only)));
        let captured = session("build", "screen");
        assert!(should_sync(Some(&captured)));
    }

    #[test]
    fn same_name_new_instance_does_not_inherit_the_old_screen() {
        let old = session("build", "old frame");
        let listed = remuda_core::registry::SessionSummary {
            id: "new-id".into(),
            name: "build".into(),
            instance_id: Some("new-instance".into()),
            output_version: Some(8),
            alive: true,
            idle: Duration::ZERO,
            output_idle: None,
            size: remuda_core::Size::new(80, 24),
            attached: false,
            human_idle: None,
            mouse_tracking: false,
        };
        assert!(previous_session(&listed, std::slice::from_ref(&old)).is_none());
        let fresh = unpolled_session_snapshot(listed, None).unwrap();
        assert!(fresh.screen.is_none());
        assert_eq!(fresh.output_version, Some(8));
        assert!(!should_sync(Some(&fresh)));
    }

    #[test]
    fn selected_sync_error_keeps_the_fresh_list_and_marks_only_that_session() {
        let prior = session("build", "last good frame");
        let listed = remuda_core::registry::SessionSummary {
            id: "session-build".into(),
            name: "build".into(),
            instance_id: Some(prior.instance_id.clone()),
            output_version: Some(5),
            alive: true,
            idle: Duration::ZERO,
            output_idle: None,
            size: remuda_core::Size::new(80, 24),
            attached: false,
            human_idle: None,
            mouse_tracking: false,
        };
        let retained =
            session_after_poll_error(listed, Some(&prior), io::Error::other("sync failed"))
                .unwrap();
        assert_eq!(retained.screen, prior.screen);
        assert_eq!(retained.output_version, Some(5));
        assert_eq!(retained.last_error.as_deref(), Some("sync failed"));
    }

    #[test]
    fn registry_targets_exclude_revoked_members() {
        let registry = crate::cluster::Registry {
            authorized_nodes: vec![
                crate::cluster::AuthorizedNode {
                    node_fp: "admitted-fingerprint".into(),
                    static_pubkey: String::new(),
                    state: crate::cluster::NodeState::Admitted,
                    version: 1,
                    by: "local".into(),
                },
                crate::cluster::AuthorizedNode {
                    node_fp: "revoked-fingerprint".into(),
                    static_pubkey: String::new(),
                    state: crate::cluster::NodeState::Revoked,
                    version: 1,
                    by: "local".into(),
                },
            ],
        };
        let targets = registry_targets(&registry, "local");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].registry_key, "admitted-fingerprint");
    }

    #[test]
    fn registry_refresh_drops_revoked_nodes_and_adds_new_members() {
        let now = Instant::now();
        let state = RemoteTreeState::default();
        let mut targets = vec![RemoteTarget {
            name: "laptop".into(),
            registry_key: "revoked-fingerprint".into(),
            addr_override: None,
        }];
        state.insert_node(RemoteNode::with_key(
            "revoked-fingerprint".into(),
            "laptop".into(),
            now,
        ));
        reconcile_registry_targets(&state, &mut targets, Vec::new());
        assert!(targets.is_empty());
        assert!(state.snapshot_at(now).nodes.is_empty());

        reconcile_registry_targets(
            &state,
            &mut targets,
            vec![RemoteTarget {
                name: "tablet".into(),
                registry_key: "new-fingerprint".into(),
                addr_override: None,
            }],
        );
        let snapshot = state.snapshot_at(now);
        assert_eq!(targets.len(), 1);
        assert_eq!(snapshot.nodes[0].name, "tablet");
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
        assert_eq!(node.snapshot_at(failed_at).state, RemoteState::Stale);
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
