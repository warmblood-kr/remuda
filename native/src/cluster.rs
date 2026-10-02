//! Cluster vocabulary composed from identity and membership registry units.

pub mod control;
pub mod encoding;
pub mod identity;
pub mod join_line;
pub mod join_token;
pub mod listener_config;
pub mod listener_control;
pub mod registry;
pub mod replication;
mod storage;
#[cfg(windows)]
pub(crate) mod windows_security;

pub use identity::NodeIdentity;
pub use registry::{load_registry, save_registry, AuthorizedNode, NodeState, Registry};
pub use replication::{
    push_now, push_now_excluding, push_now_with_revoked_target, registry_changed, PeerPushResult,
};

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::Path;

/// Take the per-user cluster-listener host lock without waiting. The returned
/// file keeps the OS lock alive until the hosting listener drops it.
pub fn try_acquire_listener_host_lock() -> io::Result<std::fs::File> {
    #[cfg(not(windows))]
    use std::fs::OpenOptions;

    let dir = storage::cluster_state_dir()?.join("cluster");
    storage::verify_directory(&dir)?;
    let path = dir.join("listener.lock");
    #[cfg(windows)]
    let file = {
        let file = windows_security::create_or_open_lock(&path)?;
        storage::check_private_file(&file, "cluster listener lock", &path)?;
        file
    };
    #[cfg(not(windows))]
    let file = {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path)?;
        storage::check_private_file(&file, "cluster listener lock", &path)?;
        file
    };
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "cluster listener hosted by another daemon",
        )),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevokeOutcome {
    Revoked,
    AlreadyRevoked,
}

/// Initialize the local identity and cluster-of-one registry.
pub fn init() -> io::Result<(NodeIdentity, bool)> {
    let (identity, created, _) = init_with_notice()?;
    Ok((identity, created))
}

/// Initialize the local identity and return a fingerprint recovered from a
/// prior cluster directory that was missing, if one was recorded.
pub fn init_with_notice() -> io::Result<(NodeIdentity, bool, Option<String>)> {
    init_at(&storage::cluster_state_dir()?)
}

fn init_at(state_dir: &Path) -> io::Result<(NodeIdentity, bool, Option<String>)> {
    let cluster_dir = state_dir.join("cluster");
    let previous_fingerprint = match fs::symlink_metadata(&cluster_dir) {
        Ok(_) => None,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            storage::read_cluster_init_marker(state_dir)?
        }
        Err(error) => return Err(error),
    };
    let dir = identity::prepare_cluster_dir_at(state_dir)?;
    let _guard = storage::StateLock::acquire(&dir)?;
    let (node, created) = identity::init_identity_locked(&dir)?;
    let mut registry = registry::load_registry_at(&dir)?;
    registry.merge(&Registry {
        authorized_nodes: vec![AuthorizedNode {
            node_fp: node.node_fp.clone(),
            static_pubkey: encoding::encode_base64(&node.static_pubkey),
            delivered_by: None,
            format_major: 1,
            format_minor: 0,
            optional_fields: std::collections::BTreeMap::new(),
            endpoint: None,
            state: NodeState::Admitted,
            version: 1,
            by: node.node_fp.clone(),
        }],
    })?;
    registry::save_registry_at(&dir, &registry)?;
    if let Err(error) = storage::write_cluster_init_marker(state_dir, &node.node_fp) {
        eprintln!(
            "warning: cluster identity initialized, but marker {} could not be written: {error}",
            state_dir.join("cluster-initialized").display()
        );
    }
    Ok((
        node,
        created,
        if created { previous_fingerprint } else { None },
    ))
}

/// Replace the local identity and reset membership to the new local node only.
/// The caller must confirm this disruptive operation in the CLI.
pub fn init_new_identity() -> io::Result<NodeIdentity> {
    let state_dir = storage::cluster_state_dir()?;
    let dir = identity::prepare_cluster_dir_at(&state_dir)?;
    let _guard = storage::StateLock::acquire(&dir)?;
    identity::load_identity_at(&dir)?;
    join_token::clear_at(&dir)?;
    let _ = control::revoked_notice_at(&dir)?;
    let node = identity::rotate_identity_locked(&dir)?;
    registry::save_registry_at(
        &dir,
        &Registry {
            authorized_nodes: vec![AuthorizedNode {
                node_fp: node.node_fp.clone(),
                static_pubkey: encoding::encode_base64(&node.static_pubkey),
                delivered_by: None,
                format_major: 1,
                format_minor: 0,
                optional_fields: std::collections::BTreeMap::new(),
                endpoint: None,
                state: NodeState::Admitted,
                version: 1,
                by: node.node_fp.clone(),
            }],
        },
    )?;
    control::clear_revoked_notice_at(&dir)?;
    storage::write_cluster_init_marker(&state_dir, &node.node_fp)?;
    Ok(node)
}

/// Return the local identity and admitted member count, or `None` before init.
pub fn status() -> io::Result<Option<(NodeIdentity, usize)>> {
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

/// Record the issuer and this node's optional advertised endpoint after a
/// pinned join exchange succeeds.
pub fn record_join_success(
    issuer_static_pubkey: &[u8],
    issuer_addr: std::net::SocketAddr,
    local_endpoint: Option<std::net::SocketAddr>,
) -> io::Result<()> {
    record_join_success_with_schedule(issuer_static_pubkey, issuer_addr, local_endpoint, true)
}

/// Record local join metadata for the CLI, which performs a bounded push after
/// importing the issuer snapshot.
pub fn record_join_success_local(
    issuer_static_pubkey: &[u8],
    issuer_addr: std::net::SocketAddr,
    local_endpoint: Option<std::net::SocketAddr>,
) -> io::Result<()> {
    record_join_success_with_schedule(issuer_static_pubkey, issuer_addr, local_endpoint, false)
}

fn record_join_success_with_schedule(
    issuer_static_pubkey: &[u8],
    issuer_addr: std::net::SocketAddr,
    local_endpoint: Option<std::net::SocketAddr>,
    schedule: bool,
) -> io::Result<()> {
    {
        if issuer_static_pubkey.len() != 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid issuer key",
            ));
        }
        join_line::validate_endpoint(issuer_addr)?;
        if let Some(endpoint) = local_endpoint {
            join_line::validate_endpoint(endpoint)?;
        }
        let dir = storage::cluster_state_dir()?.join("cluster");
        let guard = storage::StateLock::acquire(&dir)?;
        let self_node = identity::load_identity_at(&dir)?;
        let mut registry = registry::load_registry_at(&dir)?;
        let issuer_fp = encoding::fingerprint(issuer_static_pubkey);
        let issuer_key = encoding::encode_base64(issuer_static_pubkey);
        let mut changed = false;

        if let Some(issuer) = registry
            .authorized_nodes
            .iter_mut()
            .find(|entry| entry.node_fp == issuer_fp)
        {
            if issuer.static_pubkey != issuer_key || issuer.state != NodeState::Admitted {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "join issuer is not an admitted registry member",
                ));
            }
            let endpoint = issuer_addr.to_string();
            if issuer.endpoint.as_deref() != Some(endpoint.as_str()) {
                issuer.endpoint = Some(endpoint);
                issuer.version = issuer.version.saturating_add(1);
                changed = true;
            }
        } else {
            registry.authorized_nodes.push(AuthorizedNode {
                node_fp: issuer_fp.clone(),
                static_pubkey: issuer_key,
                delivered_by: None,
                format_major: 1,
                format_minor: 0,
                optional_fields: std::collections::BTreeMap::new(),
                endpoint: Some(issuer_addr.to_string()),
                state: NodeState::Admitted,
                version: 1,
                by: issuer_fp.clone(),
            });
            changed = true;
        }

        let self_entry = registry
            .authorized_nodes
            .iter_mut()
            .find(|entry| entry.node_fp == self_node.node_fp)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "local registry entry is missing",
                )
            })?;
        if self_entry.static_pubkey != encoding::encode_base64(&self_node.static_pubkey)
            || self_entry.state != NodeState::Admitted
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "local node is not admitted in its registry",
            ));
        }
        let newly_joined = self_entry.by == self_node.node_fp && issuer_fp != self_node.node_fp;
        if newly_joined {
            self_entry.by = issuer_fp;
            changed = true;
        }
        if let Some(endpoint) = local_endpoint {
            let endpoint = endpoint.to_string();
            if self_entry.endpoint.as_deref() != Some(endpoint.as_str()) {
                self_entry.endpoint = Some(endpoint);
                if !newly_joined {
                    self_entry.version = self_entry.version.saturating_add(1);
                }
                changed = true;
            }
        }
        if changed {
            registry::save_registry_at(&dir, &registry)?;
        }
        drop(guard);
        if changed && schedule {
            replication::registry_changed();
        }
        Ok(())
    }
}

/// Mint a join line for an explicitly selected listener endpoint.
pub fn mint_join_line(address: std::net::SocketAddr) -> io::Result<join_line::JoinLine> {
    {
        let dir = storage::cluster_state_dir()?.join("cluster");
        let node = identity::load_identity_at(&dir)?;
        let token_store = join_token::JoinTokenStore::open_at(
            &dir,
            std::sync::Arc::new(crate::SystemWallClock::new()),
        )?;
        let token = token_store.mint()?;
        Ok(join_line::JoinLine {
            issuer_addr: address,
            issuer_fingerprint: node.node_fp,
            issuer_static_pubkey: node.static_pubkey.try_into().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid local identity key")
            })?,
            token: token.token,
        })
    }
}

/// Admit a peer while the caller holds this node's cluster state lock.
pub(crate) fn admit_join_locked(peer_static: &[u8], endpoint: Option<&str>) -> io::Result<()> {
    let dir = storage::cluster_state_dir()?.join("cluster");
    admit_join_locked_at(&dir, peer_static, endpoint)
}

fn admit_join_locked_at(
    dir: &std::path::Path,
    peer_static: &[u8],
    endpoint: Option<&str>,
) -> io::Result<()> {
    let mut registry = registry::load_registry_at(dir)?;
    let fp = encoding::fingerprint(peer_static);
    if let Some(existing) = registry
        .authorized_nodes
        .iter()
        .find(|entry| entry.node_fp == fp)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            if existing.state == NodeState::Revoked {
                "join key is revoked"
            } else {
                "join key is already admitted"
            },
        ));
    }
    let self_node = identity::load_identity_at(dir)?;
    registry.merge(&Registry {
        authorized_nodes: vec![AuthorizedNode {
            node_fp: fp,
            static_pubkey: encoding::encode_base64(peer_static),
            delivered_by: None,
            format_major: 1,
            format_minor: 0,
            optional_fields: std::collections::BTreeMap::new(),
            endpoint: endpoint.map(str::to_owned),
            state: NodeState::Admitted,
            version: 1,
            by: self_node.node_fp,
        }],
    })?;
    registry::save_registry_at(dir, &registry)
}

/// Load the local identity and complete local membership registry.
pub fn nodes() -> io::Result<Option<(NodeIdentity, Registry)>> {
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
/// An address override controls routing only; authentication always uses
/// the key pinned in the local registry.
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
    resolve_target_in_registry(&registry, target, addr_override)
}

fn resolve_target_in_registry(
    registry: &Registry,
    target: &str,
    addr_override: Option<SocketAddr>,
) -> io::Result<ResolvedTarget> {
    let entry = resolve_node(&registry.authorized_nodes, target)?;
    if entry.state != NodeState::Admitted {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("node {} is revoked", entry.node_fp),
        ));
    }
    let address = match addr_override {
        Some(address) => address,
        None => entry
            .endpoint
            .as_deref()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "node {} has no registry endpoint; supply --addr HOST:PORT",
                        entry.node_fp
                    ),
                )
            })?
            .parse()
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid endpoint in cluster registry",
                )
            })?,
    };
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
    revoke_with_schedule(_target, true)
}

/// Revoke locally for the CLI, which performs its own bounded synchronous push.
pub fn revoke_local(_target: &str) -> io::Result<RevokeOutcome> {
    revoke_with_schedule(_target, false)
}

fn revoke_with_schedule(target: &str, schedule: bool) -> io::Result<RevokeOutcome> {
    {
        let dir = storage::cluster_state_dir()?.join("cluster");
        let outcome = revoke_at(&dir, target)?;
        if schedule && outcome == RevokeOutcome::Revoked {
            replication::registry_changed();
        }
        Ok(outcome)
    }
}

fn escape_registry_field(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}

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

fn cluster_not_initialized_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "cluster is not initialized; run `remuda cluster init`",
    )
}

pub fn node_label(fingerprint: &str) -> String {
    identity::node_name(fingerprint)
}

#[cfg(test)]
mod init_marker_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempStateDir(PathBuf);

    impl TempStateDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "remuda-cluster-init-marker-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for TempStateDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_cluster_uses_marker_and_records_the_replacement_fingerprint() {
        let state_dir = TempStateDir::new();
        let (first, first_created, first_prior) = init_at(&state_dir.0).unwrap();
        assert!(first_created);
        assert!(first_prior.is_none());
        assert_eq!(
            storage::read_cluster_init_marker(&state_dir.0)
                .unwrap()
                .as_deref(),
            Some(first.node_fp.as_str())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(state_dir.0.join("cluster-initialized"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        let (same, same_created, same_prior) = init_at(&state_dir.0).unwrap();
        assert_eq!(same.node_fp, first.node_fp);
        assert!(!same_created);
        assert!(same_prior.is_none());

        fs::remove_dir_all(state_dir.0.join("cluster")).unwrap();
        let (replacement, replacement_created, previous) = init_at(&state_dir.0).unwrap();
        assert!(replacement_created);
        assert_ne!(replacement.node_fp, first.node_fp);
        assert_eq!(previous.as_deref(), Some(first.node_fp.as_str()));
        assert_eq!(
            storage::read_cluster_init_marker(&state_dir.0)
                .unwrap()
                .as_deref(),
            Some(replacement.node_fp.as_str())
        );
    }

    #[test]
    fn existing_cluster_without_marker_does_not_report_a_previous_fingerprint() {
        let state_dir = TempStateDir::new();
        let (first, created, previous) = init_at(&state_dir.0).unwrap();
        assert!(created);
        assert!(previous.is_none());
        fs::remove_file(state_dir.0.join("cluster-initialized")).unwrap();

        let (same, created, previous) = init_at(&state_dir.0).unwrap();
        assert_eq!(same.node_fp, first.node_fp);
        assert!(!created);
        assert!(previous.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn init_succeeds_when_marker_write_fails_after_identity_save() {
        use std::os::unix::fs::PermissionsExt;

        let state_dir = TempStateDir::new();
        let cluster_dir = state_dir.0.join("cluster");
        fs::create_dir_all(&cluster_dir).unwrap();
        fs::set_permissions(&cluster_dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&state_dir.0, fs::Permissions::from_mode(0o500)).unwrap();

        let result = init_at(&state_dir.0);

        fs::set_permissions(&state_dir.0, fs::Permissions::from_mode(0o700)).unwrap();
        let (_, created, previous) = result.expect("marker write failure must not fail init");
        assert!(created);
        assert!(previous.is_none());
        assert!(cluster_dir.join("identity.key").exists());
        assert!(!state_dir.0.join("cluster-initialized").exists());
    }

    #[cfg(unix)]
    #[test]
    fn init_rewrites_existing_marker_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let state_dir = TempStateDir::new();
        init_at(&state_dir.0).unwrap();
        let marker = state_dir.0.join("cluster-initialized");
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();

        init_at(&state_dir.0).unwrap();

        assert_eq!(
            fs::metadata(marker).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
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
    fn registry_entry_endpoint_resolves_without_override() {
        let dir = temp_dir();
        let (node, _) = identity::init_identity_at(&dir).unwrap();
        let registry = Registry {
            authorized_nodes: vec![AuthorizedNode {
                node_fp: node.node_fp.clone(),
                static_pubkey: encoding::encode_base64(&node.static_pubkey),
                delivered_by: None,
                format_major: 1,
                format_minor: 0,
                optional_fields: std::collections::BTreeMap::new(),
                endpoint: Some("127.0.0.1:48402".into()),
                state: NodeState::Admitted,
                version: 1,
                by: node.node_fp.clone(),
            }],
        };

        let resolved = resolve_target_in_registry(&registry, &node.node_fp, None).unwrap();
        assert_eq!(resolved.address, "127.0.0.1:48402".parse().unwrap());
        assert_eq!(resolved.pinned_static_key, node.static_pubkey);

        let override_address = "127.0.0.1:48403".parse().unwrap();
        let overridden =
            resolve_target_in_registry(&registry, &node.node_fp, Some(override_address)).unwrap();
        assert_eq!(overridden.address, override_address);

        let mut revoked_registry = registry.clone();
        revoked_registry.authorized_nodes[0].state = NodeState::Revoked;
        let error = resolve_target_in_registry(&revoked_registry, &node.node_fp, None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn join_admission_records_the_advertised_endpoint_and_issuer() {
        let dir = temp_dir();
        let (issuer, _) = identity::init_identity_at(&dir).unwrap();
        registry::save_registry_at(
            &dir,
            &Registry {
                authorized_nodes: vec![AuthorizedNode {
                    node_fp: issuer.node_fp.clone(),
                    static_pubkey: encoding::encode_base64(&issuer.static_pubkey),
                    delivered_by: None,
                    format_major: 1,
                    format_minor: 0,
                    optional_fields: std::collections::BTreeMap::new(),
                    endpoint: None,
                    state: NodeState::Admitted,
                    version: 1,
                    by: issuer.node_fp.clone(),
                }],
            },
        )
        .unwrap();
        let (joiner, _) = identity::init_identity_at(&dir.join("joiner")).unwrap();
        admit_join_locked_at(&dir, &joiner.static_pubkey, Some("192.0.2.40:9443")).unwrap();
        let registry = registry::load_registry_at(&dir).unwrap();
        let admitted = registry
            .authorized_nodes
            .iter()
            .find(|entry| entry.node_fp == joiner.node_fp)
            .unwrap();
        assert_eq!(admitted.endpoint.as_deref(), Some("192.0.2.40:9443"));
        assert_eq!(admitted.by, issuer.node_fp);
        let _ = std::fs::remove_dir_all(dir);
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
                    delivered_by: None,
                    format_major: 1,
                    format_minor: 0,
                    optional_fields: std::collections::BTreeMap::new(),
                    endpoint: None,
                    state: NodeState::Admitted,
                    version: 1,
                    by: self_node.node_fp.clone(),
                },
                AuthorizedNode {
                    node_fp: target.node_fp.clone(),
                    static_pubkey: encoding::encode_base64(&target.static_pubkey),
                    delivered_by: None,
                    format_major: 1,
                    format_minor: 0,
                    optional_fields: std::collections::BTreeMap::new(),
                    endpoint: None,
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
                        delivered_by: None,
                        format_major: 1,
                        format_minor: 0,
                        optional_fields: std::collections::BTreeMap::new(),
                        endpoint: None,
                        state: NodeState::Admitted,
                        version: 1,
                        by: self_node.node_fp.clone(),
                    },
                    AuthorizedNode {
                        node_fp: sender.node_fp.clone(),
                        static_pubkey: encoding::encode_base64(&sender.static_pubkey),
                        delivered_by: None,
                        format_major: 1,
                        format_minor: 0,
                        optional_fields: std::collections::BTreeMap::new(),
                        endpoint: None,
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
                delivered_by: None,
                format_major: 1,
                format_minor: 0,
                optional_fields: std::collections::BTreeMap::new(),
                endpoint: None,
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
                    delivered_by: None,
                    format_major: 1,
                    format_minor: 0,
                    optional_fields: std::collections::BTreeMap::new(),
                    endpoint: None,
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
                    delivered_by: None,
                    format_major: 1,
                    format_minor: 0,
                    optional_fields: std::collections::BTreeMap::new(),
                    endpoint: None,
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
                delivered_by: None,
                format_major: 1,
                format_minor: 0,
                optional_fields: std::collections::BTreeMap::new(),
                endpoint: None,
                state: NodeState::Admitted,
                version: 1,
                by: String::new(),
            },
            AuthorizedNode {
                node_fp: "SHA256:abcdefgh-two".into(),
                static_pubkey: String::new(),
                delivered_by: None,
                format_major: 1,
                format_minor: 0,
                optional_fields: std::collections::BTreeMap::new(),
                endpoint: None,
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
                delivered_by: None,
                format_major: 1,
                format_minor: 0,
                optional_fields: std::collections::BTreeMap::new(),
                endpoint: None,
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
                delivered_by: None,
                format_major: 1,
                format_minor: 0,
                optional_fields: std::collections::BTreeMap::new(),
                endpoint: None,
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
            delivered_by: None,
            format_major: 1,
            format_minor: 0,
            optional_fields: std::collections::BTreeMap::new(),
            endpoint: None,
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
                delivered_by: None,
                format_major: 1,
                format_minor: 0,
                optional_fields: std::collections::BTreeMap::new(),
                endpoint: None,
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
