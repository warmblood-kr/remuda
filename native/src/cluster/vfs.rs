//! Local half of the cluster virtual file store (#742): content-addressed
//! blobs and the replicated name index on this node. The rules (URIs, paths,
//! placement, merge) are in `remuda_core::vfs`; peer transport is in
//! `vfs_sync`. See docs/design-742-cluster-vfs.md.

use super::storage;
use remuda_core::vfs::{self as rules, VfsEntry, VfsUri};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 10_000;
pub const DEFAULT_CLUSTER_NAME: &str = "default";
const INDEX_FORMAT: u32 = 1;
pub const GC_GRACE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// A failure a caller may want to tell apart (Lua reason codes use these).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VfsErrorKind {
    NoCluster,
    NotFound,
    NotLocal,
    BadUri,
    WrongCluster,
    Unreachable,
    Corrupt,
    TooLarge,
    Io,
}

impl VfsErrorKind {
    pub fn code(self) -> &'static str {
        match self {
            Self::NoCluster => "no_cluster",
            Self::NotFound => "not_found",
            Self::NotLocal => "not_local",
            Self::BadUri => "bad_uri",
            Self::WrongCluster => "wrong_cluster",
            Self::Unreachable => "unreachable",
            Self::Corrupt => "corrupt",
            Self::TooLarge => "too_large",
            Self::Io => "io",
        }
    }
}

#[derive(Debug)]
pub struct VfsError {
    pub kind: VfsErrorKind,
    pub detail: String,
}

impl VfsError {
    pub fn new(kind: VfsErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    /// `code: detail`, the form Lua callers receive.
    pub fn reason(&self) -> String {
        format!("{}: {}", self.kind.code(), self.detail)
    }
}

impl std::fmt::Display for VfsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for VfsError {}

impl From<io::Error> for VfsError {
    fn from(error: io::Error) -> Self {
        Self::new(VfsErrorKind::Io, error.to_string())
    }
}

pub type VfsResult<T> = Result<T, VfsError>;

/// Local settings. Not replicated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VfsSettings {
    #[serde(default = "default_cluster_name")]
    pub cluster_name: String,
    #[serde(default = "default_replicas")]
    pub default_replicas: u8,
}

fn default_cluster_name() -> String {
    DEFAULT_CLUSTER_NAME.into()
}

fn default_replicas() -> u8 {
    rules::DEFAULT_REPLICAS
}

impl Default for VfsSettings {
    fn default() -> Self {
        Self {
            cluster_name: default_cluster_name(),
            default_replicas: default_replicas(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct IndexFile {
    format: u32,
    entries: Vec<VfsEntry>,
}

/// The name index: one entry per path, live or tombstone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VfsIndex {
    pub entries: BTreeMap<String, VfsEntry>,
}

impl VfsIndex {
    pub fn live(&self, path: &str) -> Option<&VfsEntry> {
        self.entries.get(path).filter(|entry| !entry.deleted)
    }

    pub fn next_version(&self) -> u64 {
        self.entries
            .values()
            .map(|entry| entry.version)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }

    /// Stable digest of the whole index; equal digests mean equal indexes.
    pub fn digest(&self) -> String {
        let entries: Vec<&VfsEntry> = self.entries.values().collect();
        let canonical = serde_json::to_vec(&entries).unwrap_or_default();
        hex(&Sha256::digest(&canonical))
    }

    /// Merge one entry; returns true if it changed the index.
    pub fn merge(&mut self, incoming: VfsEntry) -> bool {
        match self.entries.get(&incoming.path) {
            Some(current) if !incoming.supersedes(current) => false,
            _ => {
                self.entries.insert(incoming.path.clone(), incoming);
                true
            }
        }
    }

    /// Live entries whose path starts with `prefix`.
    pub fn list(&self, prefix: &str) -> Vec<&VfsEntry> {
        self.entries
            .range(prefix.to_owned()..)
            .take_while(|(path, _)| path.starts_with(prefix))
            .map(|(_, entry)| entry)
            .filter(|entry| !entry.deleted)
            .collect()
    }
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0xf)] as char);
    }
    out
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// The store directory of one node.
#[derive(Clone, Debug)]
pub struct VfsStore {
    root: PathBuf,
}

impl VfsStore {
    /// The store of this node's cluster, or `NoCluster` when the node has no
    /// cluster identity.
    pub fn open_default() -> VfsResult<Self> {
        let cluster_dir = storage::cluster_state_dir()?.join("cluster");
        match fs::symlink_metadata(cluster_dir.join("identity.key")) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(VfsError::new(
                    VfsErrorKind::NoCluster,
                    "this node has no cluster; run `remuda cluster init` or `remuda cluster join`",
                ))
            }
            Err(error) => return Err(error.into()),
        }
        storage::verify_directory(&cluster_dir)?;
        Self::open_at(&cluster_dir.join("vfs"))
    }

    /// Open (creating if needed) a store rooted at `root`.
    pub fn open_at(root: &Path) -> VfsResult<Self> {
        storage::create_private_directory(root)?;
        storage::verify_directory(root)?;
        let blobs = root.join("blobs");
        storage::create_private_directory(&blobs)?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn lock(&self) -> io::Result<storage::StateLock> {
        storage::StateLock::acquire(&self.root)
    }

    pub fn settings(&self) -> VfsResult<VfsSettings> {
        match fs::read(self.root.join("vfs.json")) {
            Ok(bytes) => {
                let settings: VfsSettings = serde_json::from_slice(&bytes).map_err(|error| {
                    VfsError::new(VfsErrorKind::Corrupt, format!("vfs.json: {error}"))
                })?;
                rules::validate_cluster_name(&settings.cluster_name)
                    .map_err(|error| VfsError::new(VfsErrorKind::Corrupt, error.0))?;
                rules::validate_replicas(settings.default_replicas)
                    .map_err(|error| VfsError::new(VfsErrorKind::Corrupt, error.0))?;
                Ok(settings)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(VfsSettings::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save_settings(&self, settings: &VfsSettings) -> VfsResult<()> {
        rules::validate_cluster_name(&settings.cluster_name)
            .map_err(|error| VfsError::new(VfsErrorKind::BadUri, error.0))?;
        rules::validate_replicas(settings.default_replicas)
            .map_err(|error| VfsError::new(VfsErrorKind::BadUri, error.0))?;
        let _guard = self.lock()?;
        let bytes = serde_json::to_vec_pretty(settings).map_err(io::Error::other)?;
        storage::atomic_write(&self.root.join("vfs.json"), &bytes)?;
        Ok(())
    }

    /// Parse a URI and check it names this node's cluster.
    pub fn parse_uri(&self, text: &str) -> VfsResult<VfsUri> {
        let uri =
            VfsUri::parse(text).map_err(|error| VfsError::new(VfsErrorKind::BadUri, error.0))?;
        uri.check_cluster(&self.settings()?.cluster_name)
            .map_err(|error| VfsError::new(VfsErrorKind::WrongCluster, error.0))?;
        Ok(uri)
    }

    pub fn load_index(&self) -> VfsResult<VfsIndex> {
        let bytes = match fs::read(self.root.join("index.json")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(VfsIndex::default()),
            Err(error) => return Err(error.into()),
        };
        let file: IndexFile = serde_json::from_slice(&bytes).map_err(|error| {
            VfsError::new(VfsErrorKind::Corrupt, format!("index.json: {error}"))
        })?;
        if file.format != INDEX_FORMAT {
            return Err(VfsError::new(
                VfsErrorKind::Corrupt,
                format!("index.json format {} is not supported", file.format),
            ));
        }
        let mut index = VfsIndex::default();
        for entry in file.entries {
            index.merge(entry);
        }
        Ok(index)
    }

    fn save_index(&self, index: &VfsIndex) -> VfsResult<()> {
        let file = IndexFile {
            format: INDEX_FORMAT,
            entries: index.entries.values().cloned().collect(),
        };
        let bytes = serde_json::to_vec(&file).map_err(io::Error::other)?;
        storage::atomic_write(&self.root.join("index.json"), &bytes)?;
        Ok(())
    }

    /// Merge peer entries under the index lock. Invalid entries are skipped;
    /// a merge that would pass `MAX_ENTRIES` stops adding new paths. Returns
    /// the entries that changed the index.
    pub fn merge_entries(&self, incoming: Vec<VfsEntry>) -> VfsResult<Vec<VfsEntry>> {
        let _guard = self.lock()?;
        let mut index = self.load_index()?;
        let mut changed = Vec::new();
        for entry in incoming {
            if entry.validate().is_err() {
                continue;
            }
            if !index.entries.contains_key(&entry.path) && index.entries.len() >= MAX_ENTRIES {
                continue;
            }
            if index.merge(entry.clone()) {
                changed.push(entry);
            }
        }
        if !changed.is_empty() {
            self.save_index(&index)?;
        }
        Ok(changed)
    }

    /// Record that a peer index was merged at `now`.
    pub fn mark_synced(&self, now: u64) -> VfsResult<()> {
        storage::atomic_write(&self.root.join("synced_at"), now.to_string().as_bytes())?;
        Ok(())
    }

    pub fn synced_at(&self) -> Option<u64> {
        fs::read_to_string(self.root.join("synced_at"))
            .ok()
            .and_then(|text| text.trim().parse().ok())
    }

    /// Store content and name it at `path`. Returns the new entry.
    pub fn put(&self, path: &str, bytes: &[u8], replicas: u8, writer: &str) -> VfsResult<VfsEntry> {
        rules::validate_path(path).map_err(|error| VfsError::new(VfsErrorKind::BadUri, error.0))?;
        rules::validate_replicas(replicas)
            .map_err(|error| VfsError::new(VfsErrorKind::BadUri, error.0))?;
        let sha256 = self.put_blob(bytes)?;
        self.write_entry(|version| VfsEntry {
            path: path.to_owned(),
            sha256,
            size: bytes.len() as u64,
            replicas,
            version,
            writer: writer.to_owned(),
            deleted: false,
            mtime: unix_now(),
        })
    }

    /// Write a tombstone for a live path.
    pub fn delete(&self, path: &str, writer: &str) -> VfsResult<VfsEntry> {
        let _guard = self.lock()?;
        let mut index = self.load_index()?;
        let current = index
            .live(path)
            .cloned()
            .ok_or_else(|| VfsError::new(VfsErrorKind::NotFound, format!("no file at {path}")))?;
        let entry = VfsEntry {
            deleted: true,
            version: index.next_version(),
            writer: writer.to_owned(),
            mtime: unix_now(),
            ..current
        };
        index.merge(entry.clone());
        self.save_index(&index)?;
        Ok(entry)
    }

    fn write_entry(&self, make: impl FnOnce(u64) -> VfsEntry) -> VfsResult<VfsEntry> {
        let _guard = self.lock()?;
        let mut index = self.load_index()?;
        let entry = make(index.next_version());
        if !index.entries.contains_key(&entry.path) && index.entries.len() >= MAX_ENTRIES {
            return Err(VfsError::new(
                VfsErrorKind::TooLarge,
                format!("the store already holds {MAX_ENTRIES} paths"),
            ));
        }
        index.merge(entry.clone());
        self.save_index(&index)?;
        Ok(entry)
    }

    pub fn blob_path(&self, sha256: &str) -> VfsResult<PathBuf> {
        rules::validate_sha256(sha256)
            .map_err(|error| VfsError::new(VfsErrorKind::BadUri, error.0))?;
        Ok(self.root.join("blobs").join(&sha256[..2]).join(sha256))
    }

    pub fn has_blob(&self, sha256: &str) -> bool {
        self.blob_path(sha256)
            .map(|path| path.is_file())
            .unwrap_or(false)
    }

    pub fn blob_len(&self, sha256: &str) -> VfsResult<u64> {
        let path = self.blob_path(sha256)?;
        match fs::metadata(&path) {
            Ok(metadata) => Ok(metadata.len()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Err(VfsError::new(
                VfsErrorKind::NotLocal,
                format!("blob {sha256} is not on this node"),
            )),
            Err(error) => Err(error.into()),
        }
    }

    /// Store content addressed by its sha256. Idempotent.
    pub fn put_blob(&self, bytes: &[u8]) -> VfsResult<String> {
        if bytes.len() as u64 > MAX_BLOB_BYTES {
            return Err(VfsError::new(
                VfsErrorKind::TooLarge,
                format!("files are limited to {MAX_BLOB_BYTES} bytes"),
            ));
        }
        let sha256 = sha256_hex(bytes);
        let path = self.blob_path(&sha256)?;
        if !path.is_file() {
            if let Some(parent) = path.parent() {
                storage::create_private_directory(parent)?;
            }
            storage::atomic_write(&path, bytes)?;
        }
        Ok(sha256)
    }

    /// Store content received from a peer only if it matches `expected`.
    pub fn import_blob(&self, expected: &str, bytes: &[u8]) -> VfsResult<()> {
        rules::validate_sha256(expected)
            .map_err(|error| VfsError::new(VfsErrorKind::BadUri, error.0))?;
        if sha256_hex(bytes) != expected {
            return Err(VfsError::new(
                VfsErrorKind::Corrupt,
                format!("received content does not match {expected}"),
            ));
        }
        self.put_blob(bytes).map(|_| ())
    }

    /// Read and verify a whole local blob.
    pub fn read_blob(&self, sha256: &str) -> VfsResult<Vec<u8>> {
        let path = self.blob_path(sha256)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(VfsError::new(
                    VfsErrorKind::NotLocal,
                    format!("blob {sha256} is not on this node"),
                ))
            }
            Err(error) => return Err(error.into()),
        };
        if sha256_hex(&bytes) != sha256 {
            // Never serve damaged content; drop it so repair can fetch again.
            let _ = fs::remove_file(&path);
            return Err(VfsError::new(
                VfsErrorKind::Corrupt,
                format!("local copy of {sha256} was damaged and has been removed"),
            ));
        }
        Ok(bytes)
    }

    /// Read up to `max` bytes of a local blob from `offset` (no hash check:
    /// the receiver verifies the whole blob).
    pub fn read_blob_range(&self, sha256: &str, offset: u64, max: usize) -> VfsResult<Vec<u8>> {
        use std::io::{Seek, SeekFrom};
        let path = self.blob_path(sha256)?;
        let mut file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(VfsError::new(
                    VfsErrorKind::NotLocal,
                    format!("blob {sha256} is not on this node"),
                ))
            }
            Err(error) => return Err(error.into()),
        };
        file.seek(SeekFrom::Start(offset))?;
        let mut out = Vec::with_capacity(max.min(1 << 20));
        file.take(max as u64).read_to_end(&mut out)?;
        Ok(out)
    }

    /// Hashes of every local blob.
    pub fn local_blobs(&self) -> VfsResult<Vec<String>> {
        let mut out = Vec::new();
        let blobs = self.root.join("blobs");
        for shard in fs::read_dir(&blobs)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for blob in fs::read_dir(shard.path())? {
                let name = blob?.file_name();
                if let Some(name) = name.to_str() {
                    if rules::validate_sha256(name).is_ok() {
                        out.push(name.to_owned());
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Remove local blobs that no live entry references and that are older
    /// than `GC_GRACE`. The grace covers a put that wrote its blob but not yet
    /// its entry, and a pull that lands before the index merge.
    pub fn collect_garbage(&self) -> VfsResult<usize> {
        self.collect_garbage_older_than(GC_GRACE)
    }

    pub fn collect_garbage_older_than(&self, grace: std::time::Duration) -> VfsResult<usize> {
        let _guard = self.lock()?;
        let index = self.load_index()?;
        let referenced: std::collections::HashSet<&str> = index
            .entries
            .values()
            .filter(|entry| !entry.deleted)
            .map(|entry| entry.sha256.as_str())
            .collect();
        let mut removed = 0;
        for hash in self.local_blobs()? {
            if referenced.contains(hash.as_str()) {
                continue;
            }
            let path = self.blob_path(&hash)?;
            let old_enough = fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age >= grace);
            if old_enough && fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "remuda-vfs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }

        fn store(&self) -> VfsStore {
            VfsStore::open_at(&self.0.join("vfs")).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn put_then_read_round_trips_and_versions_increase() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let first = store.put("notes/a.md", b"hello", 3, "SHA256:me").unwrap();
        let second = store.put("notes/a.md", b"world", 3, "SHA256:me").unwrap();
        assert!(second.version > first.version);
        let index = store.load_index().unwrap();
        let live = index.live("notes/a.md").unwrap();
        assert_eq!(live.sha256, sha256_hex(b"world"));
        assert_eq!(store.read_blob(&live.sha256).unwrap(), b"world");
        assert!(store.has_blob(&first.sha256));
    }

    #[test]
    fn delete_writes_a_tombstone_that_old_copies_cannot_undo() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let written = store.put("a", b"x", 1, "SHA256:me").unwrap();
        store.delete("a", "SHA256:me").unwrap();
        assert!(store.load_index().unwrap().live("a").is_none());
        let changed = store.merge_entries(vec![written]).unwrap();
        assert!(changed.is_empty());
        assert!(store.load_index().unwrap().live("a").is_none());
        assert_eq!(
            store.delete("a", "SHA256:me").unwrap_err().kind,
            VfsErrorKind::NotFound
        );
    }

    #[test]
    fn merge_skips_invalid_peer_entries() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let good = VfsEntry {
            path: "ok".into(),
            sha256: "a".repeat(64),
            size: 1,
            replicas: 2,
            version: 5,
            writer: "SHA256:peer".into(),
            deleted: false,
            mtime: 0,
        };
        let mut bad = good.clone();
        bad.path = "../escape".into();
        let changed = store.merge_entries(vec![bad, good.clone()]).unwrap();
        assert_eq!(changed, vec![good]);
        // A later local write must outrank the merged peer version.
        let local = store.put("other", b"y", 1, "SHA256:me").unwrap();
        assert_eq!(local.version, 6);
    }

    #[test]
    fn import_refuses_content_that_does_not_match_its_hash() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let expected = sha256_hex(b"right");
        assert_eq!(
            store.import_blob(&expected, b"wrong").unwrap_err().kind,
            VfsErrorKind::Corrupt
        );
        assert!(!store.has_blob(&expected));
        store.import_blob(&expected, b"right").unwrap();
        assert!(store.has_blob(&expected));
    }

    #[test]
    fn damaged_local_copy_is_never_served() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let hash = store.put_blob(b"payload").unwrap();
        fs::write(store.blob_path(&hash).unwrap(), b"tampered").unwrap();
        assert_eq!(
            store.read_blob(&hash).unwrap_err().kind,
            VfsErrorKind::Corrupt
        );
        assert!(!store.has_blob(&hash));
    }

    #[test]
    fn range_reads_and_listing() {
        let scratch = Scratch::new();
        let store = scratch.store();
        store.put("d/one", b"0123456789", 1, "SHA256:me").unwrap();
        store.put("d/two", b"2", 1, "SHA256:me").unwrap();
        store.put("e/three", b"3", 1, "SHA256:me").unwrap();
        let index = store.load_index().unwrap();
        let listed: Vec<&str> = index.list("d/").iter().map(|e| e.path.as_str()).collect();
        assert_eq!(listed, vec!["d/one", "d/two"]);
        let hash = &index.live("d/one").unwrap().sha256;
        assert_eq!(store.read_blob_range(hash, 3, 4).unwrap(), b"3456");
        assert_eq!(store.read_blob_range(hash, 20, 4).unwrap(), b"");
    }

    #[test]
    fn garbage_collection_keeps_referenced_blobs() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let old = store.put("a", b"old", 1, "SHA256:me").unwrap();
        let new = store.put("a", b"new", 1, "SHA256:me").unwrap();
        assert_eq!(store.collect_garbage().unwrap(), 0, "fresh blobs are kept");
        assert!(store.has_blob(&old.sha256));
        assert_eq!(
            store
                .collect_garbage_older_than(std::time::Duration::ZERO)
                .unwrap(),
            1
        );
        assert!(!store.has_blob(&old.sha256));
        assert!(store.has_blob(&new.sha256));
    }

    #[test]
    fn settings_default_and_uri_cluster_check() {
        let scratch = Scratch::new();
        let store = scratch.store();
        assert_eq!(store.settings().unwrap(), VfsSettings::default());
        store
            .save_settings(&VfsSettings {
                cluster_name: "home".into(),
                default_replicas: 2,
            })
            .unwrap();
        assert!(store.parse_uri("remuda://home/a").is_ok());
        assert!(store.parse_uri("remuda:///a").is_ok());
        assert_eq!(
            store.parse_uri("remuda://work/a").unwrap_err().kind,
            VfsErrorKind::WrongCluster
        );
        assert_eq!(
            store.parse_uri("/tmp/a").unwrap_err().kind,
            VfsErrorKind::BadUri
        );
    }

    #[test]
    fn digest_tracks_content() {
        let mut left = VfsIndex::default();
        let mut right = VfsIndex::default();
        assert_eq!(left.digest(), right.digest());
        let entry = VfsEntry {
            path: "p".into(),
            sha256: "b".repeat(64),
            size: 1,
            replicas: 1,
            version: 1,
            writer: "w".into(),
            deleted: false,
            mtime: 0,
        };
        left.merge(entry.clone());
        assert_ne!(left.digest(), right.digest());
        right.merge(entry);
        assert_eq!(left.digest(), right.digest());
    }

    #[test]
    fn oversized_content_is_refused() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let big = vec![0u8; MAX_BLOB_BYTES as usize + 1];
        assert_eq!(
            store.put("big", &big, 1, "SHA256:me").unwrap_err().kind,
            VfsErrorKind::TooLarge
        );
    }
}
