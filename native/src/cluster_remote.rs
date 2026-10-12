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
/// Gap after a selected session's Sync returns before the next one (#622).
/// Sync long-polls, so a quiet session still costs one request per timeout.
const SELECTED_SYNC_SPACING: Duration = Duration::from_millis(150);
/// Server-side hold requested for a selected Sync (#684 QA): the peer answers
/// by then when quiet, but the worker itself is bounded by REMOTE_SYNC_TIMEOUT.
const SELECTED_SYNC_WAIT: Duration = POLL_INTERVAL;
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
    /// Original protocol name used for authenticated read requests.
    pub wire_name: String,
    pub instance_id: String,
    pub alive: bool,
    pub output_version: Option<u64>,
    pub bracketed_paste: bool,
    pub screen: Option<ScreenSnapshot>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteNodeSnapshot {
    /// Exact registry fingerprint used for routing and selection identity.
    pub registry_key: String,
    /// Short label retained for display and explicit user target lookup.
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

    /// Moves whenever a poll result lands or the node set changes.
    fn generation(&self) -> u64 {
        0
    }
}

/// Sends idempotent Input batches to a remote member. Callers own retry policy.
pub trait RemoteInputTransport: Send + Sync {
    fn send_input(&self, node: &str, request: &Request) -> io::Result<Response>;

    fn send_close(&self, node: &str, request: &Request) -> io::Result<Response> {
        self.send_input(node, request)
    }
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
    registry_key: String,
    session: String,
}

/// Mutable selection signal keyed by exact registry fingerprint.
#[derive(Clone, Default)]
pub struct RemoteSelection {
    selected: Arc<Mutex<Option<SelectedSession>>>,
}

impl RemoteSelection {
    pub fn select(&self, registry_key: impl Into<String>, session: impl Into<String>) {
        *self
            .selected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(SelectedSession {
            registry_key: registry_key.into(),
            session: session.into(),
        });
    }

    pub fn clear(&self) {
        *self
            .selected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    fn selected_session(&self, registry_key: &str) -> Option<String> {
        self.selected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .filter(|selected| selected.registry_key == registry_key)
            .map(|selected| selected.session.clone())
    }
}

/// Build poll targets from admitted registry entries. The shared target resolver
/// uses each entry's endpoint for routing and its pinned key for authentication.
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

/// One full poll: the fresh List, plus the selected read's failure, which is
/// carried even when no snapshot could keep it (#684 SEC).
#[derive(Debug, Default)]
pub struct RemotePoll {
    pub sessions: Vec<RemoteSessionSnapshot>,
    pub selected_error: Option<String>,
}

impl From<Vec<RemoteSessionSnapshot>> for RemotePoll {
    fn from(sessions: Vec<RemoteSessionSnapshot>) -> Self {
        Self {
            sessions,
            selected_error: None,
        }
    }
}

/// One read-only node poll. Implementations make at most one Sync call at a
/// time for this node and preserve the previous frame when nothing changed.
pub trait RemoteTransport: Send + Sync + 'static {
    fn poll_node(
        &self,
        target: &RemoteTarget,
        selected_session: Option<&str>,
        previous: &[RemoteSessionSnapshot],
    ) -> io::Result<RemotePoll>;

    /// Sync only the selected session against `previous`, without a List.
    /// `None` means a List must come first (no syncable prior, or a restart).
    fn sync_selected(
        &self,
        _target: &RemoteTarget,
        _selected_session: &str,
        _previous: &[RemoteSessionSnapshot],
    ) -> io::Result<Option<Vec<RemoteSessionSnapshot>>> {
        Ok(None)
    }
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
    next_list: Instant,
    polling: bool,
    published: NodeView,
}

type NodeView = (RemoteState, Option<Instant>, Option<String>);

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
            next_list: now,
            polling: false,
            published: (RemoteState::Unreachable, None, None),
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
        self.poll_delivered(sessions.into(), now, None);
    }

    /// Record a poll whose List succeeded. A selected session synced without
    /// error is polled again after the spacing; one whose read failed backs off
    /// like a failed poll (the fresh List is kept); anything else waits the interval.
    pub fn poll_delivered(&mut self, poll: RemotePoll, now: Instant, selected: Option<&str>) {
        let RemotePoll {
            sessions,
            selected_error,
        } = poll;
        let selected_session = selected.and_then(|name| {
            sessions
                .iter()
                .find(|session| session.name == name && session.alive)
        });
        let selected_error = selected_error
            .or_else(|| selected_session.and_then(|session| session.last_error.clone()));
        let follow = selected_session.is_some_and(|session| should_sync(Some(session)));
        self.sessions = sessions;
        self.last_sync = Some(now);
        self.polling = false;
        self.state = RemoteState::Reachable;
        if let Some(error) = selected_error {
            self.failure_count = self.failure_count.saturating_add(1);
            self.last_error = Some(error);
            self.next_poll = now + retry_backoff(self.failure_count);
            return;
        }
        self.failure_count = 0;
        self.last_error = None;
        self.next_poll = now
            + if follow {
                SELECTED_SYNC_SPACING
            } else {
                POLL_INTERVAL
            };
    }

    /// A Sync-only poll found that a List must come first: wait for the next
    /// List slot, and at least the spacing after any Sync it just sent.
    pub fn poll_needs_list(&mut self, now: Instant) {
        self.next_poll = self.next_list.max(now + SELECTED_SYNC_SPACING);
        self.polling = false;
    }

    /// Book a List if one is due, whatever the poll's outcome: Lists stay at
    /// least POLL_INTERVAL apart, failures and fallbacks included.
    pub fn take_list(&mut self, now: Instant) -> bool {
        if now < self.next_list {
            return false;
        }
        self.next_list = now + POLL_INTERVAL;
        true
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

    /// What a viewer sees, minus sessions: those change only with `last_sync`.
    fn view(&self) -> NodeView {
        (self.state, self.last_sync, self.last_error.clone())
    }

    fn snapshot_at(&self, now: Instant) -> RemoteNodeSnapshot {
        RemoteNodeSnapshot {
            registry_key: self.key.clone(),
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
    generation: std::sync::atomic::AtomicU64,
}

impl RemoteTreeState {
    pub fn insert_node(&self, node: RemoteNode) {
        self.changed();
        self.nodes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(node.key.clone(), node);
    }

    pub fn remove_node(&self, name: &str) -> Option<RemoteNode> {
        self.changed();
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

    /// Like `with_node`, but moves the generation when what a viewer sees differs
    /// from the last recorded or current live view; bookkeeping and removed nodes do not.
    fn record<R>(&self, name: &str, f: impl FnOnce(&mut RemoteNode) -> R) -> Option<R> {
        let (result, changed) = self.with_node(name, |node| {
            let before = node.view();
            let result = f(node);
            let view = node.view();
            let changed = view != node.published || view != before;
            node.published = view;
            (result, changed)
        })?;
        if changed {
            self.changed();
        }
        Some(result)
    }

    fn changed(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

    fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::SeqCst)
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

    /// Read the selected session with one request: a Sync long-poll against a
    /// captured prior, else a capture. The next Sync refreshes bracketed paste.
    fn selected_session_snapshot(
        &self,
        target: &crate::cluster::ResolvedTarget,
        session: &remuda_core::registry::SessionSummary,
        prior: Option<&RemoteSessionSnapshot>,
    ) -> io::Result<RemoteSessionSnapshot> {
        let request = match prior.filter(|prior| should_sync(Some(prior))) {
            Some(prior) => selected_sync_request(prior),
            None => Request::CaptureStyled {
                name: session.name.clone(),
                scrollback: 0,
            },
        };
        let response = self.request(target, &request)?;
        selected_snapshot(session, prior, &request, response)
    }
}

/// Map the selected read's reply. A reply of the other kind than the request
/// (Sync vs capture) is rejected as invalid.
fn selected_snapshot(
    session: &remuda_core::registry::SessionSummary,
    prior: Option<&RemoteSessionSnapshot>,
    request: &Request,
    response: Response,
) -> io::Result<RemoteSessionSnapshot> {
    let synced = matches!(request, Request::Sync { .. });
    match response {
        Response::Sync {
            instance_id,
            output_version,
            snapshot,
        } if synced => Ok(RemoteSessionSnapshot {
            name: display_name(&session.name),
            wire_name: session.name.clone(),
            instance_id,
            alive: true,
            output_version: Some(output_version),
            bracketed_paste: snapshot.bracketed_paste,
            screen: Some(to_screen_snapshot(snapshot)),
            last_error: None,
        }),
        Response::StyledScreen {
            rows,
            instance_id,
            output_version,
            wrapped,
            scrollback_len,
            scrollback_total,
            cursor,
        } if !synced => {
            let instance_id = instance_id
                .or_else(|| listed_instance_id(session))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "remote screen has no instance")
                })?;
            let bracketed_paste = prior.is_some_and(|prior| prior.bracketed_paste);
            Ok(RemoteSessionSnapshot {
                name: display_name(&session.name),
                wire_name: session.name.clone(),
                instance_id,
                alive: true,
                output_version,
                bracketed_paste,
                screen: Some(to_screen_snapshot(remuda_core::protocol::StyledScreen {
                    rows,
                    bracketed_paste,
                    wrapped,
                    scrollback_len,
                    scrollback_total,
                    cursor,
                })),
                last_error: None,
            })
        }
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

impl RemoteTransport for ClusterRemoteTransport {
    fn poll_node(
        &self,
        target: &RemoteTarget,
        selected_session: Option<&str>,
        previous: &[RemoteSessionSnapshot],
    ) -> io::Result<RemotePoll> {
        let resolved = (self.resolve)(target)?;
        let sessions = self.list_sessions(&resolved)?;
        Ok(poll_listed_sessions(
            sessions,
            selected_session,
            previous,
            |session, prior| self.selected_session_snapshot(&resolved, session, prior),
        ))
    }

    /// One Sync for the selected session. Overload, errors and timeouts fail
    /// the poll (backoff); a restart or a missing prior asks for a List.
    fn sync_selected(
        &self,
        target: &RemoteTarget,
        selected_session: &str,
        previous: &[RemoteSessionSnapshot],
    ) -> io::Result<Option<Vec<RemoteSessionSnapshot>>> {
        let Some(prior) = previous
            .iter()
            .find(|old| old.alive && old.name == selected_session && should_sync(Some(old)))
        else {
            return Ok(None);
        };
        let resolved = (self.resolve)(target)?;
        let response = self.request(&resolved, &selected_sync_request(prior))?;
        Ok(synced_snapshot(prior, response)?.map(|updated| {
            previous
                .iter()
                .map(|old| {
                    if old == prior {
                        updated.clone()
                    } else {
                        old.clone()
                    }
                })
                .collect()
        }))
    }
}

/// Snapshots for one List. Only the first live session shown under the
/// selected name is read, so a List costs at most one selected Sync.
fn poll_listed_sessions(
    sessions: Vec<remuda_core::registry::SessionSummary>,
    selected_session: Option<&str>,
    previous: &[RemoteSessionSnapshot],
    mut read: impl FnMut(
        &remuda_core::registry::SessionSummary,
        Option<&RemoteSessionSnapshot>,
    ) -> io::Result<RemoteSessionSnapshot>,
) -> RemotePoll {
    let mut selected_error = None;
    let mut selected_read = false;
    let mut snapshots = Vec::with_capacity(sessions.len());
    for session in sessions {
        let prior = previous_session(&session, previous);
        let selected = session.alive
            && !selected_read
            && selected_session == Some(display_name(&session.name).as_str());
        if !selected {
            snapshots.extend(unpolled_session_snapshot(session, prior));
            continue;
        }
        selected_read = true;
        match read(&session, prior) {
            Ok(snapshot) => snapshots.push(snapshot),
            Err(error) => {
                selected_error = Some(error.to_string());
                snapshots.extend(session_after_poll_error(session, prior, error));
            }
        }
    }
    RemotePoll {
        sessions: snapshots,
        selected_error,
    }
}

fn selected_sync_request(prior: &RemoteSessionSnapshot) -> Request {
    Request::Sync {
        name: prior.wire_name.clone(),
        instance_id: Some(prior.instance_id.clone()),
        since: prior.output_version.unwrap_or_default(),
        timeout_ms: SELECTED_SYNC_WAIT.as_millis() as u64,
    }
}

/// Map a selected-session Sync reply: `None` asks for a List (restart).
fn synced_snapshot(
    prior: &RemoteSessionSnapshot,
    response: Response,
) -> io::Result<Option<RemoteSessionSnapshot>> {
    match response {
        Response::Sync {
            instance_id,
            output_version,
            snapshot,
        } if instance_id == prior.instance_id => Ok(Some(RemoteSessionSnapshot {
            output_version: Some(output_version),
            bracketed_paste: snapshot.bracketed_paste,
            screen: Some(to_screen_snapshot(snapshot)),
            last_error: None,
            ..prior.clone()
        })),
        Response::Sync { .. } | Response::WrongInstance => Ok(None),
        Response::SyncAtCapacity => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "remote Sync is at capacity",
        )),
        Response::Error(error) => Err(io::Error::other(error)),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected remote Sync response: {other:?}"),
        )),
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
        bracketed_paste: prior.is_some_and(|old| old.bracketed_paste),
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
    ) -> io::Result<RemotePoll> {
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
                            let selected = selection.selected_session(&target.registry_key);
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
    let before = nodes.len();
    nodes.retain(|key, _| refreshed_keys.contains(key));
    let mut changed = nodes.len() != before;
    for target in &refreshed {
        nodes.entry(target.registry_key.clone()).or_insert_with(|| {
            changed = true;
            RemoteNode::with_key(
                target.registry_key.clone(),
                target.name.clone(),
                Instant::now(),
            )
        });
    }
    // The scheduler reconciles every second; only a real change wakes viewers.
    if changed {
        state.changed();
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
    let Some((previous, listed)) = state
        .with_node(&target.registry_key, |node| {
            let now = Instant::now();
            if !node.begin_poll(now) {
                return None;
            }
            let listed = node.take_list(now);
            if !listed && selected_session.is_none() {
                // Nothing selected to Sync and no List due: send nothing.
                node.poll_needs_list(now);
                return None;
            }
            Some((node.sessions(), listed))
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
            let result = match selected_session.as_deref() {
                Some(selected) if !listed => transport
                    .sync_selected(&target, selected, &previous)
                    .map(|sessions| sessions.map(RemotePoll::from)),
                selected => transport.poll_node(&target, selected, &previous).map(Some),
            };
            let now = Instant::now();
            let _ = state.record(&target.registry_key, |node| match result {
                Ok(Some(poll)) => node.poll_delivered(poll, now, selected_session.as_deref()),
                Ok(None) => node.poll_needs_list(now),
                Err(error) => node.poll_failed_with_reason(now, Some(error.to_string())),
            });
        });
    if let Err(error) = worker {
        let now = Instant::now();
        let _ = failed_state.record(&failed_name, |node| {
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
    use std::collections::HashMap;
    use std::sync::Condvar;

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
            bracketed_paste: false,
            screen: Some(screen(text)),
            last_error: None,
        }
    }

    #[test]
    fn remote_names_and_screen_cells_strip_terminal_injection_controls() {
        let hostile_name = "x\u{1b}]0;pwn\u{7}\u{202e}";
        assert_eq!(display_name(hostile_name), "x]0;pwn");

        let snapshot = to_screen_snapshot(remuda_core::protocol::StyledScreen {
            rows: vec![vec![remuda_core::protocol::StyledRun {
                text: "\u{1b}]52;c;aGk=\u{7}safe\u{009b}31m\u{202e}".into(),
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
            bracketed_paste: false,
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
    fn older_screen_snapshot_defaults_bracketed_paste_to_disabled() {
        let snapshot: remuda_core::protocol::StyledScreen =
            serde_json::from_str(r#"{"rows":[],"cursor":{"row":0,"col":0,"visible":true}}"#)
                .unwrap();

        assert!(!snapshot.bracketed_paste);
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
                    format_major: 1,
                    format_minor: 0,
                    endpoint: None,
                    delivered_by: None,
                    optional_fields: Default::default(),
                    state: crate::cluster::NodeState::Admitted,
                    version: 1,
                    by: "local".into(),
                },
                crate::cluster::AuthorizedNode {
                    node_fp: "revoked-fingerprint".into(),
                    static_pubkey: String::new(),
                    format_major: 1,
                    format_minor: 0,
                    endpoint: None,
                    delivered_by: None,
                    optional_fields: Default::default(),
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
    fn polling_selection_is_scoped_by_fingerprint_when_labels_collide() {
        struct RecordingTransport {
            selected: Mutex<HashMap<String, Option<String>>>,
            changed: Condvar,
        }

        impl RemoteTransport for RecordingTransport {
            fn poll_node(
                &self,
                target: &RemoteTarget,
                selected_session: Option<&str>,
                _previous: &[RemoteSessionSnapshot],
            ) -> io::Result<RemotePoll> {
                self.selected.lock().unwrap().insert(
                    target.registry_key.clone(),
                    selected_session.map(str::to_owned),
                );
                self.changed.notify_all();
                Ok(RemotePoll::default())
            }
        }

        let transport = Arc::new(RecordingTransport {
            selected: Mutex::new(HashMap::new()),
            changed: Condvar::new(),
        });
        let targets = ["fp-a", "fp-b"]
            .into_iter()
            .map(|registry_key| RemoteTarget {
                name: "laptop".into(),
                registry_key: registry_key.into(),
                addr_override: None,
            })
            .collect();
        let poller = RemotePoller::new(targets, transport.clone());
        let selection = poller.selection();
        selection.select("fp-a", "build");
        poller.start().unwrap();

        let selected = transport.selected.lock().unwrap();
        let (selected, timeout) = transport
            .changed
            .wait_timeout_while(selected, Duration::from_secs(2), |selected| {
                selected.len() < 2
            })
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "poll workers did not visit both nodes"
        );
        assert_eq!(
            selected.get("fp-a").and_then(Option::as_deref),
            Some("build")
        );
        assert_eq!(selected.get("fp-b"), Some(&None));
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

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Call {
        List,
        Sync,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SyncMode {
        /// Wait for a newer version, as a Sync long-poll does.
        LongPoll,
        /// Every Sync finds new output, as continuous output would.
        Busy,
        /// Ask for a List every time (a restart, or no syncable prior yet).
        NeedsList,
        /// The peer is overloaded.
        AtCapacity,
    }

    struct FakePeer {
        mode: SyncMode,
        version: Mutex<u64>,
        changed: Condvar,
        calls: Mutex<Vec<(Call, Instant)>>,
    }

    impl FakePeer {
        fn new(mode: SyncMode) -> Arc<Self> {
            Arc::new(Self {
                mode,
                version: Mutex::new(0),
                changed: Condvar::new(),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn bump(&self) {
            *self.version.lock().unwrap() += 1;
            self.changed.notify_all();
        }

        fn screen(version: u64) -> Vec<RemoteSessionSnapshot> {
            let mut snapshot = session("proof", &format!("v{version}"));
            snapshot.output_version = Some(version);
            vec![snapshot]
        }

        fn times(&self, kind: Call) -> Vec<Instant> {
            let calls = self.calls.lock().unwrap();
            calls
                .iter()
                .filter(|(call, _)| *call == kind)
                .map(|(_, at)| *at)
                .collect()
        }
    }

    impl RemoteTransport for FakePeer {
        fn poll_node(
            &self,
            _target: &RemoteTarget,
            _selected_session: Option<&str>,
            _previous: &[RemoteSessionSnapshot],
        ) -> io::Result<RemotePoll> {
            self.calls
                .lock()
                .unwrap()
                .push((Call::List, Instant::now()));
            Ok(Self::screen(*self.version.lock().unwrap()).into())
        }

        fn sync_selected(
            &self,
            _target: &RemoteTarget,
            _selected_session: &str,
            previous: &[RemoteSessionSnapshot],
        ) -> io::Result<Option<Vec<RemoteSessionSnapshot>>> {
            self.calls
                .lock()
                .unwrap()
                .push((Call::Sync, Instant::now()));
            let prior = previous
                .iter()
                .find_map(|old| old.output_version)
                .unwrap_or(0);
            let mut version = self.version.lock().unwrap();
            match self.mode {
                SyncMode::NeedsList => return Ok(None),
                SyncMode::AtCapacity => return Err(io::ErrorKind::WouldBlock.into()),
                SyncMode::Busy => *version += 1,
                SyncMode::LongPoll => {
                    version = self
                        .changed
                        .wait_timeout_while(version, Duration::from_secs(5), |now| *now <= prior)
                        .unwrap()
                        .0;
                }
            }
            Ok(Some(Self::screen(*version)))
        }
    }

    fn start_selected(peer: Arc<FakePeer>) -> RemotePoller {
        let target = RemoteTarget {
            name: "laptop".into(),
            registry_key: "fp-a".into(),
            addr_override: None,
        };
        let poller = RemotePoller::new(vec![target], peer);
        poller.selection().select("fp-a", "proof");
        poller.start().unwrap();
        poller
    }

    fn shown_version(poller: &RemotePoller) -> Option<u64> {
        poller
            .source()
            .snapshot()
            .nodes
            .first()?
            .sessions
            .first()?
            .output_version
    }

    fn wait_for_version(poller: &RemotePoller, at_least: u64, limit: Duration) -> Duration {
        let start = Instant::now();
        while shown_version(poller).is_none_or(|version| version < at_least) {
            assert!(
                start.elapsed() < limit,
                "version {at_least} not shown in {limit:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        start.elapsed()
    }

    fn gaps(times: &[Instant]) -> Vec<Duration> {
        times.windows(2).map(|pair| pair[1] - pair[0]).collect()
    }

    #[test]
    fn a_change_soon_after_a_delivery_reaches_the_selected_session_at_once() {
        let peer = FakePeer::new(SyncMode::LongPoll);
        let poller = start_selected(Arc::clone(&peer));
        wait_for_version(&poller, 0, Duration::from_secs(5));
        // Before #622 the next poll waited out POLL_INTERVAL: a change 0.5s later took ~1.5s.
        std::thread::sleep(Duration::from_millis(500));
        peer.bump();
        let latency = wait_for_version(&poller, 1, Duration::from_secs(5));
        assert!(
            latency < Duration::from_millis(400),
            "delivered after {latency:?}"
        );
    }

    #[test]
    fn continuous_output_keeps_syncs_at_least_the_spacing_apart_and_lists_at_the_interval() {
        let peer = FakePeer::new(SyncMode::Busy);
        let poller = start_selected(Arc::clone(&peer));
        wait_for_version(&poller, 1, Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(1500));
        drop(poller);
        let syncs = peer.times(Call::Sync);
        let sync_gaps = gaps(&syncs);
        assert!(
            sync_gaps.iter().all(|gap| *gap >= SELECTED_SYNC_SPACING),
            "{sync_gaps:?}"
        );
        // ~1.5s at >= 150ms apart is at most ~11 Syncs; before #622 there were none.
        assert!(
            (4..=12).contains(&syncs.len()),
            "{} syncs: {sync_gaps:?}",
            syncs.len()
        );
        assert!(peer.times(Call::List).len() <= 2);
    }

    #[test]
    fn a_sync_that_needs_a_list_waits_for_the_list_interval() {
        // Covers a restart and a newly selected session without a captured screen.
        let peer = FakePeer::new(SyncMode::NeedsList);
        let poller = start_selected(Arc::clone(&peer));
        std::thread::sleep(Duration::from_millis(4500));
        drop(poller);
        let lists = peer.times(Call::List);
        let list_gaps = gaps(&lists);
        assert!((2..=3).contains(&lists.len()), "{list_gaps:?}");
        assert!(
            list_gaps.iter().all(|gap| *gap >= POLL_INTERVAL),
            "{list_gaps:?}"
        );
        let syncs = peer.times(Call::Sync);
        assert!(gaps(&syncs).iter().all(|gap| *gap >= SELECTED_SYNC_SPACING));
        assert!(
            syncs.len() <= lists.len(),
            "one Sync per List slot at most: {syncs:?}"
        );
    }

    #[test]
    fn an_overloaded_peer_gets_backoff_not_the_spacing() {
        let peer = FakePeer::new(SyncMode::AtCapacity);
        let poller = start_selected(Arc::clone(&peer));
        std::thread::sleep(Duration::from_millis(3500));
        drop(poller);
        let syncs = peer.times(Call::Sync);
        let sync_gaps = gaps(&syncs);
        assert!(syncs.len() <= 3, "{} syncs: {sync_gaps:?}", syncs.len());
        assert!(
            sync_gaps.iter().all(|gap| *gap >= BACKOFF_BASE),
            "{sync_gaps:?}"
        );
        assert!(gaps(&peer.times(Call::List))
            .iter()
            .all(|gap| *gap >= POLL_INTERVAL));
    }

    #[test]
    fn delivery_spacing_list_booking_and_failures_follow_the_policy() {
        let start = Instant::now();
        let mut node = RemoteNode::new("laptop", start);
        assert!(node.begin_poll(start) && node.take_list(start));
        node.poll_delivered(vec![session("proof", "a")].into(), start, Some("proof"));
        assert_eq!(node.retry_at(), start + SELECTED_SYNC_SPACING);
        assert!(!node.take_list(node.retry_at()));

        // A failure is backed off and does not bring the next List forward.
        let failed_at = start + Duration::from_millis(300);
        assert!(node.begin_poll(failed_at));
        node.poll_failed(failed_at);
        assert_eq!(node.retry_at(), failed_at + BACKOFF_BASE);
        assert!(!node.take_list(node.retry_at()));
        assert!(node.take_list(start + POLL_INTERVAL));

        // A Sync that needs a List waits for the next List slot.
        let mut needs = RemoteNode::new("desk", start);
        assert!(needs.begin_poll(start) && needs.take_list(start));
        needs.poll_needs_list(start + Duration::from_millis(100));
        assert_eq!(needs.retry_at(), start + POLL_INTERVAL);

        // Unselected or uncaptured selected sessions keep the interval.
        let mut uncaptured = session("proof", "a");
        uncaptured.screen = None;
        for (sessions, selected) in [
            (vec![session("proof", "a")], None),
            (vec![uncaptured], Some("proof")),
        ] {
            let mut node = RemoteNode::new("tower", start);
            assert!(node.begin_poll(start));
            node.poll_delivered(sessions.into(), start, selected);
            assert_eq!(node.retry_at(), start + POLL_INTERVAL);
        }
    }

    #[test]
    fn a_selected_sync_holds_the_worker_no_longer_than_the_list_interval() {
        let Request::Sync {
            name,
            instance_id,
            since,
            timeout_ms,
        } = selected_sync_request(&session("proof", "a"))
        else {
            panic!("selected sessions are read with Sync");
        };
        assert_eq!(
            (name.as_str(), instance_id.as_deref(), since),
            ("proof", Some("instance-1"), 4)
        );
        assert!(
            Duration::from_millis(timeout_ms) <= POLL_INTERVAL,
            "{timeout_ms} ms"
        );
        // The client's read timeout must outlast the server-side hold.
        assert!(SELECTED_SYNC_WAIT < REMOTE_SYNC_TIMEOUT);
    }

    #[test]
    fn a_full_poll_whose_selected_read_failed_backs_off_and_keeps_the_list() {
        let start = Instant::now();
        let mut node = RemoteNode::new("laptop", start);
        let mut errored = session("proof", "a");
        errored.last_error = Some("remote Sync is at capacity".into());
        let mut now = start;
        for backoff in [1, 2, 4, 8] {
            assert!(node.begin_poll(now));
            node.poll_delivered(
                vec![errored.clone(), session("other", "b")].into(),
                now,
                Some("proof"),
            );
            assert_eq!(node.retry_at(), now + Duration::from_secs(backoff));
            now = node.retry_at();
        }
        let snapshot = node.snapshot_at(now);
        assert_eq!(
            snapshot.state,
            RemoteState::Reachable,
            "the List itself worked"
        );
        assert_eq!(snapshot.sessions.len(), 2);
        assert_eq!(
            snapshot.last_error.as_deref(),
            Some("remote Sync is at capacity")
        );

        // The next clean read resets the backoff.
        assert!(node.begin_poll(now));
        node.poll_delivered(vec![session("proof", "a")].into(), now, Some("proof"));
        assert_eq!(node.retry_at(), now + SELECTED_SYNC_SPACING);
    }

    #[test]
    fn a_failed_selected_read_backs_off_even_without_an_identity_to_keep() {
        // An identity-less summary cannot be kept as a snapshot after a failed
        // capture; the failure must still back off, up to the 30 s cap.
        let mut nameless = summary("proof", "");
        nameless.instance_id = None;
        let start = Instant::now();
        let mut node = RemoteNode::new("laptop", start);
        let mut now = start;
        for backoff in [1, 2, 4, 8, 16, 30, 30] {
            let poll = poll_listed_sessions(
                vec![nameless.clone(), summary("other", "2")],
                Some("proof"),
                &[],
                |_, _| Err(io::Error::other("capture failed")),
            );
            assert_eq!(poll.sessions.len(), 1, "only the identified session stays");
            assert!(node.begin_poll(now));
            node.poll_delivered(poll, now, Some("proof"));
            assert_eq!(node.retry_at(), now + Duration::from_secs(backoff));
            now = node.retry_at();
        }
        assert_eq!(
            node.snapshot_at(now).last_error.as_deref(),
            Some("capture failed")
        );
    }

    #[test]
    fn one_list_reads_at_most_one_selected_session() {
        let listed = summary;
        // Two entries that show as "proof", one only after control stripping.
        let sessions = vec![
            listed("proof", "1"),
            listed("pro\u{1b}of", "2"),
            listed("proof", "3"),
            listed("other", "4"),
        ];
        let mut reads = Vec::new();
        let snapshots = poll_listed_sessions(sessions, Some("proof"), &[], |session, _| {
            reads.push(session.id.clone());
            Ok(session_named(&display_name(&session.name), &session.id))
        });
        assert_eq!(reads, ["1"]);
        assert_eq!(snapshots.sessions.len(), 4);
    }

    fn summary(name: &str, id: &str) -> remuda_core::registry::SessionSummary {
        remuda_core::registry::SessionSummary {
            id: id.into(),
            name: name.into(),
            instance_id: None,
            output_version: None,
            alive: true,
            idle: Duration::ZERO,
            output_idle: None,
            size: remuda_core::Size::new(80, 24),
            attached: false,
            human_idle: None,
            mouse_tracking: false,
        }
    }

    fn session_named(name: &str, instance: &str) -> RemoteSessionSnapshot {
        RemoteSessionSnapshot {
            instance_id: instance.into(),
            ..session(name, "read")
        }
    }

    #[test]
    fn deselecting_sends_nothing_until_the_list_is_due() {
        let peer = FakePeer::new(SyncMode::Busy);
        let poller = start_selected(Arc::clone(&peer));
        wait_for_version(&poller, 2, Duration::from_secs(5));
        poller.selection().clear();
        let cleared = Instant::now();
        std::thread::sleep(Duration::from_millis(2600));
        drop(poller);
        let lists = peer.times(Call::List);
        let list_gaps = gaps(&lists);
        assert!(
            list_gaps.iter().all(|gap| *gap >= POLL_INTERVAL),
            "{list_gaps:?}"
        );
        let late_syncs = peer
            .times(Call::Sync)
            .into_iter()
            .filter(|at| *at > cleared + Duration::from_millis(50))
            .count();
        assert_eq!(late_syncs, 0, "no Sync once nothing is selected");
    }

    #[test]
    fn a_selected_reply_of_the_other_kind_is_rejected() {
        let prior = session("proof", "a");
        let listed = summary("proof", "instance-1");
        let cursor = Cursor {
            row: 0,
            col: 0,
            visible: true,
        };
        let sync = Response::Sync {
            instance_id: "instance-1".into(),
            output_version: 9,
            snapshot: remuda_core::protocol::StyledScreen {
                rows: Vec::new(),
                bracketed_paste: false,
                wrapped: Vec::new(),
                scrollback_len: 0,
                scrollback_total: 0,
                cursor,
            },
        };
        let screen = Response::StyledScreen {
            rows: Vec::new(),
            instance_id: Some("instance-1".into()),
            output_version: Some(9),
            wrapped: Vec::new(),
            scrollback_len: 0,
            scrollback_total: 0,
            cursor,
        };
        let sync_request = selected_sync_request(&prior);
        let capture = Request::CaptureStyled {
            name: "proof".into(),
            scrollback: 0,
        };
        assert!(selected_snapshot(&listed, Some(&prior), &sync_request, sync.clone()).is_ok());
        assert!(selected_snapshot(&listed, None, &capture, screen.clone()).is_ok());
        for (request, response) in [(&sync_request, screen), (&capture, sync)] {
            let error = selected_snapshot(&listed, Some(&prior), request, response).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn sync_replies_map_to_delivery_list_or_failure() {
        let prior = session("proof", "a");
        let styled = |instance_id: &str| Response::Sync {
            instance_id: instance_id.into(),
            output_version: 9,
            snapshot: remuda_core::protocol::StyledScreen {
                rows: Vec::new(),
                bracketed_paste: true,
                wrapped: Vec::new(),
                scrollback_len: 0,
                scrollback_total: 0,
                cursor: Cursor {
                    row: 0,
                    col: 0,
                    visible: true,
                },
            },
        };
        let delivered = synced_snapshot(&prior, styled("instance-1"))
            .unwrap()
            .unwrap();
        assert_eq!(delivered.output_version, Some(9));
        assert!(delivered.bracketed_paste);
        assert!(synced_snapshot(&prior, styled("instance-2"))
            .unwrap()
            .is_none());
        assert!(synced_snapshot(&prior, Response::WrongInstance)
            .unwrap()
            .is_none());
        let overloaded = synced_snapshot(&prior, Response::SyncAtCapacity).unwrap_err();
        assert_eq!(overloaded.kind(), io::ErrorKind::WouldBlock);
        assert!(synced_snapshot(&prior, Response::Error("boom".into())).is_err());
        assert!(synced_snapshot(&prior, Response::Ok).is_err());
    }

    #[test]
    fn a_worker_error_backs_off_and_completion_wakes_the_retry_label() {
        struct Down;

        impl RemoteTransport for Down {
            fn poll_node(
                &self,
                _target: &RemoteTarget,
                _selected_session: Option<&str>,
                _previous: &[RemoteSessionSnapshot],
            ) -> io::Result<RemotePoll> {
                Err(io::Error::other("connection refused"))
            }
        }

        let target = RemoteTarget {
            name: "laptop".into(),
            registry_key: "fp-a".into(),
            addr_override: None,
        };
        let state = Arc::new(RemoteTreeState::default());
        let transport: Arc<dyn RemoteTransport> = Arc::new(Down);
        let workers = Arc::new(RemoteWorkerCap::default());
        state.insert_node(RemoteNode::with_key(
            "fp-a".into(),
            "laptop".into(),
            Instant::now(),
        ));
        let poll_once = || {
            let due = Instant::now();
            state.with_node("fp-a", |node| {
                node.next_poll = due;
                node.next_list = due;
            });
            let started = Instant::now();
            assert!(
                schedule_remote_poll(&state, &transport, &workers, target.clone(), None).unwrap()
            );
            // The permit is released only after the worker recorded its result.
            while workers.acquire("fp-a").is_none() {
                assert!(started.elapsed() < Duration::from_secs(2), "worker hung");
                std::thread::sleep(Duration::from_millis(5));
            }
            state
                .with_node("fp-a", |node| {
                    (node.failure_count, node.retry_at() - started)
                })
                .unwrap()
        };

        let before = state.generation();
        let (failures, backoff) = poll_once();
        assert_eq!(failures, 1);
        assert!(backoff >= BACKOFF_BASE, "{backoff:?}");
        let failed = state.generation();
        assert!(failed > before, "the first failure is shown");
        assert_eq!(
            state.snapshot_at(Instant::now()).nodes[0]
                .last_error
                .as_deref(),
            Some("connection refused")
        );

        let (failures, backoff) = poll_once();
        assert_eq!(failures, 2);
        assert!(backoff >= 2 * BACKOFF_BASE, "{backoff:?}");
        assert!(
            state.generation() > failed,
            "completion replaces the retry label"
        );
    }

    #[test]
    fn recording_bookkeeping_repeated_views_and_absent_nodes_does_not_wake() {
        let state = RemoteTreeState::default();
        let now = Instant::now();
        state.insert_node(RemoteNode::new("laptop", now));
        state.record("laptop", |node| {
            node.poll_failed_with_reason(now, Some("offline".into()))
        });
        let seen = state.generation();
        state.record("laptop", |node| {
            node.poll_failed_with_reason(now, Some("offline".into()))
        });
        state.record("laptop", |node| node.next_poll = now);
        assert_eq!(
            state.record("absent", |_| panic!("absent callback ran")),
            None::<()>
        );
        assert_eq!(state.generation(), seen);
    }

    #[test]
    fn an_observed_retry_label_is_woken_by_an_identical_failure() {
        let state = RemoteTreeState::default();
        let start = Instant::now();
        state.insert_node(RemoteNode::new("laptop", start));
        state.record("laptop", |node| {
            node.poll_failed_with_reason(start, Some("offline".into()))
        });
        let retry = state.with_node("laptop", |node| node.retry_at()).unwrap();
        state.with_node("laptop", |node| assert!(node.begin_poll(retry)));
        assert_eq!(
            state.snapshot_at(retry).nodes[0].state,
            RemoteState::Reconnecting
        );
        let seen = state.generation();
        state.record("laptop", |node| {
            node.poll_failed_with_reason(retry, Some("offline".into()))
        });
        assert_eq!(
            state.snapshot_at(retry).nodes[0].state,
            RemoteState::Unreachable
        );
        assert!(
            state.generation() > seen,
            "completion must wake the observed label"
        );
    }

    #[test]
    fn generation_moves_when_a_poll_lands_and_not_while_waiting_for_the_next() {
        struct Once;

        impl RemoteTransport for Once {
            fn poll_node(
                &self,
                _target: &RemoteTarget,
                _selected_session: Option<&str>,
                _previous: &[RemoteSessionSnapshot],
            ) -> io::Result<RemotePoll> {
                Ok(vec![session("proof", "a")].into())
            }
        }

        let target = RemoteTarget {
            name: "laptop".into(),
            registry_key: "fp-a".into(),
            addr_override: None,
        };
        let poller = RemotePoller::new(vec![target], Arc::new(Once));
        let source = poller.source();
        let before = source.generation();
        poller.start().unwrap();
        let start = Instant::now();
        while source.snapshot().nodes[0].sessions.is_empty() {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "first poll never landed"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let landed = source.generation();
        assert!(landed > before);
        // The scheduler ticks every 100ms; the next poll is 2s away.
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(source.generation(), landed);
    }

    #[test]
    fn registry_reconcile_moves_the_generation_only_when_nodes_change() {
        let target = |key: &str| RemoteTarget {
            name: "laptop".into(),
            registry_key: key.into(),
            addr_override: None,
        };
        let state = RemoteTreeState::default();
        let mut targets = Vec::new();
        reconcile_registry_targets(&state, &mut targets, vec![target("fp-a")]);
        let added = state.generation();
        assert!(added > 0);
        reconcile_registry_targets(&state, &mut targets, vec![target("fp-a")]);
        assert_eq!(
            state.generation(),
            added,
            "an unchanged registry is not a change"
        );
        reconcile_registry_targets(&state, &mut targets, vec![target("fp-a"), target("fp-b")]);
        let grown = state.generation();
        assert!(grown > added);
        reconcile_registry_targets(&state, &mut targets, vec![target("fp-b")]);
        assert!(state.generation() > grown, "a revoked node is a change");
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
