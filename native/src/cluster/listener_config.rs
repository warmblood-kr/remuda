//! Persistent local settings for the cluster listener.

use serde::{Deserialize, Serialize};
use std::io;
use std::net::SocketAddr;
use std::path::Path;

const LISTENER_FILE: &str = "listener.json";
const LISTENER_MAX_BYTES: u64 = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenerBind {
    Auto,
    Explicit(SocketAddr),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenerConfig {
    pub enabled: bool,
    pub bind: ListenerBind,
    pub allow_public: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredListenerConfig {
    enabled: bool,
    bind: String,
    allow_public: bool,
}

/// Read the listener configuration, returning `None` when it is not stored.
pub fn read() -> io::Result<Option<ListenerConfig>> {
    read_at(&super::storage::cluster_state_dir()?.join("cluster"))
}

/// Read the listener configuration from one cluster state directory.
pub fn read_at(dir: &Path) -> io::Result<Option<ListenerConfig>> {
    use std::fs;
    #[cfg(not(windows))]
    use std::fs::OpenOptions;
    use std::io::Read;

    match fs::symlink_metadata(dir) {
        Ok(_) => super::storage::verify_directory(dir)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let path = dir.join(LISTENER_FILE);
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
    super::storage::check_private_file(&file, "cluster listener config", &path)?;
    if file.metadata()?.len() > LISTENER_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cluster listener config exceeds byte cap",
        ));
    }
    let mut bytes = Vec::new();
    file.take(LISTENER_MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LISTENER_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cluster listener config exceeds byte cap",
        ));
    }
    let stored = serde_json::from_slice::<StoredListenerConfig>(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let bind = if stored.bind == "auto" {
        ListenerBind::Auto
    } else {
        ListenerBind::Explicit(stored.bind.parse().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid cluster listener bind address: {error}"),
            )
        })?)
    };
    Ok(Some(ListenerConfig {
        enabled: stored.enabled,
        bind,
        allow_public: stored.allow_public,
    }))
}

/// Persist the listener configuration after verifying the local identity.
pub fn write(config: &ListenerConfig) -> io::Result<()> {
    let dir = super::storage::cluster_state_dir()?.join("cluster");
    super::identity::load_identity_at(&dir)?;
    write_at(&dir, config)
}

/// Persist the listener configuration in one cluster state directory.
pub fn write_at(dir: &Path, config: &ListenerConfig) -> io::Result<()> {
    use std::fs;

    super::storage::verify_directory(dir)?;
    let _guard = super::storage::StateLock::acquire(dir)?;
    let path = dir.join(LISTENER_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cluster listener config is a symlink; refusing",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let bind = match &config.bind {
        ListenerBind::Auto => "auto".to_owned(),
        ListenerBind::Explicit(address) => address.to_string(),
    };
    let mut bytes = serde_json::to_vec(&StoredListenerConfig {
        enabled: config.enabled,
        bind,
        allow_public: config.allow_public,
    })
    .map_err(io::Error::other)?;
    bytes.push(b'\n');
    super::storage::atomic_write(&path, &bytes)
}

#[cfg(all(test, unix))]
mod tests {
    use super::{read_at, write, write_at, ListenerBind, ListenerConfig};
    use std::io::Write;
    use std::net::SocketAddr;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn cluster_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "remuda-listener-config-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn config(bind: ListenerBind) -> ListenerConfig {
        ListenerConfig {
            enabled: true,
            bind,
            allow_public: false,
        }
    }

    fn write_private(dir: &Path, bytes: &[u8], mode: u32) {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(dir.join("listener.json"))
            .unwrap();
        file.write_all(bytes).unwrap();
    }

    #[test]
    fn missing_listener_config_returns_none() {
        let dir = cluster_dir();
        assert_eq!(read_at(&dir).unwrap(), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn listener_config_round_trips_auto_and_explicit_bind() {
        let dir = cluster_dir();
        for bind in [
            ListenerBind::Auto,
            ListenerBind::Explicit("192.0.2.4:7441".parse::<SocketAddr>().unwrap()),
        ] {
            let expected = config(bind);
            write_at(&dir, &expected).unwrap();
            assert_eq!(read_at(&dir).unwrap(), Some(expected));
        }
        let raw = std::fs::read_to_string(dir.join("listener.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(value["bind"].is_string());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn listener_config_write_requires_existing_identity() {
        let _lock = ENV_LOCK.lock().unwrap();
        let state_home = cluster_dir();
        let old = std::env::var_os("XDG_STATE_HOME");
        std::env::set_var("XDG_STATE_HOME", &state_home);
        let result = write(&config(ListenerBind::Auto));
        match old {
            Some(value) => std::env::set_var("XDG_STATE_HOME", value),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(state_home);
    }

    #[test]
    fn listener_config_file_is_private() {
        let dir = cluster_dir();
        write_at(&dir, &config(ListenerBind::Auto)).unwrap();
        assert_eq!(
            std::fs::metadata(dir.join("listener.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn listener_config_refuses_symlinks_for_read_and_write() {
        let dir = cluster_dir();
        let outside = dir.with_extension("outside");
        std::fs::write(
            &outside,
            br#"{"enabled":true,"bind":"auto","allow_public":false}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("listener.json")).unwrap();
        assert!(read_at(&dir).is_err());
        assert!(write_at(&dir, &config(ListenerBind::Auto)).is_err());
        let _ = std::fs::remove_file(dir.join("listener.json"));
        let _ = std::fs::remove_file(outside);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn listener_config_refuses_loose_file_permissions() {
        let dir = cluster_dir();
        write_private(
            &dir,
            br#"{"enabled":true,"bind":"auto","allow_public":false}"#,
            0o644,
        );
        assert!(read_at(&dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn listener_config_rejects_unknown_fields() {
        let dir = cluster_dir();
        write_private(
            &dir,
            br#"{"enabled":true,"bind":"auto","allow_public":false,"unexpected":true}"#,
            0o600,
        );
        assert!(read_at(&dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn listener_config_rejects_files_over_4_kib() {
        let dir = cluster_dir();
        let mut bytes = br#"{"enabled":true,"bind":"auto","allow_public":false}"#.to_vec();
        bytes.resize(4097, b' ');
        write_private(&dir, &bytes, 0o600);
        assert!(read_at(&dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
