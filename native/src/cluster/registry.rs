//! Per-node authorized membership entries, persistence, and merge semantics.

use super::encoding;
#[cfg(not(windows))]
use super::storage;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[cfg(not(windows))]
use std::fs::{self, OpenOptions};
use std::io;
#[cfg(not(windows))]
use std::path::Path;

#[cfg(not(windows))]
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
    pub fn merge(&mut self, received: &Registry) -> io::Result<()> {
        let mut merged = BTreeMap::new();
        for entry in self
            .authorized_nodes
            .iter()
            .chain(&received.authorized_nodes)
        {
            let key = validate_entry(entry)?;
            match merged.get(&key) {
                Some(current) => {
                    if prefer(entry, current) {
                        merged.insert(key, entry.clone());
                    }
                }
                None => {
                    merged.insert(key, entry.clone());
                }
            }
        }
        self.authorized_nodes = merged.into_values().collect();
        Ok(())
    }
}

fn validate_entry(entry: &AuthorizedNode) -> io::Result<Vec<u8>> {
    let public_key = encoding::decode_base64(&entry.static_pubkey)?;
    if public_key.len() != 32 || encoding::fingerprint(&public_key) != entry.node_fp {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "registry fingerprint does not match public key",
        ));
    }
    Ok(public_key)
}

fn prefer(incoming: &AuthorizedNode, current: &AuthorizedNode) -> bool {
    if incoming.state != current.state {
        return incoming.state == NodeState::Revoked;
    }
    incoming.version > current.version
        || (incoming.version == current.version && incoming.by > current.by)
}

pub fn load_registry() -> io::Result<Registry> {
    #[cfg(windows)]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cluster identity storage is not yet hardened on Windows; see warmblood-kr/remuda#214",
    ));
    #[cfg(not(windows))]
    load_registry_at(&storage::cluster_state_dir()?.join("cluster"))
}

pub fn save_registry(_registry: &Registry) -> io::Result<()> {
    #[cfg(windows)]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cluster identity storage is not yet hardened on Windows; see warmblood-kr/remuda#214",
    ));
    #[cfg(not(windows))]
    {
        let dir = storage::cluster_state_dir()?.join("cluster");
        storage::create_private_directory(&dir)?;
        storage::verify_directory(&dir)?;
        let lock_guard = storage::StateLock::acquire(&dir)?;
        let result = save_registry_at(&dir, _registry);
        drop(lock_guard);
        result
    }
}

#[cfg(not(windows))]
pub(super) fn load_registry_at(dir: &Path) -> io::Result<Registry> {
    if fs::symlink_metadata(dir).is_ok() {
        storage::verify_directory(dir)?;
    }
    let path = dir.join(REGISTRY_FILE);
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Registry::default()),
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is a symlink; refusing", path.display()),
            ))
        }
        Err(error) => return Err(error),
    };
    storage::check_private_file(&file, "cluster registry", &path)?;
    use std::io::Read;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let registry: Registry = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    let mut folded = Registry::default();
    for entry in registry.authorized_nodes {
        validate_entry(&entry)?;
        folded.merge(&Registry {
            authorized_nodes: vec![entry],
        })?;
    }
    Ok(folded)
}

#[cfg(not(windows))]
pub(super) fn save_registry_at(dir: &Path, registry: &Registry) -> io::Result<()> {
    let mut validated = Registry::default();
    validated.merge(registry)?;
    let bytes = serde_json::to_vec_pretty(&validated).map_err(io::Error::other)?;
    storage::atomic_write(&dir.join(REGISTRY_FILE), &bytes)
}

#[cfg(all(test, unix))]
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
        storage::create_private_directory(&path).unwrap();
        path
    }

    fn entry(fp: &str, state: NodeState, version: u64, by: &str) -> AuthorizedNode {
        let mut key = [0u8; 32];
        for (index, byte) in fp.bytes().enumerate() {
            key[index % 32] ^= byte;
        }
        let actual_fp = encoding::fingerprint(&key);
        AuthorizedNode {
            node_fp: actual_fp,
            static_pubkey: encoding::encode_base64(&key),
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
    fn load_folds_duplicate_pubkeys_and_preserves_tombstone() {
        let dir = temp_dir();
        let revoked = entry("fp-a", NodeState::Revoked, 2, "a");
        let admitted = entry("fp-a", NodeState::Admitted, 1, "z");
        let json = serde_json::to_vec(&Registry {
            authorized_nodes: vec![revoked, admitted],
        })
        .unwrap();
        storage::atomic_write(&dir.join(REGISTRY_FILE), &json).unwrap();
        let loaded = load_registry_at(&dir).unwrap();
        assert_eq!(loaded.authorized_nodes.len(), 1);
        assert_eq!(loaded.authorized_nodes[0].state, NodeState::Revoked);
    }

    #[test]
    fn load_rejects_fingerprint_that_does_not_match_public_key() {
        let dir = temp_dir();
        let mut invalid = entry("fp-a", NodeState::Admitted, 1, "a");
        invalid.node_fp = "SHA256:wrong".into();
        let json = serde_json::to_vec(&Registry {
            authorized_nodes: vec![invalid],
        })
        .unwrap();
        storage::atomic_write(&dir.join(REGISTRY_FILE), &json).unwrap();
        assert_eq!(
            load_registry_at(&dir).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn load_rejects_registry_with_loose_permissions() {
        let dir = temp_dir();
        storage::atomic_write(&dir.join(REGISTRY_FILE), b"{}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.join(REGISTRY_FILE), fs::Permissions::from_mode(0o644))
                .unwrap();
            assert_eq!(
                load_registry_at(&dir).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
    }

    #[test]
    fn rejects_public_keys_that_are_not_32_bytes() {
        let short_key = [7u8; 31];
        let invalid = AuthorizedNode {
            node_fp: encoding::fingerprint(&short_key),
            static_pubkey: encoding::encode_base64(&short_key),
            state: NodeState::Admitted,
            version: 1,
            by: "node".into(),
        };
        let mut registry = Registry::default();
        assert_eq!(
            registry
                .merge(&Registry {
                    authorized_nodes: vec![invalid]
                })
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn registry_open_refuses_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir();
        let target = dir.join("registry-target");
        storage::atomic_write(&target, b"{\"authorized_nodes\":[]}").unwrap();
        symlink(&target, dir.join(REGISTRY_FILE)).unwrap();
        assert_eq!(
            load_registry_at(&dir).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn save_rejects_invalid_fingerprint_before_writing() {
        let dir = temp_dir();
        let mut invalid = entry("fp-a", NodeState::Admitted, 1, "a");
        invalid.node_fp = "SHA256:wrong".into();
        assert_eq!(
            save_registry_at(
                &dir,
                &Registry {
                    authorized_nodes: vec![invalid]
                }
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(!dir.join(REGISTRY_FILE).exists());
    }

    #[test]
    fn load_refuses_symlinked_cluster_directory() {
        use std::os::unix::fs::symlink;
        let target = temp_dir();
        let link = target.with_extension("link");
        symlink(&target, &link).unwrap();
        let error = load_registry_at(&link).unwrap_err();
        assert!(error.to_string().contains(&link.display().to_string()));
        assert!(error.to_string().contains("symlink"));
    }

    #[test]
    fn merge_uses_highest_version_for_admitted_entries() {
        let mut registry = Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 2, "b")],
        };
        registry
            .merge(&Registry {
                authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 3, "c")],
            })
            .unwrap();
        assert_eq!(registry.authorized_nodes[0].version, 3);
    }

    #[test]
    fn existing_higher_version_admit_is_retained() {
        let mut registry = Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 4, "a")],
        };
        registry
            .merge(&Registry {
                authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 3, "z")],
            })
            .unwrap();
        assert_eq!(registry.authorized_nodes[0].version, 4);
    }

    #[test]
    fn equal_version_admit_uses_larger_by_tiebreak() {
        let mut registry = Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 3, "a")],
        };
        registry
            .merge(&Registry {
                authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 3, "z")],
            })
            .unwrap();
        assert_eq!(registry.authorized_nodes[0].by, "z");
    }

    #[test]
    fn concurrent_revoke_beats_same_version_admit() {
        let mut registry = Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 2, "b")],
        };
        registry
            .merge(&Registry {
                authorized_nodes: vec![entry("fp-a", NodeState::Revoked, 2, "c")],
            })
            .unwrap();
        assert_eq!(registry.authorized_nodes[0].state, NodeState::Revoked);
    }

    #[test]
    fn held_tombstone_survives_old_and_higher_version_admit_lists() {
        let mut registry = Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Revoked, 2, "b")],
        };
        registry
            .merge(&Registry {
                authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 1, "a")],
            })
            .unwrap();
        registry
            .merge(&Registry {
                authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 99, "c")],
            })
            .unwrap();
        assert_eq!(registry.authorized_nodes[0].state, NodeState::Revoked);
    }

    #[test]
    fn lower_version_revoke_wins_over_higher_version_admit() {
        let mut registry = Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 5, "z")],
        };
        registry
            .merge(&Registry {
                authorized_nodes: vec![entry("fp-a", NodeState::Revoked, 1, "a")],
            })
            .unwrap();
        assert_eq!(registry.authorized_nodes[0].state, NodeState::Revoked);
    }

    #[test]
    fn same_version_revoke_wins_even_when_by_favors_admit() {
        let mut registry = Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 2, "z")],
        };
        registry
            .merge(&Registry {
                authorized_nodes: vec![entry("fp-a", NodeState::Revoked, 2, "a")],
            })
            .unwrap();
        assert_eq!(registry.authorized_nodes[0].state, NodeState::Revoked);
    }

    #[test]
    fn mismatched_fingerprint_is_rejected_during_merge() {
        let mut registry = Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Revoked, 2, "a")],
        };
        let mut incoming = entry("fp-b", NodeState::Admitted, 99, "z");
        incoming.static_pubkey = registry.authorized_nodes[0].static_pubkey.clone();
        let before = registry.clone();
        assert!(registry
            .merge(&Registry {
                authorized_nodes: vec![incoming]
            })
            .is_err());
        assert_eq!(registry, before);
        assert_eq!(registry.authorized_nodes.len(), 1);
        assert_eq!(registry.authorized_nodes[0].state, NodeState::Revoked);
    }

    #[test]
    fn same_fingerprint_cannot_swap_the_public_key() {
        let mut registry = Registry {
            authorized_nodes: vec![entry("fp-a", NodeState::Admitted, 2, "a")],
        };
        let mut incoming = entry("fp-a", NodeState::Admitted, 3, "z");
        incoming.static_pubkey = entry("fp-b", NodeState::Admitted, 1, "b").static_pubkey;
        let before = registry.clone();
        assert!(registry
            .merge(&Registry {
                authorized_nodes: vec![incoming]
            })
            .is_err());
        assert_eq!(registry, before);
        assert_eq!(
            registry.authorized_nodes[0].static_pubkey,
            entry("fp-a", NodeState::Admitted, 1, "a").static_pubkey
        );
    }

    #[test]
    fn duplicate_local_entries_are_folded_through_merge_rules() {
        let mut registry = Registry {
            authorized_nodes: vec![
                entry("fp-a", NodeState::Revoked, 2, "a"),
                entry("fp-a", NodeState::Admitted, 1, "z"),
            ],
        };
        registry.merge(&Registry::default()).unwrap();
        assert_eq!(registry.authorized_nodes.len(), 1);
        assert_eq!(registry.authorized_nodes[0].state, NodeState::Revoked);
    }
}
