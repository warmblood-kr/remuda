//! The daemon's socket ownership, backed by [`Lifecycle`] over the legacy bind.
//! One receipt per endpoint: shutdown sets terminal first, then unlinks only
//! the node that receipt names. Lock order: `SocketLock`, then lifecycle.

use super::{startup::Published, unlink_if_matches, Generation, Lifecycle, NodeReceipt};
use std::ffi::OsString;
use std::io;
use std::path::Path;

pub(crate) struct Owner {
    lifecycle: Lifecycle,
    name: OsString,
}

impl Owner {
    /// Record the node the legacy bind just created at `path`.
    pub(crate) fn capture(path: &Path) -> io::Result<Self> {
        let (parent, name) = split(path)?;
        let node = NodeReceipt::capture(&parent, &name)?;
        let lifecycle = Lifecycle::new(parent);
        lifecycle.install(None, generation(1, node, name.clone()))?;
        Ok(Self { lifecycle, name })
    }

    /// Adopt the node a staged publication just committed: its receipt came
    /// from the transaction, not from a later look at the path.
    pub(crate) fn from_published(published: Published) -> io::Result<Self> {
        let Published {
            parent,
            name,
            receipt,
        } = published;
        let lifecycle = Lifecycle::new(parent);
        lifecycle.install(None, generation(1, receipt, name.clone()))?;
        Ok(Self { lifecycle, name })
    }

    /// Whether shutdown has begun; a rebind after that must not install.
    pub(crate) fn is_terminal(&self) -> bool {
        self.lifecycle.is_terminal()
    }

    /// Shutdown: terminal first, then remove the current node if it matches.
    pub(crate) fn terminate(&self) -> io::Result<bool> {
        self.lifecycle.terminate()
    }

    pub(crate) fn owns_path(&self) -> bool {
        self.lifecycle.owns_current()
    }

    /// Adopt the node a rebind just created. After shutdown began it is
    /// refused and that node is removed, so no endpoint outlives the daemon.
    pub(crate) fn refresh(&self) -> io::Result<()> {
        let node = NodeReceipt::capture(&self.lifecycle.parent, &self.name)?;
        let number = self.lifecycle.current_number()?.unwrap_or(0);
        let next = generation(number + 1, node, self.name.clone());
        self.lifecycle
            .install(Some(number), next)
            .map(drop)
            .inspect_err(|_| {
                let _ = unlink_if_matches(&self.lifecycle.parent, &self.name, node);
            })
    }
}

/// Remove a dead daemon's socket by receipt, never anything else at the path.
pub(crate) fn remove_stale_socket(path: &Path) -> io::Result<()> {
    let (parent, name) = split(path)?;
    let node = match NodeReceipt::capture(&parent, &name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    if node.kind != super::NodeKind::Socket {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a socket", path.display()),
        ));
    }
    unlink_if_matches(&parent, &name, node).map(drop)
}

fn generation(number: u64, node: NodeReceipt, name: OsString) -> Generation {
    Generation {
        number,
        listener: None,
        node,
        name,
    }
}

fn split(path: &Path) -> io::Result<(std::fs::File, OsString)> {
    let invalid = |what| io::Error::new(io::ErrorKind::InvalidInput, what);
    let parent = path
        .parent()
        .ok_or_else(|| invalid("socket has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("socket has no name"))?;
    Ok((
        super::startup::open_dir_nofollow(parent)?,
        name.to_os_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    struct Dir(std::path::PathBuf);
    impl Dir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = Path::new("/tmp").join(format!("r660o-{}-{n}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn socket(&self) -> std::path::PathBuf {
            self.0.join("s")
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Rebind as SIGUSR1 does: the old node goes, a new one is bound.
    fn rebind(dir: &Dir) -> UnixListener {
        let _ = std::fs::remove_file(dir.socket());
        UnixListener::bind(dir.socket()).unwrap()
    }

    /// Shutdown won the race: the rebound node must not outlive it.
    fn shutdown_then_rebind(refresh: impl FnOnce(&Owner) -> io::Result<()>) {
        let dir = Dir::new();
        let _first = UnixListener::bind(dir.socket()).unwrap();
        let owner = Owner::capture(&dir.socket()).unwrap();
        assert!(owner.terminate().unwrap());
        let _second = rebind(&dir);
        assert!(refresh(&owner).is_err(), "refresh after shutdown must fail");
        assert!(!dir.socket().exists(), "rebound node outlived shutdown");
    }

    #[test]
    fn rebind_after_shutdown_is_refused_and_leaves_no_node() {
        shutdown_then_rebind(|owner| owner.refresh());
    }

    /// Rebind won the race: shutdown must remove the NEW node, not the old one.
    fn rebind_then_shutdown(terminate: impl FnOnce(&Owner, NodeReceipt) -> io::Result<bool>) {
        let dir = Dir::new();
        let _first = UnixListener::bind(dir.socket()).unwrap();
        let owner = Owner::capture(&dir.socket()).unwrap();
        let stale = NodeReceipt::capture(&owner.lifecycle.parent, &owner.name).unwrap();
        let _second = rebind(&dir);
        owner.refresh().unwrap();
        assert!(owner.owns_path());
        terminate(&owner, stale).unwrap();
        assert!(!dir.socket().exists(), "current node survived shutdown");
    }

    #[test]
    fn shutdown_after_rebind_removes_the_current_node() {
        rebind_then_shutdown(|owner, _| owner.terminate());
    }

    // Controls: the obsolete owners each leave a node behind, so each is RED.

    #[test]
    #[should_panic(expected = "refresh after shutdown must fail")]
    fn control_identity_only_refresh_ignores_shutdown() {
        // The legacy refresh just re-read the path's identity, terminal or not.
        shutdown_then_rebind(|owner| {
            NodeReceipt::capture(&owner.lifecycle.parent, &owner.name).map(drop)
        });
    }

    #[test]
    #[should_panic(expected = "current node survived shutdown")]
    fn control_stale_receipt_cleanup_misses_the_rebound_node() {
        rebind_then_shutdown(|owner, stale| {
            unlink_if_matches(&owner.lifecycle.parent, &owner.name, stale)
        });
    }

    #[test]
    fn failed_refresh_without_shutdown_is_not_terminal() {
        // The SIGUSR1 caller keeps serving on such a failure.
        let dir = Dir::new();
        let _first = UnixListener::bind(dir.socket()).unwrap();
        let owner = Owner::capture(&dir.socket()).unwrap();
        std::fs::remove_file(dir.socket()).unwrap();
        assert!(owner.refresh().is_err());
        assert!(!owner.is_terminal());
        owner.terminate().unwrap();
        assert!(owner.is_terminal());
    }

    #[test]
    fn replaced_foreign_node_is_never_unlinked() {
        let dir = Dir::new();
        let _first = UnixListener::bind(dir.socket()).unwrap();
        let owner = Owner::capture(&dir.socket()).unwrap();
        std::fs::remove_file(dir.socket()).unwrap();
        std::fs::write(dir.socket(), b"theirs").unwrap();
        assert!(!owner.owns_path());
        assert!(!owner.terminate().unwrap());
        assert_eq!(std::fs::read(dir.socket()).unwrap(), b"theirs");
    }
}
