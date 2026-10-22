//! Peer half of the cluster virtual file store (#742): index replication,
//! blob pulls, and the repair pass that restores copies after a node is lost.
//! Content moves by pull only: a cluster request carries about 64 KB, while
//! a response may carry 4 MiB.

use super::vfs::{unix_now, VfsError, VfsErrorKind, VfsIndex, VfsResult, VfsStore, MAX_BLOB_BYTES};
use super::vfs_repair::RepairQueue;
use super::{NodeState, Registry};
use crate::net::cluster_client::{ClientTimeouts, ClusterClient};
use base64::Engine;
use remuda_core::protocol::{Request, Response};
use remuda_core::vfs::{self as rules, VfsEntry};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Index entries per sync page and per update request.
pub const INDEX_PAGE_ENTRIES: usize = 256;
pub const MAX_UPDATE_JSON_BYTES: usize = 48 * 1024;
pub const MAX_STAT_HASHES: usize = 64;
pub const BLOB_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_REPAIRS_PER_PASS: usize = 256;
const TAKE_TTL: Duration = Duration::from_secs(60 * 60);
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
    if let Some((seen_seq, seen_index_id)) = store.check_puller(peer_fp, since, unix_now())? {
        return Ok(Response::ClusterVfsChangesPage {
            index_id: store.index_id()?,
            entries_json: "[]".into(),
            last_seq: 0,
            more: false,
            reset: true,
            stale: true,
            seen_seq,
            seen_index_id,
            caps: caps(),
        });
    }
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
        stale: false,
        seen_seq: 0,
        seen_index_id: None,
        caps: caps(),
    })
}

/// Requests beyond the base set that this build serves, sent with each page.
fn caps() -> Vec<String> {
    vec![remuda_core::protocol::VFS_CAP_TAKE.to_owned()]
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
        Request::ClusterVfsTake { hashes } => {
            if hashes.len() > MAX_STAT_HASHES {
                return Err(VfsError::new(VfsErrorKind::TooLarge, "too many hashes"));
            }
            Ok(Response::ClusterVfsAck {
                digest: String::new(),
                applied: take(store, hashes)?,
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

/// How long `peer_fp` has been down, if it is.
pub(super) fn down_for(peer_fp: &str) -> Option<Duration> {
    let since = *lock(health()).get(peer_fp)?;
    (since.elapsed() >= down_grace()).then(|| since.elapsed())
}

/// Whether `peer_fp` answered without change cursors (an older build).
pub(super) fn is_legacy(peer_fp: &str) -> bool {
    lock(&LEGACY_PEERS).contains_key(peer_fp)
}

/// Copies the repair queue holds back for a later try.
pub(super) fn retry_waiting() -> usize {
    lock(repair_queue()).waiting(Instant::now())
}

pub(super) fn is_down(peer_fp: &str) -> bool {
    health()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(peer_fp)
        .is_some_and(|since| since.elapsed() >= down_grace())
}

// ---------------------------------------------------------------------------
// Membership and placement.

pub(super) struct Members {
    pub(super) self_fp: String,
    /// Admitted nodes, this one included.
    pub(super) admitted: Vec<String>,
    /// Admitted peers with an endpoint, which this node can reach.
    reachable: Vec<String>,
}

impl Members {
    /// The same members with `node` gone, as if it were already revoked.
    pub(super) fn without(&self, node: &str) -> Members {
        let keep = |list: &[String]| list.iter().filter(|fp| *fp != node).cloned().collect();
        Members {
            self_fp: self.self_fp.clone(),
            admitted: keep(&self.admitted),
            reachable: keep(&self.reachable),
        }
    }
}

pub(super) fn members() -> VfsResult<Members> {
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

pub(super) fn live_placement(entry: &VfsEntry, members: &Members) -> Vec<String> {
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

pub(super) fn request_peer(peer_fp: &str, request: &Request) -> Result<Response, String> {
    let result = request_peer_inner(peer_fp, request);
    record_health(peer_fp, result.is_ok());
    result
}

fn request_peer_inner(peer_fp: &str, request: &Request) -> Result<Response, String> {
    request_peer_within(peer_fp, request, REQUEST_TIMEOUT)
}

/// [`request_peer`] with its own answer timeout and no health record.
pub(super) fn request_peer_within(
    peer_fp: &str,
    request: &Request,
    timeout: Duration,
) -> Result<Response, String> {
    let material = super::replication::peer_material(peer_fp).map_err(|error| error.to_string())?;
    let client = ClusterClient::with_timeouts(
        Arc::new(crate::SystemWallClock::new()),
        ClientTimeouts {
            connect: Duration::from_secs(3),
            read: timeout,
            total: timeout,
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
/// What a listener answers to a request it cannot decode (an older build).
pub(super) const OLD_BUILD_REFUSALS: [&str; 2] =
    ["invalid remote request", "invalid cluster request"];

/// Pull what `peer` changed since our cursor; fall back to the whole-index
/// digest pull for a peer that does not know change cursors.
fn pull(store: &VfsStore, me: &str, peer: &str) -> Result<usize, String> {
    let legacy = LEGACY_PEERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(peer)
        .is_some_and(|since| since.elapsed() < LEGACY_RETRY);
    if legacy {
        return pull_index(store, peer);
    }
    match pull_changes(store, me, peer, &mut |request| request_peer(peer, request)) {
        Ok(Some(merged)) => Ok(merged),
        Ok(None) => {
            let merged = pull_index(store, peer)?;
            LEGACY_PEERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(peer.to_owned(), Instant::now());
            Ok(merged)
        }
        // A timeout or a bad page is not an older build: pull the whole
        // index this once, and ask for changes again next time.
        Err(_) => pull_index(store, peer),
    }
}

/// Page through `peer`'s changes after our cursor, merging each page with
/// its cursor in one transaction. `Ok(None)`: the peer does not speak it.
pub fn pull_changes(
    store: &VfsStore,
    me: &str,
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
            stale,
            seen_seq,
            seen_index_id,
            caps,
        } = response
        else {
            return match response {
                Response::Error(reason) if !OLD_BUILD_REFUSALS.contains(&reason.as_str()) => {
                    Err(reason)
                }
                _ => Ok(None),
            };
        };
        lock(peer_caps()).insert(peer.to_owned(), caps);
        if reset {
            resets += 1;
            if resets > 1 {
                return Err("peer reset the change cursor twice".into());
            }
            if stale {
                let dropped = store
                    .rejoin(me, seen_seq, seen_index_id.as_deref())
                    .map_err(|e| e.to_string())?;
                eprintln!(
                    "remuda: vfs: {} says this node was away longer than the stale horizon; dropped {dropped} index rows to pull them again",
                    super::node_label(peer)
                );
            } else {
                store.reset_cursor(peer).map_err(|e| e.to_string())?;
            }
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
        let holders = count_holders(&store, &members.self_fp, entry, &wanted);
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

/// How many of `nodes` hold the entry's blob, asking peers by `BlobStat`.
fn count_holders(store: &VfsStore, self_fp: &str, entry: &VfsEntry, nodes: &[String]) -> usize {
    let has = |node: &String| {
        if node == self_fp {
            return store.has_blob(&entry.sha256, entry.size);
        }
        matches!(
            request_peer(node, &Request::ClusterVfsBlobStat { hashes: vec![entry.sha256.clone()] }),
            Ok(Response::ClusterVfsBlobHave(have)) if have.first() == Some(&true)
        )
    };
    nodes.iter().filter(|node| has(node)).count()
}

/// Poll until `min` nodes hold the entry's blob, this one included, or
/// `timeout` passes. Returns the last count; the caller compares it to `min`.
pub fn wait_for_copies(entry: &VfsEntry, min: usize, timeout: Duration) -> VfsResult<usize> {
    let deadline = Instant::now() + timeout;
    let store = VfsStore::open_default()?;
    loop {
        let members = members()?;
        let mut nodes = vec![members.self_fp.clone()];
        nodes.extend(members.reachable.iter().cloned());
        let holders = count_holders(&store, &members.self_fp, entry, &nodes);
        if holders >= min || Instant::now() >= deadline {
            return Ok(holders);
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
    pub copies_waiting: usize,
    pub copies_trimmed: usize,
    pub garbage_removed: usize,
    pub tombstones_removed: usize,
    pub error: Option<String>,
}

impl std::fmt::Display for SyncReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(error) = &self.error {
            return write!(formatter, "vfs sync failed: {error}");
        }
        write!(
            formatter,
            "Peers reached {}, unreachable {}; index entries merged {}; copies fetched {}, failed {}, waiting to retry {}, trimmed {}; old blobs removed {}, old deletes forgotten {}.",
            self.peers_reached,
            self.peers_failed,
            self.entries_merged,
            self.copies_fetched,
            self.copies_failed,
            self.copies_waiting,
            self.copies_trimmed,
            self.garbage_removed,
            self.tombstones_removed
        )
    }
}

/// Pull indexes from every reachable peer, then fetch the copies this node
/// should hold under the current placement. `force` skips retry waits.
pub fn sync_once(force: bool) -> SyncReport {
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
        match pull(&store, &members.self_fp, peer) {
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
    repair(&store, &members, &index, force, &mut report);
    report.copies_trimmed = trim(&store, &members, &index);
    report.garbage_removed = store.collect_garbage().unwrap_or(0);
    let others: Vec<String> = members
        .admitted
        .iter()
        .filter(|fp| **fp != members.self_fp)
        .cloned()
        .collect();
    report.tombstones_removed = store.collect_tombstones(&others, unix_now()).unwrap_or(0);
    report
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Blobs a draining peer asked this node to keep, and when it asked.
fn takes() -> &'static Mutex<HashMap<String, Instant>> {
    static TAKES: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    TAKES.get_or_init(Mutex::default)
}

/// Taken blobs not yet fetched; a request lapses after `TAKE_TTL`.
fn taken() -> HashSet<String> {
    let mut takes = lock(takes());
    takes.retain(|_, asked| asked.elapsed() < TAKE_TTL);
    takes.keys().cloned().collect()
}

/// `ClusterVfsTake`: queue the live blobs this node lacks for the repair
/// pass. Returns how many were queued.
fn take(store: &VfsStore, hashes: &[String]) -> VfsResult<usize> {
    let index = store.load_index()?;
    let live: HashMap<&str, &VfsEntry> = index
        .entries
        .values()
        .filter(|entry| !entry.deleted)
        .map(|entry| (entry.sha256.as_str(), entry))
        .collect();
    let mut queued = 0;
    for hash in hashes {
        if let Some(entry) = live.get(hash.as_str()) {
            if !store.has_blob(&entry.sha256, entry.size) {
                lock(takes()).insert(hash.clone(), Instant::now());
                queued += 1;
            }
        }
    }
    Ok(queued)
}

/// What each peer listed in `caps` on its last change page.
pub(super) fn peer_caps() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static CAPS: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    CAPS.get_or_init(Mutex::default)
}

fn repair_queue() -> &'static Mutex<RepairQueue> {
    static QUEUE: OnceLock<Mutex<RepairQueue>> = OnceLock::new();
    QUEUE.get_or_init(Mutex::default)
}

/// Fetch the missing copies this node should hold, through the repair queue.
fn repair(
    store: &VfsStore,
    members: &Members,
    index: &VfsIndex,
    force: bool,
    report: &mut SyncReport,
) {
    let taken = taken();
    let mut missing: BTreeMap<&str, &VfsEntry> = BTreeMap::new();
    for entry in index.entries.values().filter(|entry| !entry.deleted) {
        if !missing.contains_key(entry.sha256.as_str())
            && !store.has_blob(&entry.sha256, entry.size)
            && (taken.contains(&entry.sha256)
                || live_placement(entry, members).contains(&members.self_fp))
        {
            missing.insert(&entry.sha256, entry);
        }
    }
    let wanted: Vec<String> = missing.keys().map(|sha| (*sha).to_owned()).collect();
    let due = lock(repair_queue()).due(&wanted, Instant::now(), force, MAX_REPAIRS_PER_PASS);
    for sha in &due {
        // A copy taken for a draining peer is kept, not trimmed later.
        let fetched = fetch_blob(store, missing[sha.as_str()]).is_ok()
            && (taken.contains(sha) || store.mark_stand_in(sha).is_ok());
        if fetched {
            lock(takes()).remove(sha);
        }
        lock(repair_queue()).tried(sha, fetched, Instant::now());
        if fetched {
            report.copies_fetched += 1;
        } else {
            report.copies_failed += 1;
        }
    }
    report.copies_waiting = lock(repair_queue()).waiting(Instant::now());
}

/// Delete stand-in copies this node no longer places, once every placement
/// node confirms a copy. Writer copies and kept conflicts stay.
fn trim(store: &VfsStore, members: &Members, index: &VfsIndex) -> usize {
    let Ok(marked) = store.stand_ins() else {
        return 0;
    };
    let mut users: HashMap<&str, Vec<&VfsEntry>> = HashMap::new();
    let mut pinned: HashSet<&str> = HashSet::new();
    for entry in index.entries.values() {
        pinned.extend(
            entry
                .conflicts
                .iter()
                .map(|conflict| conflict.sha256.as_str()),
        );
        if entry.deleted {
            continue;
        }
        if entry.writer == members.self_fp {
            pinned.insert(&entry.sha256);
        }
        users.entry(&entry.sha256).or_default().push(entry);
    }
    let mut trimmed = 0;
    for sha in &marked {
        let Some(entries) = users.get(sha.as_str()) else {
            continue; // unreferenced: blob GC removes it with its mark
        };
        if pinned.contains(sha.as_str()) {
            continue;
        }
        let mut holders = BTreeSet::new();
        for entry in entries {
            holders.extend(live_placement(entry, members));
        }
        if holders.is_empty() || holders.contains(&members.self_fp) {
            continue;
        }
        let holders: Vec<String> = holders.into_iter().collect();
        if count_holders(store, &members.self_fp, entries[0], &holders) == holders.len()
            && store.trim_stand_in(sha).unwrap_or(false)
        {
            trimmed += 1;
        }
    }
    trimmed
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
            let report = sync_once(false);
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
            vv: Default::default(),
            conflicts: Vec::new(),
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
        let merged = pull_changes(client, "SHA256:client", peer, &mut via(server, &calls))
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
    fn concurrent_writes_end_as_one_winner_and_one_conflict_on_every_node() {
        let (_a, a) = scratch_store();
        let (_b, b) = scratch_store();
        let (_c, c) = scratch_store();
        a.put("same.txt", b"from a", 2, "SHA256:a").unwrap();
        b.put("same.txt", b"from b", 2, "SHA256:b").unwrap();
        pull_from(&a, &b, "SHA256:b");
        pull_from(&b, &a, "SHA256:a");
        // C only talks to B, and still learns about the conflict.
        pull_from(&c, &b, "SHA256:b");
        let entries: Vec<VfsEntry> = [&a, &b, &c]
            .iter()
            .map(|store| store.load_index().unwrap().entries["same.txt"].clone())
            .collect();
        assert_eq!(entries[0], entries[1]);
        assert_eq!(entries[1], entries[2]);
        assert_eq!(
            entries[0].writer, "SHA256:b",
            "the usual tie-break still wins"
        );
        assert_eq!(entries[0].conflicts.len(), 1);
        assert_eq!(entries[0].conflicts[0].writer, "SHA256:a");
        // Pulling again changes nothing anywhere.
        assert_eq!(pull_from(&a, &b, "SHA256:b").0, 0);
        assert_eq!(pull_from(&c, &b, "SHA256:b").0, 0);
    }

    #[test]
    fn a_delete_is_forgotten_only_after_every_peer_pulled_it_and_it_aged() {
        let (_a, server) = scratch_store();
        let (_b, client) = scratch_store();
        server.put("gone.txt", b"x", 2, "SHA256:s").unwrap();
        let tombstone = server.delete("gone.txt", "SHA256:s").unwrap();
        // A later write elsewhere, so the tombstone is not the newest version.
        server.put("kept.txt", b"y", 2, "SHA256:s").unwrap();
        let later = tombstone.mtime + 8 * 86_400;
        let peers = ["SHA256:client".to_owned()];
        assert_eq!(
            server.collect_tombstones(&peers, later).unwrap(),
            0,
            "not pulled yet"
        );
        pull_from(&client, &server, "SHA256:server");
        assert_eq!(
            server.collect_tombstones(&peers, later).unwrap(),
            0,
            "pulled, not acked"
        );
        // The next pull says how far the client got: that is the ack.
        pull_from(&client, &server, "SHA256:server");
        assert_eq!(
            server
                .collect_tombstones(&peers, tombstone.mtime + 86_400)
                .unwrap(),
            0,
            "too young"
        );
        let two = [peers[0].clone(), "SHA256:silent".to_owned()];
        assert_eq!(
            server.collect_tombstones(&two, later).unwrap(),
            0,
            "a silent member blocks"
        );
        assert_eq!(server.collect_tombstones(&peers, later).unwrap(), 1);
        let index = server.load_index().unwrap();
        assert!(!index.entries.contains_key("gone.txt"));
        assert!(index.live("kept.txt").is_some());
        // The client still has the tombstone; pulling again brings nothing back.
        assert_eq!(pull_from(&client, &server, "SHA256:server").0, 0);
        assert!(client.load_index().unwrap().entries["gone.txt"].deleted);
    }

    #[test]
    fn a_member_away_past_the_stale_horizon_stops_blocking_and_rebuilds_on_return() {
        let (_a, server) = scratch_store();
        let (_b, client) = scratch_store();
        server.put("gone.txt", b"x", 2, "SHA256:s").unwrap();
        pull_from(&client, &server, "SHA256:server");
        // The client's last pull was 40 days ago.
        let now = unix_now();
        server
            .check_puller("SHA256:client", 0, now - 40 * 86_400)
            .unwrap();
        server.delete("gone.txt", "SHA256:s").unwrap();
        server.put("new.txt", b"y", 2, "SHA256:s").unwrap();
        let peers = ["SHA256:client".to_owned()];
        assert_eq!(
            server.collect_tombstones(&peers, now + 8 * 86_400).unwrap(),
            1
        );
        // Meanwhile the client wrote something of its own.
        client.put("mine.txt", b"m", 2, "SHA256:client").unwrap();

        let calls = std::cell::Cell::new(0);
        pull_changes(
            &client,
            "SHA256:client",
            "SHA256:server",
            &mut via(&server, &calls),
        )
        .unwrap()
        .unwrap();
        let index = client.load_index().unwrap();
        assert!(
            !index.entries.contains_key("gone.txt"),
            "a delete came back"
        );
        assert!(index.live("new.txt").is_some());
        assert!(
            index.live("mine.txt").is_some(),
            "its own write was dropped"
        );
        // Back in time: the next pull is an ordinary one.
        assert_eq!(pull_from(&client, &server, "SHA256:server"), (0, 1));
    }

    #[test]
    fn change_pages_keep_the_older_wire_shape_unless_stale() {
        let page = Response::ClusterVfsChangesPage {
            index_id: "i".into(),
            entries_json: "[]".into(),
            last_seq: 3,
            more: false,
            reset: false,
            stale: false,
            seen_seq: 0,
            seen_index_id: None,
            caps: vec![],
        };
        let json = serde_json::to_string(&page).unwrap();
        assert!(
            !json.contains("stale") && !json.contains("seen_index_id") && !json.contains("caps"),
            "{json}"
        );
        // A page from an older server reads as not stale.
        let older = json.replace(",\"seen_seq\":0", "");
        assert_eq!(serde_json::from_str::<Response>(&older).unwrap(), page);
    }

    #[test]
    fn take_queues_only_live_blobs_this_node_lacks_and_pages_offer_it() {
        let (_root, store) = scratch_store();
        let missing = "7".repeat(64);
        store
            .merge_entries(vec![entry("t/missing.txt", &missing)])
            .unwrap();
        let held = store.put("t/held.txt", b"held", 2, "SHA256:w").unwrap();
        let unknown = "8".repeat(64);
        let asked = Request::ClusterVfsTake {
            hashes: vec![missing.clone(), held.sha256.clone(), unknown.clone()],
        };
        let Response::ClusterVfsAck { applied, .. } = serve_with(&store, &asked).unwrap() else {
            panic!()
        };
        assert_eq!(applied, 1);
        assert!(taken().contains(&missing) && !taken().contains(&unknown));
        let too_many = Request::ClusterVfsTake {
            hashes: vec![missing; MAX_STAT_HASHES + 1],
        };
        assert!(serve_with(&store, &too_many).is_err());

        let Response::ClusterVfsChangesPage { caps, .. } =
            serve_changes(&store, "SHA256:p", None, u64::MAX, 1).unwrap()
        else {
            panic!()
        };
        assert_eq!(caps, [remuda_core::protocol::VFS_CAP_TAKE]);
    }

    #[test]
    fn a_member_that_pulled_recently_is_not_stale() {
        let (_a, server) = scratch_store();
        let (_b, client) = scratch_store();
        server.put("a.txt", b"a", 2, "SHA256:s").unwrap();
        server.delete("a.txt", "SHA256:s").unwrap();
        server.put("b.txt", b"b", 2, "SHA256:s").unwrap();
        pull_from(&client, &server, "SHA256:server");
        pull_from(&client, &server, "SHA256:server");
        let peers = ["SHA256:client".to_owned()];
        assert_eq!(
            server
                .collect_tombstones(&peers, unix_now() + 8 * 86_400)
                .unwrap(),
            1
        );
        // It acked past the collected row, so it is never told to rebuild.
        assert_eq!(
            server.check_puller("SHA256:client", 0, unix_now()).unwrap(),
            None
        );
        assert_eq!(pull_from(&client, &server, "SHA256:server"), (0, 1));
    }

    #[test]
    fn the_newest_version_and_deletes_with_conflicts_are_never_forgotten() {
        let (_a, store) = scratch_store();
        store.put("a.txt", b"a", 2, "SHA256:s").unwrap();
        let newest = store.delete("a.txt", "SHA256:s").unwrap();
        let later = newest.mtime + 30 * 86_400;
        assert_eq!(store.collect_tombstones(&[], later).unwrap(), 0);
        let next = store.put("b.txt", b"b", 2, "SHA256:s").unwrap();
        assert!(next.version > newest.version, "versions never go back");
        // A delete that won over a concurrent write keeps that write's content.
        let concurrent = VfsEntry {
            path: "c.txt".into(),
            sha256: store.put_blob(b"theirs").unwrap(),
            size: 6,
            replicas: 2,
            version: 1,
            writer: "SHA256:t".into(),
            deleted: false,
            mtime: 0,
            vv: VfsEntry::next_vv(None, "SHA256:t"),
            conflicts: Vec::new(),
        };
        store.put("c.txt", b"mine", 2, "SHA256:s").unwrap();
        store.delete("c.txt", "SHA256:s").unwrap();
        store.merge_entries(vec![concurrent]).unwrap();
        store.put("d.txt", b"d", 2, "SHA256:s").unwrap();
        assert_eq!(
            store.collect_tombstones(&[], later).unwrap(),
            1,
            "only a.txt"
        );
        let index = store.load_index().unwrap();
        assert!(index.entries["c.txt"].deleted && index.entries["c.txt"].conflicts.len() == 1);
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
        let error = pull_changes(&client, "SHA256:client", "SHA256:server", &mut |request| {
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
        let answer = pull_changes(&client, "SHA256:client", "SHA256:old", &mut |_| {
            Ok(Response::error("invalid cluster request"))
        });
        assert_eq!(answer, Ok(None));
    }

    #[test]
    fn a_busy_peer_is_an_error_not_an_older_build() {
        let (_b, client) = scratch_store();
        let answer = pull_changes(&client, "SHA256:client", "SHA256:busy", &mut |_| {
            Ok(Response::error("request timed out"))
        });
        assert_eq!(answer, Err("request timed out".to_owned()));
    }
}
