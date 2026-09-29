//! Per-node authorized membership entries, persistence, and merge semantics.

use super::encoding;
use super::storage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
#[cfg(not(windows))]
use std::fs::OpenOptions;
use std::io;
use std::path::Path;

const REGISTRY_FILE: &str = "authorized_nodes.json";
pub const MAX_UPDATE_BYTES: usize = 1024 * 1024;
pub const MAX_UPDATE_ENTRIES: usize = 1024;
pub const MAX_REGISTRY_BYTES: usize = 1024 * 1024;
pub const MAX_REGISTRY_ENTRIES: usize = 1024;
pub const REGISTRY_FORMAT_MAJOR: u16 = 1;
pub const REGISTRY_FORMAT_MINOR: u16 = 0;
const MAX_OPTIONAL_FIELDS: usize = 8;
const MAX_OPTIONAL_FIELDS_BYTES: usize = 1024;

#[derive(Debug, PartialEq, Eq)]
enum RegistryLimitError {
    EntryCount,
    EncodedBytes,
}

impl std::fmt::Display for RegistryLimitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::EntryCount => "registry exceeds total entry cap",
            Self::EncodedBytes => "registry exceeds byte cap",
        })
    }
}

impl std::error::Error for RegistryLimitError {}

fn registry_entry_cap_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, RegistryLimitError::EntryCount)
}

fn is_registry_entry_cap_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<RegistryLimitError>())
        == Some(&RegistryLimitError::EntryCount)
}
const RESERVED_ENTRY_FIELDS: [&str; 9] = [
    "node_fp",
    "static_pubkey",
    "format_major",
    "format_minor",
    "endpoint",
    "state",
    "version",
    "by",
    "delivered_by",
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AuthorizedNode {
    pub node_fp: String,
    pub static_pubkey: String,
    #[serde(default = "current_format_major")]
    pub format_major: u16,
    #[serde(default = "current_format_minor")]
    pub format_minor: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Receiver-local audit metadata. This field is never present on the wire
    /// and is excluded from the cluster digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_by: Option<String>,
    pub state: NodeState,
    pub version: u64,
    pub by: String,
    #[serde(flatten)]
    pub optional_fields: BTreeMap<String, Value>,
}

fn current_format_major() -> u16 {
    REGISTRY_FORMAT_MAJOR
}

fn current_format_minor() -> u16 {
    REGISTRY_FORMAT_MINOR
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryUpdate {
    pub sender_fp: String,
    pub entries: Vec<AuthorizedNode>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct WireAuthorizedNode {
    node_fp: String,
    static_pubkey: String,
    #[serde(default = "current_format_major")]
    format_major: u16,
    #[serde(default = "current_format_minor")]
    format_minor: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    state: NodeState,
    version: u64,
    by: String,
    #[serde(flatten)]
    optional_fields: BTreeMap<String, Value>,
}

impl From<&AuthorizedNode> for WireAuthorizedNode {
    fn from(entry: &AuthorizedNode) -> Self {
        Self {
            node_fp: entry.node_fp.clone(),
            static_pubkey: entry.static_pubkey.clone(),
            format_major: entry.format_major,
            format_minor: entry.format_minor,
            endpoint: entry.endpoint.clone(),
            state: entry.state,
            version: entry.version,
            by: entry.by.clone(),
            optional_fields: entry.optional_fields.clone(),
        }
    }
}

impl From<WireAuthorizedNode> for AuthorizedNode {
    fn from(entry: WireAuthorizedNode) -> Self {
        Self {
            node_fp: entry.node_fp,
            static_pubkey: entry.static_pubkey,
            format_major: entry.format_major,
            format_minor: entry.format_minor,
            endpoint: entry.endpoint,
            delivered_by: None,
            state: entry.state,
            version: entry.version,
            by: entry.by,
            optional_fields: entry.optional_fields,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireRegistryUpdate {
    #[serde(default = "current_format_major")]
    format_major: u16,
    #[serde(default = "current_format_minor")]
    format_minor: u16,
    sender_fp: String,
    entries: Vec<WireAuthorizedNode>,
}

#[derive(Debug)]
pub struct UnsupportedRegistryMajor {
    kind: &'static str,
    major: u16,
}

impl std::fmt::Display for UnsupportedRegistryMajor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "unsupported {} format major {}",
            self.kind, self.major
        )
    }
}

impl std::error::Error for UnsupportedRegistryMajor {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub applied: Vec<AuthorizedNode>,
    pub alerts: Vec<String>,
    pub local_metadata_changed: bool,
    pub dropped_origin_entries: usize,
    pub dropped_invalid_entries: usize,
    pub dropped_registry_cap_entries: usize,
    pub compacted_optional_metadata_entries: usize,
}

impl RegistryUpdate {
    /// Encode a validated update as bounded strict JSON.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        validate_update(self)?;
        let encoded = serde_json::to_vec(&WireRegistryUpdate {
            format_major: REGISTRY_FORMAT_MAJOR,
            format_minor: REGISTRY_FORMAT_MINOR,
            sender_fp: self.sender_fp.clone(),
            entries: self.entries.iter().map(WireAuthorizedNode::from).collect(),
        })
        .map_err(io::Error::other)?;
        if encoded.len() > MAX_UPDATE_BYTES {
            return Err(invalid_update("registry update exceeds byte cap"));
        }
        Ok(encoded)
    }

    /// Decode bounded JSON, rejecting unsupported majors before merge filters
    /// individual invalid entries.
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_UPDATE_BYTES {
            return Err(invalid_update("registry update exceeds byte cap"));
        }
        let wire: WireRegistryUpdate = serde_json::from_slice(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if wire.format_major != REGISTRY_FORMAT_MAJOR {
            return Err(unsupported_major("update", wire.format_major));
        }
        let _format_minor = wire.format_minor;
        let update = Self {
            sender_fp: wire.sender_fp,
            entries: wire.entries.into_iter().map(AuthorizedNode::from).collect(),
        };
        validate_update_header(&update)?;
        for entry in &update.entries {
            ensure_supported_entry_major(entry)?;
        }
        Ok(update)
    }

    /// Encode a page without receiver-local audit metadata.
    pub fn encode_entries_json(entries: &[AuthorizedNode]) -> io::Result<String> {
        serde_json::to_string(
            &entries
                .iter()
                .map(WireAuthorizedNode::from)
                .collect::<Vec<_>>(),
        )
        .map_err(io::Error::other)
    }

    /// Decode a registry page while initializing receiver-local audit metadata.
    pub fn decode_entries_json(json: &str) -> io::Result<Vec<AuthorizedNode>> {
        let entries: Vec<WireAuthorizedNode> = serde_json::from_str(json)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let entries = entries
            .into_iter()
            .map(AuthorizedNode::from)
            .collect::<Vec<_>>();
        for entry in &entries {
            ensure_supported_entry_major(entry)?;
        }
        Ok(entries)
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
                    merged.insert(key, merge_known_entry(entry, current));
                }
                None => {
                    merged.insert(key, entry.clone());
                }
            }
        }
        if merged.len() > MAX_REGISTRY_ENTRIES {
            return Err(registry_entry_cap_error());
        }
        let candidate = Registry {
            authorized_nodes: merged.into_values().collect(),
        };
        if serde_json::to_vec_pretty(&candidate)
            .map_err(io::Error::other)?
            .len()
            > MAX_REGISTRY_BYTES
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                RegistryLimitError::EncodedBytes,
            ));
        }
        self.authorized_nodes = candidate.authorized_nodes;
        Ok(())
    }

    /// Return a canonical SHA-256 digest independent of entry order.
    pub fn digest(&self) -> io::Result<String> {
        let mut canonical = Registry::default();
        canonical.merge(self)?;
        Self::digest_replication_snapshot(&canonical.authorized_nodes)
    }

    /// Digest a received full wire snapshot before invalid individual entries
    /// are filtered during merge.
    pub fn digest_replication_snapshot(entries: &[AuthorizedNode]) -> io::Result<String> {
        let mut wire = entries
            .iter()
            .map(|entry| {
                let mut wire = WireAuthorizedNode::from(entry);
                if let Ok(public_key) = encoding::decode_base64(&wire.static_pubkey) {
                    wire.static_pubkey = encoding::encode_base64(&public_key);
                }
                let encoded = serde_json::to_vec(&wire).map_err(io::Error::other)?;
                Ok((entry.node_fp.clone(), encoded, wire))
            })
            .collect::<io::Result<Vec<_>>>()?;
        wire.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
        let encoded = serde_json::to_vec(
            &wire
                .into_iter()
                .map(|(_, _, entry)| entry)
                .collect::<Vec<_>>(),
        )
        .map_err(io::Error::other)?;
        Ok(encoding::fingerprint(&encoded))
    }
}

fn merge_known_entry(incoming: &AuthorizedNode, current: &AuthorizedNode) -> AuthorizedNode {
    let state_changed = incoming.state != current.state;
    let incoming_wins = prefer(incoming, current);
    let mut winner = if incoming_wins {
        incoming.clone()
    } else {
        current.clone()
    };
    if state_changed {
        winner.optional_fields.clear();
        return winner;
    }
    if incoming.version == current.version
        && incoming.by == current.by
        && optional_fields_wins(&incoming.optional_fields, &current.optional_fields)
    {
        winner.optional_fields.clone_from(&incoming.optional_fields);
    }
    winner.format_minor = incoming.format_minor.max(current.format_minor);
    winner
}

fn optional_fields_wins(
    candidate: &BTreeMap<String, Value>,
    current: &BTreeMap<String, Value>,
) -> bool {
    if candidate.len() != current.len() {
        return candidate.len() > current.len();
    }
    let mut candidate_bytes = Vec::new();
    let mut current_bytes = Vec::new();
    serde_json::to_writer(&mut candidate_bytes, candidate)
        .expect("JSON optional fields always serialize");
    serde_json::to_writer(&mut current_bytes, current)
        .expect("JSON optional fields always serialize");
    candidate_bytes > current_bytes
}

fn validate_update(update: &RegistryUpdate) -> io::Result<()> {
    validate_update_header(update)?;
    for entry in &update.entries {
        validate_entry(entry)?;
    }
    Ok(())
}

fn validate_update_header(update: &RegistryUpdate) -> io::Result<()> {
    if !valid_fingerprint(&update.sender_fp) {
        return Err(invalid_update(
            "registry update sender is not a SHA256 fingerprint",
        ));
    }
    if update.entries.len() > MAX_UPDATE_ENTRIES {
        return Err(invalid_update("registry update exceeds entry cap"));
    }
    Ok(())
}

fn invalid_update(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn validate_entry(entry: &AuthorizedNode) -> io::Result<Vec<u8>> {
    if entry.format_major != REGISTRY_FORMAT_MAJOR {
        return Err(unsupported_major("entry", entry.format_major));
    }
    let optional_bytes = serde_json::to_vec(&entry.optional_fields).map_err(io::Error::other)?;
    if entry.optional_fields.len() > MAX_OPTIONAL_FIELDS
        || optional_bytes.len() > MAX_OPTIONAL_FIELDS_BYTES
        || entry
            .optional_fields
            .keys()
            .any(|key| RESERVED_ENTRY_FIELDS.contains(&key.as_str()))
        || entry.optional_fields.contains_key("optional_fields")
    {
        return Err(invalid_update(
            "registry entry optional fields are reserved or exceed cap",
        ));
    }
    validate_entry_core(entry)
}

fn ensure_supported_entry_major(entry: &AuthorizedNode) -> io::Result<()> {
    if entry.format_major != REGISTRY_FORMAT_MAJOR {
        return Err(unsupported_major("entry", entry.format_major));
    }
    Ok(())
}

fn validate_entry_core(entry: &AuthorizedNode) -> io::Result<Vec<u8>> {
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
    if entry
        .delivered_by
        .as_deref()
        .is_some_and(|fingerprint| !valid_fingerprint(fingerprint))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "registry delivery fingerprint is invalid",
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

fn unsupported_major(kind: &'static str, major: u16) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        UnsupportedRegistryMajor { kind, major },
    )
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
    let current = authenticated_registry(registry, update, authenticated_sender_pubkey)?;
    let (merged, outcome) = merge_update(current, update, receiver_fp)?;
    registry.authorized_nodes = merged.authorized_nodes;
    Ok(outcome)
}

fn authenticated_registry(
    registry: &Registry,
    update: &RegistryUpdate,
    authenticated_sender_pubkey: &[u8],
) -> io::Result<Registry> {
    validate_update_header(update)?;
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
    Ok(current)
}

fn merge_update(
    mut current: Registry,
    update: &RegistryUpdate,
    receiver_fp: &str,
) -> io::Result<(Registry, UpdateOutcome)> {
    let previous = current.clone();
    let mut outcome = UpdateOutcome::default();
    let mut accepted = Vec::with_capacity(update.entries.len());
    let mut compacted_metadata = false;
    for entry in &update.entries {
        if validate_entry(entry).is_err() {
            outcome.dropped_invalid_entries += 1;
            continue;
        }
        if entry.node_fp == receiver_fp && entry.state == NodeState::Revoked {
            outcome.alerts.push(format!(
                "dropped peer tombstone for receiver own key {}",
                receiver_fp
            ));
            continue;
        }
        let known = current
            .authorized_nodes
            .iter()
            .find(|known| known.node_fp == entry.node_fp);
        if !known.is_some_and(|known| same_replicated_entry(known, entry)) {
            let origin = current
                .authorized_nodes
                .iter()
                .find(|known| known.node_fp == entry.by);
            if origin.is_none_or(|known| known.state == NodeState::Revoked) {
                outcome.dropped_origin_entries += 1;
                continue;
            }
        }
        let mut accepted_entry = entry.clone();
        if let Some(known) = known {
            let endpoint_changed = known.endpoint != entry.endpoint;
            if endpoint_changed
                && entry.state == NodeState::Admitted
                && entry.node_fp != update.sender_fp
            {
                // A relay cannot claim the version of an owner endpoint hint;
                // preserve the prior record so the owner's equal-version
                // direct update can still win on receipt.
                accepted_entry.endpoint = known.endpoint.clone();
                accepted_entry.version = known.version;
                accepted_entry.by = known.by.clone();
            }
        }
        if !compacted_metadata {
            accepted_entry.delivered_by = Some(update.sender_fp.clone());
        }
        let Some((candidate, compacted_entries)) = merge_update_entry(&current, &accepted_entry)?
        else {
            outcome.dropped_registry_cap_entries += 1;
            continue;
        };
        if compacted_entries > 0 {
            outcome.compacted_optional_metadata_entries += compacted_entries;
            compacted_metadata = true;
            accepted.clear();
        }
        current = candidate;
        accepted.push(accepted_entry);
    }
    if !compacted_metadata {
        for incoming in &accepted {
            if let Some(merged) = current
                .authorized_nodes
                .iter_mut()
                .find(|known| known.node_fp == incoming.node_fp)
            {
                if same_replicated_core(merged, incoming) {
                    outcome.local_metadata_changed |=
                        merged.delivered_by.as_deref() != Some(update.sender_fp.as_str());
                    merged.delivered_by = Some(update.sender_fp.clone());
                }
            }
        }
    }
    outcome.applied = current
        .authorized_nodes
        .iter()
        .filter(|entry| {
            !previous
                .authorized_nodes
                .iter()
                .any(|prior| same_replicated_entry(prior, entry))
        })
        .cloned()
        .collect();
    Ok((current, outcome))
}

fn merge_update_entry(
    current: &Registry,
    incoming: &AuthorizedNode,
) -> io::Result<Option<(Registry, usize)>> {
    let received = Registry {
        authorized_nodes: vec![incoming.clone()],
    };
    let mut candidate = current.clone();
    if candidate.merge(&received).is_ok() {
        return Ok(Some((candidate, 0)));
    }
    if incoming.state != NodeState::Revoked {
        return Ok(None);
    }
    let mut compacted = current.clone();
    let mut stripped = 0;
    for known in &mut compacted.authorized_nodes {
        if !known.optional_fields.is_empty() || known.delivered_by.is_some() {
            known.optional_fields.clear();
            known.delivered_by = None;
            stripped += 1;
        }
    }
    if let Err(error) = compacted.merge(&received) {
        if is_registry_entry_cap_error(&error) {
            return Ok(None);
        }
        return Err(error);
    }
    Ok(Some((compacted, stripped)))
}

fn same_replicated_entry(left: &AuthorizedNode, right: &AuthorizedNode) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    left.delivered_by = None;
    right.delivered_by = None;
    left == right
}

fn same_replicated_core(left: &AuthorizedNode, right: &AuthorizedNode) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    left.optional_fields.clear();
    right.optional_fields.clear();
    same_replicated_entry(&left, &right)
}

/// Apply an update to the local persisted registry under one state lock.
pub fn apply_registry_update(
    _update: &RegistryUpdate,
    _authenticated_sender_pubkey: &[u8],
) -> io::Result<UpdateOutcome> {
    apply_update_at(
        &storage::cluster_state_dir()?.join("cluster"),
        _update,
        _authenticated_sender_pubkey,
    )
}

/// Apply the pinned issuer's one-time snapshot after the local join exchange.
/// This is crate-private and intentionally absent from all wire dispatch paths.
pub(crate) fn apply_join_bootstrap(
    update: &RegistryUpdate,
    authenticated_issuer_pubkey: &[u8],
) -> io::Result<()> {
    apply_join_bootstrap_at(
        &storage::cluster_state_dir()?.join("cluster"),
        update,
        authenticated_issuer_pubkey,
    )
}

fn apply_join_bootstrap_at(
    dir: &Path,
    update: &RegistryUpdate,
    authenticated_issuer_pubkey: &[u8],
) -> io::Result<()> {
    let guard = storage::StateLock::acquire(dir)?;
    let mut registry = load_registry_at(dir)?;
    let identity = super::identity::load_identity_at(dir)?;
    let previous = registry.clone();
    apply_join_bootstrap_snapshot(
        &mut registry,
        update,
        authenticated_issuer_pubkey,
        &identity.node_fp,
    )?;
    if registry != previous {
        save_registry_at(dir, &registry)?;
        drop(guard);
        super::replication::registry_changed();
    }
    Ok(())
}

fn apply_join_bootstrap_snapshot(
    registry: &mut Registry,
    update: &RegistryUpdate,
    authenticated_issuer_pubkey: &[u8],
    receiver_fp: &str,
) -> io::Result<()> {
    validate_update(update)?;
    let mut current = authenticated_registry(registry, update, authenticated_issuer_pubkey)?;
    let mut snapshot = update
        .entries
        .iter()
        .filter(|entry| !(entry.node_fp == receiver_fp && entry.state == NodeState::Revoked))
        .cloned()
        .collect::<Vec<_>>();
    for entry in &mut snapshot {
        entry.delivered_by = Some(update.sender_fp.clone());
    }
    current.merge(&Registry {
        authorized_nodes: snapshot,
    })?;
    registry.authorized_nodes = current.authorized_nodes;
    Ok(())
}

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
    let guard = storage::StateLock::acquire(dir)?;
    let mut registry = load_registry_at(dir)?;
    let receiver = super::identity::load_identity_at(dir)?;
    let outcome = apply_update(
        &mut registry,
        update,
        authenticated_sender_pubkey,
        &receiver.node_fp,
    )?;
    if !outcome.applied.is_empty() || outcome.local_metadata_changed {
        save_registry_at(dir, &registry)?;
    }
    for alert in &outcome.alerts {
        eprintln!("remuda: cluster replication alert: {alert}");
    }
    if outcome.dropped_origin_entries > 0 {
        eprintln!(
            "remuda: cluster replication dropped {} entries with unknown or revoked origins",
            outcome.dropped_origin_entries
        );
    }
    if outcome.dropped_invalid_entries > 0 {
        eprintln!(
            "remuda: cluster replication dropped {} invalid or over-cap entries",
            outcome.dropped_invalid_entries
        );
    }
    if outcome.dropped_registry_cap_entries > 0 {
        eprintln!(
            "remuda: cluster replication dropped {} entries exceeding registry total cap",
            outcome.dropped_registry_cap_entries
        );
    }
    if outcome.compacted_optional_metadata_entries > 0 {
        eprintln!(
            "remuda: cluster replication cleared optional registry metadata from {} entries to retain revocations within the byte cap",
            outcome.compacted_optional_metadata_entries
        );
    }
    drop(guard);
    if !outcome.applied.is_empty() {
        super::replication::registry_changed();
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

pub(super) fn load_registry_at(dir: &Path) -> io::Result<Registry> {
    if fs::symlink_metadata(dir).is_ok() {
        storage::verify_directory(dir)?;
    }
    let path = dir.join(REGISTRY_FILE);
    #[cfg(not(windows))]
    let file = {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(&path) {
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
        }
    };
    #[cfg(windows)]
    let file = match super::windows_security::open_for_read(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Registry::default()),
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
    folded.merge(&registry)?;
    Ok(folded)
}

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

    #[path = "pr95_probes2.rs"]
    mod pr95_probes2;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone)]
    struct RegistryTempDir(std::sync::Arc<RegistryTempDirInner>);

    struct RegistryTempDirInner(PathBuf);

    impl Drop for RegistryTempDirInner {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    impl std::ops::Deref for RegistryTempDir {
        type Target = std::path::Path;

        fn deref(&self) -> &Self::Target {
            &self.0 .0
        }
    }

    impl AsRef<std::path::Path> for RegistryTempDir {
        fn as_ref(&self) -> &std::path::Path {
            self
        }
    }

    fn temp_dir() -> RegistryTempDir {
        loop {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "remuda-cluster-registry-{}-{nonce}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match storage::create_private_directory(&path) {
                Ok(()) => return RegistryTempDir(std::sync::Arc::new(RegistryTempDirInner(path))),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create test registry directory: {error}"),
            }
        }
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
            delivered_by: None,
            format_major: 1,
            format_minor: 0,
            optional_fields: std::collections::BTreeMap::new(),
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

    fn near_byte_cap_registry(sender: &AuthorizedNode) -> Registry {
        let mut entries = vec![sender.clone()];
        for index in 0..MAX_REGISTRY_ENTRIES - 2 {
            let mut node = entry(
                &format!("aggregate-size-entry-{index}"),
                NodeState::Admitted,
                1,
                "sender",
            );
            let mut seed = index as u64 + 1;
            let mut key = [0u8; 32];
            for byte in &mut key {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                *byte = (seed >> 32) as u8;
            }
            node.static_pubkey = encoding::encode_base64(&key);
            node.node_fp = encoding::fingerprint(&key);
            node.by = sender.node_fp.clone();
            entries.push(node);
        }

        let mut low = 1;
        let mut high = MAX_OPTIONAL_FIELDS_BYTES - 32;
        let mut near_cap = None;
        while low <= high {
            let padding_len = low + (high - low) / 2;
            let mut candidate_entries = entries.clone();
            for node in candidate_entries.iter_mut().skip(1) {
                node.optional_fields
                    .insert("padding".into(), serde_json::json!("x".repeat(padding_len)));
            }
            let candidate = Registry {
                authorized_nodes: candidate_entries,
            };
            let encoded_len = serde_json::to_vec_pretty(&candidate).unwrap().len();
            if encoded_len <= MAX_REGISTRY_BYTES - 200 {
                near_cap = Some(candidate);
                low = padding_len + 1;
            } else {
                high = padding_len - 1;
            }
        }
        near_cap.expect("fixture should reach the registry byte cap")
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
        let decoded = RegistryUpdate::decode(nested_unknown.as_bytes()).unwrap();
        assert_eq!(decoded.entries[0].optional_fields["extra"], true);
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
    fn optional_entry_fields_survive_codec_merge_relay_and_affect_digest() {
        let sender = admitted_sender();
        let mut target = entry("future-field-target", NodeState::Admitted, 1, "sender");
        target.by = sender.node_fp.clone();
        target.optional_fields.insert(
            "future_signature".into(),
            serde_json::json!({"key_id": "k1", "signature": "opaque"}),
        );
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![target.clone()],
        };
        let wire = update.encode().unwrap();
        let decoded = RegistryUpdate::decode(&wire).unwrap();
        assert_eq!(decoded.entries[0].optional_fields, target.optional_fields);
        let baseline_digest = Registry {
            authorized_nodes: vec![sender.clone(), {
                let mut entry = target.clone();
                entry.optional_fields.clear();
                entry
            }],
        }
        .digest()
        .unwrap();
        let mut known_target = target.clone();
        known_target.optional_fields.clear();
        let mut receiver = registry_with_sender(&sender);
        receiver.authorized_nodes.push(known_target);
        apply_as_sender(&mut receiver, &decoded, &public_key(&sender)).unwrap();
        let received = receiver
            .authorized_nodes
            .iter()
            .find(|entry| entry.node_fp == target.node_fp)
            .unwrap();
        assert_eq!(received.optional_fields, target.optional_fields);
        assert_ne!(receiver.digest().unwrap(), baseline_digest);
        let relayed = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: receiver.authorized_nodes.clone(),
        }
        .encode()
        .unwrap();
        let relay_entries = RegistryUpdate::decode(&relayed).unwrap().entries;
        assert_eq!(
            relay_entries
                .iter()
                .find(|entry| entry.node_fp == target.node_fp)
                .unwrap()
                .optional_fields,
            target.optional_fields
        );
    }

    #[test]
    fn unknown_format_majors_return_a_typed_error() {
        let sender = admitted_sender();
        let update = RegistryUpdate {
            sender_fp: sender.node_fp,
            entries: vec![entry("versioned-entry", NodeState::Admitted, 1, "sender")],
        };
        let encoded = update.encode().unwrap();
        let mut update_value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        update_value["format_major"] = serde_json::json!(99);
        let error =
            RegistryUpdate::decode(&serde_json::to_vec(&update_value).unwrap()).unwrap_err();
        assert!(error
            .get_ref()
            .and_then(|source| source.downcast_ref::<UnsupportedRegistryMajor>())
            .is_some());

        update_value["format_major"] = serde_json::json!(REGISTRY_FORMAT_MAJOR);
        update_value["entries"][0]["format_major"] = serde_json::json!(99);
        let error =
            RegistryUpdate::decode(&serde_json::to_vec(&update_value).unwrap()).unwrap_err();
        assert!(error
            .get_ref()
            .and_then(|source| source.downcast_ref::<UnsupportedRegistryMajor>())
            .is_some());
    }

    #[test]
    fn state_transition_clears_unknown_optional_fields() {
        let mut admitted = entry("future-signed", NodeState::Admitted, 1, "owner");
        admitted.optional_fields.insert(
            "future_signature".into(),
            serde_json::json!("signature-over-admission"),
        );
        let mut revoked = admitted.clone();
        revoked.state = NodeState::Revoked;
        revoked.version = 2;
        let mut registry = Registry {
            authorized_nodes: vec![admitted],
        };
        registry
            .merge(&Registry {
                authorized_nodes: vec![revoked],
            })
            .unwrap();
        assert!(registry.authorized_nodes[0].optional_fields.is_empty());
    }

    #[test]
    fn bad_optional_field_entry_is_counted_and_does_not_block_valid_entry() {
        let sender = admitted_sender();
        let mut oversized = entry("oversized-optionals", NodeState::Admitted, 1, "sender");
        oversized.by = sender.node_fp.clone();
        for index in 0..MAX_OPTIONAL_FIELDS + 1 {
            oversized
                .optional_fields
                .insert(format!("future_{index}"), serde_json::json!(index));
        }
        let mut oversized_bytes =
            entry("oversized-optional-bytes", NodeState::Admitted, 1, "sender");
        oversized_bytes.by = sender.node_fp.clone();
        oversized_bytes.optional_fields.insert(
            "future_signature".into(),
            serde_json::json!("x".repeat(MAX_OPTIONAL_FIELDS_BYTES + 1)),
        );
        let mut valid = entry("valid-next-to-oversized", NodeState::Admitted, 1, "sender");
        valid.by = sender.node_fp.clone();
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![oversized, oversized_bytes, valid.clone()],
        };
        let wire = serde_json::to_vec(&WireRegistryUpdate {
            format_major: REGISTRY_FORMAT_MAJOR,
            format_minor: REGISTRY_FORMAT_MINOR,
            sender_fp: update.sender_fp.clone(),
            entries: update
                .entries
                .iter()
                .map(WireAuthorizedNode::from)
                .collect(),
        })
        .unwrap();
        let update = RegistryUpdate::decode(&wire).unwrap();
        let mut registry = registry_with_sender(&sender);
        let outcome = apply_as_sender(&mut registry, &update, &public_key(&sender)).unwrap();
        assert_eq!(outcome.dropped_invalid_entries, 2);
        assert!(registry
            .authorized_nodes
            .iter()
            .any(|entry| entry.node_fp == valid.node_fp));
    }

    #[test]
    fn update_over_aggregate_registry_cap_is_dropped_and_saved_registry_loads() {
        let sender = admitted_sender();
        let mut registry = near_byte_cap_registry(&sender);
        let mut additional = entry("aggregate-cap-update", NodeState::Admitted, 1, "sender");
        additional.by = sender.node_fp.clone();
        additional
            .optional_fields
            .insert("padding".into(), serde_json::json!("y".repeat(900)));
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![additional.clone()],
        };
        let outcome = apply_as_sender(&mut registry, &update, &public_key(&sender)).unwrap();
        assert_eq!(outcome.dropped_registry_cap_entries, 1);
        assert!(!registry
            .authorized_nodes
            .iter()
            .any(|node| node.node_fp == additional.node_fp));

        let dir = temp_dir();
        save_registry_at(&dir, &registry).unwrap();
        assert_eq!(load_registry_at(&dir).unwrap(), registry);
    }

    #[test]
    fn tombstone_compacts_optional_metadata_instead_of_exceeding_byte_cap() {
        let sender = admitted_sender();
        let mut registry = near_byte_cap_registry(&sender);
        let target_index = 1;
        registry.authorized_nodes[target_index]
            .optional_fields
            .clear();
        registry.authorized_nodes[target_index].version = 9;
        let mut remaining =
            MAX_REGISTRY_BYTES - 1 - serde_json::to_vec_pretty(&registry).unwrap().len();
        for node in registry.authorized_nodes.iter_mut().skip(target_index + 1) {
            if remaining == 0 {
                break;
            }
            let Some(padding) = node.optional_fields.get_mut("padding") else {
                continue;
            };
            let current_len = padding.as_str().unwrap().len();
            let added = remaining.min(MAX_OPTIONAL_FIELDS_BYTES - 64 - current_len);
            *padding = serde_json::json!("x".repeat(current_len + added));
            remaining -= added;
        }
        assert_eq!(remaining, 0, "fixture should sit one byte under the cap");
        let mut tombstone = registry.authorized_nodes[target_index].clone();
        tombstone.state = NodeState::Revoked;
        tombstone.version = 10;
        tombstone.by = sender.node_fp.clone();
        tombstone.endpoint = Some("127.0.0.1:9443".into());
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![tombstone.clone()],
        };

        let outcome = apply_as_sender(&mut registry, &update, &public_key(&sender)).unwrap();
        assert!(outcome.compacted_optional_metadata_entries > 0);
        assert_eq!(
            registry
                .authorized_nodes
                .iter()
                .find(|entry| entry.node_fp == tombstone.node_fp)
                .unwrap()
                .state,
            NodeState::Revoked
        );

        let dir = temp_dir();
        save_registry_at(&dir, &registry).unwrap();
        assert_eq!(load_registry_at(&dir).unwrap(), registry);
    }

    #[test]
    fn receiver_local_metadata_cannot_be_smuggled_as_an_optional_field() {
        let sender = admitted_sender();
        let mut bad = entry("spoofed-delivery", NodeState::Admitted, 1, "sender");
        bad.by = sender.node_fp.clone();
        bad.optional_fields
            .insert("delivered_by".into(), serde_json::json!(sender.node_fp));
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![bad],
        };
        let mut registry = registry_with_sender(&sender);
        let outcome = apply_as_sender(&mut registry, &update, &public_key(&sender)).unwrap();
        assert_eq!(outcome.dropped_invalid_entries, 1);
        assert_eq!(registry.authorized_nodes, vec![sender]);
    }

    #[test]
    fn revoked_origin_rows_do_not_block_a_new_valid_entry() {
        let sender = admitted_sender();
        let mut revoked_origin = entry("revoked-origin", NodeState::Revoked, 2, "seed");
        revoked_origin.by = sender.node_fp.clone();
        let mut old_admission = entry("old-origin-admission", NodeState::Admitted, 1, "origin");
        old_admission.by = revoked_origin.node_fp.clone();
        let mut registry = Registry {
            authorized_nodes: vec![
                sender.clone(),
                revoked_origin.clone(),
                old_admission.clone(),
            ],
        };
        let mut new_entry = entry("new-valid-admission", NodeState::Admitted, 1, "sender");
        new_entry.by = sender.node_fp.clone();
        let mut changed_old = old_admission.clone();
        changed_old.version += 1;
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![old_admission.clone(), changed_old, new_entry.clone()],
        };
        let outcome = apply_as_sender(&mut registry, &update, &public_key(&sender)).unwrap();
        assert_eq!(outcome.dropped_origin_entries, 1);
        assert!(registry
            .authorized_nodes
            .iter()
            .any(|entry| entry.node_fp == old_admission.node_fp && entry.version == 1));
        assert!(registry
            .authorized_nodes
            .iter()
            .any(|entry| entry.node_fp == new_entry.node_fp));
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
        let too_many_json = serde_json::to_vec(&WireRegistryUpdate {
            format_major: REGISTRY_FORMAT_MAJOR,
            format_minor: REGISTRY_FORMAT_MINOR,
            sender_fp: too_many.sender_fp.clone(),
            entries: too_many
                .entries
                .iter()
                .map(WireAuthorizedNode::from)
                .collect(),
        })
        .unwrap();
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
        let mut nonmember_tombstone = entry("nonmember-push", NodeState::Revoked, 2, "sender");
        nonmember_tombstone.by = sender.node_fp.clone();
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![nonmember_tombstone],
        };
        assert!(apply_as_sender(&mut registry, &update, &public_key(&sender)).is_err());
        assert!(registry.authorized_nodes.is_empty());
    }

    #[test]
    fn probe_owner_endpoint_update_is_not_shadowed_by_relay() {
        let sender = admitted_sender();
        let mut owner = entry("endpoint-owner", NodeState::Admitted, 1, "origin");
        owner.by = owner.node_fp.clone();
        owner.endpoint = Some("192.0.2.10:9443".into());
        let receiver = entry("endpoint-receiver", NodeState::Admitted, 1, "origin");
        let mut receiver = receiver;
        receiver.by = owner.node_fp.clone();
        let mut view = Registry {
            authorized_nodes: vec![sender.clone(), owner.clone(), receiver.clone()],
        };
        let mut moved = owner.clone();
        moved.endpoint = Some("192.0.2.20:9443".into());
        moved.version += 1;

        let relayed = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![moved.clone()],
        };
        apply_update(&mut view, &relayed, &public_key(&sender), &receiver.node_fp).unwrap();

        let direct = RegistryUpdate {
            sender_fp: owner.node_fp.clone(),
            entries: vec![moved],
        };
        apply_update(&mut view, &direct, &public_key(&owner), &receiver.node_fp).unwrap();
        assert_eq!(
            view.authorized_nodes
                .iter()
                .find(|entry| entry.node_fp == owner.node_fp)
                .unwrap()
                .endpoint
                .as_deref(),
            Some("192.0.2.20:9443")
        );
    }

    #[test]
    fn relayed_admission_preserves_origin_and_converges_digest() {
        let mut origin = admitted_sender();
        origin.by = origin.node_fp.clone();
        let mut relay = entry("relay", NodeState::Admitted, 1, "origin");
        relay.by = origin.node_fp.clone();
        let mut receiver = entry("receiver", NodeState::Admitted, 1, "origin");
        receiver.by = origin.node_fp.clone();
        let mut new_member = entry("new-member", NodeState::Admitted, 1, "origin");
        new_member.by = origin.node_fp.clone();
        let source = Registry {
            authorized_nodes: vec![origin.clone(), relay.clone(), receiver.clone(), new_member],
        };
        let mut destination = Registry {
            authorized_nodes: vec![origin.clone(), relay.clone(), receiver.clone()],
        };
        let update = RegistryUpdate {
            sender_fp: relay.node_fp.clone(),
            entries: source.authorized_nodes.clone(),
        };
        apply_update(
            &mut destination,
            &update,
            &public_key(&relay),
            &receiver.node_fp,
        )
        .unwrap();
        assert_eq!(source.digest().unwrap(), destination.digest().unwrap());
        let learned = destination
            .authorized_nodes
            .iter()
            .find(|entry| entry.node_fp == update.entries[3].node_fp)
            .unwrap();
        assert_eq!(
            learned.delivered_by.as_deref(),
            Some(relay.node_fp.as_str())
        );
        assert_eq!(learned.by, origin.node_fp);
    }

    #[test]
    fn probe_two_converged_nodes_have_equal_digests() {
        let mut a = admitted_sender();
        a.by = a.node_fp.clone();
        let mut b = entry("digest-relay", NodeState::Admitted, 1, "origin");
        b.by = a.node_fp.clone();
        let mut added = entry("digest-added", NodeState::Admitted, 1, "origin");
        added.by = a.node_fp.clone();
        let mut a_view = Registry {
            authorized_nodes: vec![a.clone(), b.clone(), added],
        };
        let mut b_view = Registry {
            authorized_nodes: vec![a.clone(), b.clone()],
        };
        for _ in 0..8 {
            let update = RegistryUpdate {
                sender_fp: a.node_fp.clone(),
                entries: a_view.authorized_nodes.clone(),
            };
            apply_update(&mut b_view, &update, &public_key(&a), &b.node_fp).unwrap();
            let update = RegistryUpdate {
                sender_fp: b.node_fp.clone(),
                entries: b_view.authorized_nodes.clone(),
            };
            apply_update(&mut a_view, &update, &public_key(&b), &a.node_fp).unwrap();
        }
        assert_eq!(a_view.digest().unwrap(), b_view.digest().unwrap());
        let update = RegistryUpdate {
            sender_fp: a.node_fp.clone(),
            entries: a_view.authorized_nodes.clone(),
        };
        assert!(
            apply_update(&mut b_view, &update, &public_key(&a), &b.node_fp)
                .unwrap()
                .applied
                .is_empty()
        );
    }

    #[test]
    fn receiver_delivery_metadata_is_local_and_does_not_change_digest() {
        let sender = admitted_sender();
        let mut target = entry("metadata-target", NodeState::Admitted, 1, "sender");
        target.by = sender.node_fp.clone();
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![target.clone()],
        };
        let original_digest = Registry {
            authorized_nodes: vec![target.clone()],
        }
        .digest()
        .unwrap();
        let mut delivered = target;
        delivered.delivered_by = Some(sender.node_fp.clone());
        let local_digest = Registry {
            authorized_nodes: vec![delivered.clone()],
        }
        .digest()
        .unwrap();
        assert_eq!(original_digest, local_digest);

        let encoded = RegistryUpdate {
            entries: vec![delivered],
            ..update
        }
        .encode()
        .unwrap();
        assert!(!String::from_utf8_lossy(&encoded).contains("delivered_by"));
        let decoded = RegistryUpdate::decode(&encoded).unwrap();
        assert_eq!(decoded.entries[0].delivered_by, None);
        let page = RegistryUpdate::encode_entries_json(&decoded.entries).unwrap();
        assert!(!page.contains("delivered_by"));
        assert_eq!(
            RegistryUpdate::decode_entries_json(&page).unwrap()[0].delivered_by,
            None
        );
    }

    #[test]
    fn relayed_admission_refuses_unknown_or_tombstoned_origins() {
        let sender = admitted_sender();
        for origin_state in [Some(NodeState::Revoked), None] {
            let mut registry = registry_with_sender(&sender);
            let origin = entry(
                "origin",
                origin_state.unwrap_or(NodeState::Admitted),
                1,
                "seed",
            );
            if origin_state.is_some() {
                registry.authorized_nodes.push(origin.clone());
            }
            let mut new_member = entry("relayed-new-member", NodeState::Admitted, 1, "seed");
            new_member.by = origin.node_fp.clone();
            let update = RegistryUpdate {
                sender_fp: sender.node_fp.clone(),
                entries: vec![new_member],
            };
            let outcome = apply_as_sender(&mut registry, &update, &public_key(&sender)).unwrap();
            assert_eq!(outcome.dropped_origin_entries, 1);
            assert_eq!(
                registry.authorized_nodes.len(),
                if origin_state.is_some() { 2 } else { 1 }
            );
        }
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
        let mut delivered_other = other.clone();
        delivered_other.delivered_by = Some(sender.node_fp.clone());
        assert_eq!(outcome.applied, vec![delivered_other]);
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
    fn update_rejects_revoked_origin_but_accepts_admitted_relay_origin() {
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
        let mut unchanged = registry.clone();
        let outcome = apply_update(
            &mut unchanged,
            &update,
            &public_key(&sender),
            &receiver.node_fp,
        )
        .unwrap();
        assert_eq!(outcome.dropped_origin_entries, 1);
        assert_eq!(unchanged.digest().unwrap(), registry.digest().unwrap());

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
        apply_update(
            &mut registry,
            &update,
            &public_key(&sender),
            &receiver.node_fp,
        )
        .unwrap();
        assert!(registry
            .authorized_nodes
            .iter()
            .any(|entry| entry.node_fp == update.entries[0].node_fp));
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
    fn update_drops_entries_over_total_registry_cap() {
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
        let outcome = apply_update(
            &mut registry,
            &update,
            &public_key(&sender),
            &receiver.node_fp,
        )
        .unwrap();
        assert_eq!(outcome.dropped_registry_cap_entries, 1);
        assert_eq!(registry.authorized_nodes.len(), MAX_REGISTRY_ENTRIES);
    }

    #[test]
    fn tombstone_entry_cap_drop_does_not_abort_later_valid_update() {
        let sender = admitted_sender();
        let receiver = entry(
            "local-receiver-at-entry-cap",
            NodeState::Admitted,
            1,
            "sender",
        );
        let mut registry = registry_with_sender(&sender);
        registry.authorized_nodes.push(receiver.clone());
        let mut valid = entry(
            "entry-cap-valid-after-tombstone",
            NodeState::Admitted,
            1,
            "sender",
        );
        valid.by = sender.node_fp.clone();
        registry.authorized_nodes.push(valid.clone());
        for index in 0..MAX_REGISTRY_ENTRIES - 3 {
            registry.authorized_nodes.push(entry(
                &format!("entry-cap-existing-{index}"),
                NodeState::Admitted,
                1,
                "sender",
            ));
        }
        assert_eq!(registry.authorized_nodes.len(), MAX_REGISTRY_ENTRIES);
        let mut unknown_tombstone = entry(
            "entry-cap-unknown-tombstone",
            NodeState::Revoked,
            1,
            "sender",
        );
        unknown_tombstone.by = sender.node_fp.clone();
        valid.version += 1;
        let update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![unknown_tombstone, valid.clone()],
        };

        let outcome = apply_update(
            &mut registry,
            &update,
            &public_key(&sender),
            &receiver.node_fp,
        )
        .unwrap();

        assert_eq!(outcome.dropped_registry_cap_entries, 1);
        assert!(registry
            .authorized_nodes
            .iter()
            .any(|known| known.node_fp == valid.node_fp && known.version == valid.version));
        assert_eq!(registry.authorized_nodes.len(), MAX_REGISTRY_ENTRIES);
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
        let outcome = apply_as_sender(&mut registry, &swap_update, &public_key(&sender)).unwrap();
        assert_eq!(outcome.dropped_invalid_entries, 1);

        let mut alternate = original;
        alternate.node_fp = entry("alternate", NodeState::Admitted, 1, "sender").node_fp;
        let alternate_update = RegistryUpdate {
            sender_fp: sender.node_fp.clone(),
            entries: vec![alternate],
        };
        let outcome =
            apply_as_sender(&mut registry, &alternate_update, &public_key(&sender)).unwrap();
        assert_eq!(outcome.dropped_invalid_entries, 1);
        assert_eq!(registry, registry_with_sender(&sender));
    }

    #[test]
    fn revoked_entry_survives_old_and_newer_update_lists() {
        let sender = admitted_sender();
        let mut target = entry("replication-target", NodeState::Revoked, 7, "sender");
        target.by = sender.node_fp.clone();
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
            delivered_by: None,
            format_major: 1,
            format_minor: 0,
            optional_fields: std::collections::BTreeMap::new(),
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
        joined.delivered_by = Some(origin("relay"));
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
            delivered_by: None,
            format_major: 1,
            format_minor: 0,
            optional_fields: std::collections::BTreeMap::new(),
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
        let mut sender = admitted_sender();
        sender.by = sender.node_fp.clone();
        let mut target = entry("endpoint-target", NodeState::Admitted, 1, "sender");
        target.by = sender.node_fp.clone();
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
        assert_eq!(updated.version, target.version);
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
        target.by = sender.node_fp.clone();
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
            delivered_by: None,
            format_major: 1,
            format_minor: 0,
            optional_fields: std::collections::BTreeMap::new(),
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

#[cfg(all(test, windows))]
mod windows_private_state_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn loading_legacy_default_acl_settings_tightens_acl_and_loads_data() {
        let dir = std::env::temp_dir().join(format!(
            "remuda-registry-upgrade-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, br#"{"allow_remote_control":false}"#).unwrap();

        assert!(!super::super::control::enabled_at(&dir).unwrap());
        let directory = super::super::windows_security::open_for_check(&dir, true, true).unwrap();
        assert!(super::super::windows_security::is_owner_acl_conforming(&directory).unwrap());
        let file = super::super::windows_security::open_for_check(&path, false, true).unwrap();
        assert!(super::super::windows_security::is_owner_acl_conforming(&file).unwrap());
        drop(file);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
