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
pub const MAX_REGISTRY_BYTES: usize = 1024 * 1024;
pub const MAX_REGISTRY_ENTRIES: usize = 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthorizedNode {
    pub node_fp: String,
    pub static_pubkey: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub applied: Vec<AuthorizedNode>,
    pub alerts: Vec<String>,
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
        if merged.len() > MAX_REGISTRY_ENTRIES {
            return Err(invalid_update("registry exceeds total entry cap"));
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
    crate::net::frame::validate_static_public_key(&public_key).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "registry static public key is a low-order X25519 point",
        )
    })?;
    if !valid_fingerprint(&entry.by) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "registry by is not a valid SHA256 fingerprint",
        ));
    }
    if let Some(endpoint) = &entry.endpoint {
        let parsed: std::net::SocketAddr = endpoint.parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "registry endpoint must be host:port",
            )
        })?;
        if parsed.to_string() != *endpoint || super::join_line::validate_endpoint(parsed).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "registry endpoint must be a canonical unicast host:port",
            ));
        }
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
#[cfg_attr(windows, allow(dead_code))]
pub(super) fn apply_update(
    registry: &mut Registry,
    update: &RegistryUpdate,
    authenticated_sender_pubkey: &[u8],
    receiver_fp: &str,
) -> io::Result<UpdateOutcome> {
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
    let mut outcome = UpdateOutcome::default();
    let mut accepted = Vec::with_capacity(update.entries.len());
    for entry in &update.entries {
        if entry.node_fp == receiver_fp && entry.state == NodeState::Revoked {
            outcome.alerts.push(format!(
                "dropped peer tombstone for receiver own key {}",
                receiver_fp
            ));
            continue;
        }
        if current
            .authorized_nodes
            .iter()
            .any(|known| known.node_fp == entry.by && known.state == NodeState::Revoked)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "registry entry is attributed to a revoked member",
            ));
        }
        let known = current
            .authorized_nodes
            .iter()
            .find(|known| known.node_fp == entry.node_fp);
        let exists = known.is_some();
        let endpoint_admission =
            !exists && entry.state == NodeState::Admitted && entry.by == update.sender_fp;
        if !exists && entry.state == NodeState::Admitted && entry.by != update.sender_fp {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "new admission must be vouched for by the update sender",
            ));
        }
        let mut accepted_entry = entry.clone();
        if let Some(known) = known {
            let endpoint_changed = known.endpoint != entry.endpoint;
            let wins = prefer(entry, known);
            if endpoint_changed
                && wins
                && entry.state == NodeState::Admitted
                && entry.node_fp != update.sender_fp
                && !endpoint_admission
            {
                // Endpoint hints are owned by their node, but an untrusted
                // hint must not block an otherwise winning registry update.
                accepted_entry.endpoint = known.endpoint.clone();
            }
        }
        accepted.push(accepted_entry);
    }
    let received = Registry {
        authorized_nodes: accepted,
    };
    current.merge(&received)?;
    outcome.applied = current
        .authorized_nodes
        .iter()
        .filter(|entry| !previous.authorized_nodes.contains(entry))
        .cloned()
        .collect();
    registry.authorized_nodes = current.authorized_nodes;
    Ok(outcome)
}

/// Apply an update to the local persisted registry under one state lock.
pub fn apply_registry_update(
    _update: &RegistryUpdate,
    _authenticated_sender_pubkey: &[u8],
) -> io::Result<UpdateOutcome> {
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
) -> io::Result<UpdateOutcome> {
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
    let receiver = super::identity::load_identity_at(dir)?;
    let outcome = apply_update(
        &mut registry,
        update,
        authenticated_sender_pubkey,
        &receiver.node_fp,
    )?;
    if !outcome.applied.is_empty() {
        save_registry_at(dir, &registry)?;
    }
    for alert in &outcome.alerts {
        eprintln!("remuda: cluster replication alert: {alert}");
    }
    Ok(outcome)
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

/// Return a cheap change token for the atomically replaced membership file.
/// Listener request paths use this to invalidate a cached registry without
/// taking the cluster state lock for every admitted peer.
pub fn registry_revision_token() -> io::Result<String> {
    #[cfg(windows)]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cluster identity storage is not yet hardened on Windows; see warmblood-kr/remuda#214",
    ));
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::MetadataExt;
        use std::time::UNIX_EPOCH;
        let path = storage::cluster_state_dir()?
            .join("cluster")
            .join(REGISTRY_FILE);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cluster registry is a symlink; refusing",
                ));
            }
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok("missing".to_owned());
            }
            Err(error) => return Err(error),
        };
        let modified = metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Ok(format!(
            "{}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            modified,
            metadata.mode() & 0o777
        ))
    }
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
    let file = match options.open(&path) {
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
    if file.metadata()?.len() > MAX_REGISTRY_BYTES as u64 {
        return Err(invalid_update("cluster registry exceeds byte cap"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_REGISTRY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_REGISTRY_BYTES {
        return Err(invalid_update("cluster registry exceeds byte cap"));
    }
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
            endpoint: None,
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

    fn apply_as_sender(
        registry: &mut Registry,
        update: &RegistryUpdate,
        authenticated_sender_pubkey: &[u8],
    ) -> io::Result<UpdateOutcome> {
        apply_update(
            registry,
            update,
            authenticated_sender_pubkey,
            &update.sender_fp,
        )
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

        let mut endpoint = left.clone();
        endpoint.authorized_nodes[0].endpoint = Some("192.0.2.4:9443".into());
        assert_ne!(left.digest().unwrap(), endpoint.digest().unwrap());

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
        assert!(apply_as_sender(&mut registry, &update, &public_key(&sender)).is_err());
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
        assert!(apply_as_sender(&mut registry, &update, &public_key(&sender)).is_err());
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
        assert!(apply_as_sender(&mut registry, &update, &public_key(&other)).is_err());
        assert_eq!(registry, registry_with_sender(&sender));
    }

    #[test]
    fn update_rejects_sender_fingerprint_mismatch_alone() {
        let sender = admitted_sender();
        let registry = registry_with_sender(&sender);
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![],
        };
        let other = entry("other", NodeState::Admitted, 1, "other");
        assert!(apply_update(
            &mut registry.clone(),
            &update,
            &public_key(&other),
            &sender.node_fp
        )
        .is_err());
    }

    #[test]
    fn update_drops_self_tombstone_surfaces_alert_and_applies_other_entries() {
        let sender = admitted_sender();
        let receiver = entry("local-receiver", NodeState::Admitted, 1, "sender");
        let mut registry = registry_with_sender(&sender);
        registry.authorized_nodes.push(receiver.clone());
        let mut self_tombstone = receiver.clone();
        self_tombstone.state = NodeState::Revoked;
        self_tombstone.version = 2;
        let mut other = entry("other-valid-member", NodeState::Admitted, 1, "sender");
        other.by = sender.node_fp.clone();
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![self_tombstone, other.clone()],
        };
        let outcome = apply_update(
            &mut registry,
            &update,
            &public_key(&sender),
            &receiver.node_fp,
        )
        .unwrap();
        assert_eq!(outcome.applied, vec![other]);
        assert_eq!(outcome.alerts.len(), 1);
        assert!(outcome.alerts[0].contains("own key"));
        assert_eq!(
            registry
                .authorized_nodes
                .iter()
                .find(|entry| entry.node_fp == receiver.node_fp)
                .unwrap()
                .state,
            NodeState::Admitted
        );
    }

    #[test]
    fn update_rejects_revoked_attribution_and_non_sender_new_admission() {
        let sender = admitted_sender();
        let mut revoked_voucher = entry("revoked-voucher", NodeState::Revoked, 2, "sender");
        let receiver = entry("local-receiver", NodeState::Admitted, 1, "sender");
        let mut registry = registry_with_sender(&sender);
        registry
            .authorized_nodes
            .extend([receiver.clone(), revoked_voucher.clone()]);
        let mut target = entry("target", NodeState::Admitted, 1, "sender");
        target.by = revoked_voucher.node_fp.clone();
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![target],
        };
        assert!(apply_update(
            &mut registry.clone(),
            &update,
            &public_key(&sender),
            &receiver.node_fp
        )
        .is_err());

        revoked_voucher.state = NodeState::Admitted;
        registry
            .authorized_nodes
            .retain(|entry| entry.node_fp != revoked_voucher.node_fp);
        registry.authorized_nodes.push(revoked_voucher.clone());
        let mut new_target = entry("new-target", NodeState::Admitted, 1, "sender");
        new_target.by = revoked_voucher.node_fp;
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![new_target],
        };
        assert!(apply_update(
            &mut registry,
            &update,
            &public_key(&sender),
            &receiver.node_fp
        )
        .is_err());
    }

    #[test]
    fn later_voucher_revocation_does_not_cascade_to_existing_entry() {
        let sender = admitted_sender();
        let voucher = entry("voucher", NodeState::Admitted, 1, "sender");
        let mut target = entry("already-vouched", NodeState::Admitted, 3, "sender");
        target.by = voucher.node_fp.clone();
        let receiver = entry("local-receiver", NodeState::Admitted, 1, "sender");
        let mut registry = registry_with_sender(&sender);
        registry
            .authorized_nodes
            .extend([receiver.clone(), voucher.clone(), target.clone()]);

        // The source can be revoked later; this does not cascade to its old entry.
        registry
            .authorized_nodes
            .iter_mut()
            .find(|entry| entry.node_fp == voucher.node_fp)
            .unwrap()
            .state = NodeState::Revoked;
        let mut unrelated = entry("unrelated-new-entry", NodeState::Admitted, 1, "sender");
        unrelated.by = sender.node_fp.clone();
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![unrelated],
        };
        apply_update(
            &mut registry,
            &update,
            &public_key(&sender),
            &receiver.node_fp,
        )
        .unwrap();
        let stored = registry
            .authorized_nodes
            .iter()
            .find(|entry| entry.node_fp == target.node_fp)
            .unwrap();
        assert_eq!(stored.state, NodeState::Admitted);
    }

    #[test]
    fn update_refuses_more_than_total_registry_cap() {
        let sender = admitted_sender();
        let receiver = entry("local-receiver", NodeState::Admitted, 1, "sender");
        let mut registry = registry_with_sender(&sender);
        registry.authorized_nodes.push(receiver.clone());
        for index in 0..MAX_REGISTRY_ENTRIES - 2 {
            registry.authorized_nodes.push(entry(
                &format!("existing-{index}"),
                NodeState::Admitted,
                1,
                "sender",
            ));
        }
        let mut overflow = entry("overflow", NodeState::Admitted, 1, "sender");
        overflow.by = sender.node_fp.clone();
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![overflow],
        };
        assert!(apply_update(
            &mut registry,
            &update,
            &public_key(&sender),
            &receiver.node_fp
        )
        .is_err());
    }

    #[test]
    fn registry_load_refuses_file_over_byte_cap() {
        let dir = temp_dir();
        fs::write(dir.join(REGISTRY_FILE), vec![b' '; MAX_REGISTRY_BYTES + 1]).unwrap();
        assert!(load_registry_at(&dir).is_err());
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
        assert!(apply_as_sender(&mut registry, &swap_update, &public_key(&sender)).is_err());

        let mut alternate = original;
        alternate.node_fp = entry("alternate", NodeState::Admitted, 1, "sender").node_fp;
        let alternate_update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![alternate],
        };
        assert!(apply_as_sender(&mut registry, &alternate_update, &public_key(&sender)).is_err());
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
            apply_as_sender(&mut registry, &update, &public_key(&sender)).unwrap();
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
        let (receiver, _) = super::super::identity::init_identity_at(&dir).unwrap();
        let sender = admitted_sender();
        let mut initial = registry_with_sender(&sender);
        initial.authorized_nodes.push(AuthorizedNode {
            node_fp: receiver.node_fp.clone(),
            static_pubkey: encoding::encode_base64(&receiver.static_pubkey),
            endpoint: None,
            state: NodeState::Admitted,
            version: 1,
            by: sender.node_fp.clone(),
        });
        save_registry_at(&dir, &initial).unwrap();
        let sender_fp = sender.node_fp.clone();
        let auth_key = public_key(&sender);
        let workers: Vec<_> = (0..8)
            .map(|index| {
                let dir = dir.clone();
                let sender_fp = sender_fp.clone();
                let auth_key = auth_key.clone();
                thread::spawn(move || {
                    let mut target = entry(
                        &format!("concurrent-{index}"),
                        NodeState::Admitted,
                        1,
                        "sender",
                    );
                    target.by = sender_fp.clone();
                    let update = RegistryUpdate {
                        sender_fp,
                        entries: vec![target],
                    };
                    apply_update_at(&dir, &update, &auth_key).unwrap()
                })
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap().applied.len(), 1);
        }
        let registry = load_registry_at(&dir).unwrap();
        assert_eq!(registry.authorized_nodes.len(), 10);
    }

    #[test]
    fn registry_round_trips_per_entry_fields() {
        let dir = temp_dir();
        let mut joined = entry("fp-a", NodeState::Admitted, 1, "fp-a");
        joined.endpoint = Some("192.0.2.4:9443".into());
        let registry = Registry {
            authorized_nodes: vec![joined],
        };
        save_registry_at(&dir, &registry).unwrap();
        assert_eq!(load_registry_at(&dir).unwrap(), registry);
    }

    #[test]
    fn old_registry_entries_without_endpoint_still_load() {
        let dir = temp_dir();
        let mut entry = entry("old-node", NodeState::Admitted, 1, "old-node");
        entry.endpoint = Some("192.0.2.4:9443".into());
        let mut value = serde_json::to_value(Registry {
            authorized_nodes: vec![entry],
        })
        .unwrap();
        value["authorized_nodes"][0]
            .as_object_mut()
            .unwrap()
            .remove("endpoint");
        storage::atomic_write(
            &dir.join(REGISTRY_FILE),
            &serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        assert_eq!(
            load_registry_at(&dir).unwrap().authorized_nodes[0].endpoint,
            None
        );
    }

    #[test]
    fn registry_rejects_invalid_endpoint_hints() {
        let mut invalid = entry("bad-endpoint", NodeState::Admitted, 1, "node");
        invalid.endpoint = Some("0.0.0.0:9443".into());
        assert!(Registry {
            authorized_nodes: vec![invalid]
        }
        .digest()
        .is_err());
    }

    #[test]
    fn registry_rejects_known_low_order_x25519_points() {
        let mut low_order = [0u8; 32];
        low_order[0] = 1;
        let invalid = AuthorizedNode {
            node_fp: encoding::fingerprint(&low_order),
            static_pubkey: encoding::encode_base64(&low_order),
            endpoint: None,
            state: NodeState::Admitted,
            version: 1,
            by: origin("node"),
        };
        assert!(Registry {
            authorized_nodes: vec![invalid]
        }
        .digest()
        .is_err());
    }

    #[test]
    fn non_owner_endpoint_change_is_dropped_without_rejecting_update() {
        let sender = admitted_sender();
        let mut target = entry("endpoint-target", NodeState::Admitted, 1, "sender");
        target.endpoint = Some("192.0.2.10:9443".into());
        let mut registry = Registry {
            authorized_nodes: vec![sender.clone(), target.clone()],
        };
        let mut forged = target.clone();
        forged.endpoint = Some("192.0.2.20:9443".into());
        forged.version += 1;
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![forged],
        };
        apply_as_sender(&mut registry, &update, &public_key(&sender)).unwrap();
        let updated = registry
            .authorized_nodes
            .iter()
            .find(|entry| entry.node_fp == target.node_fp)
            .unwrap();
        assert_eq!(updated.version, target.version + 1);
        assert_eq!(updated.endpoint.as_deref(), Some("192.0.2.10:9443"));

        let mut self_update = sender.clone();
        self_update.endpoint = Some("192.0.2.10:9443".into());
        self_update.version += 1;
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![self_update],
        };
        apply_as_sender(&mut registry, &update, &public_key(&sender)).unwrap();
        assert_eq!(
            registry
                .authorized_nodes
                .iter()
                .find(|entry| entry.node_fp == sender.node_fp)
                .unwrap()
                .endpoint
                .as_deref(),
            Some("192.0.2.10:9443")
        );
    }

    #[test]
    fn probe_revoke_is_not_blocked_by_endpoint_mismatch() {
        let sender = admitted_sender();
        let mut target = entry("revoked-endpoint-target", NodeState::Admitted, 4, "sender");
        target.endpoint = Some("192.0.2.10:9443".into());
        let mut registry = Registry {
            authorized_nodes: vec![sender.clone(), target.clone()],
        };
        let mut tombstone = target.clone();
        tombstone.state = NodeState::Revoked;
        tombstone.version += 1;
        tombstone.by = sender.node_fp.clone();
        tombstone.endpoint = None;
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![tombstone],
        };
        apply_as_sender(&mut registry, &update, &public_key(&sender)).unwrap();
        let revoked = registry
            .authorized_nodes
            .iter()
            .find(|entry| entry.node_fp == target.node_fp)
            .unwrap();
        assert_eq!(revoked.state, NodeState::Revoked);
        assert_eq!(revoked.endpoint, None);
    }

    #[test]
    fn probe_stale_relayed_entry_with_old_endpoint_does_not_reject_update() {
        let sender = admitted_sender();
        let mut target = entry("stale-endpoint-target", NodeState::Admitted, 5, "sender");
        target.endpoint = Some("192.0.2.20:9443".into());
        let mut registry = Registry {
            authorized_nodes: vec![sender.clone(), target.clone()],
        };
        let mut stale = target.clone();
        stale.version -= 1;
        stale.endpoint = Some("192.0.2.10:9443".into());
        let mut unrelated = entry("unrelated-admission", NodeState::Admitted, 1, "sender");
        unrelated.by = sender.node_fp.clone();
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![stale, unrelated.clone()],
        };
        apply_as_sender(&mut registry, &update, &public_key(&sender)).unwrap();
        let retained = registry
            .authorized_nodes
            .iter()
            .find(|entry| entry.node_fp == target.node_fp)
            .unwrap();
        assert_eq!(retained.version, target.version);
        assert_eq!(retained.endpoint.as_deref(), Some("192.0.2.20:9443"));
        assert!(registry
            .authorized_nodes
            .iter()
            .any(|entry| entry.node_fp == unrelated.node_fp));
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
            endpoint: None,
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
