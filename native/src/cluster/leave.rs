//! `remuda cluster leave` (#778): a member asks a peer to revoke it, then
//! forgets its membership.
//!
//! The peer revokes only the authenticated caller, and only once every live
//! file keeps its placement without the caller (`ClusterVfsBlobStat`). The
//! leaving node then moves its cluster directory aside and deletes its key
//! and membership files; `vfs/` and the audit log stay in the moved folder.

use super::vfs::VfsStore;
use super::vfs_status::copy_status;
use super::vfs_sync::{members, request_peer_within};
use remuda_core::protocol::{Request, Response};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long the peer checks copies; under the listener's 30 s hold.
const CHECK_DEADLINE: Duration = Duration::from_secs(20);
/// How long the leaving node waits for each peer's answer.
const ASK_TIMEOUT: Duration = Duration::from_secs(29);
/// Deleted from the moved folder: the node key and membership.
pub const FORGOTTEN_FILES: [&str; 4] = [
    "identity.key",
    "authorized_nodes.json",
    "join_tokens.json",
    "revoked_notice.json",
];

/// The peer's half of `ClusterLeave`, for the authenticated `caller`.
pub fn serve(caller: &str) -> Response {
    match check_and_revoke(caller) {
        Ok(()) => Response::Ok,
        Err(reason) => Response::error(reason),
    }
}

fn check_and_revoke(caller: &str) -> Result<(), String> {
    let members = members().map_err(|error| error.to_string())?;
    let label = super::node_label(caller);
    if caller == members.self_fp || !members.admitted.iter().any(|fp| fp == caller) {
        return Err(format!("{label} is not another admitted member"));
    }
    let store = VfsStore::open_default().map_err(|error| error.to_string())?;
    let copies = copy_status(&store, &members.without(caller), CHECK_DEADLINE)
        .map_err(|error| error.to_string())?;
    if !copies.short.is_empty() {
        return Err(format!(
            "{} files would be short of copies without {label}; run `remuda vfs drain` there first",
            copies.short.len()
        ));
    }
    super::revoke(caller).map_err(|error| error.to_string())?;
    Ok(())
}

/// What the peers said to this node's `ClusterLeave`.
#[derive(Debug, PartialEq, Eq)]
pub enum Asked {
    /// This peer revoked this node.
    Revoked(String),
    /// Every peer that answered refused: (node, reason).
    Refused(Vec<(String, String)>),
    /// No peer answered at all.
    Unreachable,
}

/// Ask each admitted peer in turn to revoke this node, until one does.
pub fn ask_peers() -> io::Result<Asked> {
    let members = members().map_err(io::Error::other)?;
    let mut refused = Vec::new();
    for peer in members.admitted.iter().filter(|fp| **fp != members.self_fp) {
        let label = super::node_label(peer);
        match request_peer_within(peer, &Request::ClusterLeave, ASK_TIMEOUT) {
            Ok(Response::Ok) => return Ok(Asked::Revoked(label)),
            Ok(Response::Error(reason)) if reason == "invalid remote request" => {
                refused.push((label, "too old to revoke a leaving node".into()))
            }
            Ok(Response::Error(reason)) => refused.push((label, reason)),
            Ok(other) => refused.push((label, format!("unexpected answer {other:?}"))),
            Err(_) => {}
        }
    }
    Ok(if refused.is_empty() {
        Asked::Unreachable
    } else {
        Asked::Refused(refused)
    })
}

/// Where `forget_at` will move `<state>/cluster` today.
pub fn left_dir() -> io::Result<PathBuf> {
    let state = super::storage::cluster_state_dir()?;
    Ok(free_left_dir(&state, &today()))
}

fn free_left_dir(state: &Path, date: &str) -> PathBuf {
    let base = state.join(format!("cluster.left-{date}"));
    let mut dir = base.clone();
    let mut n = 2;
    while dir.symlink_metadata().is_ok() {
        dir = PathBuf::from(format!("{}-{n}", base.display()));
        n += 1;
    }
    dir
}

/// Move `<state>/cluster` to `left` in one rename, then delete the key and
/// membership files there. A failed rename leaves everything in place.
pub fn forget(left: &Path) -> io::Result<()> {
    let cluster = super::storage::cluster_state_dir()?.join("cluster");
    forget_at(&cluster, left)
}

fn forget_at(cluster: &Path, left: &Path) -> io::Result<()> {
    std::fs::rename(cluster, left)?;
    for name in FORGOTTEN_FILES {
        match std::fs::remove_file(left.join(name)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    Ok(())
}

fn today() -> String {
    let days = super::vfs::unix_now() / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}{month:02}{day:02}")
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_civil() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_748), (2026, 10, 22));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn forgetting_moves_the_folder_and_deletes_only_key_and_membership() {
        let root = std::env::temp_dir().join(format!("remuda-leave-{}", std::process::id()));
        let cluster = root.join("cluster");
        std::fs::create_dir_all(cluster.join("vfs/blobs")).unwrap();
        for name in ["identity.key", "authorized_nodes.json", "audit.jsonl"] {
            std::fs::write(cluster.join(name), name).unwrap();
        }
        std::fs::write(cluster.join("vfs/index.db"), "db").unwrap();
        std::fs::create_dir_all(root.join("cluster.left-20261022")).unwrap();

        let left = free_left_dir(&root, "20261022");
        assert_eq!(left, root.join("cluster.left-20261022-2"));
        forget_at(&cluster, &left).unwrap();

        assert!(!cluster.exists());
        assert!(!left.join("identity.key").exists());
        assert!(!left.join("authorized_nodes.json").exists());
        assert!(left.join("audit.jsonl").is_file());
        assert!(left.join("vfs/index.db").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }
}
