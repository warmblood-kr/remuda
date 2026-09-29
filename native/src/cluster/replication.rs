//! Bounded asynchronous registry replication over authenticated cluster peers.

use super::{identity, registry, AuthorizedNode, NodeState, Registry};
use crate::net::cluster_client::ClusterClient;
use remuda_core::protocol::{Request, Response};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const WORKERS: usize = 4;
const MAX_FETCH_PAGES: usize =
    registry::MAX_REGISTRY_ENTRIES.div_ceil(crate::net::REGISTRY_REPLICATION_PAGE_ENTRIES) + 8;
const REPLICATION_OPERATION_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Default)]
struct WorkState {
    ready: VecDeque<String>,
    pending: HashMap<String, bool>,
    active: HashSet<String>,
}

struct WorkPool {
    state: Mutex<WorkState>,
    wake: Condvar,
}

struct PeerMaterial {
    endpoint: SocketAddr,
    peer_key: Vec<u8>,
    local_private: Zeroizing<Vec<u8>>,
    registry: Registry,
}

static POOL: OnceLock<Arc<WorkPool>> = OnceLock::new();
static CHANGE_PENDING: AtomicBool = AtomicBool::new(false);
static CHANGE_SCHEDULER_RUNNING: AtomicBool = AtomicBool::new(false);

/// Queue replication to all configured peers. At most one operation runs and
/// one newest operation waits for each peer, even during a burst of changes.
pub fn registry_changed() {
    CHANGE_PENDING.store(true, Ordering::Release);
    if CHANGE_SCHEDULER_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
        && thread::Builder::new()
            .name("remuda-cluster-replication-queue".into())
            .spawn(schedule_changes)
            .is_err()
    {
        CHANGE_SCHEDULER_RUNNING.store(false, Ordering::Release);
    }
}

fn schedule_changes() {
    loop {
        CHANGE_PENDING.store(false, Ordering::Release);
        enqueue_all(true);
        CHANGE_SCHEDULER_RUNNING.store(false, Ordering::Release);
        if !CHANGE_PENDING.swap(false, Ordering::AcqRel)
            || CHANGE_SCHEDULER_RUNNING
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
    }
}

/// Schedule initial fetches without making listener or daemon startup wait.
pub fn startup_sync() {
    enqueue_all(false);
}

fn enqueue_all(push: bool) {
    let Ok(Some((identity, registry))) = super::nodes() else {
        return;
    };
    let peers = registry
        .authorized_nodes
        .iter()
        .filter(|node| {
            node.node_fp != identity.node_fp
                && node.state == NodeState::Admitted
                && node.endpoint.is_some()
        })
        .map(|node| node.node_fp.clone())
        .collect::<Vec<_>>();
    if peers.is_empty() {
        return;
    }
    let pool = POOL.get_or_init(WorkPool::start).clone();
    let mut state = pool
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for peer in peers {
        queue_peer(&mut state, peer, push);
    }
    pool.wake.notify_all();
}

fn queue_peer(state: &mut WorkState, peer: String, push: bool) {
    if let Some(pending_push) = state.pending.get_mut(&peer) {
        *pending_push |= push;
    } else {
        state.pending.insert(peer.clone(), push);
        if !state.active.contains(&peer) {
            state.ready.push_back(peer);
        }
    }
}

impl WorkPool {
    fn start() -> Arc<Self> {
        let pool = Arc::new(Self {
            state: Mutex::new(WorkState::default()),
            wake: Condvar::new(),
        });
        for index in 0..WORKERS {
            let worker_pool = pool.clone();
            let _ = thread::Builder::new()
                .name(format!("remuda-cluster-replication-{index}"))
                .spawn(move || worker_loop(worker_pool));
        }
        pool
    }
}

fn worker_loop(pool: Arc<WorkPool>) {
    loop {
        let peer = {
            let mut state = pool
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            loop {
                if let Some(peer) = state.ready.pop_front() {
                    if let Some(push) = state.pending.remove(&peer) {
                        state.active.insert(peer.clone());
                        break (peer, push);
                    }
                    continue;
                }
                state = pool
                    .wake
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };
        if let Err(error) = replicate_peer(&peer.0, peer.1) {
            eprintln!(
                "remuda: cluster replication with {} failed: {error}",
                peer.0
            );
        }
        let mut state = pool
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active.remove(&peer.0);
        if state.pending.contains_key(&peer.0) {
            state.ready.push_back(peer.0);
            pool.wake.notify_one();
        }
    }
}

fn peer_material(peer_fp: &str) -> io::Result<PeerMaterial> {
    let (_, registry) = super::nodes()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cluster is not initialized"))?;
    let peer = registry
        .authorized_nodes
        .iter()
        .find(|node| node.node_fp == peer_fp && node.state == NodeState::Admitted)
        .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "peer is not admitted"))?;
    let endpoint = peer.endpoint.as_deref().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "peer has no configured endpoint")
    })?;
    let endpoint = endpoint
        .parse::<SocketAddr>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "peer endpoint is invalid"))?;
    let public = super::encoding::decode_base64(&peer.static_pubkey)?;
    let private = identity::load_static_private_key()?;
    Ok(PeerMaterial {
        endpoint,
        peer_key: public,
        local_private: private,
        registry,
    })
}

fn replicate_peer(peer_fp: &str, push: bool) -> io::Result<()> {
    let deadline = Instant::now() + REPLICATION_OPERATION_TIMEOUT;
    let material = peer_material(peer_fp)?;
    let endpoint = material.endpoint;
    let peer_key = material.peer_key;
    let local_private = material.local_private;
    let local_registry = material.registry;
    if push {
        let (identity, _) = super::nodes()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cluster is not initialized"))?;
        let entries = snapshot_for_wire(&local_registry.authorized_nodes);
        for chunk in entries.chunks(crate::net::REGISTRY_REPLICATION_PAGE_ENTRIES) {
            ensure_peer_admitted(peer_fp)?;
            let update = registry::RegistryUpdate {
                sender_fp: identity.node_fp.clone(),
                entries: chunk.to_vec(),
            };
            let encoded = update.encode()?;
            let update_json = String::from_utf8(encoded).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "registry update is not UTF-8")
            })?;
            if update_json.len() > crate::net::REGISTRY_REPLICATION_PAGE_MAX_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registry update page exceeds byte cap",
                ));
            }
            match request_with_rate_retry(
                endpoint,
                &peer_key,
                &local_private,
                &Request::ClusterRegistryUpdate { update_json },
                deadline,
            ) {
                Ok(Response::ClusterRegistryAck { .. }) => {}
                Ok(Response::Error(reason)) => {
                    return Err(io::Error::other(format!(
                        "peer refused registry update: {reason}"
                    )))
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unexpected update response",
                    ))
                }
                Err(error) => return Err(io::Error::other(error)),
            }
        }
    }
    fetch_peer(
        endpoint,
        &peer_key,
        &local_private,
        peer_fp,
        &local_registry,
        deadline,
    )
}

fn ensure_peer_admitted(peer_fp: &str) -> io::Result<()> {
    let (_, registry) = super::nodes()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cluster is not initialized"))?;
    if registry.authorized_nodes.iter().any(|node| {
        node.node_fp == peer_fp && node.state == NodeState::Admitted && node.endpoint.is_some()
    }) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "peer is no longer admitted for registry replication",
        ))
    }
}

struct FetchAccumulator {
    known_digest: String,
    offset: usize,
    snapshot_digest: Option<String>,
    entries: Vec<AuthorizedNode>,
    pages: usize,
    unchanged: bool,
}

impl FetchAccumulator {
    fn new(known_digest: String) -> Self {
        Self {
            known_digest,
            offset: 0,
            snapshot_digest: None,
            entries: Vec::new(),
            pages: 0,
            unchanged: false,
        }
    }

    fn accept(&mut self, response: Response, peer_fp: &str) -> io::Result<bool> {
        self.pages += 1;
        if self.pages > MAX_FETCH_PAGES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "registry sync exceeded its page limit",
            ));
        }
        let Response::ClusterRegistryPage {
            sender_fp,
            digest,
            offset,
            entries_json,
            next_offset,
            unchanged,
        } = response
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected sync response",
            ));
        };
        if entries_json.len() > crate::net::REGISTRY_REPLICATION_PAGE_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sync page exceeds byte cap",
            ));
        }
        if sender_fp != peer_fp || offset != self.offset {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sync peer or offset mismatch",
            ));
        }
        if self
            .snapshot_digest
            .as_ref()
            .is_some_and(|known| known != &digest)
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "peer registry changed during sync",
            ));
        }
        self.snapshot_digest = Some(digest.clone());
        if unchanged {
            if self.offset != 0 || digest != self.known_digest || entries_json != "[]" {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid unchanged sync page",
                ));
            }
            self.unchanged = true;
            return Ok(true);
        }
        let page_entries = registry::RegistryUpdate::decode_entries_json(&entries_json)?;
        if page_entries.len() > crate::net::REGISTRY_REPLICATION_PAGE_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sync page exceeds entry cap",
            ));
        }
        if page_entries.len() > registry::MAX_REGISTRY_ENTRIES.saturating_sub(self.entries.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "registry sync exceeds total entry cap",
            ));
        }
        let page_len = page_entries.len();
        self.entries.extend(page_entries);
        match next_offset {
            Some(next) if next == self.offset.saturating_add(page_len) && page_len > 0 => {
                self.offset = next;
                Ok(false)
            }
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sync offset does not match page length",
            )),
            None => {
                if Some(Registry::digest_replication_snapshot(&self.entries)?)
                    != self.snapshot_digest
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "registry sync digest did not match its pages",
                    ));
                }
                Ok(true)
            }
        }
    }
}

fn fetch_peer(
    endpoint: SocketAddr,
    peer_key: &[u8],
    local_private: &[u8],
    peer_fp: &str,
    local_registry: &Registry,
    deadline: Instant,
) -> io::Result<()> {
    let mut fetched = FetchAccumulator::new(local_registry.digest()?);
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "registry replication operation timed out",
            ));
        }
        ensure_peer_admitted(peer_fp)?;
        let digest = if fetched.offset == 0 {
            Some(fetched.known_digest.clone())
        } else {
            None
        };
        let response = request_with_rate_retry(
            endpoint,
            peer_key,
            local_private,
            &Request::ClusterRegistrySync {
                digest,
                offset: fetched.offset,
            },
            deadline,
        )
        .map_err(io::Error::other)?;
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "registry replication operation timed out",
            ));
        }
        if !fetched.accept(response, peer_fp)? {
            continue;
        }
        if fetched.unchanged {
            return Ok(());
        }
        if !fetched.entries.is_empty() {
            let update = registry::RegistryUpdate {
                sender_fp: peer_fp.to_owned(),
                entries: std::mem::take(&mut fetched.entries),
            };
            registry::apply_registry_update(&update, peer_key)?;
        }
        return Ok(());
    }
}

fn request_with_rate_retry(
    endpoint: SocketAddr,
    peer_key: &[u8],
    local_private: &[u8],
    request: &Request,
    deadline: Instant,
) -> Result<Response, crate::net::cluster_client::ClientError> {
    for _ in 0..60 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(crate::net::cluster_client::ClientError::Timeout);
        }
        let client = ClusterClient::with_timeouts(
            Arc::new(crate::SystemWallClock::new()),
            crate::net::cluster_client::ClientTimeouts {
                connect: remaining.min(Duration::from_secs(5)),
                read: remaining.min(Duration::from_secs(5)),
                total: remaining,
            },
        );
        let response = client.request(endpoint, peer_key, local_private, request)?;
        if matches!(&response, Response::Error(reason) if reason == "registry replication rate limit exceeded")
        {
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(1)),
            );
            continue;
        }
        return Ok(response);
    }
    Err(crate::net::cluster_client::ClientError::Timeout)
}

fn snapshot_for_wire(entries: &[AuthorizedNode]) -> Vec<AuthorizedNode> {
    entries
        .iter()
        .cloned()
        .map(|mut entry| {
            entry.delivered_by = None;
            entry
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relayed_snapshot_keeps_origin_attribution_and_tombstones() {
        let entries = vec![
            AuthorizedNode {
                node_fp: "SHA256:entry-a".into(),
                static_pubkey: "key-a".into(),
                delivered_by: None,
                format_major: 1,
                format_minor: 0,
                optional_fields: std::collections::BTreeMap::new(),
                endpoint: None,
                state: NodeState::Admitted,
                version: 3,
                by: "SHA256:origin-a".into(),
            },
            AuthorizedNode {
                node_fp: "SHA256:entry-b".into(),
                static_pubkey: "key-b".into(),
                delivered_by: None,
                format_major: 1,
                format_minor: 0,
                optional_fields: std::collections::BTreeMap::new(),
                endpoint: None,
                state: NodeState::Revoked,
                version: 1,
                by: "SHA256:origin-b".into(),
            },
        ];
        let mut local = entries.clone();
        local[0].delivered_by = Some("SHA256:old-relay".into());
        let relayed = snapshot_for_wire(&local);
        assert_eq!(relayed.len(), entries.len());
        assert_eq!(relayed[0].by, "SHA256:origin-a");
        assert_eq!(relayed[1].by, "SHA256:origin-b");
        assert_eq!(relayed[0].delivered_by, None);
        assert_eq!(relayed[0].state, NodeState::Admitted);
        assert_eq!(relayed[1].state, NodeState::Revoked);
    }

    #[test]
    fn push_wire_codec_preserves_original_by_attribution() {
        let public_key = [42u8; 32];
        let relay_key = [43u8; 32];
        let origin_key = [44u8; 32];
        let mut entry = AuthorizedNode {
            node_fp: super::super::encoding::fingerprint(&public_key),
            static_pubkey: super::super::encoding::encode_base64(&public_key),
            endpoint: None,
            delivered_by: Some("SHA256:prior-relay".into()),
            format_major: registry::REGISTRY_FORMAT_MAJOR,
            format_minor: registry::REGISTRY_FORMAT_MINOR,
            optional_fields: std::collections::BTreeMap::new(),
            state: NodeState::Admitted,
            version: 1,
            by: super::super::encoding::fingerprint(&origin_key),
        };
        entry.optional_fields.insert(
            "future_signature".into(),
            serde_json::json!("opaque-signature"),
        );
        let wire = registry::RegistryUpdate {
            sender_fp: super::super::encoding::fingerprint(&relay_key),
            entries: snapshot_for_wire(&[entry]).to_vec(),
        }
        .encode()
        .unwrap();
        let decoded = registry::RegistryUpdate::decode(&wire).unwrap();
        assert_eq!(
            decoded.entries[0].by,
            super::super::encoding::fingerprint(&origin_key)
        );
        assert!(decoded.entries[0].delivered_by.is_none());
        assert!(decoded.entries[0]
            .optional_fields
            .contains_key("future_signature"));
    }

    #[test]
    fn queues_one_latest_operation_per_peer_and_preserves_pushes() {
        let mut state = WorkState::default();
        queue_peer(&mut state, "peer-a".into(), false);
        queue_peer(&mut state, "peer-a".into(), true);
        queue_peer(&mut state, "peer-a".into(), false);
        assert_eq!(state.ready, VecDeque::from(["peer-a".to_owned()]));
        assert_eq!(state.pending.get("peer-a"), Some(&true));

        state.pending.remove("peer-a");
        state.ready.clear();
        state.active.insert("peer-a".into());
        queue_peer(&mut state, "peer-a".into(), true);
        assert!(state.ready.is_empty());
        assert_eq!(state.pending.len(), 1);
        state.active.remove("peer-a");
        if state.pending.contains_key("peer-a") {
            state.ready.push_back("peer-a".into());
        }
        assert_eq!(state.ready, VecDeque::from(["peer-a".to_owned()]));
    }

    #[test]
    fn fetch_accumulator_stops_at_its_page_cap() {
        let mut fetched = FetchAccumulator::new("digest".into());
        let public_key = [51u8; 32];
        let origin_key = [52u8; 32];
        let entry = AuthorizedNode {
            node_fp: super::super::encoding::fingerprint(&public_key),
            static_pubkey: super::super::encoding::encode_base64(&public_key),
            endpoint: None,
            delivered_by: None,
            format_major: 1,
            format_minor: 0,
            optional_fields: std::collections::BTreeMap::new(),
            state: NodeState::Admitted,
            version: 1,
            by: super::super::encoding::fingerprint(&origin_key),
        };
        for offset in 0..MAX_FETCH_PAGES {
            let response = Response::ClusterRegistryPage {
                sender_fp: "peer".into(),
                digest: "digest".into(),
                offset,
                entries_json: registry::RegistryUpdate::encode_entries_json(std::slice::from_ref(
                    &entry,
                ))
                .unwrap(),
                next_offset: Some(offset + 1),
                unchanged: false,
            };
            assert!(!fetched.accept(response, "peer").unwrap());
        }
        let extra = Response::ClusterRegistryPage {
            sender_fp: "peer".into(),
            digest: "digest".into(),
            offset: MAX_FETCH_PAGES,
            entries_json: "[]".into(),
            next_offset: None,
            unchanged: false,
        };
        let error = fetched.accept(extra, "peer").unwrap_err();
        assert!(error.to_string().contains("page limit"));
    }

    #[test]
    fn fetched_snapshot_digest_is_checked_before_over_cap_entries_are_dropped() {
        let public_key = [61u8; 32];
        let origin_key = [62u8; 32];
        let mut entry = AuthorizedNode {
            node_fp: super::super::encoding::fingerprint(&public_key),
            static_pubkey: super::super::encoding::encode_base64(&public_key),
            endpoint: None,
            delivered_by: None,
            format_major: registry::REGISTRY_FORMAT_MAJOR,
            format_minor: registry::REGISTRY_FORMAT_MINOR,
            optional_fields: std::collections::BTreeMap::new(),
            state: NodeState::Admitted,
            version: 1,
            by: super::super::encoding::fingerprint(&origin_key),
        };
        for index in 0..9 {
            entry
                .optional_fields
                .insert(format!("future_{index}"), serde_json::json!(index));
        }
        let digest = Registry::digest_replication_snapshot(std::slice::from_ref(&entry)).unwrap();
        let mut fetched = FetchAccumulator::new("old-digest".into());
        let response = Response::ClusterRegistryPage {
            sender_fp: "peer".into(),
            digest,
            offset: 0,
            entries_json: registry::RegistryUpdate::encode_entries_json(&[entry]).unwrap(),
            next_offset: None,
            unchanged: false,
        };
        assert!(fetched.accept(response, "peer").unwrap());
    }

    #[test]
    fn expired_operation_deadline_prevents_another_request_attempt() {
        let result = request_with_rate_retry(
            "127.0.0.1:1".parse().unwrap(),
            &[],
            &[],
            &Request::ClusterRegistrySync {
                digest: None,
                offset: 0,
            },
            Instant::now() - Duration::from_millis(1),
        );
        assert_eq!(
            result.unwrap_err(),
            crate::net::cluster_client::ClientError::Timeout
        );
    }
}
