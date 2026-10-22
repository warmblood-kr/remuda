//! `remuda vfs drain` (#778, design step 6): before a node leaves, every
//! file it holds must keep its copies on the nodes that remain.
//!
//! For each blob held here, drain works out the placement without this node
//! and asks each placement node that lacks the blob to take it
//! (`ClusterVfsTake`). Only peers that list the "take" capability on their
//! change pages are asked; older peers are reported instead. Drain revokes
//! nothing: the owner runs `remuda cluster revoke` on another node after.

use super::vfs::{VfsResult, VfsStore};
use super::vfs_sync::{is_down, lock, members, peer_caps, request_peer, MAX_STAT_HASHES};
use remuda_core::protocol::{Request, Response, VFS_CAP_TAKE};
use remuda_core::vfs::{self as rules, VfsEntry};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct DrainReport {
    /// This node, as other nodes name it.
    pub node: String,
    /// Live files whose content this node holds.
    pub files: usize,
    /// Of those, the paths still short of copies without this node.
    pub short: Vec<String>,
    /// Peers that must take copies but are too old to be asked.
    pub too_old: Vec<String>,
    /// Admitted nodes other than this one that are not down.
    pub remaining: usize,
}

/// Ask peers to take copies until no file is short, or `timeout` passes.
pub fn drain(timeout: Duration) -> VfsResult<DrainReport> {
    let deadline = Instant::now() + timeout;
    let store = VfsStore::open_default()?;
    loop {
        let report = drain_step(&store)?;
        if report.short.is_empty() || Instant::now() >= deadline {
            return Ok(report);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn drain_step(store: &VfsStore) -> VfsResult<DrainReport> {
    let members = members()?;
    let others: Vec<&str> = members
        .admitted
        .iter()
        .map(String::as_str)
        .filter(|fp| *fp != members.self_fp && !is_down(fp))
        .collect();
    let index = store.load_index()?;
    let files: Vec<&VfsEntry> = index
        .entries
        .values()
        .filter(|entry| !entry.deleted && store.has_blob(&entry.sha256, entry.size))
        .collect();
    let mut wanted: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for entry in &files {
        for node in rules::placement(&entry.sha256, entry.replicas, &others) {
            wanted.entry(node).or_default().insert(&entry.sha256);
        }
    }
    let mut report = DrainReport {
        node: super::node_label(&members.self_fp),
        files: files.len(),
        remaining: others.len(),
        ..DrainReport::default()
    };
    let mut lacking: HashSet<&str> = HashSet::new();
    for (node, hashes) in wanted {
        let missing = missing_on(node, hashes.into_iter().collect());
        if missing.is_empty() {
            continue;
        }
        match offers_take(node) {
            Some(true) => {
                for chunk in missing.chunks(MAX_STAT_HASHES) {
                    let hashes = chunk.iter().map(|hash| (*hash).to_owned()).collect();
                    let _ = request_peer(node, &Request::ClusterVfsTake { hashes });
                }
            }
            Some(false) => report.too_old.push(super::node_label(node)),
            None => {} // unreachable now; asked again on the next round
        }
        lacking.extend(missing);
    }
    report.short = files
        .iter()
        .filter(|entry| others.is_empty() || lacking.contains(entry.sha256.as_str()))
        .map(|entry| entry.path.clone())
        .collect();
    Ok(report)
}

/// The hashes `node` does not confirm holding; an unreachable node holds none.
fn missing_on<'a>(node: &str, hashes: Vec<&'a str>) -> Vec<&'a str> {
    let mut missing = Vec::new();
    for chunk in hashes.chunks(MAX_STAT_HASHES) {
        let request = Request::ClusterVfsBlobStat {
            hashes: chunk.iter().map(|hash| (*hash).to_owned()).collect(),
        };
        let have = match request_peer(node, &request) {
            Ok(Response::ClusterVfsBlobHave(have)) if have.len() == chunk.len() => have,
            _ => vec![false; chunk.len()],
        };
        missing.extend(
            chunk
                .iter()
                .zip(have)
                .filter(|(_, has)| !has)
                .map(|(hash, _)| *hash),
        );
    }
    missing
}

/// Whether `node` serves `ClusterVfsTake`; `None` if it did not answer. A
/// node never pulled from yet is asked for an empty change page.
fn offers_take(node: &str) -> Option<bool> {
    if let Some(caps) = lock(peer_caps()).get(node) {
        return Some(caps.iter().any(|cap| cap == VFS_CAP_TAKE));
    }
    let probe = Request::ClusterVfsChanges {
        index_id: None,
        since: u64::MAX,
        limit: 1,
    };
    let caps = match request_peer(node, &probe) {
        Ok(Response::ClusterVfsChangesPage { caps, .. }) => caps,
        Ok(_) => Vec::new(),
        Err(_) => return None,
    };
    let offers = caps.iter().any(|cap| cap == VFS_CAP_TAKE);
    lock(peer_caps()).insert(node.to_owned(), caps);
    Some(offers)
}
