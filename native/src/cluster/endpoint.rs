//! Publish this node's routing hint without changing its membership authority.

use super::{encoding, identity, join_line, registry, replication, storage, NodeState};
use remuda_core::protocol::ListenerStatus;
use std::{io, net::SocketAddr, path::Path};

/// Refresh from the running daemon, without reloading its listener or sessions.
/// No caller-supplied address is accepted by this repair operation.
pub fn refresh(daemon_path: &Path) -> io::Result<(SocketAddr, bool)> {
    match super::listener_control::status(daemon_path) {
        ListenerStatus::On { addr, .. } if !addr.ip().is_unspecified() => {
            publish_bound(addr).map(|changed| (addr, changed))
        }
        _ => Err(io::Error::other(
            "endpoint refresh requires a running listener bound to a concrete address",
        )),
    }
}

pub(crate) fn publish_bound(address: SocketAddr) -> io::Result<bool> {
    let dir = storage::cluster_state_dir()?.join("cluster");
    let changed = publish_at(&dir, address)?;
    if changed {
        replication::registry_changed();
    }
    Ok(changed)
}

fn publish_at(dir: &Path, address: SocketAddr) -> io::Result<bool> {
    join_line::validate_endpoint(address)?;
    let _guard = storage::StateLock::acquire(dir)?;
    let node = identity::load_identity_at(dir)?;
    let mut registry = registry::load_registry_at(dir)?;
    let own = registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == node.node_fp)
        .ok_or_else(|| io::Error::other("local registry entry is missing"))?;
    if own.state != NodeState::Admitted
        || own.static_pubkey != encoding::encode_base64(&node.static_pubkey)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local node is not admitted with its current key",
        ));
    }
    let endpoint = address.to_string();
    if own.endpoint.as_deref() == Some(endpoint.as_str()) {
        return Ok(false);
    }
    own.version = own
        .version
        .checked_add(1)
        .ok_or_else(|| io::Error::other("local registry version is exhausted"))?;
    own.endpoint = Some(endpoint);
    registry::save_registry_at(dir, &registry)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct State(std::path::PathBuf);

    impl State {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "remuda-endpoint-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            super::super::init_at(&root).unwrap();
            Self(root)
        }

        fn dir(&self) -> std::path::PathBuf {
            self.0.join("cluster")
        }
    }

    impl Drop for State {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn endpoint_move_preserves_membership_and_noop_version() {
        let state = State::new();
        let dir = state.dir();
        let original = registry::load_registry_at(&dir).unwrap().authorized_nodes[0].clone();
        assert!(publish_at(&dir, "192.168.100.3:7441".parse().unwrap()).unwrap());
        assert!(publish_at(&dir, "192.168.100.238:7441".parse().unwrap()).unwrap());
        let moved = registry::load_registry_at(&dir).unwrap();
        let mut expected = original;
        expected.endpoint = Some("192.168.100.238:7441".into());
        expected.version += 2;
        assert_eq!(moved.authorized_nodes, vec![expected]);
        assert!(!publish_at(&dir, "192.168.100.238:7441".parse().unwrap()).unwrap());
        assert_eq!(registry::load_registry_at(&dir).unwrap(), moved);
    }

    #[test]
    fn endpoint_publish_rejects_revoked_self_and_version_exhaustion_without_write() {
        let state = State::new();
        let dir = state.dir();
        let mut registry = registry::load_registry_at(&dir).unwrap();
        for (status, version) in [(NodeState::Revoked, 2), (NodeState::Admitted, u64::MAX)] {
            registry.authorized_nodes[0].state = status;
            registry.authorized_nodes[0].version = version;
            registry::save_registry_at(&dir, &registry).unwrap();
            assert!(publish_at(&dir, "192.168.100.238:7441".parse().unwrap()).is_err());
            assert_eq!(registry::load_registry_at(&dir).unwrap(), registry);
        }
    }

    #[test]
    fn endpoint_publish_rejects_unspecified_without_write() {
        let state = State::new();
        let dir = state.dir();
        let original = registry::load_registry_at(&dir).unwrap();
        assert!(publish_at(&dir, "0.0.0.0:7441".parse().unwrap()).is_err());
        assert_eq!(registry::load_registry_at(&dir).unwrap(), original);
    }
}
