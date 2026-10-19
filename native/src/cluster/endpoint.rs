//! Publish this node's routing hint without changing its membership authority.

use super::{encoding, identity, join_line, registry, replication, storage, NodeState};
use remuda_core::protocol::ListenerStatus;
use std::{io, net::SocketAddr, path::Path};

/// Refresh from the running daemon, without reloading its listener or sessions.
/// No caller-supplied address is accepted by this repair operation.
pub fn refresh(daemon_path: &Path) -> io::Result<(SocketAddr, bool)> {
    refresh_with(
        || super::listener_control::status(daemon_path),
        own_version,
        publish_bound_if_version,
    )
}

/// Outcome of a version-guarded endpoint write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Publish {
    /// The record already advertised this address.
    Unchanged,
    /// The record was rewritten and its version bumped.
    Updated,
    /// The record changed since the caller's snapshot; nothing was written.
    Superseded,
}

/// Compare-and-set on the own-record version taken before the status sample,
/// so a stale sample never overwrites a newer listener endpoint.
fn refresh_with(
    mut status: impl FnMut() -> ListenerStatus,
    mut version: impl FnMut() -> io::Result<u64>,
    mut publish: impl FnMut(SocketAddr, u64) -> io::Result<Publish>,
) -> io::Result<(SocketAddr, bool)> {
    let mut changed = false;
    for _ in 0..4 {
        let seen = version()?;
        let address = concrete_address(status())?;
        match publish(address, seen)? {
            Publish::Superseded => continue,
            Publish::Updated => changed = true,
            Publish::Unchanged => {}
        }
        // Do not hold the state lock during IPC: Reload waits for publication.
        // The daemon exposes its new status before publishing its routing hint,
        // so a rebind that raced this write republishes after it.
        if concrete_address(status())? == address {
            return Ok((address, changed));
        }
    }
    Err(io::Error::other(
        "listener address kept changing; retry endpoint refresh",
    ))
}

fn concrete_address(status: ListenerStatus) -> io::Result<SocketAddr> {
    match status {
        ListenerStatus::On { addr, .. } if !addr.ip().is_unspecified() => Ok(addr),
        _ => Err(io::Error::other(
            "endpoint refresh requires a running listener bound to a concrete address",
        )),
    }
}

/// Publish the address the listener just bound. The listener is the source of
/// truth for the address, so this write is unconditional.
pub(crate) fn publish_bound(address: SocketAddr) -> io::Result<bool> {
    let dir = storage::cluster_state_dir()?.join("cluster");
    let changed = publish_at(&dir, address)?;
    if changed {
        replication::registry_changed();
    }
    Ok(changed)
}

fn own_version() -> io::Result<u64> {
    let dir = storage::cluster_state_dir()?.join("cluster");
    own_version_at(&dir)
}

fn publish_bound_if_version(address: SocketAddr, expected: u64) -> io::Result<Publish> {
    let dir = storage::cluster_state_dir()?.join("cluster");
    let outcome = publish_guarded(&dir, address, Some(expected))?;
    if outcome == Publish::Updated {
        replication::registry_changed();
    }
    Ok(outcome)
}

fn own_version_at(dir: &Path) -> io::Result<u64> {
    let _guard = storage::StateLock::acquire(dir)?;
    let node = identity::load_identity_at(dir)?;
    let registry = registry::load_registry_at(dir)?;
    registry
        .authorized_nodes
        .iter()
        .find(|entry| entry.node_fp == node.node_fp)
        .map(|entry| entry.version)
        .ok_or_else(|| io::Error::other("local registry entry is missing"))
}

fn publish_at(dir: &Path, address: SocketAddr) -> io::Result<bool> {
    Ok(publish_guarded(dir, address, None)? == Publish::Updated)
}

/// Write the own endpoint under the state lock. With `expected`, refuse the
/// write when the record's version moved since the caller's snapshot.
fn publish_guarded(dir: &Path, address: SocketAddr, expected: Option<u64>) -> io::Result<Publish> {
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
        return Ok(Publish::Unchanged);
    }
    if expected.is_some_and(|version| version != own.version) {
        return Ok(Publish::Superseded);
    }
    own.version = own
        .version
        .checked_add(1)
        .ok_or_else(|| io::Error::other("local registry version is exhausted"))?;
    own.endpoint = Some(endpoint);
    registry::save_registry_at(dir, &registry)?;
    Ok(Publish::Updated)
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

    fn own_entry(dir: &Path) -> (Option<String>, u64) {
        let node = registry::load_registry_at(dir).unwrap().authorized_nodes[0].clone();
        (node.endpoint, node.version)
    }

    fn on(addr: SocketAddr) -> ListenerStatus {
        ListenerStatus::On {
            addr,
            auto: true,
            advertise_addr: Some(addr),
            listen_addrs: vec![addr],
        }
    }

    fn guarded(dir: &Path) -> impl FnMut(SocketAddr, u64) -> io::Result<Publish> + '_ {
        move |address, version| publish_guarded(dir, address, Some(version))
    }

    #[test]
    fn refresh_retries_when_a_rebind_publishes_after_the_first_status_sample() {
        let state = State::new();
        let dir = state.dir();
        let old: SocketAddr = "192.168.100.3:7441".parse().unwrap();
        let current: SocketAddr = "192.168.100.238:7441".parse().unwrap();
        publish_at(&dir, old).unwrap();
        let mut publications = 0;
        let mut samples = 0;
        let mut write = guarded(&dir);
        let (address, _) = refresh_with(
            || {
                samples += 1;
                on(if samples == 1 { old } else { current })
            },
            || own_version_at(&dir),
            |address, version| {
                if publications == 0 {
                    // Rebind wins the state lock after the stale Status sample.
                    publish_at(&dir, current).unwrap();
                }
                publications += 1;
                write(address, version)
            },
        )
        .unwrap();
        assert_eq!(address, current);
        assert_eq!(publications, 2);
        assert_eq!(own_entry(&dir).0, Some(current.to_string()));
    }

    #[test]
    fn refresh_never_overwrites_a_newer_listener_endpoint_with_its_stale_sample() {
        let state = State::new();
        let dir = state.dir();
        let old: SocketAddr = "192.168.100.3:7441".parse().unwrap();
        let current: SocketAddr = "192.168.100.238:7441".parse().unwrap();
        publish_at(&dir, old).unwrap();
        let mut samples = 0;
        let mut states = Vec::new();
        let mut write = guarded(&dir);
        refresh_with(
            || {
                samples += 1;
                // The first sample is stale: the listener rebinds right after it.
                on(if samples == 1 { old } else { current })
            },
            || own_version_at(&dir),
            |address, version| {
                if states.is_empty() {
                    // Listener rebind wins the state lock before refresh writes.
                    publish_at(&dir, current).unwrap();
                    states.push(own_entry(&dir));
                }
                let result = write(address, version);
                states.push(own_entry(&dir));
                result
            },
        )
        .unwrap();
        // After the listener published `current`, no persisted (hence
        // replicable) state may carry the stale address.
        for (endpoint, version) in &states {
            assert_eq!(
                endpoint.as_deref(),
                Some(current.to_string().as_str()),
                "v{version}"
            );
        }
        assert_eq!(
            states[0],
            *states.last().unwrap(),
            "stale write bumped version"
        );
    }

    #[test]
    fn guarded_publish_is_superseded_by_any_intervening_version_bump() {
        let state = State::new();
        let dir = state.dir();
        let a: SocketAddr = "192.168.100.3:7441".parse().unwrap();
        let b: SocketAddr = "192.168.100.238:7441".parse().unwrap();
        let c: SocketAddr = "192.168.100.9:7441".parse().unwrap();
        let seen = own_version_at(&dir).unwrap();
        publish_at(&dir, b).unwrap();
        let before = registry::load_registry_at(&dir).unwrap();
        assert_eq!(
            publish_guarded(&dir, a, Some(seen)).unwrap(),
            Publish::Superseded
        );
        assert_eq!(registry::load_registry_at(&dir).unwrap(), before);
        let seen = own_version_at(&dir).unwrap();
        assert_eq!(
            publish_guarded(&dir, c, Some(seen)).unwrap(),
            Publish::Updated
        );
        assert_eq!(own_entry(&dir), (Some(c.to_string()), seen + 1));
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

    #[test]
    fn concurrent_publishers_share_a_persisted_version_sequence() {
        let state = State::new();
        let dir = state.dir();
        let threads: Vec<_> = (1..=8)
            .map(|last| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    publish_at(&dir, format!("192.168.100.{last}:7441").parse().unwrap()).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            registry::load_registry_at(&dir).unwrap().authorized_nodes[0].version,
            9
        );
        // A fresh caller reads the persisted version, with no process-local counter.
        publish_at(&dir, "192.168.100.238:7441".parse().unwrap()).unwrap();
        assert_eq!(
            registry::load_registry_at(&dir).unwrap().authorized_nodes[0].version,
            10
        );
    }

    #[test]
    fn refresh_stops_after_four_attempts_when_listener_keeps_changing() {
        let mut samples = 0;
        let mut publications = 0;
        let result = refresh_with(
            || {
                samples += 1;
                on(if samples % 2 == 1 {
                    "192.168.100.3:7441"
                } else {
                    "192.168.100.238:7441"
                }
                .parse()
                .unwrap())
            },
            || Ok(0),
            |_, _| {
                publications += 1;
                Ok(Publish::Updated)
            },
        );
        assert!(result.unwrap_err().to_string().contains("kept changing"));
        assert_eq!(publications, 4);
        assert_eq!(samples, 8);
    }

    #[test]
    fn refresh_gives_up_when_every_write_is_superseded() {
        let mut publications = 0;
        let addr = "192.168.100.3:7441".parse().unwrap();
        let result = refresh_with(
            || on(addr),
            || Ok(0),
            |_, _| {
                publications += 1;
                Ok(Publish::Superseded)
            },
        );
        assert!(result.is_err());
        assert_eq!(publications, 4);
    }
}
