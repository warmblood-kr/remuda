//! Local, nonreplicated cluster control settings.

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
    use std::fs;
    #[cfg(not(windows))]
    use std::fs::OpenOptions;
    use std::io::Read;

    match fs::symlink_metadata(dir) {
        Ok(_) => super::storage::verify_directory(dir)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
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
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error),
        }
    };
    #[cfg(windows)]
    let file = match super::windows_security::open_for_read(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
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
    serde_json::from_slice::<Settings>(&bytes)
        .map(|settings| settings.allow_remote_control)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Persist the setting in one cluster state directory.
pub fn set_enabled_at(dir: &Path, value: bool) -> io::Result<()> {
    use std::fs;

    super::storage::verify_directory(dir)?;
    let _guard = super::storage::StateLock::acquire(dir)?;
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
    let mut bytes = serde_json::to_vec(&Settings {
        allow_remote_control: value,
    })
    .map_err(io::Error::other)?;
    bytes.push(b'\n');
    super::storage::atomic_write(&path, &bytes)
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::{enabled_at, set_enabled_at};
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
