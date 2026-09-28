//! Cluster vocabulary composed from identity and membership registry units.

pub mod encoding;
pub mod identity;
pub mod join_line;
pub mod join_token;
pub mod registry;
#[cfg(not(windows))]
mod storage;

pub use identity::NodeIdentity;
pub use registry::{load_registry, save_registry, AuthorizedNode, NodeState, Registry};

use std::io;

/// Initialize the local identity and cluster-of-one registry.
pub fn init() -> io::Result<(NodeIdentity, bool)> {
    #[cfg(windows)]
    return Err(identity::windows_storage_error());
    #[cfg(not(windows))]
    {
        let dir = identity::prepare_cluster_dir()?;
        let _guard = storage::StateLock::acquire(&dir)?;
        let (node, created) = identity::init_identity_locked(&dir)?;
        let mut registry = registry::load_registry_at(&dir)?;
        registry.merge(&Registry {
            authorized_nodes: vec![AuthorizedNode {
                node_fp: node.node_fp.clone(),
                static_pubkey: encoding::encode_base64(&node.static_pubkey),
                state: NodeState::Admitted,
                version: 1,
                by: node.node_fp.clone(),
            }],
        })?;
        registry::save_registry_at(&dir, &registry)?;
        Ok((node, created))
    }
}

/// Return the local identity and admitted member count, or `None` before init.
pub fn status() -> io::Result<Option<(NodeIdentity, usize)>> {
    #[cfg(windows)]
    return Err(identity::windows_storage_error());
    #[cfg(not(windows))]
    {
        let dir = storage::cluster_state_dir()?.join("cluster");
        match std::fs::symlink_metadata(&dir) {
            Ok(_) => {
                identity::check_identity_path(&dir)?;
                storage::verify_directory(&dir)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
        match identity::load_identity_at(&dir) {
            Ok(node) => {
                let registry = registry::load_registry_at(&dir)?;
                let members = registry
                    .authorized_nodes
                    .iter()
                    .filter(|entry| entry.state == NodeState::Admitted)
                    .count();
                Ok(Some((node, members)))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}
