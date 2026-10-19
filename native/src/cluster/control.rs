//! Local, nonreplicated cluster control settings.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

const SETTINGS_FILE: &str = "settings.json";
const SETTINGS_MAX_BYTES: u64 = 4096;
const REVOKED_NOTICE_FILE: &str = "revoked_notice.json";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RevokedNotice {
    pub by_fp: String,
    pub at: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    allow_remote_control: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    node_aliases: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            allow_remote_control: true,
            node_aliases: BTreeMap::new(),
        }
    }
}

/// Read display aliases keyed by fingerprint. Aliases replicate through the
/// membership registry; legacy receiver-local settings fill in only nodes with
/// no replicated record (including a replicated clear).
pub fn node_aliases() -> io::Result<BTreeMap<String, String>> {
    let dir = super::storage::cluster_state_dir()?.join("cluster");
    let registry = super::registry::load_registry_at(&dir)?;
    Ok(merged_aliases(&registry, settings_at(&dir)?.node_aliases))
}

fn merged_aliases(
    registry: &super::Registry,
    mut aliases: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    for node in &registry.authorized_nodes {
        if let Some(record) = super::registry::alias_record(&node.optional_fields) {
            match record.name {
                Some(name) => aliases.insert(node.node_fp.clone(), name),
                None => aliases.remove(&node.node_fp),
            };
        }
    }
    aliases
}

/// Assign or clear a cluster-wide display alias using a node id or full
/// fingerprint, then push it to peers before the short-lived CLI exits.
pub fn set_node_alias(target: &str, alias: Option<&str>) -> io::Result<Vec<super::PeerPushResult>> {
    let dir = super::storage::cluster_state_dir()?.join("cluster");
    super::identity::load_identity_at(&dir)?;
    super::storage::verify_directory(&dir)?;
    {
        let _guard = super::storage::StateLock::acquire(&dir)?;
        let registry = super::registry::load_registry_at(&dir)?;
        let node = super::resolve_node(&registry.authorized_nodes, target)?;
        set_node_alias_at(&dir, &node.node_fp, alias, now_millis())?;
    }
    Ok(super::replication::push_now())
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

pub(super) fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 64
        && alias == alias.trim()
        && !alias.contains('/')
        && super::super::text::strip_terminal_controls(alias) == alias
}

/// Write an alias record into the registry (caller holds the state lock).
/// The record time is forced above any existing record so a local set always
/// wins locally despite clock skew. Legacy local aliases are migrated into the
/// registry with time zero, so any real set or clear supersedes them.
fn set_node_alias_at(
    dir: &Path,
    fingerprint: &str,
    alias: Option<&str>,
    now_ms: u64,
) -> io::Result<()> {
    if alias.is_some_and(|alias| !valid_alias(alias)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "alias must be 1–64 bytes, without controls, slash or surrounding whitespace",
        ));
    }
    let me = super::identity::load_identity_at(dir)?.node_fp;
    let mut registry = super::registry::load_registry_at(dir)?;
    let mut settings = settings_at(dir)?;
    let legacy = std::mem::take(&mut settings.node_aliases);
    for (fp, name) in legacy {
        let Some(entry) = registry
            .authorized_nodes
            .iter_mut()
            .find(|entry| entry.node_fp == fp && entry.state == super::NodeState::Admitted)
        else {
            settings.node_aliases.insert(fp, name);
            continue;
        };
        if super::registry::alias_record(&entry.optional_fields).is_none() {
            let record = super::registry::AliasRecord {
                at: 0,
                by: me.clone(),
                name: Some(name),
            };
            super::registry::put_alias_record(entry, &record);
        }
    }
    let entry = registry
        .authorized_nodes
        .iter_mut()
        .find(|entry| entry.node_fp == fingerprint)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown cluster node"))?;
    let previous = super::registry::alias_record(&entry.optional_fields).map_or(0, |r| r.at);
    let record = super::registry::AliasRecord {
        at: now_ms.max(previous.saturating_add(1)),
        by: me,
        name: alias.map(str::to_owned),
    };
    super::registry::put_alias_record(entry, &record);
    super::registry::save_registry_at(dir, &registry)?;
    save_settings_at(dir, &settings)
}

/// Return whether this node accepts remote control requests. Missing settings
/// use the owner-approved default: enabled.
pub fn enabled() -> io::Result<bool> {
    enabled_at(&super::storage::cluster_state_dir()?.join("cluster"))
}

/// Set whether this node accepts remote control requests.
pub fn set_enabled(value: bool) -> io::Result<()> {
    let dir = super::storage::cluster_state_dir()?.join("cluster");
    super::identity::load_identity_at(&dir)?;
    set_enabled_at(&dir, value)
}

/// Read this node's local revocation notice, if an admitted member has sent one.
pub fn revoked_notice() -> io::Result<Option<RevokedNotice>> {
    revoked_notice_at(&super::storage::cluster_state_dir()?.join("cluster"))
}

pub(super) fn revoked_notice_at(dir: &Path) -> io::Result<Option<RevokedNotice>> {
    use std::fs;
    #[cfg(not(windows))]
    use std::fs::OpenOptions;
    use std::io::Read;

    match fs::symlink_metadata(dir) {
        Ok(_) => super::storage::verify_directory(dir)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let path = dir.join(REVOKED_NOTICE_FILE);
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
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
    };
    #[cfg(windows)]
    let file = match super::windows_security::open_for_read(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    super::storage::check_private_file(&file, "cluster revocation notice", &path)?;
    if file.metadata()?.len() > SETTINGS_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cluster revocation notice exceeds byte cap",
        ));
    }
    let mut bytes = Vec::new();
    file.take(SETTINGS_MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > SETTINGS_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cluster revocation notice exceeds byte cap",
        ));
    }
    let notice: RevokedNotice = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !notice.by_fp.starts_with("SHA256:") || notice.at.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cluster revocation notice has invalid fields",
        ));
    }
    Ok(Some(notice))
}

pub(super) fn save_revoked_notice_at(dir: &Path, by_fp: &str) -> io::Result<()> {
    super::storage::verify_directory(dir)?;
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();
    let mut bytes = serde_json::to_vec(&RevokedNotice {
        by_fp: by_fp.to_owned(),
        at,
    })
    .map_err(io::Error::other)?;
    bytes.push(b'\n');
    super::storage::atomic_write(&dir.join(REVOKED_NOTICE_FILE), &bytes)
}

pub(super) fn clear_revoked_notice_at(dir: &Path) -> io::Result<()> {
    use std::fs;

    super::storage::verify_directory(dir)?;
    let path = dir.join(REVOKED_NOTICE_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cluster revocation notice is a symlink; refusing",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let _ = revoked_notice_at(dir)?;
    fs::remove_file(path)
}

/// Read the setting from one cluster state directory.
pub fn enabled_at(dir: &Path) -> io::Result<bool> {
    Ok(settings_at(dir)?.allow_remote_control)
}

fn settings_at(dir: &Path) -> io::Result<Settings> {
    use std::fs;
    #[cfg(not(windows))]
    use std::fs::OpenOptions;
    use std::io::Read;

    match fs::symlink_metadata(dir) {
        Ok(_) => super::storage::verify_directory(dir)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(error) => return Err(error),
    }
    let path = dir.join(SETTINGS_FILE);
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
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(error) => return Err(error),
        }
    };
    #[cfg(windows)]
    let file = match super::windows_security::open_for_read(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(error) => return Err(error),
    };
    super::storage::check_private_file(&file, "cluster settings", &path)?;
    if file.metadata()?.len() > SETTINGS_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cluster settings exceed byte cap",
        ));
    }
    let mut bytes = Vec::new();
    file.take(SETTINGS_MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > SETTINGS_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cluster settings exceed byte cap",
        ));
    }
    let settings: Settings = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if settings
        .node_aliases
        .values()
        .any(|alias| !valid_alias(alias))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid node alias in cluster settings",
        ));
    }
    Ok(settings)
}

/// Persist the setting in one cluster state directory.
pub fn set_enabled_at(dir: &Path, value: bool) -> io::Result<()> {
    super::storage::verify_directory(dir)?;
    let _guard = super::storage::StateLock::acquire(dir)?;
    let mut settings = settings_at(dir)?;
    settings.allow_remote_control = value;
    save_settings_at(dir, &settings)
}

fn save_settings_at(dir: &Path, settings: &Settings) -> io::Result<()> {
    use std::fs;

    super::storage::verify_directory(dir)?;
    let path = dir.join(SETTINGS_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cluster settings is a symlink; refusing",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut bytes = serde_json::to_vec(settings).map_err(io::Error::other)?;
    bytes.push(b'\n');
    if bytes.len() as u64 > SETTINGS_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cluster settings exceed byte cap",
        ));
    }
    super::storage::atomic_write(&path, &bytes)
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::{enabled_at, merged_aliases, set_enabled_at, settings_at};
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn cluster_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "remuda-control-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        dir
    }

    #[test]
    fn missing_control_setting_defaults_to_enabled() {
        let dir = cluster_dir();
        assert!(enabled_at(&dir).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn node_alias_settings_preserve_remote_control_preference() {
        let dir = cluster_dir();
        super::super::storage::atomic_write(
            &dir.join("settings.json"),
            br#"{"allow_remote_control":false,"node_aliases":{"SHA256:peer":"macbook"}}"#,
        )
        .unwrap();
        assert!(!enabled_at(&dir).unwrap());
        set_enabled_at(&dir, true).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("settings.json")).unwrap()).unwrap();
        assert_eq!(saved["node_aliases"]["SHA256:peer"], "macbook");
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn alias_cluster() -> (PathBuf, String, String) {
        use super::super::{encoding, registry, AuthorizedNode, NodeState, Registry};
        let dir = cluster_dir().join("cluster");
        crate::cluster::storage::create_private_directory(&dir).unwrap();
        let (me, _) = crate::cluster::identity::init_identity_at(&dir).unwrap();
        let key = [7u8; 32];
        let peer_fp = encoding::fingerprint(&key);
        let node = |node_fp: &str, pubkey: &[u8]| AuthorizedNode {
            node_fp: node_fp.into(),
            static_pubkey: encoding::encode_base64(pubkey),
            delivered_by: None,
            format_major: 1,
            format_minor: 0,
            optional_fields: BTreeMap::new(),
            endpoint: None,
            state: NodeState::Admitted,
            version: 1,
            by: me.node_fp.clone(),
        };
        let registry = Registry {
            authorized_nodes: vec![node(&me.node_fp, &me.static_pubkey), node(&peer_fp, &key)],
        };
        registry::save_registry_at(&dir, &registry).unwrap();
        (dir, me.node_fp, peer_fp)
    }

    fn registry_aliases(dir: &Path) -> BTreeMap<String, String> {
        let registry = super::super::registry::load_registry_at(dir).unwrap();
        merged_aliases(&registry, settings_at(dir).unwrap().node_aliases)
    }

    #[test]
    fn aliases_are_stored_in_the_registry_clear_with_a_tombstone_and_reject_controls() {
        let (dir, me, peer) = alias_cluster();
        set_enabled_at(&dir, false).unwrap();
        super::set_node_alias_at(&dir, &peer, Some("맥북"), 1_000).unwrap();
        assert_eq!(registry_aliases(&dir)[&peer], "맥북");
        assert!(settings_at(&dir).unwrap().node_aliases.is_empty());
        let before = std::fs::read(dir.join("authorized_nodes.json")).unwrap();
        for invalid in [
            "",
            " x",
            "x/guest",
            "x\x1b[31m",
            "x\u{202e}",
            &"a".repeat(65),
        ] {
            assert!(super::set_node_alias_at(&dir, &peer, Some(invalid), 2_000).is_err());
            assert_eq!(
                std::fs::read(dir.join("authorized_nodes.json")).unwrap(),
                before
            );
        }
        // A clock earlier than the stored record still supersedes it.
        super::set_node_alias_at(&dir, &me, Some("self"), 0).unwrap();
        super::set_node_alias_at(&dir, &peer, None, 500).unwrap();
        let aliases = registry_aliases(&dir);
        assert!(!aliases.contains_key(&peer));
        assert_eq!(aliases[&me], "self");
        let registry = super::super::registry::load_registry_at(&dir).unwrap();
        let entry = registry
            .authorized_nodes
            .iter()
            .find(|n| n.node_fp == peer)
            .unwrap();
        let record = super::super::registry::alias_record(&entry.optional_fields).unwrap();
        assert_eq!((record.name, record.at), (None, 1_001));
        assert!(!enabled_at(&dir).unwrap());
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn legacy_local_aliases_are_read_and_migrated_without_overriding_replicated_ones() {
        let (dir, me, peer) = alias_cluster();
        super::super::storage::atomic_write(
            &dir.join("settings.json"),
            format!(
                r#"{{"allow_remote_control":true,"node_aliases":{{"{peer}":"legacy","{me}":"mine","SHA256:unknown":"gone"}}}}"#
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(registry_aliases(&dir)[&peer], "legacy");
        super::set_node_alias_at(&dir, &me, Some("renamed"), 10).unwrap();
        let aliases = registry_aliases(&dir);
        assert_eq!(aliases[&peer], "legacy");
        assert_eq!(aliases[&me], "renamed");
        // Only the entry that is not an admitted member stays local.
        let legacy = settings_at(&dir).unwrap().node_aliases;
        assert_eq!(legacy.keys().collect::<Vec<_>>(), ["SHA256:unknown"]);
        // A replicated clear beats the legacy value on read.
        super::set_node_alias_at(&dir, &peer, None, 20).unwrap();
        assert!(!registry_aliases(&dir).contains_key(&peer));
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn control_setting_persists_and_can_be_changed_back() {
        let dir = cluster_dir();
        set_enabled_at(&dir, false).unwrap();
        assert!(!enabled_at(&dir).unwrap());
        set_enabled_at(&dir, true).unwrap();
        assert!(enabled_at(&dir).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn revoked_notice_persists_until_a_local_clear() {
        let dir = cluster_dir();
        super::save_revoked_notice_at(&dir, "SHA256:issuer").unwrap();
        let notice = super::revoked_notice_at(&dir).unwrap().unwrap();
        assert_eq!(notice.by_fp, "SHA256:issuer");
        assert!(notice.at.parse::<i64>().is_ok());
        super::clear_revoked_notice_at(&dir).unwrap();
        assert!(super::revoked_notice_at(&dir).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn control_setting_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = cluster_dir();
        set_enabled_at(&dir, false).unwrap();
        assert_eq!(
            std::fs::metadata(dir.join("settings.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn settings_symlink_is_refused() {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = cluster_dir();
        let outside = dir.with_extension("outside");
        let mut target = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&outside)
            .unwrap();
        target
            .write_all(br#"{"allow_remote_control":false}"#)
            .unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("settings.json")).unwrap();
        assert!(enabled_at(&dir).is_err());
        assert!(set_enabled_at(&dir, true).is_err());
        let _ = std::fs::remove_file(dir.join("settings.json"));
        let _ = std::fs::remove_file(outside);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_or_unknown_settings_fail_closed() {
        let dir = cluster_dir();
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(dir.join("settings.json"))
                .unwrap();
            use std::io::Write;
            let mut file = file;
            file.write_all(br#"{"allow_remote_control":true,"unexpected":true}"#)
                .unwrap();
        }
        #[cfg(not(unix))]
        std::fs::write(
            dir.join("settings.json"),
            br#"{"allow_remote_control":true,"unexpected":true}"#,
        )
        .unwrap();
        assert!(enabled_at(&dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
