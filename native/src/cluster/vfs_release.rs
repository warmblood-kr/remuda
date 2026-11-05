//! `remuda vfs release` (#887): give back the copies this node fetched to read
//! and is not responsible for keeping.
//!
//! A `get`, `read` or `resolve` stores the blob on the node that read it, and
//! nothing removed it again: garbage collection skips referenced hashes, and
//! the sync pass trims only repair stand-ins. A node that only looks at files
//! therefore drifted into holding a copy of everything anyone opened.
//!
//! Release is explicit, never automatic, and never deletes the last copy: a
//! copy goes only when every node placement names for it has confirmed one by
//! `BlobStat`, the same check the stand-in trim already relies on. The index is
//! untouched — no version, no tombstone, no membership change.

use crate::cluster::vfs::{VfsError, VfsErrorKind, VfsResult, VfsStore};
use crate::cluster::vfs_sync;
use remuda_core::vfs::VfsEntry;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// Default for `--older-than`: no grace. A reader that already opened the file
/// keeps it on unix; the real protection is that the last copy is never taken.
pub const DEFAULT_OLDER_THAN: Duration = Duration::from_secs(0);

/// What one `vfs release` did, or would do with `--dry-run`.
#[derive(Debug, Default, Serialize)]
pub struct ReleaseReport {
    pub dry_run: bool,
    pub removed: usize,
    pub bytes: u64,
    /// Kept: placement names this node, so it owes a copy.
    pub placed: usize,
    /// Kept: this node wrote it, or it is a conflict's content.
    pub written: usize,
    /// Kept: the copy is younger than `--older-than`.
    pub recent: usize,
    /// Kept: placement is empty from here, so it may be the last copy.
    pub last_copy: usize,
    /// Kept: `resolve` handed this copy's path to a tool.
    pub resolved: usize,
    /// Kept: a placement node did not confirm a copy.
    pub unconfirmed: usize,
}

/// Why one local copy stays, or that it may go. The whole rule is this enum
/// and [`verdict`], so each reason has a unit test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// This node wrote it, or it is a kept conflict's content.
    Written,
    /// `resolve` handed the path to a tool.
    Resolved,
    /// Placement names this node, so it owes a copy.
    Placed,
    /// No other node could take it: possibly the last copy.
    LastCopy,
    /// Younger than `--older-than`.
    Recent,
    /// A placement node did not confirm a copy.
    Unconfirmed,
    /// Nothing needs it here.
    Release,
}

/// The facts one decision needs. No store, no clock, no network.
pub struct Facts<'a> {
    pub pinned: bool,
    pub kept: bool,
    pub include_resolved: bool,
    pub placement: &'a [String],
    pub self_fp: &'a str,
    pub recent: bool,
    pub holders: usize,
}

/// The reasons that need no disk or network. `None` means the age and holder
/// probes decide, so a pinned or placed copy never pays for them.
pub fn early_verdict(facts: &Facts<'_>) -> Option<Verdict> {
    if facts.pinned {
        return Some(Verdict::Written);
    }
    if facts.kept && !facts.include_resolved {
        return Some(Verdict::Resolved);
    }
    if facts.placement.is_empty() {
        return Some(Verdict::LastCopy);
    }
    if facts.placement.iter().any(|node| node == facts.self_fp) {
        return Some(Verdict::Placed);
    }
    None
}

/// The decision, in the order the reasons are checked.
pub fn verdict(facts: &Facts<'_>) -> Verdict {
    if let Some(verdict) = early_verdict(facts) {
        return verdict;
    }
    if facts.recent {
        return Verdict::Recent;
    }
    if facts.holders < facts.placement.len() {
        return Verdict::Unconfirmed;
    }
    Verdict::Release
}

/// One copy's inputs, so the decision keeps a single argument.
struct Decision<'a> {
    store: &'a VfsStore,
    members: &'a vfs_sync::Members,
    sha: &'a str,
    entries: &'a [&'a VfsEntry],
    pinned: &'a HashSet<&'a str>,
    older_than: Duration,
    include_resolved: bool,
}

/// Decide one copy, probing age and holders only when the cheap reasons do not
/// decide. Returns the verdict and whether a resolve pin must be dropped.
fn decide(decision: &Decision<'_>) -> VfsResult<(Verdict, bool)> {
    let placement = placement_of(decision.entries, decision.members);
    let kept = decision.store.is_kept(decision.sha);
    let cheap = Facts {
        pinned: decision.pinned.contains(decision.sha),
        kept,
        include_resolved: decision.include_resolved,
        placement: &placement,
        self_fp: &decision.members.self_fp,
        // Filled only if the cheap reasons do not decide.
        recent: false,
        holders: placement.len(),
    };
    if let Some(verdict) = early_verdict(&cheap) {
        return Ok((verdict, false));
    }
    let facts = Facts {
        recent: too_new(decision.store, decision.sha, decision.older_than)?,
        holders: vfs_sync::count_holders(
            decision.store,
            &decision.members.self_fp,
            decision.entries[0],
            &placement,
        ),
        ..cheap
    };
    Ok((verdict(&facts), kept))
}

/// Drop the pin and the blob, counting what actually went.
fn commit(
    store: &VfsStore,
    self_fp: &str,
    sha: &str,
    kept: bool,
    entry: &VfsEntry,
    report: &mut ReleaseReport,
) -> VfsResult<()> {
    if kept {
        store.unkeep_blob(sha)?;
    }
    // The store re-checks the local pins under the index lock; peer custody is
    // the check above, as observed a moment ago.
    if store.release_blob_if_unneeded(sha, self_fp)? {
        report.removed += 1;
        report.bytes = report.bytes.saturating_add(entry.size);
    }
    Ok(())
}

/// Release this node's unnecessary copies. `dry_run` counts without deleting;
/// `older_than` keeps copies younger than that.
pub fn release(
    dry_run: bool,
    older_than: Duration,
    include_resolved: bool,
) -> VfsResult<ReleaseReport> {
    let store = VfsStore::open_default()?;
    let members = vfs_sync::members()?;
    let index = store.load_index()?;
    let (users, pinned) = group_by_blob(&index, &members.self_fp);
    let mut report = ReleaseReport {
        dry_run,
        ..ReleaseReport::default()
    };
    for sha in store.local_blobs()? {
        // No live entry names it: garbage collection owns that, not release.
        let Some(entries) = users.get(sha.as_str()) else {
            continue;
        };
        let decision = Decision {
            store: &store,
            members: &members,
            sha: &sha,
            entries,
            pinned: &pinned,
            older_than,
            include_resolved,
        };
        match decide(&decision)? {
            (Verdict::Release, kept) => {
                if dry_run {
                    // A dry run changes nothing, pins included.
                    report.removed += 1;
                    report.bytes = report.bytes.saturating_add(entries[0].size);
                } else {
                    commit(
                        &store,
                        &members.self_fp,
                        &sha,
                        kept,
                        entries[0],
                        &mut report,
                    )?;
                }
            }
            (kept, _) => report.count(kept),
        }
    }
    Ok(report)
}

impl ReleaseReport {
    /// Count a copy the decision keeps, one bucket per reason.
    fn count(&mut self, verdict: Verdict) {
        match verdict {
            Verdict::Written => self.written += 1,
            Verdict::Resolved => self.resolved += 1,
            Verdict::Placed => self.placed += 1,
            Verdict::LastCopy => self.last_copy += 1,
            Verdict::Recent => self.recent += 1,
            Verdict::Unconfirmed => self.unconfirmed += 1,
            Verdict::Release => {}
        }
    }
}

/// Live entries by content hash, plus the hashes release must never touch: the
/// ones this node wrote and the content of a kept conflict.
fn group_by_blob<'a>(
    index: &'a crate::cluster::vfs::VfsIndex,
    self_fp: &str,
) -> (HashMap<&'a str, Vec<&'a VfsEntry>>, HashSet<&'a str>) {
    let mut users: HashMap<&str, Vec<&VfsEntry>> = HashMap::new();
    let mut pinned: HashSet<&str> = HashSet::new();
    for entry in index.entries.values() {
        // A kept conflict's own content, not the winner's: pinning the winner
        // left a losing hash releasable (#887 review).
        for conflict in &entry.conflicts {
            if !conflict.deleted {
                pinned.insert(&conflict.sha256);
            }
        }
        if entry.writer == self_fp {
            pinned.insert(&entry.sha256);
        }
        if !entry.deleted {
            users.entry(&entry.sha256).or_default().push(entry);
        }
    }
    (users, pinned)
}

/// Every node that should hold this content, as this node sees the cluster.
fn placement_of(entries: &[&VfsEntry], members: &vfs_sync::Members) -> Vec<String> {
    let mut placement: Vec<String> = Vec::new();
    for entry in entries {
        for node in vfs_sync::live_placement(entry, members) {
            if !placement.contains(&node) {
                placement.push(node);
            }
        }
    }
    placement
}

/// The rule half of `too_new`: an age we cannot read counts as new, so an
/// unknown copy is kept rather than released.
fn is_too_new(age: Option<Duration>, older_than: Duration) -> bool {
    if older_than.is_zero() {
        return false;
    }
    age.map(|age| age < older_than).unwrap_or(true)
}

fn too_new(store: &VfsStore, sha: &str, older_than: Duration) -> VfsResult<bool> {
    if older_than.is_zero() {
        return Ok(false);
    }
    let modified = std::fs::metadata(store.blob_path(sha)?)
        .and_then(|metadata| metadata.modified())
        .map_err(VfsError::from)?;
    Ok(is_too_new(modified.elapsed().ok(), older_than))
}

/// The cluster has no node that could take this copy, so say why rather than
/// removing the only one.
pub fn last_copy_error() -> VfsError {
    VfsError::new(
        VfsErrorKind::Unreachable,
        "no other node is placed on those files, so release kept every copy",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_starts_empty_and_says_whether_it_deleted() {
        let report = ReleaseReport::default();
        assert_eq!(report.removed, 0);
        assert!(!report.dry_run);
        assert!(DEFAULT_OLDER_THAN.is_zero());
    }

    fn facts<'a>(placement: &'a [String], self_fp: &'a str) -> Facts<'a> {
        Facts {
            pinned: false,
            kept: false,
            include_resolved: false,
            placement,
            self_fp,
            recent: false,
            holders: placement.len(),
        }
    }

    #[test]
    fn a_copy_this_node_wrote_or_a_conflict_kept_is_never_released() {
        let none: Vec<String> = Vec::new();
        let mut value = facts(&none, "me");
        value.pinned = true;
        assert_eq!(verdict(&value), Verdict::Written);
    }

    #[test]
    fn a_resolved_copy_stays_until_the_operator_asks() {
        let none: Vec<String> = Vec::new();
        let mut value = facts(&none, "me");
        value.kept = true;
        assert_eq!(verdict(&value), Verdict::Resolved);
        value.include_resolved = true;
        assert_eq!(verdict(&value), Verdict::LastCopy);
    }

    #[test]
    fn an_empty_placement_means_it_might_be_the_last_copy() {
        let none: Vec<String> = Vec::new();
        assert_eq!(verdict(&facts(&none, "me")), Verdict::LastCopy);
    }

    #[test]
    fn a_placement_that_names_this_node_keeps_the_copy_here() {
        let placed = vec!["other".to_string(), "me".to_string()];
        assert_eq!(verdict(&facts(&placed, "me")), Verdict::Placed);
    }

    #[test]
    fn a_recent_copy_and_an_unconfirmed_one_each_stay_for_their_own_reason() {
        let placed = vec!["other".to_string()];
        let mut value = facts(&placed, "me");
        value.recent = true;
        assert_eq!(verdict(&value), Verdict::Recent);
        value.recent = false;
        value.holders = 0;
        assert_eq!(verdict(&value), Verdict::Unconfirmed);
        value.holders = 1;
        assert_eq!(verdict(&value), Verdict::Release);
    }

    #[test]
    fn the_cheap_reasons_decide_without_a_probe() {
        let none: Vec<String> = Vec::new();
        let mut value = facts(&none, "me");
        value.pinned = true;
        assert_eq!(early_verdict(&value), Some(Verdict::Written));
        value.pinned = false;
        value.kept = true;
        assert_eq!(early_verdict(&value), Some(Verdict::Resolved));
        value.kept = false;
        assert_eq!(early_verdict(&value), Some(Verdict::LastCopy));
        let placed = vec!["me".to_string()];
        assert_eq!(early_verdict(&facts(&placed, "me")), Some(Verdict::Placed));
    }

    #[test]
    fn only_a_free_copy_asks_for_age_and_holders() {
        let placed = vec!["other".to_string()];
        assert_eq!(early_verdict(&facts(&placed, "me")), None);
    }

    #[test]
    fn counting_a_kept_copy_lands_in_its_own_bucket() {
        let mut report = ReleaseReport::default();
        for verdict in [
            Verdict::Written,
            Verdict::Resolved,
            Verdict::Placed,
            Verdict::LastCopy,
            Verdict::Recent,
            Verdict::Unconfirmed,
            Verdict::Release,
        ] {
            report.count(verdict);
        }
        assert_eq!(report.written, 1);
        assert_eq!(report.resolved, 1);
        assert_eq!(report.placed, 1);
        assert_eq!(report.last_copy, 1);
        assert_eq!(report.recent, 1);
        assert_eq!(report.unconfirmed, 1);
        assert_eq!(report.removed, 0, "a kept copy is never counted as removed");
    }

    #[test]
    fn an_unreadable_age_is_treated_as_new() {
        assert!(!is_too_new(Some(Duration::ZERO), Duration::ZERO));
        assert!(!is_too_new(None, Duration::ZERO));
        let hour = Duration::from_secs(3600);
        assert!(is_too_new(Some(hour / 2), hour));
        assert!(!is_too_new(Some(hour), hour));
        assert!(!is_too_new(Some(hour * 2), hour));
        assert!(is_too_new(None, hour), "an unknown age must not release");
    }

    fn entry(path: &str, sha: &str, writer: &str, replicas: u8) -> VfsEntry {
        VfsEntry {
            path: path.to_owned(),
            sha256: sha.to_owned(),
            size: 10,
            replicas,
            version: 1,
            writer: writer.to_owned(),
            deleted: false,
            mtime: 0,
            vv: Default::default(),
            conflicts: Vec::new(),
        }
    }

    #[test]
    fn grouping_keeps_live_winners_and_pins_conflict_content() {
        let mut index = crate::cluster::vfs::VfsIndex::default();
        let winner = entry("a", &"a".repeat(64), "SHA256:them", 1);
        let mut conflicted = winner.clone();
        conflicted.path = "b".into();
        conflicted.conflicts.push(remuda_core::vfs::VfsConflict {
            sha256: "c".repeat(64),
            size: 9,
            replicas: 1,
            version: 1,
            writer: "SHA256:them".into(),
            deleted: false,
            vv: Default::default(),
            mtime: 0,
        });
        index.entries.insert("a".into(), winner);
        index.entries.insert("b".into(), conflicted);
        let mut deleted = entry("gone", &"d".repeat(64), "SHA256:them", 1);
        deleted.deleted = true;
        index.entries.insert("gone".into(), deleted);
        index.entries.insert(
            "mine".into(),
            entry("mine", &"e".repeat(64), "SHA256:me", 1),
        );
        let (users, pinned) = group_by_blob(&index, "SHA256:me");
        assert_eq!(users[&*"a".repeat(64)].len(), 2, "both live winners group");
        assert!(
            !users.contains_key(&*"d".repeat(64)),
            "a tombstone is not a user"
        );
        assert!(
            pinned.contains(&*"c".repeat(64)),
            "a conflict's content is pinned"
        );
        assert!(
            pinned.contains(&*"e".repeat(64)),
            "a copy this node wrote is pinned"
        );
        assert!(!pinned.contains(&*"a".repeat(64)));
    }

    #[test]
    fn placement_unions_every_winners_nodes_without_repeats() {
        let members = vfs_sync::members_for_test("me", &["me", "n2", "n3", "n4"], &[]);
        let sha = "a".repeat(64);
        let one = entry("a", &sha, "SHA256:them", 1);
        let two = entry("b", &sha, "SHA256:them", 2);
        let placement = placement_of(&[&one, &two], &members);
        assert_eq!(placement.len(), 2, "{placement:?}");
        let mut unique = placement.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), placement.len());
    }

    #[test]
    fn an_unreachable_reason_names_the_copies_it_kept() {
        assert!(last_copy_error().detail.contains("kept every copy"));
    }
}
