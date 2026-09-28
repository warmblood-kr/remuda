//! Per-node authorized membership entries, persistence, and merge semantics.

use super::identity;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

const REGISTRY_FILE: &str = "authorized_nodes.json";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AuthorizedNode {
    pub node_fp: String,
    pub static_pubkey: String,
    pub state: NodeState,
    pub version: u64,
    pub by: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NodeState {
    Admitted,
    Revoked,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Registry {
    pub authorized_nodes: Vec<AuthorizedNode>,
}

impl Registry {
    pub fn merge(&mut self, received: &Registry) {
        let mut merged: BTreeMap<String, AuthorizedNode> = self
            .authorized_nodes
            .drain(..)
            .map(|entry| (entry.node_fp.clone(), entry))
            .collect();
        for incoming in &received.authorized_nodes {
            match merged.get(&incoming.node_fp) {
                Some(current)
                    if current.state == NodeState::Revoked
                        && (incoming.state == NodeState::Admitted
                            || current.version >= incoming.version) => {}
                Some(_) if incoming.state == NodeState::Revoked => {
                    merged.insert(incoming.node_fp.clone(), incoming.clone());
                }
                Some(current) if current.version > incoming.version => {}
                Some(current)
                    if current.version == incoming.version && current.by >= incoming.by => {}
                _ => {
                    merged.insert(incoming.node_fp.clone(), incoming.clone());
                }
            }
        }
        self.authorized_nodes = merged.into_values().collect();
    }
}

impl Registry {
    pub fn ensure_self(&mut self, identity: &identity::NodeIdentity) {
        if !self
            .authorized_nodes
            .iter()
            .any(|entry| entry.node_fp == identity.node_fp)
        {
            self.authorized_nodes.push(AuthorizedNode {
                node_fp: identity.node_fp.clone(),
                static_pubkey: identity::public_key_text(&identity.static_pubkey),
                state: NodeState::Admitted,
                version: 1,
                by: identity.node_fp.clone(),
            });
        }
    }
}

pub fn load_registry() -> io::Result<Registry> {
    load_registry_at(&identity::cluster_state_dir()?.join("cluster"))
}

pub fn save_registry(registry: &Registry) -> io::Result<()> {
    let dir = identity::cluster_state_dir()?.join("cluster");
    fs::create_dir_all(&dir)?;
    identity::secure_directory(&dir)?;
    let lock_guard = identity::IdentityLock::acquire(&dir)?;
    let result = save_registry_at(&dir, registry);
    drop(lock_guard);
    result
}

pub(super) fn load_registry_at(dir: &Path) -> io::Result<Registry> {
    let path = dir.join(REGISTRY_FILE);
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(error) => Err(error),
    }
}

pub(super) fn save_registry_at(dir: &Path, registry: &Registry) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(registry).map_err(io::Error::other)?;
    identity::atomic_write(&dir.join(REGISTRY_FILE), &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "remuda-cluster-registry-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn entry(fp: &str, state: NodeState, version: u64, by: &str) -> AuthorizedNode {
        AuthorizedNode {
            node_fp: fp.into(),
            static_pubkey: format!("pub-{fp}"),
            state,
            version,
            by: by.into(),
        }
    }

    #[test]
    fn registry_round_trips_per_entry_fields() {
        let dir = temp_dir();
        let registry = Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 1, "fp-a")],
        };
        save_registry_at(&dir, &registry).unwrap();
        assert_eq!(load_registry_at(&dir).unwrap(), registry);
    }

    #[test]
    fn merge_uses_highest_version_for_admitted_entries() {
        let mut registry = Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 2, "b")],
        };
        registry.merge(&Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 3, "c")],
        });
        assert_eq!(registry.authorized_nodes[0].version, 3);
    }

    #[test]
    fn concurrent_revoke_beats_same_version_admit() {
        let mut registry = Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 2, "b")],
        };
        registry.merge(&Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Revoked, 2, "c")],
        });
        assert_eq!(registry.authorized_nodes[0].state, NodeState::Revoked);
    }

    #[test]
    fn held_tombstone_survives_old_and_higher_version_admit_lists() {
        let mut registry = Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Revoked, 2, "b")],
        };
        registry.merge(&Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 1, "a")],
        });
        registry.merge(&Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 99, "c")],
        });
        assert_eq!(registry.authorized_nodes[0].state, NodeState::Revoked);
    }
}
