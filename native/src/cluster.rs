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
use std::net::SocketAddr;

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

/// A node's authenticated transport target, resolved from local membership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub address: SocketAddr,
    pub pinned_static_key: Vec<u8>,
}

/// Resolve a node label to its admitted pin and routing address.
///
/// The address is a caller-provided hint until registry endpoints land. The
/// Noise pin always comes from the local registry; routing never changes the
/// identity being authenticated.
pub fn resolve_target(
    target: &str,
    addr_override: Option<SocketAddr>,
) -> io::Result<ResolvedTarget> {
    let (_, registry) = nodes()?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cluster is not initialized; run `remuda cluster init`",
        )
    })?;
    let entry = resolve_node(&registry.authorized_nodes, target)?;
    if entry.state != NodeState::Admitted {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("node {} is revoked", entry.node_fp),
        ));
    }
    // PR9 adds an optional endpoint to AuthorizedNode. Until then, callers
    // provide --addr; this function is the shared seam for that fallback.
    let address = addr_override
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "supply --addr HOST:PORT"))?;
    let pinned_static_key = encoding::decode_base64(&entry.static_pubkey)?;
    if pinned_static_key.len() != 32 || encoding::fingerprint(&pinned_static_key) != entry.node_fp {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid pinned key in registry",
        ));
    }
    Ok(ResolvedTarget {
        address,
        pinned_static_key,
    })
}

/// Format the local registry with an asterisk on this node's row.
pub fn format_nodes_table(identity: &NodeIdentity, registry: &Registry) -> String {
    let rows: Vec<_> = registry
        .authorized_nodes
        .iter()
        .map(|entry| {
            (
                escape_registry_field(&node_label(&entry.node_fp)),
                escape_registry_field(&entry.node_fp),
                match entry.state {
                    NodeState::Admitted => "admitted",
                    NodeState::Revoked => "revoked",
                },
                entry.version.to_string(),
                escape_registry_field(&entry.by),
                entry.node_fp == identity.node_fp,
            )
        })
        .collect();
    let node_width = rows
        .iter()
        .map(|row| row.0.len() + usize::from(row.5))
        .max()
        .unwrap_or(0)
        .max("NODE".len());
    let fingerprint_width = rows
        .iter()
        .map(|row| row.1.len())
        .max()
        .unwrap_or(0)
        .max("FINGERPRINT".len());
    let version_width = rows
        .iter()
        .map(|row| row.3.len())
        .max()
        .unwrap_or(0)
        .max("VERSION".len());
    let by_width = rows
        .iter()
        .map(|row| row.4.len())
        .max()
        .unwrap_or(0)
        .max("BY".len());
    let mut output = String::new();
    let _ = std::fmt::Write::write_fmt(
        &mut output,
        format_args!(
            " {:<node_width$}  {:<fingerprint_width$}  {:<8} {:<version_width$}  {:<by_width$}\n",
            "NODE", "FINGERPRINT", "STATE", "VERSION", "BY"
        ),
    );
    for (node, fingerprint, state, version, by, is_self) in rows {
        let marker = if is_self { "*" } else { " " };
        let _ = std::fmt::Write::write_fmt(
            &mut output,
            format_args!(
                "{marker}{node:<node_width$}  {fingerprint:<fingerprint_width$}  {state:<8} {version:<version_width$}  {by:<by_width$}\n"
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
        let dir = storage::cluster_state_dir()?.join("cluster");
        revoke_at(&dir, _target)
    }
}

fn escape_registry_field(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}

#[cfg(not(windows))]
fn revoke_locked_at(
    dir: &std::path::Path,
    target: &str,
    self_node: &NodeIdentity,
) -> io::Result<RevokeOutcome> {
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
    entry.version = entry.version.saturating_add(1);
    entry.by = self_node.node_fp.clone();
    registry::save_registry_at(dir, &registry)?;
    Ok(RevokeOutcome::Revoked)
}

#[cfg(not(windows))]
fn revoke_at(dir: &std::path::Path, target: &str) -> io::Result<RevokeOutcome> {
    match std::fs::symlink_metadata(dir) {
        Ok(_) => storage::verify_directory(dir)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(cluster_not_initialized_error())
        }
        Err(error) => return Err(error),
    }
    let _guard = storage::StateLock::acquire(dir)?;
    let self_node = identity::load_identity_at(dir).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            cluster_not_initialized_error()
        } else {
            error
        }
    })?;
    revoke_locked_at(dir, target, &self_node)
}

#[cfg(not(windows))]
fn cluster_not_initialized_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "cluster is not initialized; run `remuda cluster init`",
    )
}

pub fn node_label(fingerprint: &str) -> String {
    identity::node_name(fingerprint)
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
        let result = revoke_at(&dir, &target.node_name).unwrap();
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
    fn local_revoke_succeeds_after_max_version_admission_and_tombstone_wins() {
        let dir = temp_dir();
        let (self_node, _) = identity::init_identity_at(&dir).unwrap();
        let (sender, _) = identity::init_identity_at(&dir.join("sender")).unwrap();
        let (target, _) = identity::init_identity_at(&dir.join("target")).unwrap();
        registry::save_registry_at(
            &dir,
            &Registry {
                authorized_nodes: vec![
                    AuthorizedNode {
                        node_fp: self_node.node_fp.clone(),
                        static_pubkey: encoding::encode_base64(&self_node.static_pubkey),
                        state: NodeState::Admitted,
                        version: 1,
                        by: self_node.node_fp.clone(),
                    },
                    AuthorizedNode {
                        node_fp: sender.node_fp.clone(),
                        static_pubkey: encoding::encode_base64(&sender.static_pubkey),
                        state: NodeState::Admitted,
                        version: 1,
                        by: self_node.node_fp.clone(),
                    },
                ],
            },
        )
        .unwrap();
        let mut admitted_at_max = Registry {
            authorized_nodes: vec![AuthorizedNode {
                node_fp: target.node_fp.clone(),
                static_pubkey: encoding::encode_base64(&target.static_pubkey),
                state: NodeState::Admitted,
                version: u64::MAX,
                by: sender.node_fp.clone(),
            }],
        };
        let mut registry = registry::load_registry_at(&dir).unwrap();
        registry::apply_update(
            &mut registry,
            &registry::RegistryUpdate {
                sender_fp: sender.node_fp.clone(),
                entries: std::mem::take(&mut admitted_at_max.authorized_nodes),
            },
            &sender.static_pubkey,
            &self_node.node_fp,
        )
        .unwrap();
        registry::save_registry_at(&dir, &registry).unwrap();
        assert_eq!(
            revoke_at(&dir, &target.node_fp).unwrap(),
            RevokeOutcome::Revoked
        );
        let revoked = registry::load_registry_at(&dir)
            .unwrap()
            .authorized_nodes
            .into_iter()
            .find(|entry| entry.node_fp == target.node_fp)
            .unwrap();
        assert_eq!(revoked.state, NodeState::Revoked);
        assert_eq!(revoked.version, u64::MAX);
    }

    #[test]
    fn revoke_refuses_self() {
        let dir = temp_dir();
        let (self_node, _) = identity::init_identity_at(&dir).unwrap();
        registry::save_registry_at(
            &dir,
            &Registry {
                authorized_nodes: vec![AuthorizedNode {
                    node_fp: self_node.node_fp.clone(),
                    static_pubkey: encoding::encode_base64(&self_node.static_pubkey),
                    state: NodeState::Admitted,
                    version: 1,
                    by: self_node.node_fp.clone(),
                }],
            },
        )
        .unwrap();
        let result = revoke_at(&dir, &self_node.node_name).unwrap_err();
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
            revoke_at(&dir, &target.node_fp).unwrap(),
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
        let fingerprint = encoding::fingerprint(&[7; 32]);
        let identity = NodeIdentity {
            node_name: identity::node_name(&fingerprint),
            node_fp: fingerprint.clone(),
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
        assert!(table.contains(&format!("*{}", identity.node_name)));
        assert!(table.contains("FINGERPRINT"));
        assert!(table.contains("STATE"));
        assert!(table.contains("VERSION"));
        assert!(table.contains("BY"));
        assert!(table.contains("admitted"));
        assert!(table.contains("1"));
        assert!(table.contains(&identity.node_fp));
        assert!(identity.node_fp.len() > 46);
        assert_eq!(
            table.lines().next().unwrap().find("STATE"),
            table.lines().nth(1).unwrap().find("admitted")
        );
    }

    #[test]
    fn nodes_table_escapes_control_characters_in_registry_fields() {
        let fingerprint = encoding::fingerprint(&[7; 32]);
        let identity = NodeIdentity {
            node_name: identity::node_name(&fingerprint),
            node_fp: fingerprint,
            static_pubkey: vec![],
        };
        let registry = Registry {
            authorized_nodes: vec![AuthorizedNode {
                node_fp: "SHA256:other".into(),
                static_pubkey: String::new(),
                state: NodeState::Admitted,
                version: 1,
                by: "SHA256:bad\x1b\nvalue".into(),
            }],
        };
        let table = format_nodes_table(&identity, &registry);
        assert!(table.contains("SHA256:bad\\u{1b}\\nvalue"));
        assert!(!table.contains('\x1b'));
        assert_eq!(table.lines().count(), 2);
    }

    #[test]
    fn revoke_does_not_create_a_missing_cluster_directory() {
        let dir = temp_dir().join("missing-cluster");
        let error = revoke_at(&dir, "unknown").unwrap_err();
        assert!(error.to_string().contains("cluster is not initialized"));
        assert!(!dir.exists());
    }

    #[test]
    fn concurrent_revoke_keeps_all_tombstones() {
        use std::thread;

        let dir = temp_dir();
        let (self_node, _) = identity::init_identity_at(&dir).unwrap();
        let mut entries = vec![AuthorizedNode {
            node_fp: self_node.node_fp.clone(),
            static_pubkey: encoding::encode_base64(&self_node.static_pubkey),
            state: NodeState::Admitted,
            version: 1,
            by: self_node.node_fp.clone(),
        }];
        let mut targets = Vec::new();
        for index in 0..8 {
            let target_dir = dir.join(format!("target-{index}"));
            let (target, _) = identity::init_identity_at(&target_dir).unwrap();
            entries.push(AuthorizedNode {
                node_fp: target.node_fp.clone(),
                static_pubkey: encoding::encode_base64(&target.static_pubkey),
                state: NodeState::Admitted,
                version: 3,
                by: self_node.node_fp.clone(),
            });
            targets.push(target.node_fp);
        }
        registry::save_registry_at(
            &dir,
            &Registry {
                authorized_nodes: entries,
            },
        )
        .unwrap();
        let workers: Vec<_> = targets
            .into_iter()
            .map(|target| {
                let dir = dir.clone();
                thread::spawn(move || revoke_at(&dir, &target).unwrap())
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), RevokeOutcome::Revoked);
        }
        let registry = registry::load_registry_at(&dir).unwrap();
        assert_eq!(
            registry
                .authorized_nodes
                .iter()
                .filter(|entry| entry.state == NodeState::Revoked)
                .count(),
            8
        );
    }
}
