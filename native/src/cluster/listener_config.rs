//! Persistent local settings for the cluster listener.

#[cfg(test)]
use std::io;
#[cfg(test)]
use std::net::SocketAddr;
#[cfg(test)]
use std::path::Path;

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenerBind {
    Auto,
    Explicit(SocketAddr),
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenerConfig {
    pub enabled: bool,
    pub bind: ListenerBind,
    pub allow_public: bool,
}

#[cfg(test)]
pub fn read() -> io::Result<Option<ListenerConfig>> {
    Ok(None)
}

#[cfg(test)]
pub fn read_at(_dir: &Path) -> io::Result<Option<ListenerConfig>> {
    Ok(None)
}

#[cfg(test)]
pub fn write(_config: &ListenerConfig) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
pub fn write_at(_dir: &Path, _config: &ListenerConfig) -> io::Result<()> {
    Ok(())
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
