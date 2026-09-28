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
use std::time::Duration;
use zeroize::Zeroizing;

const WORKERS: usize = 4;

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
    {
        if thread::Builder::new()
            .name("remuda-cluster-replication-queue".into())
            .spawn(schedule_changes)
            .is_err()
        {
            CHANGE_SCHEDULER_RUNNING.store(false, Ordering::Release);
        }
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

fn peer_material(peer_fp: &str) -> io::Result<(SocketAddr, Vec<u8>, Zeroizing<Vec<u8>>, Registry)> {
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
    Ok((endpoint, public, private, registry))
}

fn replicate_peer(peer_fp: &str, push: bool) -> io::Result<()> {
    let (endpoint, peer_key, local_private, local_registry) = peer_material(peer_fp)?;
    let client = ClusterClient::system();
    if push {
        let (identity, _) = super::nodes()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cluster is not initialized"))?;
        let entries = attest_snapshot(&local_registry.authorized_nodes, &identity.node_fp);
        for chunk in entries.chunks(crate::net::REGISTRY_REPLICATION_PAGE_ENTRIES) {
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
                &client,
                endpoint,
                &peer_key,
                &local_private,
                &Request::ClusterRegistryUpdate { update_json },
            ) {
                Ok(Response::ClusterRegistryAck { .. }) => {}
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
        &client,
        endpoint,
        &peer_key,
        &local_private,
        peer_fp,
        &local_registry,
    )
}

fn fetch_peer(
    client: &ClusterClient,
    endpoint: SocketAddr,
    peer_key: &[u8],
    local_private: &[u8],
    peer_fp: &str,
    local_registry: &Registry,
) -> io::Result<()> {
    let known_digest = local_registry.digest()?;
    let mut offset = 0usize;
    let mut snapshot_digest: Option<String> = None;
    loop {
        let digest = if offset == 0 {
            Some(known_digest.clone())
        } else {
            None
        };
        let response = request_with_rate_retry(
            client,
            endpoint,
            peer_key,
            local_private,
            &Request::ClusterRegistrySync { digest, offset },
        )
        .map_err(io::Error::other)?;
        let Response::ClusterRegistryPage {
            sender_fp,
            digest,
            offset: response_offset,
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
        if sender_fp != peer_fp || response_offset != offset {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sync peer or offset mismatch",
            ));
        }
        if snapshot_digest
            .as_ref()
            .is_some_and(|known| known != &digest)
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "peer registry changed during sync",
            ));
        }
        snapshot_digest = Some(digest);
        if unchanged {
            if offset != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid unchanged sync page",
                ));
            }
            return Ok(());
        }
        let entries: Vec<AuthorizedNode> = serde_json::from_str(&entries_json)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid sync entries"))?;
        if entries.len() > crate::net::REGISTRY_REPLICATION_PAGE_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sync page exceeds entry cap",
            ));
        }
        let accepted = attest_snapshot(&entries, peer_fp);
        if !accepted.is_empty() {
            let update = registry::RegistryUpdate {
                sender_fp: peer_fp.to_owned(),
                entries: accepted,
            };
            registry::apply_registry_update(&update, peer_key)?;
        }
        match next_offset {
            Some(next) if next > offset => offset = next,
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "sync offset did not advance",
                ))
            }
            None => return Ok(()),
        }
    }
}

fn request_with_rate_retry(
    client: &ClusterClient,
    endpoint: SocketAddr,
    peer_key: &[u8],
    local_private: &[u8],
    request: &Request,
) -> Result<Response, crate::net::cluster_client::ClientError> {
    for _ in 0..60 {
        let response = client.request(endpoint, peer_key, local_private, request)?;
        if matches!(&response, Response::Error(reason) if reason == "registry replication rate limit exceeded")
        {
            thread::sleep(Duration::from_secs(1));
            continue;
        }
        return Ok(response);
    }
    Err(crate::net::cluster_client::ClientError::Timeout)
}

fn attest_snapshot(entries: &[AuthorizedNode], sender_fp: &str) -> Vec<AuthorizedNode> {
    entries
        .iter()
        .cloned()
        .map(|mut entry| {
            // The authenticated relay vouches for every row in its full
            // snapshot. `by` therefore records the latest attesting member;
            // this is deliberately forgeable by a compromised current member.
            entry.by = sender_fp.to_owned();
            entry
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relayed_snapshot_keeps_every_entry_and_attributes_it_to_sender() {
        let entries = vec![
            AuthorizedNode {
                node_fp: "SHA256:entry-a".into(),
                static_pubkey: "key-a".into(),
                endpoint: None,
                state: NodeState::Admitted,
                version: 3,
                by: "SHA256:origin-a".into(),
            },
            AuthorizedNode {
                node_fp: "SHA256:entry-b".into(),
                static_pubkey: "key-b".into(),
                endpoint: None,
                state: NodeState::Revoked,
                version: 1,
                by: "SHA256:origin-b".into(),
            },
        ];
        let relayed = attest_snapshot(&entries, "SHA256:relay");
        assert_eq!(relayed.len(), entries.len());
        assert!(relayed.iter().all(|entry| entry.by == "SHA256:relay"));
        assert_eq!(relayed[0].state, NodeState::Admitted);
        assert_eq!(relayed[1].state, NodeState::Revoked);
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
}
