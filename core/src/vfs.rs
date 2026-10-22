//! Cluster virtual file store rules (#742): URIs, path validation, replica
//! placement and the last-writer-wins merge, which keeps concurrent losers as
//! conflicts. Pure code, so every node computes the same answers. Storage and
//! transport live in `remuda-native`.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

pub const SCHEME: &str = "remuda://";
pub const MAX_PATH_BYTES: usize = 1024;
pub const MAX_COMPONENT_BYTES: usize = 255;
pub const MAX_CLUSTER_NAME_BYTES: usize = 64;
pub const DEFAULT_REPLICAS: u8 = 3;
pub const MAX_REPLICAS: u8 = 16;
pub const SHA256_HEX_LEN: usize = 64;
pub const MAX_VV_NODES: usize = 256;
pub const MAX_CONFLICTS: usize = 16;

/// Per writer node, the highest of its write counters an entry has seen.
pub type VersionVector = BTreeMap<String, u64>;

/// True when `a` has seen every write that `b` has seen.
pub fn vv_covers(a: &VersionVector, b: &VersionVector) -> bool {
    b.iter()
        .all(|(node, count)| a.get(node).is_some_and(|seen| seen >= count))
}

/// Why a URI, path or cluster name was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VfsError(pub String);

impl fmt::Display for VfsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for VfsError {}

fn error(message: impl Into<String>) -> VfsError {
    VfsError(message.into())
}

/// A parsed `remuda://<cluster>/<path>` URI. An empty cluster means "this
/// cluster".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VfsUri {
    pub cluster: Option<String>,
    pub path: String,
}

impl VfsUri {
    pub fn parse(text: &str) -> Result<Self, VfsError> {
        let rest = text
            .strip_prefix(SCHEME)
            .ok_or_else(|| error(format!("not a remuda:// URI: {text}")))?;
        let (cluster, path) = rest
            .split_once('/')
            .ok_or_else(|| error("remuda:// URI has no path"))?;
        let cluster = if cluster.is_empty() {
            None
        } else {
            validate_cluster_name(cluster)?;
            Some(cluster.to_owned())
        };
        validate_path(path)?;
        Ok(Self {
            cluster,
            path: path.to_owned(),
        })
    }

    /// Check that the URI belongs to `local_cluster`.
    pub fn check_cluster(&self, local_cluster: &str) -> Result<(), VfsError> {
        match &self.cluster {
            Some(name) if name != local_cluster => Err(error(format!(
                "URI names cluster '{name}', but this node is in cluster '{local_cluster}'"
            ))),
            _ => Ok(()),
        }
    }

    pub fn format(cluster: &str, path: &str) -> String {
        format!("{SCHEME}{cluster}/{path}")
    }
}

/// Cluster names are short labels: ASCII letters, digits, `-`, `_` and `.`.
pub fn validate_cluster_name(name: &str) -> Result<(), VfsError> {
    if name.is_empty() || name.len() > MAX_CLUSTER_NAME_BYTES {
        return Err(error(format!(
            "cluster name must be 1-{MAX_CLUSTER_NAME_BYTES} bytes"
        )));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(error(
            "cluster name may use only ASCII letters, digits, '-', '_' and '.'",
        ));
    }
    Ok(())
}

/// Validate a store path. Paths are relative and `/`-separated; the rules
/// keep every path safe to map onto a local file name on any platform.
pub fn validate_path(path: &str) -> Result<(), VfsError> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err(error(format!("path must be 1-{MAX_PATH_BYTES} bytes")));
    }
    if path
        .chars()
        .any(|ch| ch.is_control() || ch == '\\' || ch == '\0')
    {
        return Err(error("path may not contain control characters or '\\'"));
    }
    for component in path.split('/') {
        if component.is_empty() {
            return Err(error("path may not have empty components"));
        }
        if component == "." || component == ".." {
            return Err(error("path may not use '.' or '..'"));
        }
        if component.len() > MAX_COMPONENT_BYTES {
            return Err(error(format!(
                "path component must be at most {MAX_COMPONENT_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

/// Validate a lower-case hex sha256 digest. It is used as a file name, so
/// nothing else is accepted.
pub fn validate_sha256(hash: &str) -> Result<(), VfsError> {
    if hash.len() == SHA256_HEX_LEN
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(error("blob hash must be 64 lower-case hex digits"))
    }
}

pub fn validate_replicas(replicas: u8) -> Result<(), VfsError> {
    if (1..=MAX_REPLICAS).contains(&replicas) {
        Ok(())
    } else {
        Err(error(format!("replicas must be 1-{MAX_REPLICAS}")))
    }
}

/// One name in the store. Every node keeps every entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VfsEntry {
    pub path: String,
    pub sha256: String,
    pub size: u64,
    pub replicas: u8,
    pub version: u64,
    pub writer: String,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub mtime: u64,
    /// Empty from nodes older than step 4 of docs/design-778-vfs-delta-sync.md.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vv: VersionVector,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<VfsConflict>,
}

/// A write that lost to a concurrent one, kept with its content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VfsConflict {
    pub sha256: String,
    pub size: u64,
    pub replicas: u8,
    pub version: u64,
    pub writer: String,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub mtime: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vv: VersionVector,
}

impl VfsConflict {
    /// The losing write as an entry at `path`.
    pub fn entry(&self, path: &str) -> VfsEntry {
        VfsEntry {
            path: path.to_owned(),
            sha256: self.sha256.clone(),
            size: self.size,
            replicas: self.replicas,
            version: self.version,
            writer: self.writer.clone(),
            deleted: self.deleted,
            mtime: self.mtime,
            vv: self.vv.clone(),
            conflicts: Vec::new(),
        }
    }
}

impl VfsEntry {
    /// Check every field a peer could have sent.
    pub fn validate(&self) -> Result<(), VfsError> {
        validate_path(&self.path)?;
        validate_replicas(self.replicas)?;
        if self.deleted {
            if !self.sha256.is_empty() {
                validate_sha256(&self.sha256)?;
            }
        } else {
            validate_sha256(&self.sha256)?;
        }
        if self.writer.is_empty() || self.writer.len() > 128 {
            return Err(error("entry writer must be 1-128 bytes"));
        }
        if self.vv.len() > MAX_VV_NODES
            || self
                .vv
                .keys()
                .any(|node| node.is_empty() || node.len() > 128)
        {
            return Err(error("entry version vector is malformed"));
        }
        if self.conflicts.len() > MAX_CONFLICTS {
            return Err(error(format!(
                "an entry keeps at most {MAX_CONFLICTS} conflicts"
            )));
        }
        for conflict in &self.conflicts {
            conflict.entry(&self.path).validate()?;
        }
        Ok(())
    }

    /// This entry as a conflict, without its own conflicts.
    pub fn as_conflict(&self) -> VfsConflict {
        VfsConflict {
            sha256: self.sha256.clone(),
            size: self.size,
            replicas: self.replicas,
            version: self.version,
            writer: self.writer.clone(),
            deleted: self.deleted,
            mtime: self.mtime,
            vv: self.vv.clone(),
        }
    }

    /// The vector for a write by `node` on top of `current`. It covers the
    /// entry and all its conflicts, so any later write resolves them.
    pub fn next_vv(current: Option<&Self>, node: &str) -> VersionVector {
        let mut vv = VersionVector::new();
        let seen = current.into_iter().flat_map(|current| {
            std::iter::once(&current.vv).chain(current.conflicts.iter().map(|c| &c.vv))
        });
        for other in seen {
            for (writer, count) in other {
                let slot = vv.entry(writer.clone()).or_insert(0);
                *slot = (*slot).max(*count);
            }
        }
        let top = vv.values().copied().max().unwrap_or(0);
        vv.insert(node.to_owned(), top.saturating_add(1));
        while vv.len() > MAX_VV_NODES {
            // Forgetting a writer can only add a conflict, never lose a write.
            let oldest = vv
                .iter()
                .filter(|(writer, _)| writer.as_str() != node)
                .min_by_key(|(writer, count)| (**count, (*writer).clone()))
                .map(|(writer, _)| writer.clone());
            vv.remove(&oldest.unwrap_or_default());
        }
        vv
    }

    /// What to store when `incoming` meets `current`; `None` keeps `current`.
    /// `merge_order` picks the winner, as on older nodes. A loser that the
    /// winner's vector does not cover was concurrent: it becomes a conflict.
    pub fn merged(current: &Self, incoming: Self) -> Option<Self> {
        let order = incoming.merge_order(current);
        let (mut winner, loser) = if order == Ordering::Greater {
            (incoming, current.clone())
        } else {
            (current.clone(), incoming)
        };
        if order == Ordering::Equal {
            if winner.vv.is_empty() {
                winner.vv = loser.vv;
            }
            winner.conflicts.extend(loser.conflicts);
        } else if !winner.vv.is_empty() && !loser.vv.is_empty() {
            winner.conflicts.push(loser.as_conflict());
            winner.conflicts.extend(loser.conflicts);
        }
        winner.normalize_conflicts();
        (winner != *current).then_some(winner)
    }

    /// Drop conflicts this entry repeats or has seen; keep the newest few.
    pub fn normalize_conflicts(&mut self) {
        let path = self.path.clone();
        let (version, writer, vv) = (self.version, self.writer.clone(), self.vv.clone());
        self.conflicts.retain(|conflict| {
            let same = conflict.version == version && conflict.writer == writer;
            let seen = !vv.is_empty() && !conflict.vv.is_empty() && vv_covers(&vv, &conflict.vv);
            !same && !seen
        });
        self.conflicts
            .sort_by(|a, b| b.entry(&path).merge_order(&a.entry(&path)));
        self.conflicts
            .dedup_by(|a, b| a.entry(&path).merge_order(&b.entry(&path)) == Ordering::Equal);
        self.conflicts.truncate(MAX_CONFLICTS);
    }

    /// Order used by the merge: the higher `(version, writer)` wins. Equal
    /// keys with different content break the tie on the content itself, so
    /// two nodes always pick the same winner.
    pub fn merge_order(&self, other: &Self) -> Ordering {
        self.version
            .cmp(&other.version)
            .then_with(|| self.writer.cmp(&other.writer))
            .then_with(|| self.deleted.cmp(&other.deleted))
            .then_with(|| self.sha256.cmp(&other.sha256))
            .then_with(|| self.size.cmp(&other.size))
            .then_with(|| self.replicas.cmp(&other.replicas))
            .then_with(|| self.mtime.cmp(&other.mtime))
    }

    /// True when `self` should replace `current`.
    pub fn supersedes(&self, current: &Self) -> bool {
        self.merge_order(current) == Ordering::Greater
    }
}

/// The 64-bit FNV-1a hash. Stable across platforms and versions; it ranks
/// nodes, it does not protect content.
pub fn fnv1a64(parts: &[&[u8]]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in *part {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // Separator so ("ab","c") and ("a","bc") differ.
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Rank `candidates` for a blob by rendezvous hashing, best first. Duplicates
/// are removed. Ties (practically impossible) fall back to the fingerprint.
pub fn rank_nodes<'a>(blob_sha256: &str, candidates: &[&'a str]) -> Vec<&'a str> {
    let mut scored: Vec<(u64, &'a str)> = candidates
        .iter()
        .map(|node| (fnv1a64(&[node.as_bytes(), blob_sha256.as_bytes()]), *node))
        .collect();
    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(right.1)));
    scored.dedup_by(|left, right| left.1 == right.1);
    scored.into_iter().map(|(_, node)| node).collect()
}

/// The nodes that should hold a blob: the top `replicas` of the ranking.
pub fn placement<'a>(blob_sha256: &str, replicas: u8, candidates: &[&'a str]) -> Vec<&'a str> {
    let mut ranked = rank_nodes(blob_sha256, candidates);
    ranked.truncate(usize::from(replicas.max(1)));
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(version: u64, writer: &str, sha: char) -> VfsEntry {
        VfsEntry {
            path: "notes/a.md".into(),
            sha256: sha.to_string().repeat(64),
            size: 1,
            replicas: 3,
            version,
            writer: writer.into(),
            deleted: false,
            mtime: 0,
            vv: VersionVector::new(),
            conflicts: Vec::new(),
        }
    }

    #[test]
    fn parses_uris_with_and_without_cluster() {
        assert_eq!(
            VfsUri::parse("remuda://home/notes/a.md").unwrap(),
            VfsUri {
                cluster: Some("home".into()),
                path: "notes/a.md".into()
            }
        );
        assert_eq!(
            VfsUri::parse("remuda:///a").unwrap(),
            VfsUri {
                cluster: None,
                path: "a".into()
            }
        );
        assert_eq!(VfsUri::format("home", "x/y"), "remuda://home/x/y");
    }

    #[test]
    fn refuses_bad_uris() {
        for bad in [
            "mxc://home/a",
            "remuda://home",
            "remuda://home/",
            "remuda://ho me/a",
            "remuda://home//a",
            "remuda://home/a/../b",
            "remuda://home/./a",
            "remuda://home/a\\b",
            "remuda://home/a\nb",
            "remuda://home/a/",
        ] {
            assert!(VfsUri::parse(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn path_limits_are_enforced() {
        assert!(validate_path(&"a".repeat(MAX_COMPONENT_BYTES)).is_ok());
        assert!(validate_path(&"a".repeat(MAX_COMPONENT_BYTES + 1)).is_err());
        let long = vec!["a".repeat(200); 6].join("/");
        assert!(long.len() > MAX_PATH_BYTES);
        assert!(validate_path(&long).is_err());
        assert!(validate_path("한글/파일.md").is_ok());
    }

    #[test]
    fn cluster_mismatch_is_a_clear_error() {
        let uri = VfsUri::parse("remuda://work/a").unwrap();
        let message = uri.check_cluster("home").unwrap_err().to_string();
        assert!(message.contains("'work'") && message.contains("'home'"));
        assert!(VfsUri::parse("remuda:///a")
            .unwrap()
            .check_cluster("home")
            .is_ok());
    }

    #[test]
    fn sha256_must_be_lower_hex() {
        assert!(validate_sha256(&"a".repeat(64)).is_ok());
        assert!(validate_sha256(&"A".repeat(64)).is_err());
        assert!(validate_sha256(&"a".repeat(63)).is_err());
        assert!(validate_sha256("../../../../etc/passwd").is_err());
    }

    fn write(current: Option<&VfsEntry>, version: u64, writer: &str, sha: char) -> VfsEntry {
        VfsEntry {
            vv: VfsEntry::next_vv(current, writer),
            ..entry(version, writer, sha)
        }
    }

    #[test]
    fn concurrent_writes_keep_the_loser_on_every_node_in_any_order() {
        let base = write(None, 1, "SHA256:a", 'a');
        let left = write(Some(&base), 2, "SHA256:a", 'b');
        let right = write(Some(&base), 2, "SHA256:b", 'c');
        let one = VfsEntry::merged(&left, right.clone()).unwrap();
        let other = VfsEntry::merged(&right, left.clone()).unwrap_or(right.clone());
        assert_eq!(one, other);
        assert_eq!(one.sha256, right.sha256);
        assert_eq!(one.conflicts, vec![left.as_conflict()]);
        // Merging either side again changes nothing.
        assert!(VfsEntry::merged(&one, left).is_none());
        assert!(VfsEntry::merged(&one, right).is_none());
    }

    #[test]
    fn a_write_that_saw_the_other_is_not_a_conflict() {
        let first = write(None, 1, "SHA256:a", 'a');
        let second = write(Some(&first), 2, "SHA256:b", 'b');
        assert_eq!(
            VfsEntry::merged(&first, second.clone()),
            Some(second.clone())
        );
        assert!(VfsEntry::merged(&second, first).is_none());
    }

    #[test]
    fn entries_without_a_vector_merge_as_before() {
        let old = entry(2, "SHA256:old", 'a');
        let new = write(None, 1, "SHA256:new", 'b');
        let merged = VfsEntry::merged(&new, old.clone()).unwrap();
        assert_eq!(merged, old);
        assert!(VfsEntry::merged(&old, new).is_none());
    }

    #[test]
    fn the_next_write_covers_and_clears_every_conflict() {
        let base = write(None, 1, "SHA256:a", 'a');
        let left = write(Some(&base), 2, "SHA256:a", 'b');
        let right = write(Some(&base), 2, "SHA256:b", 'c');
        let conflicted = VfsEntry::merged(&left, right).unwrap();
        let resolved = write(Some(&conflicted), 3, "SHA256:a", 'b');
        let merged = VfsEntry::merged(&conflicted, resolved.clone()).unwrap();
        assert!(merged.conflicts.is_empty());
        assert_eq!(merged, resolved);
    }

    #[test]
    fn an_entry_relayed_without_its_vector_keeps_the_stored_one() {
        let base = write(None, 1, "SHA256:a", 'a');
        let left = write(Some(&base), 2, "SHA256:a", 'b');
        let right = write(Some(&base), 2, "SHA256:b", 'c');
        let conflicted = VfsEntry::merged(&left, right.clone()).unwrap();
        let stripped = VfsEntry {
            vv: VersionVector::new(),
            conflicts: Vec::new(),
            ..right.clone()
        };
        assert!(VfsEntry::merged(&conflicted, stripped.clone()).is_none());
        // And the other way round the vector and conflicts are filled in.
        assert_eq!(
            VfsEntry::merged(&stripped, conflicted.clone()),
            Some(conflicted)
        );
    }

    #[test]
    fn conflicts_are_capped_and_checked() {
        let mut many = write(None, 100, "SHA256:z", 'f');
        for version in 0..40 {
            let mut loser = write(None, version, &format!("SHA256:{version}"), 'a');
            loser.path.clone_from(&many.path);
            many.conflicts.push(loser.as_conflict());
        }
        many.normalize_conflicts();
        assert_eq!(many.conflicts.len(), MAX_CONFLICTS);
        assert_eq!(many.conflicts[0].version, 39);
        assert!(many.validate().is_ok());
        many.conflicts[0].sha256 = "bad".into();
        assert!(many.validate().is_err());
    }

    #[test]
    fn merge_prefers_higher_version_then_writer_then_content() {
        let old = entry(1, "SHA256:b", 'a');
        let new = entry(2, "SHA256:a", 'a');
        assert!(new.supersedes(&old));
        assert!(!old.supersedes(&new));
        let left = entry(2, "SHA256:a", 'a');
        let right = entry(2, "SHA256:b", 'a');
        assert!(right.supersedes(&left));
        let same_key_a = entry(2, "SHA256:a", 'a');
        let same_key_b = entry(2, "SHA256:a", 'b');
        assert!(same_key_b.supersedes(&same_key_a));
        assert!(!same_key_a.supersedes(&same_key_b));
        assert!(!same_key_a.supersedes(&same_key_a.clone()));
    }

    #[test]
    fn tombstone_beats_same_key_write() {
        let write = entry(3, "SHA256:a", 'a');
        let mut delete = write.clone();
        delete.deleted = true;
        assert!(delete.supersedes(&write));
    }

    #[test]
    fn entry_validation_rejects_peer_garbage() {
        let mut bad = entry(1, "SHA256:a", 'a');
        bad.sha256 = "../x".into();
        assert!(bad.validate().is_err());
        let mut bad = entry(1, "SHA256:a", 'a');
        bad.replicas = 0;
        assert!(bad.validate().is_err());
        let mut tomb = entry(1, "SHA256:a", 'a');
        tomb.deleted = true;
        tomb.sha256.clear();
        assert!(tomb.validate().is_ok());
    }

    #[test]
    fn placement_is_deterministic_and_order_independent() {
        let nodes = [
            "SHA256:n1",
            "SHA256:n2",
            "SHA256:n3",
            "SHA256:n4",
            "SHA256:n5",
        ];
        let mut reversed = nodes;
        reversed.reverse();
        let hash = "c".repeat(64);
        assert_eq!(placement(&hash, 3, &nodes), placement(&hash, 3, &reversed));
        assert_eq!(placement(&hash, 3, &nodes).len(), 3);
        assert_eq!(placement(&hash, 9, &nodes).len(), 5);
        assert_eq!(placement(&hash, 3, &["SHA256:n1", "SHA256:n1"]).len(), 1);
    }

    #[test]
    fn losing_a_holder_promotes_the_next_ranked_node_only() {
        let nodes = [
            "SHA256:n1",
            "SHA256:n2",
            "SHA256:n3",
            "SHA256:n4",
            "SHA256:n5",
        ];
        let hash = "d".repeat(64);
        let ranked = rank_nodes(&hash, &nodes);
        let before = placement(&hash, 3, &nodes);
        let lost = before[0];
        let survivors: Vec<&str> = nodes.iter().copied().filter(|n| *n != lost).collect();
        let after = placement(&hash, 3, &survivors);
        assert_eq!(&after[..2], &before[1..]);
        assert_eq!(after[2], ranked[3]);
    }

    #[test]
    fn placement_spreads_blobs_across_nodes() {
        let nodes = ["SHA256:n1", "SHA256:n2", "SHA256:n3", "SHA256:n4"];
        let mut firsts = std::collections::BTreeMap::new();
        for index in 0..400u32 {
            let hash = format!("{:064x}", u128::from(index) * 0x9e37_79b9_7f4a_7c15);
            *firsts.entry(rank_nodes(&hash, &nodes)[0]).or_insert(0) += 1;
        }
        assert_eq!(firsts.len(), 4);
        assert!(firsts.values().all(|count| *count > 50), "{firsts:?}");
    }
}
