//! Peer half of the cluster virtual file store (#742): index replication,
//! blob pulls, and the repair pass that restores copies after a node is lost.
//! Content moves by pull only: a cluster request carries about 64 KB, while
//! a response may carry 4 MiB.

use super::vfs::{unix_now, VfsError, VfsErrorKind, VfsResult, VfsStore, MAX_BLOB_BYTES};
use super::{NodeState, Registry};
use crate::net::cluster_client::{ClientTimeouts, ClusterClient};
use base64::Engine;
use remuda_core::protocol::{Request, Response};
use remuda_core::vfs::{self as rules, VfsEntry};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Index entries per sync page and per update request.
pub const INDEX_PAGE_ENTRIES: usize = 256;
pub const MAX_UPDATE_JSON_BYTES: usize = 48 * 1024;
pub const MAX_STAT_HASHES: usize = 64;
pub const BLOB_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_REPAIRS_PER_PASS: usize = 256;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_DOWN_GRACE: Duration = Duration::from_secs(120);

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

// ---------------------------------------------------------------------------
// Serving peers (called by the cluster listener for an admitted peer).

/// Answer one VFS request from the authenticated, admitted peer `peer_fp`.
pub fn serve(peer_fp: &str, request: &Request) -> Response {
    let store = match VfsStore::open_default() {
        Ok(store) => store,
        Err(error) => return Response::error(error.reason()),
    };
    let answered = match request {
        Request::ClusterVfsChanges {
            index_id,
            since,
            limit,
        } => serve_changes(&store, peer_fp, index_id.as_deref(), *since, *limit),
        request => serve_with(&store, request),
    };
    match answered {
        Ok(response) => {
            // New names may place a copy here: fetch it now rather than at
            // the next anti-entropy tick.
            if matches!(response, Response::ClusterVfsAck { applied, .. } if applied > 0) {
                background_pass();
            }
            response
        }
        Err(error) => Response::error(error.reason()),
    }
}

/// `ClusterVfsChanges`: rows after the caller's cursor. The cursor also
/// tells us the caller holds every change of ours up to `since`.
pub fn serve_changes(
    store: &VfsStore,
    peer_fp: &str,
    index_id: Option<&str>,
    since: u64,
    limit: u32,
) -> VfsResult<Response> {
    let mut page = store.changes(
        index_id,
        since,
        (limit as usize).clamp(1, INDEX_PAGE_ENTRIES),
    )?;
    // Keep a page under the frame byte cap: ask again for half as many rows.
    while page.entries.len() > 1 && !batch_fits(&page.entries.iter().collect::<Vec<_>>()) {
        page = store.changes(index_id, since, page.entries.len() / 2)?;
    }
    if !page.reset && index_id.is_some() {
        store.record_ack(peer_fp, since);
    }
    Ok(Response::ClusterVfsChangesPage {
        index_id: page.index_id,
        entries_json: serde_json::to_string(&page.entries).map_err(std::io::Error::other)?,
        last_seq: page.last_seq,
        more: page.more,
        reset: page.reset,
    })
}

pub fn serve_with(store: &VfsStore, request: &Request) -> VfsResult<Response> {
    match request {
        Request::ClusterVfsIndexSync { digest, offset } => {
            let index = store.load_index()?;
            let current = index.digest();
            let entries: Vec<&VfsEntry> = index.entries.values().collect();
            if *offset > entries.len() {
                return Err(VfsError::new(
                    VfsErrorKind::BadUri,
                    "index page offset is out of bounds",
                ));
            }
            let unchanged = *offset == 0 && digest.as_deref() == Some(current.as_str());
            let end = offset.saturating_add(INDEX_PAGE_ENTRIES).min(entries.len());
            let page: &[&VfsEntry] = if unchanged {
                &[]
            } else {
                &entries[*offset..end]
            };
            Ok(Response::ClusterVfsIndexPage {
                digest: current,
                offset: *offset,
                entries_json: serde_json::to_string(page).map_err(std::io::Error::other)?,
                next_offset: (!unchanged && end < entries.len()).then_some(end),
                unchanged,
            })
        }
        Request::ClusterVfsIndexUpdate { entries_json } => {
            if entries_json.len() > MAX_UPDATE_JSON_BYTES {
                return Err(VfsError::new(
                    VfsErrorKind::TooLarge,
                    "index update is too large",
                ));
            }
            let entries: Vec<VfsEntry> = serde_json::from_str(entries_json)
                .map_err(|_| VfsError::new(VfsErrorKind::Corrupt, "invalid index update"))?;
            if entries.len() > INDEX_PAGE_ENTRIES {
                return Err(VfsError::new(
                    VfsErrorKind::TooLarge,
                    "index update has too many entries",
                ));
            }
            let changed = store.merge_entries(entries)?;
            Ok(Response::ClusterVfsAck {
                digest: store.load_index()?.digest(),
                applied: changed.len(),
            })
        }
        Request::ClusterVfsBlobStat { hashes } => {
            if hashes.len() > MAX_STAT_HASHES {
                return Err(VfsError::new(VfsErrorKind::TooLarge, "too many hashes"));
            }
            Ok(Response::ClusterVfsBlobHave(
                hashes.iter().map(|hash| store.blob_present(hash)).collect(),
            ))
        }
        Request::ClusterVfsBlobGet { sha256, offset } => {
            // Check the content before a peer starts pulling it, so a damaged
            // copy is dropped here instead of failing the peer's whole pull.
            // Later chunks are not re-hashed; the peer checks the whole blob.
            if *offset == 0 {
                store.verify_blob(sha256)?;
            }
            let total = store.blob_len(sha256)?;
            if *offset > total {
                return Err(VfsError::new(
                    VfsErrorKind::BadUri,
                    "blob offset is out of bounds",
                ));
            }
            let data = store.read_blob_range(sha256, *offset, BLOB_CHUNK_BYTES)?;
            Ok(Response::ClusterVfsBlobChunk {
                sha256: sha256.clone(),
                offset: *offset,
                total,
                data_b64: b64().encode(data),
            })
        }
        _ => Err(VfsError::new(VfsErrorKind::BadUri, "not a vfs request")),
    }
}

// ---------------------------------------------------------------------------
// Peer health: a peer that has failed every request for the grace period is
// treated as down for placement, so the next-ranked node takes its copies.

fn health() -> &'static Mutex<HashMap<String, Instant>> {
    static HEALTH: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    HEALTH.get_or_init(Default::default)
}

fn down_grace() -> Duration {
    std::env::var("REMUDA_TEST_VFS_DOWN_GRACE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_DOWN_GRACE)
}

fn record_health(peer_fp: &str, reached: bool) {
    let mut map = health()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if reached {
        map.remove(peer_fp);
    } else {
        map.entry(peer_fp.to_owned()).or_insert_with(Instant::now);
    }
}

fn is_down(peer_fp: &str) -> bool {
    health()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(peer_fp)
        .is_some_and(|since| since.elapsed() >= down_grace())
}

// ---------------------------------------------------------------------------
// Membership and placement.

struct Members {
    self_fp: String,
    /// Admitted nodes, this one included.
    admitted: Vec<String>,
    /// Admitted peers with an endpoint, which this node can reach.
    reachable: Vec<String>,
}

fn members() -> VfsResult<Members> {
    let (identity, registry) = super::nodes()?
        .ok_or_else(|| VfsError::new(VfsErrorKind::NoCluster, "this node has no cluster"))?;
    Ok(members_of(&identity.node_fp, &registry))
}

fn members_of(self_fp: &str, registry: &Registry) -> Members {
    let mut admitted: Vec<String> = registry
        .authorized_nodes
        .iter()
        .filter(|node| node.state == NodeState::Admitted)
        .map(|node| node.node_fp.clone())
        .collect();
    if !admitted.iter().any(|fp| fp == self_fp) {
        admitted.push(self_fp.to_owned());
    }
    let reachable = registry
        .authorized_nodes
        .iter()
        .filter(|node| {
            node.state == NodeState::Admitted && node.node_fp != self_fp && node.endpoint.is_some()
        })
        .map(|node| node.node_fp.clone())
        .collect();
    Members {
        self_fp: self_fp.to_owned(),
        admitted,
        reachable,
    }
}

/// Where an entry's copies belong, from this node's view (down peers left out).
pub fn placement_for(entry: &VfsEntry) -> Vec<String> {
    match members() {
        Ok(members) => live_placement(entry, &members),
        Err(_) => Vec::new(),
    }
}

fn live_placement(entry: &VfsEntry, members: &Members) -> Vec<String> {
    let candidates: Vec<&str> = members
        .admitted
        .iter()
        .map(String::as_str)
        .filter(|fp| *fp == members.self_fp || !is_down(fp))
        .collect();
    rules::placement(&entry.sha256, entry.replicas, &candidates)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

// ---------------------------------------------------------------------------
// Talking to peers.

fn request_peer(peer_fp: &str, request: &Request) -> Result<Response, String> {
    let result = request_peer_inner(peer_fp, request);
    record_health(peer_fp, result.is_ok());
    result
}

fn request_peer_inner(peer_fp: &str, request: &Request) -> Result<Response, String> {
    let material = super::replication::peer_material(peer_fp).map_err(|error| error.to_string())?;
    let client = ClusterClient::with_timeouts(
        Arc::new(crate::SystemWallClock::new()),
        ClientTimeouts {
            connect: Duration::from_secs(3),
            read: REQUEST_TIMEOUT,
            total: REQUEST_TIMEOUT,
        },
    );
    for attempt in 0..20 {
        let response = client
            .request(
                material.endpoint,
                &material.peer_key,
                &material.local_private,
                request,
            )
            .map_err(|error| error.to_string())?;
        match response {
            Response::Error(reason) if reason == crate::net::listener::VFS_RATE_LIMITED => {
                std::thread::sleep(Duration::from_millis(50 * (attempt + 1)));
            }
            response => return Ok(response),
        }
    }
    Err("peer kept refusing: rate limited".into())
}

#[derive(Clone, Debug)]
pub struct PushResult {
    pub peer_fp: String,
    pub reached: bool,
}

/// Tell every reachable peer about new or changed entries. Best effort; the
/// anti-entropy pass catches up with any peer this misses.
pub fn push_entries(entries: &[VfsEntry]) -> Vec<PushResult> {
    let Ok(members) = members() else {
        return Vec::new();
    };
    let batches = batch_entries(entries);
    let results = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for peer in &members.reachable {
            let batches = &batches;
            let results = &results;
            scope.spawn(move || {
                let reached = batches.iter().all(|entries_json| {
                    matches!(
                        request_peer(
                            peer,
                            &Request::ClusterVfsIndexUpdate {
                                entries_json: entries_json.clone(),
                            },
                        ),
                        Ok(Response::ClusterVfsAck { .. })
                    )
                });
                results
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(PushResult {
                        peer_fp: peer.clone(),
                        reached,
                    });
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The batch must fit the server's cap as sent, and also once escaped into
/// the request JSON (a path may contain quotes), within one cluster request.
fn batch_fits(entries: &[&VfsEntry]) -> bool {
    let encoded = serde_json::to_string(entries).unwrap_or_default();
    let escaped = serde_json::to_string(&encoded).unwrap_or_default();
    entries.len() <= INDEX_PAGE_ENTRIES
        && encoded.len() <= MAX_UPDATE_JSON_BYTES
        && escaped.len() + 128 <= crate::net::frame::MAX_REQUEST_PAYLOAD
}

fn batch_entries(entries: &[VfsEntry]) -> Vec<String> {
    let mut batches = Vec::new();
    let mut current: Vec<&VfsEntry> = Vec::new();
    for entry in entries {
        current.push(entry);
        if current.len() > 1 && !batch_fits(&current) {
            current.pop();
            batches.push(serde_json::to_string(&current).unwrap_or_default());
            current = vec![entry];
        }
    }
    if !current.is_empty() {
        batches.push(serde_json::to_string(&current).unwrap_or_default());
    }
    batches
}

/// Peers that did not answer `ClusterVfsChanges` (older builds), and when.
/// They get the digest pull, and are asked again after `LEGACY_RETRY`.
static LEGACY_PEERS: std::sync::Mutex<BTreeMap<String, Instant>> =
    std::sync::Mutex::new(BTreeMap::new());
const LEGACY_RETRY: Duration = Duration::from_secs(3600);

/// Pull what `peer` changed since our cursor; fall back to the whole-index
/// digest pull for a peer that does not know change cursors.
fn pull(store: &VfsStore, peer: &str) -> Result<usize, String> {
    let legacy = LEGACY_PEERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(peer)
        .is_some_and(|since| since.elapsed() < LEGACY_RETRY);
    if legacy {
        return pull_index(store, peer);
    }
    match pull_changes(store, peer, &mut |request| request_peer(peer, request)) {
        Ok(Some(merged)) => Ok(merged),
        Ok(None) | Err(_) => {
            let merged = pull_index(store, peer)?;
            LEGACY_PEERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(peer.to_owned(), Instant::now());
            Ok(merged)
        }
    }
}

/// Page through `peer`'s changes after our cursor, merging each page with
/// its cursor in one transaction. `Ok(None)`: the peer does not speak it.
pub fn pull_changes(
    store: &VfsStore,
    peer: &str,
    fetch: &mut dyn FnMut(&Request) -> Result<Response, String>,
) -> Result<Option<usize>, String> {
    let mut merged = 0;
    let mut resets = 0;
    for _ in 0..(super::vfs::MAX_INDEX_ENTRIES / INDEX_PAGE_ENTRIES + 4) {
        let (index_id, since) = store.cursor(peer).map_err(|e| e.to_string())?;
        let response = fetch(&Request::ClusterVfsChanges {
            index_id,
            since,
            limit: INDEX_PAGE_ENTRIES as u32,
        })?;
        let Response::ClusterVfsChangesPage {
            index_id,
            entries_json,
            last_seq,
            more,
            reset,
        } = response
        else {
            return Ok(None);
        };
        if reset {
            resets += 1;
            if resets > 1 {
                return Err("peer reset the change cursor twice".into());
            }
            store.reset_cursor(peer).map_err(|e| e.to_string())?;
            continue;
        }
        let entries: Vec<VfsEntry> =
            serde_json::from_str(&entries_json).map_err(|_| "bad changes page".to_owned())?;
        let page = super::vfs_index_db::ChangesPage {
            index_id,
            entries,
            last_seq,
            more,
            reset,
        };
        merged += store
            .merge_page(peer, &page)
            .map_err(|e| e.to_string())?
            .changed
            .len();
        if !more {
            return Ok(Some(merged));
        }
    }
    Err("peer change pages did not end".into())
}

/// Pull one peer's whole index and merge it. Returns how many entries changed.
fn pull_index(store: &VfsStore, peer_fp: &str) -> Result<usize, String> {
    let known = store
        .load_index()
        .map_err(|error| error.to_string())?
        .digest();
    let mut offset = 0;
    let mut collected = Vec::new();
    for _ in 0..(super::vfs::MAX_INDEX_ENTRIES / INDEX_PAGE_ENTRIES + 2) {
        let response = request_peer(
            peer_fp,
            &Request::ClusterVfsIndexSync {
                digest: (offset == 0).then(|| known.clone()),
                offset,
            },
        )?;
        let Response::ClusterVfsIndexPage {
            entries_json,
            next_offset,
            unchanged,
            ..
        } = response
        else {
            return Err(describe(&response));
        };
        if unchanged {
            return Ok(0);
        }
        let page: Vec<VfsEntry> =
            serde_json::from_str(&entries_json).map_err(|_| "bad index page".to_owned())?;
        collected.extend(page);
        match next_offset {
            Some(next) if next > offset => offset = next,
            Some(_) => return Err("peer index paging did not advance".into()),
            None => break,
        }
    }
    store
        .merge_entries(collected)
        .map(|changed| changed.len())
        .map_err(|error| error.to_string())
}

fn describe(response: &Response) -> String {
    match response {
        Response::Error(reason) => reason.clone(),
        _ => "unexpected response".into(),
    }
}

/// Download a blob from one peer and verify it.
fn pull_blob_from(peer_fp: &str, entry: &VfsEntry) -> Result<Vec<u8>, String> {
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let response = request_peer(
            peer_fp,
            &Request::ClusterVfsBlobGet {
                sha256: entry.sha256.clone(),
                offset: bytes.len() as u64,
            },
        )?;
        let Response::ClusterVfsBlobChunk {
            offset,
            total,
            data_b64,
            ..
        } = response
        else {
            return Err(describe(&response));
        };
        if total > MAX_BLOB_BYTES || total != entry.size || offset != bytes.len() as u64 {
            return Err("peer sent a blob of the wrong size".into());
        }
        let chunk = b64()
            .decode(data_b64)
            .map_err(|_| "peer sent bad base64".to_owned())?;
        if chunk.is_empty() && (bytes.len() as u64) < total {
            return Err("peer sent an empty chunk".into());
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > total {
            return Err("peer sent more than the blob size".into());
        }
        if bytes.len() as u64 == total {
            return Ok(bytes);
        }
    }
}

/// Fetch a blob this node lacks: placement nodes first, then every other
/// reachable peer. The copy is verified and kept locally.
pub fn fetch_blob(store: &VfsStore, entry: &VfsEntry) -> VfsResult<Vec<u8>> {
    let members = members()?;
    let reachable: Vec<&str> = members.reachable.iter().map(String::as_str).collect();
    let ranked = rules::rank_nodes(&entry.sha256, &reachable);
    let mut tried = Vec::new();
    for peer in ranked {
        match pull_blob_from(peer, entry) {
            Ok(bytes) => {
                store.import_blob(&entry.sha256, &bytes)?;
                return Ok(bytes);
            }
            Err(reason) => tried.push(format!("{} ({reason})", super::node_label(peer))),
        }
    }
    Err(VfsError::new(
        VfsErrorKind::Unreachable,
        if tried.is_empty() {
            format!(
                "{} is not on this node and no other node is reachable",
                entry.path
            )
        } else {
            format!(
                "no reachable copy of {}; tried {}",
                entry.path,
                tried.join(", ")
            )
        },
    ))
}

/// Count the placement nodes that hold the entry's blob, polling until
/// `replicas` (or every live candidate) hold it, or `timeout` passes.
pub fn wait_for_replicas(entry: &VfsEntry, timeout: Duration) -> VfsResult<usize> {
    let deadline = Instant::now() + timeout;
    let store = VfsStore::open_default()?;
    loop {
        let members = members()?;
        let wanted = live_placement(entry, &members);
        let mut holders = 0;
        for node in &wanted {
            let has = if *node == members.self_fp {
                store.has_blob(&entry.sha256, entry.size)
            } else {
                matches!(
                    request_peer(node, &Request::ClusterVfsBlobStat { hashes: vec![entry.sha256.clone()] }),
                    Ok(Response::ClusterVfsBlobHave(have)) if have.first() == Some(&true)
                )
            };
            holders += usize::from(has);
        }
        if holders >= wanted.len() {
            return Ok(holders);
        }
        if Instant::now() >= deadline {
            return Err(VfsError::new(
                VfsErrorKind::Unreachable,
                format!(
                    "only {holders} of {} copies confirmed in time",
                    wanted.len()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

// ---------------------------------------------------------------------------
// The repair pass.

#[derive(Debug, Default)]
pub struct SyncReport {
    pub peers_reached: usize,
    pub peers_failed: usize,
    pub entries_merged: usize,
    pub copies_fetched: usize,
    pub copies_failed: usize,
    pub garbage_removed: usize,
    pub error: Option<String>,
}

impl std::fmt::Display for SyncReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(error) = &self.error {
            return write!(formatter, "vfs sync failed: {error}");
        }
        write!(
            formatter,
            "Peers reached {}, unreachable {}; index entries merged {}; copies fetched {}, failed {}; old blobs removed {}.",
            self.peers_reached,
            self.peers_failed,
            self.entries_merged,
            self.copies_fetched,
            self.copies_failed,
            self.garbage_removed
        )
    }
}

/// Pull indexes from every reachable peer, then fetch the copies this node
/// should hold under the current placement.
pub fn sync_once() -> SyncReport {
    let mut report = SyncReport::default();
    let (store, members) = match VfsStore::open_default().and_then(|store| Ok((store, members()?)))
    {
        Ok(pair) => pair,
        Err(error) => {
            report.error = Some(error.to_string());
            return report;
        }
    };
    // A daemon a reload replaced stops syncing quietly; the new one syncs.
    if !store.writes_here() {
        return report;
    }
    for peer in &members.reachable {
        match pull(&store, peer) {
            Ok(merged) => {
                report.peers_reached += 1;
                report.entries_merged += merged;
            }
            Err(_) => report.peers_failed += 1,
        }
    }
    if report.peers_reached > 0 {
        let _ = store.mark_synced(unix_now());
    }
    let index = match store.load_index() {
        Ok(index) => index,
        Err(error) => {
            report.error = Some(error.to_string());
            return report;
        }
    };
    let mut attempts = 0;
    for entry in index.entries.values().filter(|entry| !entry.deleted) {
        if store.has_blob(&entry.sha256, entry.size) {
            continue;
        }
        if !live_placement(entry, &members).contains(&members.self_fp) {
            continue;
        }
        if attempts >= MAX_REPAIRS_PER_PASS {
            break;
        }
        attempts += 1;
        match fetch_blob(&store, entry) {
            Ok(_) => report.copies_fetched += 1,
            Err(_) => report.copies_failed += 1,
        }
    }
    report.garbage_removed = store.collect_garbage().unwrap_or(0);
    report
}

static PASS_RUNNING: AtomicBool = AtomicBool::new(false);
static PASS_PENDING: AtomicBool = AtomicBool::new(false);

/// Run `sync_once` on a background thread. At most one pass runs; a request
/// during a pass makes it run once more.
pub fn background_pass() {
    PASS_PENDING.store(true, Ordering::Release);
    if PASS_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("remuda-vfs-repair".into())
        .spawn(|| loop {
            PASS_PENDING.store(false, Ordering::Release);
            let report = sync_once();
            if report.copies_failed > 0 {
                eprintln!("remuda: {report}");
            }
            PASS_RUNNING.store(false, Ordering::Release);
            if !PASS_PENDING.load(Ordering::Acquire)
                || PASS_RUNNING
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
            {
                return;
            }
        });
    if spawned.is_err() {
        PASS_RUNNING.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::AuthorizedNode;

    fn node(fp: &str, state: NodeState, endpoint: Option<&str>) -> AuthorizedNode {
        AuthorizedNode {
            node_fp: fp.into(),
            static_pubkey: String::new(),
            format_major: 1,
            format_minor: 0,
            endpoint: endpoint.map(str::to_owned),
            delivered_by: None,
            state,
            version: 1,
            by: fp.into(),
            optional_fields: Default::default(),
        }
    }

    fn scratch_store() -> (std::path::PathBuf, VfsStore) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "remuda-vfs-sync-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = VfsStore::open_at(&root.join("vfs")).unwrap();
        (root, store)
    }

    fn entry(path: &str, sha: &str) -> VfsEntry {
        VfsEntry {
            path: path.into(),
            sha256: sha.into(),
            size: 1,
            replicas: 2,
            version: 1,
            writer: "SHA256:w".into(),
            deleted: false,
            mtime: 0,
        }
    }

    #[test]
    fn revoked_nodes_are_not_candidates_and_self_always_is() {
        let registry = Registry {
            authorized_nodes: vec![
                node("SHA256:a", NodeState::Admitted, Some("127.0.0.1:1")),
                node("SHA256:b", NodeState::Revoked, Some("127.0.0.1:2")),
                node("SHA256:c", NodeState::Admitted, None),
            ],
        };
        let members = members_of("SHA256:self", &registry);
        assert_eq!(
            members.admitted,
            vec!["SHA256:a", "SHA256:c", "SHA256:self"]
        );
        assert_eq!(members.reachable, vec!["SHA256:a"]);
    }

    #[test]
    fn a_peer_down_past_the_grace_leaves_the_placement() {
        let registry = Registry {
            authorized_nodes: vec![
                node("SHA256:pa", NodeState::Admitted, Some("127.0.0.1:1")),
                node("SHA256:pb", NodeState::Admitted, Some("127.0.0.1:2")),
                node("SHA256:pc", NodeState::Admitted, Some("127.0.0.1:3")),
            ],
        };
        let members = members_of("SHA256:pself", &registry);
        let mut item = entry("x", &"e".repeat(64));
        item.replicas = 2;
        let before = live_placement(&item, &members);
        let lost = before
            .iter()
            .find(|fp| **fp != "SHA256:pself")
            .unwrap()
            .clone();
        // Failing just now is not yet down (grace is 120 s by default).
        record_health(&lost, false);
        assert_eq!(live_placement(&item, &members), before);
        // Backdate the failure past the grace.
        health().lock().unwrap().insert(
            lost.clone(),
            Instant::now() - DEFAULT_DOWN_GRACE - Duration::from_secs(1),
        );
        let after = live_placement(&item, &members);
        assert!(!after.contains(&lost));
        assert_eq!(after.len(), 2);
        record_health(&lost, true);
        assert_eq!(live_placement(&item, &members), before);
    }

    #[test]
    fn index_sync_pages_and_reports_unchanged() {
        let (root, store) = scratch_store();
        let entries: Vec<VfsEntry> = (0..300)
            .map(|index| entry(&format!("f{index:03}"), &"a".repeat(64)))
            .collect();
        store.merge_entries(entries).unwrap();
        let first = serve_with(
            &store,
            &Request::ClusterVfsIndexSync {
                digest: None,
                offset: 0,
            },
        )
        .unwrap();
        let Response::ClusterVfsIndexPage {
            digest,
            entries_json,
            next_offset,
            unchanged,
            ..
        } = first
        else {
            panic!("expected a page");
        };
        assert!(!unchanged);
        assert_eq!(next_offset, Some(INDEX_PAGE_ENTRIES));
        assert_eq!(
            serde_json::from_str::<Vec<VfsEntry>>(&entries_json)
                .unwrap()
                .len(),
            INDEX_PAGE_ENTRIES
        );
        let second = serve_with(
            &store,
            &Request::ClusterVfsIndexSync {
                digest: None,
                offset: INDEX_PAGE_ENTRIES,
            },
        )
        .unwrap();
        let Response::ClusterVfsIndexPage {
            next_offset,
            entries_json,
            ..
        } = second
        else {
            panic!()
        };
        assert_eq!(next_offset, None);
        assert_eq!(
            serde_json::from_str::<Vec<VfsEntry>>(&entries_json)
                .unwrap()
                .len(),
            44
        );
        let same = serve_with(
            &store,
            &Request::ClusterVfsIndexSync {
                digest: Some(digest),
                offset: 0,
            },
        )
        .unwrap();
        assert!(matches!(
            same,
            Response::ClusterVfsIndexPage {
                unchanged: true,
                ..
            }
        ));
        assert!(serve_with(
            &store,
            &Request::ClusterVfsIndexSync {
                digest: None,
                offset: 999
            }
        )
        .is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn blob_get_serves_chunks_and_stat_reports_presence() {
        let (root, store) = scratch_store();
        let data: Vec<u8> = (0..(BLOB_CHUNK_BYTES + 10))
            .map(|i| (i % 251) as u8)
            .collect();
        let hash = store.put_blob(&data).unwrap();
        let Response::ClusterVfsBlobChunk {
            total, data_b64, ..
        } = serve_with(
            &store,
            &Request::ClusterVfsBlobGet {
                sha256: hash.clone(),
                offset: 0,
            },
        )
        .unwrap()
        else {
            panic!()
        };
        assert_eq!(total, data.len() as u64);
        assert_eq!(b64().decode(data_b64).unwrap().len(), BLOB_CHUNK_BYTES);
        let Response::ClusterVfsBlobChunk { data_b64, .. } = serve_with(
            &store,
            &Request::ClusterVfsBlobGet {
                sha256: hash.clone(),
                offset: BLOB_CHUNK_BYTES as u64,
            },
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(b64().decode(data_b64).unwrap(), &data[BLOB_CHUNK_BYTES..]);
        let missing = "f".repeat(64);
        let Response::ClusterVfsBlobHave(have) = serve_with(
            &store,
            &Request::ClusterVfsBlobStat {
                hashes: vec![hash.clone(), missing.clone(), "../x".into()],
            },
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(have, vec![true, false, false]);
        assert_eq!(
            serve_with(
                &store,
                &Request::ClusterVfsBlobGet {
                    sha256: missing,
                    offset: 0
                }
            )
            .unwrap_err()
            .kind,
            VfsErrorKind::NotLocal
        );
        assert!(serve_with(
            &store,
            &Request::ClusterVfsBlobGet {
                sha256: "../../etc".into(),
                offset: 0
            }
        )
        .is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn blob_get_refuses_to_serve_a_damaged_copy() {
        let (root, store) = scratch_store();
        let hash = store.put_blob(b"good content").unwrap();
        std::fs::write(store.blob_path(&hash).unwrap(), b"bad content!").unwrap();
        let request = Request::ClusterVfsBlobGet {
            sha256: hash.clone(),
            offset: 0,
        };
        assert_eq!(
            serve_with(&store, &request).unwrap_err().kind,
            VfsErrorKind::Corrupt
        );
        // The damaged copy is gone, so this node now reports it does not
        // hold the blob and its repair pass fetches a good one.
        assert_eq!(
            serve_with(&store, &request).unwrap_err().kind,
            VfsErrorKind::NotLocal
        );
        assert!(!store.blob_present(&hash));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn index_update_merges_and_refuses_oversize() {
        let (root, store) = scratch_store();
        let update = serde_json::to_string(&vec![entry("a", &"b".repeat(64))]).unwrap();
        let ack = serve_with(
            &store,
            &Request::ClusterVfsIndexUpdate {
                entries_json: update.clone(),
            },
        )
        .unwrap();
        assert!(matches!(ack, Response::ClusterVfsAck { applied: 1, .. }));
        let again = serve_with(
            &store,
            &Request::ClusterVfsIndexUpdate {
                entries_json: update,
            },
        )
        .unwrap();
        assert!(matches!(again, Response::ClusterVfsAck { applied: 0, .. }));
        let huge = "x".repeat(MAX_UPDATE_JSON_BYTES + 1);
        assert!(serve_with(
            &store,
            &Request::ClusterVfsIndexUpdate { entries_json: huge }
        )
        .is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn batches_respect_the_request_cap() {
        let long = "p".repeat(900);
        let entries: Vec<VfsEntry> = (0..200)
            .map(|index| entry(&format!("{long}{index}"), &"c".repeat(64)))
            .collect();
        let batches = batch_entries(&entries);
        assert!(batches.len() > 1);
        for batch in &batches {
            assert!(batch.len() <= MAX_UPDATE_JSON_BYTES);
            let request = serde_json::to_vec(&Request::ClusterVfsIndexUpdate {
                entries_json: batch.clone(),
            })
            .unwrap();
            assert!(request.len() <= crate::net::frame::MAX_REQUEST_PAYLOAD);
        }
        // Quote-heavy paths double in size when escaped.
        let quoted: Vec<VfsEntry> = (0..100)
            .map(|index| entry(&format!("{}{index}", "\"".repeat(900)), &"c".repeat(64)))
            .collect();
        for batch in batch_entries(&quoted) {
            let request = serde_json::to_vec(&Request::ClusterVfsIndexUpdate {
                entries_json: batch,
            })
            .unwrap();
            assert!(request.len() <= crate::net::frame::MAX_REQUEST_PAYLOAD);
        }
        let total: usize = batches
            .iter()
            .map(|batch| serde_json::from_str::<Vec<VfsEntry>>(batch).unwrap().len())
            .sum();
        assert_eq!(total, 200);
    }

    /// `fetch` that answers from `server` in this process, counting requests.
    fn via<'a>(
        server: &'a VfsStore,
        calls: &'a std::cell::Cell<usize>,
    ) -> impl FnMut(&Request) -> Result<Response, String> + 'a {
        move |request| {
            calls.set(calls.get() + 1);
            let Request::ClusterVfsChanges {
                index_id,
                since,
                limit,
            } = request
            else {
                return Err("unexpected request".into());
            };
            serve_changes(server, "SHA256:client", index_id.as_deref(), *since, *limit)
                .map_err(|error| error.to_string())
        }
    }

    fn pull_from(client: &VfsStore, server: &VfsStore, peer: &str) -> (usize, usize) {
        let calls = std::cell::Cell::new(0);
        let merged = pull_changes(client, peer, &mut via(server, &calls))
            .unwrap()
            .unwrap();
        (merged, calls.get())
    }

    #[test]
    fn an_unchanged_peer_costs_one_request_and_one_put_moves_one_row() {
        let (_a, server) = scratch_store();
        let (_b, client) = scratch_store();
        let rows: Vec<VfsEntry> = (0..9_999)
            .map(|i| entry(&format!("d{}/f{i:05}", i % 50), &format!("{i:064x}")))
            .collect();
        server.merge_entries(rows).unwrap();
        let (merged, calls) = pull_from(&client, &server, "SHA256:server");
        assert_eq!(merged, 9_999);
        assert!(calls >= 9_999 / INDEX_PAGE_ENTRIES, "{calls} pages");
        assert_eq!(client.load_index().unwrap(), server.load_index().unwrap());

        assert_eq!(pull_from(&client, &server, "SHA256:server"), (0, 1));
        server.put("new.txt", b"new", 2, "SHA256:server").unwrap();
        assert_eq!(pull_from(&client, &server, "SHA256:server"), (1, 1));
        server.delete("new.txt", "SHA256:server").unwrap();
        assert_eq!(pull_from(&client, &server, "SHA256:server"), (1, 1));
        assert!(client.load_index().unwrap().entries["new.txt"].deleted);
    }

    #[test]
    fn merged_rows_relay_to_a_peer_that_never_talks_to_the_writer() {
        let (_a, a) = scratch_store();
        let (_b, b) = scratch_store();
        let (_c, c) = scratch_store();
        a.put("from-a.txt", b"a", 2, "SHA256:a").unwrap();
        pull_from(&b, &a, "SHA256:a");
        assert_eq!(pull_from(&c, &b, "SHA256:b").0, 1);
        assert!(c.load_index().unwrap().live("from-a.txt").is_some());
    }

    #[test]
    fn a_recreated_peer_index_resets_the_cursor_and_is_pulled_whole() {
        let (_a, server) = scratch_store();
        let (_b, client) = scratch_store();
        server.put("one.txt", b"1", 2, "SHA256:s").unwrap();
        pull_from(&client, &server, "SHA256:server");
        // The peer loses its index and starts again with a new index id.
        let (_c, fresh) = scratch_store();
        fresh.put("two.txt", b"2", 2, "SHA256:s").unwrap();
        let (merged, calls) = pull_from(&client, &fresh, "SHA256:server");
        assert_eq!((merged, calls), (1, 2), "one reset, then the whole index");
        let index = client.load_index().unwrap();
        assert!(index.live("one.txt").is_some() && index.live("two.txt").is_some());
    }

    #[test]
    fn a_page_that_fails_to_merge_leaves_the_cursor_on_the_last_good_page() {
        let (_a, server) = scratch_store();
        let (_b, client) = scratch_store();
        let rows: Vec<VfsEntry> = (0..600)
            .map(|i| entry(&format!("f{i:04}"), &format!("{i:064x}")))
            .collect();
        server.merge_entries(rows).unwrap();
        let mut pages = 0;
        let calls = std::cell::Cell::new(0);
        let mut inner = via(&server, &calls);
        let error = pull_changes(&client, "SHA256:server", &mut |request| {
            pages += 1;
            let mut response = inner(request)?;
            if pages == 2 {
                if let Response::ClusterVfsChangesPage { entries_json, .. } = &mut response {
                    *entries_json = "not json".into();
                }
            }
            Ok(response)
        })
        .unwrap_err();
        assert_eq!(error, "bad changes page");
        let (_, cursor) = client.cursor("SHA256:server").unwrap();
        assert_eq!(
            client.load_index().unwrap().entries.len(),
            INDEX_PAGE_ENTRIES
        );
        assert!(cursor > 0);
        // The next pull continues from there and ends with every row.
        pull_from(&client, &server, "SHA256:server");
        assert_eq!(client.load_index().unwrap(), server.load_index().unwrap());
    }

    #[test]
    fn a_peer_that_does_not_know_change_cursors_is_reported_for_fallback() {
        let (_b, client) = scratch_store();
        let answer = pull_changes(&client, "SHA256:old", &mut |_| {
            Ok(Response::error("invalid cluster request"))
        });
        assert_eq!(answer, Ok(None));
    }
}
