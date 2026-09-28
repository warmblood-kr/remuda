//! Cluster vocabulary composed from identity and membership registry units.

pub mod encoding;
pub mod identity;
pub mod registry;
#[cfg(not(windows))]
mod storage;

pub use identity::NodeIdentity;
pub use registry::{load_registry, save_registry, AuthorizedNode, NodeState, Registry};

use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevokeOutcome {
    Revoked,
    AlreadyRevoked,
}

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

/// Load the local identity and complete local membership registry.
pub fn nodes() -> io::Result<Option<(NodeIdentity, Registry)>> {
    #[cfg(windows)]
    return Err(identity::windows_storage_error());
    #[cfg(not(windows))]
    {
        let dir = storage::cluster_state_dir()?.join("cluster");
        match std::fs::symlink_metadata(&dir) {
            Ok(_) => storage::verify_directory(&dir)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
        let _guard = storage::StateLock::acquire(&dir)?;
        let node = match identity::load_identity_at(&dir) {
            Ok(node) => node,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let registry = registry::load_registry_at(&dir)?;
        Ok(Some((node, registry)))
    }
}

/// Resolve an exact fingerprint or deterministic node label in a registry.
pub fn resolve_node<'a>(
    entries: &'a [AuthorizedNode],
    target: &str,
) -> io::Result<&'a AuthorizedNode> {
    if let Some(entry) = entries.iter().find(|entry| entry.node_fp == target) {
        return Ok(entry);
    }
    let mut matches = entries
        .iter()
        .filter(|entry| node_label(&entry.node_fp) == target);
    let entry = matches.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no cluster node matches {target}"),
        )
    })?;
    if matches.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("node label {target} is ambiguous; use the fingerprint"),
        ));
    }
    Ok(entry)
}

/// Format the local registry with an asterisk on this node's row.
pub fn format_nodes_table(identity: &NodeIdentity, registry: &Registry) -> String {
    let mut output = String::from(
        "NODE              FINGERPRINT                                      STATE    VERSION  BY\n",
    );
    for entry in &registry.authorized_nodes {
        let marker = if entry.node_fp == identity.node_fp {
            "*"
        } else {
            " "
        };
        let _ = std::fmt::Write::write_fmt(
            &mut output,
            format_args!(
                "{marker}{:<16}  {:<46}  {:<8} {:<7}  {}\n",
                node_label(&entry.node_fp),
                entry.node_fp,
                match entry.state {
                    NodeState::Admitted => "admitted",
                    NodeState::Revoked => "revoked",
                },
                entry.version,
                entry.by
            ),
        );
    }
    output
}

/// Revoke a member by node label or exact fingerprint under the state lock.
pub fn revoke(_target: &str) -> io::Result<RevokeOutcome> {
    #[cfg(windows)]
    return Err(identity::windows_storage_error());
    #[cfg(not(windows))]
    {
        let dir = identity::prepare_cluster_dir()?;
        let _guard = storage::StateLock::acquire(&dir)?;
        let self_node = identity::load_identity_at(&dir)?;
        revoke_locked_at(&dir, _target, &self_node)
    }
}

#[cfg(not(windows))]
fn revoke_locked_at(
    dir: &std::path::Path,
    target: &str,
    self_node: &NodeIdentity,
) -> io::Result<RevokeOutcome> {
    if target == self_node.node_fp || target == node_label(&self_node.node_fp) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cannot revoke self",
        ));
    }
    let mut registry = registry::load_registry_at(dir)?;
    let entry = resolve_node(&registry.authorized_nodes, target)?;
    if entry.node_fp == self_node.node_fp {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cannot revoke self",
        ));
    }
    if entry.state == NodeState::Revoked {
        return Ok(RevokeOutcome::AlreadyRevoked);
    }
    let index = registry
        .authorized_nodes
        .iter()
        .position(|item| item.node_fp == entry.node_fp)
        .expect("resolved registry entry");
    let entry = &mut registry.authorized_nodes[index];
    entry.state = NodeState::Revoked;
    entry.version = entry
        .version
        .checked_add(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "registry version overflow"))?;
    entry.by = self_node.node_fp.clone();
    registry::save_registry_at(dir, &registry)?;
    Ok(RevokeOutcome::Revoked)
}

#[cfg(all(test, not(windows)))]
fn revoke_at(
    dir: &std::path::Path,
    target: &str,
    self_node: &NodeIdentity,
) -> io::Result<RevokeOutcome> {
    let _guard = storage::StateLock::acquire(dir)?;
    revoke_locked_at(dir, target, self_node)
}

pub fn node_label(fingerprint: &str) -> String {
    let suffix: String = fingerprint
        .strip_prefix("SHA256:")
        .unwrap_or(fingerprint)
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .take(8)
        .map(|byte| char::from(byte).to_ascii_lowercase())
        .collect();
    format!("node-{suffix}")
}

#[cfg(all(test, unix))]
mod nodes_revoke_tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "remuda-cluster-revoke-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        storage::create_private_directory(&path).unwrap();
        path
    }

    #[test]
    fn revoke_records_local_fingerprint_and_increments_version() {
        let dir = temp_dir();
        let (self_node, _) = identity::init_identity_at(&dir).unwrap();
        let (target, _) = identity::init_identity_at(&dir.join("target")).unwrap();
        let mut registry = Registry {
            authorized_nodes: vec![
                AuthorizedNode {
                    node_fp: self_node.node_fp.clone(),
                    static_pubkey: encoding::encode_base64(&self_node.static_pubkey),
                    state: NodeState::Admitted,
                    version: 1,
                    by: self_node.node_fp.clone(),
                },
                AuthorizedNode {
                    node_fp: target.node_fp.clone(),
                    static_pubkey: encoding::encode_base64(&target.static_pubkey),
                    state: NodeState::Admitted,
                    version: 7,
                    by: self_node.node_fp.clone(),
                },
            ],
        };
        registry::save_registry_at(&dir, &registry).unwrap();
        let result = revoke_at(&dir, &target.node_name, &self_node).unwrap();
        assert_eq!(result, RevokeOutcome::Revoked);
        registry = registry::load_registry_at(&dir).unwrap();
        let revoked = registry
            .authorized_nodes
            .iter()
            .find(|entry| entry.node_fp == target.node_fp)
            .unwrap();
        assert_eq!(revoked.state, NodeState::Revoked);
        assert_eq!(revoked.version, 8);
        assert_eq!(revoked.by, self_node.node_fp);
    }

    #[test]
    fn revoke_refuses_self() {
        let dir = temp_dir();
        let (self_node, _) = identity::init_identity_at(&dir).unwrap();
        let result = revoke_at(&dir, &self_node.node_name, &self_node).unwrap_err();
        assert!(result.to_string().contains("cannot revoke self"));
    }

    #[test]
    fn revoke_is_idempotent_for_existing_tombstone() {
        let dir = temp_dir();
        let (self_node, _) = identity::init_identity_at(&dir).unwrap();
        let (target, _) = identity::init_identity_at(&dir.join("target")).unwrap();
        registry::save_registry_at(
            &dir,
            &Registry {
                authorized_nodes: vec![AuthorizedNode {
                    node_fp: target.node_fp.clone(),
                    static_pubkey: encoding::encode_base64(&target.static_pubkey),
                    state: NodeState::Revoked,
                    version: 12,
                    by: self_node.node_fp.clone(),
                }],
            },
        )
        .unwrap();
        assert_eq!(
            revoke_at(&dir, &target.node_fp, &self_node).unwrap(),
            RevokeOutcome::AlreadyRevoked
        );
        assert_eq!(
            registry::load_registry_at(&dir).unwrap().authorized_nodes[0].version,
            12
        );
    }

    #[test]
    fn ambiguous_node_label_requires_fingerprint() {
        let entries = vec![
            AuthorizedNode {
                node_fp: "SHA256:abcdefgh-one".into(),
                static_pubkey: String::new(),
                state: NodeState::Admitted,
                version: 1,
                by: String::new(),
            },
            AuthorizedNode {
                node_fp: "SHA256:abcdefgh-two".into(),
                static_pubkey: String::new(),
                state: NodeState::Admitted,
                version: 1,
                by: String::new(),
            },
        ];
        let error = resolve_node(&entries, "node-abcdefgh").unwrap_err();
        assert!(error.to_string().contains("use the fingerprint"));
    }

    #[test]
    fn unknown_revoke_target_is_a_clear_error() {
        let error = resolve_node(&[], "missing-node").unwrap_err();
        assert!(error.to_string().contains("no cluster node matches"));
    }

    #[test]
    fn nodes_table_shows_membership_fields_and_marks_self() {
        let identity = NodeIdentity {
            node_name: "node-self1234".into(),
            node_fp: "SHA256:self1234".into(),
            static_pubkey: vec![],
        };
        let registry = Registry {
            authorized_nodes: vec![AuthorizedNode {
                node_fp: identity.node_fp.clone(),
                static_pubkey: String::new(),
                state: NodeState::Admitted,
                version: 1,
                by: identity.node_fp.clone(),
            }],
        };
        let table = format_nodes_table(&identity, &registry);
        assert!(table.contains("*node-self1234"));
        assert!(table.contains("FINGERPRINT"));
        assert!(table.contains("STATE"));
        assert!(table.contains("VERSION"));
        assert!(table.contains("BY"));
        assert!(table.contains("admitted"));
        assert!(table.contains("1"));
        assert!(table.contains(&identity.node_fp));
        assert!(table.lines().nth(1).unwrap().ends_with(&identity.node_fp));
    }
}
