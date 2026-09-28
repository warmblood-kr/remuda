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
pub const MAX_UPDATE_BYTES: usize = 1024 * 1024;
pub const MAX_UPDATE_ENTRIES: usize = 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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

/// One authenticated member's bounded membership update.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RegistryUpdate {
    pub sender_fp: String,
    pub entries: Vec<AuthorizedNode>,
}

impl RegistryUpdate {
    /// Encode a validated update as bounded strict JSON.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        validate_update(self)?;
        let encoded = serde_json::to_vec(self).map_err(io::Error::other)?;
        if encoded.len() > MAX_UPDATE_BYTES {
            return Err(invalid_update("registry update exceeds byte cap"));
        }
        Ok(encoded)
    }

    /// Decode bounded strict JSON and validate all registry fingerprints.
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_UPDATE_BYTES {
            return Err(invalid_update("registry update exceeds byte cap"));
        }
        let update: Self = serde_json::from_slice(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        validate_update(&update)?;
        Ok(update)
    }
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

    /// Return a canonical SHA-256 digest independent of entry order.
    pub fn digest(&self) -> io::Result<String> {
        let mut canonical = Registry::default();
        canonical.merge(self)?;
        for entry in &mut canonical.authorized_nodes {
            let public_key = encoding::decode_base64(&entry.static_pubkey)?;
            entry.static_pubkey = encoding::encode_base64(&public_key);
        }
        let encoded = serde_json::to_vec(&canonical.authorized_nodes).map_err(io::Error::other)?;
        Ok(encoding::fingerprint(&encoded))
    }
}

fn validate_update(update: &RegistryUpdate) -> io::Result<()> {
    if !valid_fingerprint(&update.sender_fp) {
        return Err(invalid_update(
            "registry update sender is not a SHA256 fingerprint",
        ));
    }
    if update.entries.len() > MAX_UPDATE_ENTRIES {
        return Err(invalid_update("registry update exceeds entry cap"));
    }
    for entry in &update.entries {
        validate_entry(entry)?;
    }
    Ok(())
}

fn invalid_update(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn validate_entry(entry: &AuthorizedNode) -> io::Result<Vec<u8>> {
    let public_key = encoding::decode_base64(&entry.static_pubkey)?;
    if public_key.len() != 32 || encoding::fingerprint(&public_key) != entry.node_fp {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "registry fingerprint does not match public key",
        ));
    }
    if !valid_fingerprint(&entry.by) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "registry by is not a valid SHA256 fingerprint",
        ));
    }
    Ok(public_key)
}

fn valid_fingerprint(value: &str) -> bool {
    value
        .strip_prefix("SHA256:")
        .and_then(|encoded| {
            encoding::decode_base64(encoded)
                .ok()
                .map(|decoded| (encoded, decoded))
        })
        .is_some_and(|(encoded, decoded)| {
            decoded.len() == 32
                && encoding::encode_base64(&decoded).trim_end_matches('=') == encoded
        })
}

/// Validate and merge one authenticated member's update into the local view.
pub fn apply_update(
    registry: &mut Registry,
    update: &RegistryUpdate,
    authenticated_sender_pubkey: &[u8],
) -> io::Result<Vec<AuthorizedNode>> {
    validate_update(update)?;
    if authenticated_sender_pubkey.len() != 32
        || encoding::fingerprint(authenticated_sender_pubkey) != update.sender_fp
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "authenticated sender key does not match update sender fingerprint",
        ));
    }

    let mut current = Registry::default();
    current.merge(registry)?;
    let sender = current
        .authorized_nodes
        .iter()
        .find(|entry| entry.node_fp == update.sender_fp)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "registry update sender is not a current member",
            )
        })?;
    if sender.state != NodeState::Admitted {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "registry update sender is revoked",
        ));
    }
    if validate_entry(sender)? != authenticated_sender_pubkey {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "authenticated sender key does not match current registry member",
        ));
    }

    let previous = current.clone();
    let received = Registry {
        authorized_nodes: update.entries.clone(),
    };
    current.merge(&received)?;
    let changed = current
        .authorized_nodes
        .iter()
        .filter(|entry| !previous.authorized_nodes.contains(entry))
        .cloned()
        .collect();
    registry.authorized_nodes = current.authorized_nodes;
    Ok(changed)
}

/// Apply an update to the local persisted registry under one state lock.
pub fn apply_registry_update(
    _update: &RegistryUpdate,
    _authenticated_sender_pubkey: &[u8],
) -> io::Result<Vec<AuthorizedNode>> {
    #[cfg(windows)]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cluster identity storage is not yet hardened on Windows; see warmblood-kr/remuda#214",
    ));
    #[cfg(not(windows))]
    apply_update_at(
        &storage::cluster_state_dir()?.join("cluster"),
        _update,
        _authenticated_sender_pubkey,
    )
}

#[cfg(not(windows))]
fn apply_update_at(
    dir: &Path,
    update: &RegistryUpdate,
    authenticated_sender_pubkey: &[u8],
) -> io::Result<Vec<AuthorizedNode>> {
    match fs::symlink_metadata(dir) {
        Ok(_) => storage::verify_directory(dir)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "cluster is not initialized; run `remuda cluster init`",
            ))
        }
        Err(error) => return Err(error),
    }
    let _guard = storage::StateLock::acquire(dir)?;
    let mut registry = load_registry_at(dir)?;
    let changed = apply_update(&mut registry, update, authenticated_sender_pubkey)?;
    if !changed.is_empty() {
        save_registry_at(dir, &registry)?;
    }
    Ok(changed)
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
            by: origin(by),
        }
    }

    fn origin(label: &str) -> String {
        let byte = label.as_bytes()[0];
        format!(
            "SHA256:{}",
            encoding::encode_base64(&[byte; 32]).trim_end_matches('=')
        )
    }

    fn admitted_sender() -> AuthorizedNode {
        entry("replication-sender", NodeState::Admitted, 1, "sender")
    }

    fn public_key(entry: &AuthorizedNode) -> Vec<u8> {
        encoding::decode_base64(&entry.static_pubkey).unwrap()
    }

    fn registry_with_sender(sender: &AuthorizedNode) -> Registry {
        Registry {
            authorized_nodes: vec![sender.clone()],
        }
    }

    #[test]
    fn digest_is_independent_of_entry_order() {
        let first = entry("digest-a", NodeState::Admitted, 1, "a");
        let second = entry("digest-b", NodeState::Revoked, 4, "b");
        let left = Registry {
            authorized_nodes: vec![first.clone(), second.clone()],
        };
        let right = Registry {
            authorized_nodes: vec![second, first],
        };
        assert_eq!(left.digest().unwrap(), right.digest().unwrap());

        let mut unpadded = left.authorized_nodes[0].clone();
        unpadded.static_pubkey = unpadded.static_pubkey.trim_end_matches('=').into();
        assert_eq!(
            Registry {
                authorized_nodes: vec![unpadded],
            }
            .digest()
            .unwrap(),
            Registry {
                authorized_nodes: vec![left.authorized_nodes[0].clone()],
            }
            .digest()
            .unwrap()
        );
    }

    #[test]
    fn registry_update_round_trips_and_rejects_unknown_fields() {
        let sender = admitted_sender();
        let update = RegistryUpdate {
            sender_fp: sender.node_fp,
            entries: vec![entry(
                "replication-target",
                NodeState::Admitted,
                1,
                "sender",
            )],
        };
        let encoded = update.encode().unwrap();
        assert_eq!(RegistryUpdate::decode(&encoded).unwrap(), update);
        let mut unknown = encoded;
        unknown.pop();
        unknown.extend_from_slice(b",\"unknown\":true}");
        assert_eq!(
            RegistryUpdate::decode(&unknown).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let encoded = String::from_utf8(update.encode().unwrap()).unwrap();
        let nested_unknown = encoded.replace(
            "\"state\":\"admitted\"",
            "\"state\":\"admitted\",\"extra\":true",
        );
        assert_eq!(
            RegistryUpdate::decode(nested_unknown.as_bytes())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn registry_update_rejects_invalid_entry_attribution() {
        let sender = admitted_sender();
        let mut target = entry("invalid-attribution", NodeState::Admitted, 1, "sender");
        target.by.push('\n');
        let update = RegistryUpdate {
            sender_fp: sender.node_fp,
            entries: vec![target],
        };
        assert_eq!(
            update.encode().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn registry_update_enforces_byte_and_entry_caps() {
        let sender = admitted_sender();
        let too_many = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![
                entry("capped", NodeState::Admitted, 1, "sender");
                MAX_UPDATE_ENTRIES + 1
            ],
        };
        assert!(too_many.encode().is_err());
        assert!(RegistryUpdate::decode(&vec![b' '; MAX_UPDATE_BYTES + 1]).is_err());
        let too_many_json = serde_json::to_vec(&too_many).unwrap();
        assert!(RegistryUpdate::decode(&too_many_json).is_err());
    }

    #[test]
    fn update_refuses_tombstoned_sender_without_changing_registry() {
        let mut sender = admitted_sender();
        sender.state = NodeState::Revoked;
        let mut registry = registry_with_sender(&sender);
        let before = registry.clone();
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![entry("revoked-push", NodeState::Admitted, 1, "sender")],
        };
        assert!(apply_update(&mut registry, &update, &public_key(&sender)).is_err());
        assert_eq!(registry, before);
    }

    #[test]
    fn update_refuses_nonmember_sender() {
        let sender = admitted_sender();
        let mut registry = Registry::default();
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![entry("nonmember-push", NodeState::Admitted, 1, "sender")],
        };
        assert!(apply_update(&mut registry, &update, &public_key(&sender)).is_err());
        assert!(registry.authorized_nodes.is_empty());
    }

    #[test]
    fn update_refuses_a_different_authenticated_sender_key() {
        let sender = admitted_sender();
        let other = entry("other-authenticated-key", NodeState::Admitted, 1, "other");
        let mut registry = registry_with_sender(&sender);
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![entry("forged-sender", NodeState::Admitted, 1, "sender")],
        };
        assert!(apply_update(&mut registry, &update, &public_key(&other)).is_err());
        assert_eq!(registry, registry_with_sender(&sender));
    }

    #[test]
    fn update_rejects_pubkey_swap_and_alternate_fingerprint() {
        let sender = admitted_sender();
        let mut registry = registry_with_sender(&sender);
        let original = entry("victim", NodeState::Admitted, 1, "sender");
        let mut swapped = original.clone();
        swapped.static_pubkey =
            entry("different-key", NodeState::Admitted, 1, "sender").static_pubkey;
        let swap_update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![swapped],
        };
        assert!(apply_update(&mut registry, &swap_update, &public_key(&sender)).is_err());

        let mut alternate = original;
        alternate.node_fp = entry("alternate", NodeState::Admitted, 1, "sender").node_fp;
        let alternate_update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![alternate],
        };
        assert!(apply_update(&mut registry, &alternate_update, &public_key(&sender)).is_err());
        assert_eq!(registry, registry_with_sender(&sender));
    }

    #[test]
    fn revoked_entry_survives_old_and_newer_update_lists() {
        let sender = admitted_sender();
        let mut target = entry("replication-target", NodeState::Revoked, 7, "sender");
        let mut registry = registry_with_sender(&sender);
        registry.authorized_nodes.push(target.clone());
        for version in [1, 99] {
            target.state = NodeState::Admitted;
            target.version = version;
            let update = RegistryUpdate {
                sender_fp: sender.node_fp.clone(),
                entries: vec![target.clone()],
            };
            apply_update(&mut registry, &update, &public_key(&sender)).unwrap();
            let stored = registry
                .authorized_nodes
                .iter()
                .find(|entry| entry.node_fp == target.node_fp)
                .unwrap();
            assert_eq!(stored.state, NodeState::Revoked);
        }
    }

    #[test]
    fn concurrent_persisted_updates_keep_all_changes() {
        use std::thread;

        let dir = temp_dir();
        let sender = admitted_sender();
        save_registry_at(&dir, &registry_with_sender(&sender)).unwrap();
        let sender_fp = sender.node_fp.clone();
        let auth_key = public_key(&sender);
        let workers: Vec<_> = (0..8)
            .map(|index| {
                let dir = dir.clone();
                let sender_fp = sender_fp.clone();
                let auth_key = auth_key.clone();
                thread::spawn(move || {
                    let update = RegistryUpdate {
                        sender_fp,
                        entries: vec![entry(
                            &format!("concurrent-{index}"),
                            NodeState::Admitted,
                            1,
                            "sender",
                        )],
                    };
                    apply_update_at(&dir, &update, &auth_key).unwrap()
                })
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap().len(), 1);
        }
        let registry = load_registry_at(&dir).unwrap();
        assert_eq!(registry.authorized_nodes.len(), 9);
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
    fn load_rejects_control_characters_in_by_fingerprint() {
        let dir = temp_dir();
        let mut invalid = entry("fp-a", NodeState::Admitted, 1, "a");
        invalid.by = format!("{}\x1b\n", invalid.by);
        let bytes = serde_json::to_vec(&Registry {
            authorized_nodes: vec![invalid],
        })
        .unwrap();
        storage::atomic_write(&dir.join(REGISTRY_FILE), &bytes).unwrap();
        assert_eq!(
            load_registry_at(&dir).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn merge_rejects_malformed_by_fingerprint() {
        let mut invalid = entry("fp-a", NodeState::Admitted, 1, "a");
        invalid.by = "SHA256:not a fingerprint".into();
        let mut registry = Registry::default();
        assert_eq!(
            registry
                .merge(&Registry {
                    authorized_nodes: vec![invalid],
                })
                .unwrap_err()
                .kind(),
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
            by: origin("node"),
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
        assert_eq!(registry.authorized_nodes[0].by, origin("z"));
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
