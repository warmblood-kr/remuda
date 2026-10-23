//! `remuda vfs status` (#778): the cluster store at a glance.
//!
//! The report uses only what this node already knows: index counts, kept
//! conflicts, its own copies, the repair queue, and per peer its health and
//! its pulls from here. With `copies`, it also asks each placement node which
//! blobs it holds (`ClusterVfsBlobStat`), until a deadline.

use super::vfs::{unix_now, VfsResult, VfsStore};
use super::vfs_sync::{
    down_for, is_down, is_legacy, live_placement, lock, members, peer_caps, request_peer,
    retry_waiting, Members, MAX_STAT_HASHES,
};
use remuda_core::protocol::{Request, Response};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Debug, Serialize)]
pub struct PeerStatus {
    pub node: String,
    /// Seconds it has failed every request, once past the down grace.
    pub down_secs: Option<u64>,
    /// Unix time of its last pull from this node.
    pub last_pull: Option<u64>,
    /// No pull for `stale_days`: it no longer holds deletes back.
    pub stale: bool,
    /// It answered without change cursors (an older build).
    pub old_build: bool,
    /// What it listed on its last change page, if this node pulled from it.
    pub caps: Option<Vec<String>>,
    /// Deletes it has not pulled from here yet.
    pub deletes_unpulled: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct CopyStatus {
    pub complete: usize,
    /// (path, placement nodes confirmed holding it, placement size)
    pub short: Vec<(String, usize, usize)>,
    /// Live files no node confirmed holding.
    pub no_copy: usize,
    /// Placement nodes that did not answer in time.
    pub unanswered: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub node: String,
    pub live: usize,
    pub entries: usize,
    pub conflict_paths: usize,
    pub local_copies: usize,
    pub local_bytes: u64,
    pub retry_waiting: usize,
    pub replicas: u8,
    pub tombstone_days: u32,
    pub stale_days: u32,
    pub now: u64,
    pub peers: Vec<PeerStatus>,
    pub copies: Option<CopyStatus>,
    /// Stores `cluster leave` or `init --new-identity` set aside.
    pub old_stores: Vec<OldStore>,
}

#[derive(Debug, Serialize)]
pub struct OldStore {
    pub path: String,
    pub bytes: u64,
}

fn old_stores() -> Vec<OldStore> {
    let Ok(state) = super::storage::cluster_state_dir() else {
        return Vec::new();
    };
    super::leave::old_stores(&state)
        .into_iter()
        .map(|(path, bytes)| OldStore {
            path: path.display().to_string(),
            bytes,
        })
        .collect()
}

/// Build the report; with `copies`, ask placement nodes until `deadline`.
pub fn status(copies: bool, deadline: Duration) -> VfsResult<StatusReport> {
    let store = VfsStore::open_default()?;
    let members = members()?;
    let settings = store.settings()?;
    let now = unix_now();
    let peers: Vec<String> = members
        .admitted
        .iter()
        .filter(|fp| **fp != members.self_fp)
        .cloned()
        .collect();
    let index = store.index_status(&peers)?;
    let stale_before = now.saturating_sub(u64::from(settings.stale_days) * 86_400);
    let caps = lock(peer_caps()).clone();
    let peer_rows = peers
        .iter()
        .zip(&index.pulls)
        .map(|(fp, (last_pull, unpulled))| {
            let stale = last_pull.is_some_and(|seen| seen < stale_before);
            PeerStatus {
                node: super::node_label(fp),
                down_secs: down_for(fp).map(|down| down.as_secs()),
                last_pull: *last_pull,
                stale,
                old_build: is_legacy(fp),
                caps: caps.get(fp).cloned(),
                deletes_unpulled: if stale { 0 } else { *unpulled },
            }
        })
        .collect();
    let blobs = store.local_blobs()?;
    let local_bytes = blobs
        .iter()
        .filter_map(|sha| store.blob_len(sha).ok())
        .sum();
    let copies = if copies {
        Some(copy_status(&store, &members, deadline)?)
    } else {
        None
    };
    Ok(StatusReport {
        node: super::node_label(&members.self_fp),
        live: index.counts.live,
        entries: index.counts.total,
        conflict_paths: index.conflict_paths,
        local_copies: blobs.len(),
        local_bytes,
        retry_waiting: retry_waiting(),
        replicas: settings.default_replicas,
        tombstone_days: settings.tombstone_days,
        stale_days: settings.stale_days,
        now,
        peers: peer_rows,
        copies,
        old_stores: old_stores(),
    })
}

/// Which placement nodes confirm each live file's blob.
pub(super) fn copy_status(
    store: &VfsStore,
    members: &Members,
    deadline: Duration,
) -> VfsResult<CopyStatus> {
    let until = Instant::now() + deadline;
    let self_fp = members.self_fp.as_str();
    let index = store.load_index()?;
    let files: Vec<_> = index
        .entries
        .values()
        .filter(|entry| !entry.deleted)
        .collect();
    let mut asks: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    let placements: Vec<Vec<String>> = files
        .iter()
        .map(|entry| live_placement(entry, members))
        .collect();
    for (entry, placement) in files.iter().zip(&placements) {
        for node in placement.iter().filter(|node| *node != self_fp) {
            asks.entry(node.clone())
                .or_default()
                .insert(entry.sha256.clone());
        }
    }
    let mut held: HashMap<String, HashSet<String>> = HashMap::new();
    let mut report = CopyStatus::default();
    for (node, hashes) in asks {
        let hashes: Vec<String> = hashes.into_iter().collect();
        let mut confirmed = HashSet::new();
        let mut answered = !is_down(&node);
        for chunk in hashes.chunks(MAX_STAT_HASHES) {
            if !answered || Instant::now() >= until {
                answered = false;
                break;
            }
            let request = Request::ClusterVfsBlobStat {
                hashes: chunk.to_vec(),
            };
            match request_peer(&node, &request) {
                Ok(Response::ClusterVfsBlobHave(have)) if have.len() == chunk.len() => {
                    confirmed.extend(
                        chunk
                            .iter()
                            .zip(have)
                            .filter(|(_, has)| *has)
                            .map(|(hash, _)| hash.clone()),
                    );
                }
                _ => answered = false,
            }
        }
        if !answered {
            report.unanswered.push(super::node_label(&node));
        }
        held.insert(node, confirmed);
    }
    for (entry, placement) in files.iter().zip(&placements) {
        let holders = placement
            .iter()
            .filter(|node| {
                if *node == self_fp {
                    store.has_blob(&entry.sha256, entry.size)
                } else {
                    held.get(*node)
                        .is_some_and(|set| set.contains(&entry.sha256))
                }
            })
            .count();
        if holders >= placement.len() {
            report.complete += 1;
        } else {
            if holders == 0 && !store.has_blob(&entry.sha256, entry.size) {
                report.no_copy += 1;
            }
            report
                .short
                .push((entry.path.clone(), holders, placement.len()));
        }
    }
    report.short.sort();
    Ok(report)
}
