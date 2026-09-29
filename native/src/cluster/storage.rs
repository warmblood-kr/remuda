//! Shared hardened local state-file operations used by cluster units.

#[cfg(windows)]
use std::fs::{self, File};
#[cfg(not(windows))]
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

#[cfg(not(windows))]
pub(super) fn cluster_state_dir() -> io::Result<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(
            std::env::var_os("HOME")
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?,
        )
        .join(".local/state"),
    };
    Ok(base.join("remuda"))
}

#[cfg(not(windows))]
pub(super) fn create_private_directory(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

#[cfg(windows)]
pub(super) fn cluster_state_dir() -> io::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|profile| !profile.is_empty())
                .map(|profile| {
                    PathBuf::from(profile)
                        .join("AppData/Local")
                        .into_os_string()
                })
        })
        .filter(|value| !value.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is not set"))?;
    let dir = PathBuf::from(base).join("remuda");
    match fs::symlink_metadata(&dir) {
        Ok(_) => verify_directory(&dir)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(dir)
}

#[cfg(windows)]
pub(super) fn create_private_directory(dir: &Path) -> io::Result<()> {
    if let Some(parent) = dir
        .parent()
        .filter(|p| p.file_name().is_some_and(|name| name == "remuda"))
    {
        match super::windows_security::create_directory(parent) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => verify_directory(parent)?,
            Err(error) => return Err(error),
        }
    } else if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    match super::windows_security::create_directory(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => verify_directory(dir),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
pub(super) fn verify_directory(dir: &Path) -> io::Result<()> {
    let file = super::windows_security::open_for_check(dir, true, true)?;
    super::windows_security::validate_open_object(&file, dir, true)?;
    secure_known_children(dir)?;
    super::windows_security::secure_or_upgrade(&file, dir, true)?;
    Ok(())
}

#[cfg(windows)]
fn secure_known_children(dir: &Path) -> io::Result<()> {
    for name in [
        "identity.key",
        "identity.lock",
        "settings.json",
        "authorized_nodes.json",
        "join_tokens.json",
    ] {
        let path = dir.join(name);
        match super::windows_security::open_for_check(&path, false, true) {
            Ok(file) => {
                super::windows_security::secure_or_upgrade(&file, &path, false)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if dir.file_name().is_some_and(|name| name == "remuda") {
        let cluster = dir.join("cluster");
        match fs::symlink_metadata(&cluster) {
            Ok(_) => verify_directory(&cluster)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn check_directory_type(dir: &Path) -> io::Result<()> {
    verify_directory(dir)
}

#[cfg(windows)]
pub(super) fn check_private_file(file: &File, description: &str, path: &Path) -> io::Result<()> {
    super::windows_security::secure_or_upgrade(file, path, false)
        .map(|_| ())
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{description} {}: {error}", path.display()),
            )
        })
}

#[cfg(windows)]
pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            let file = super::windows_security::open_for_check(path, false, true)?;
            check_private_file(&file, "cluster state", path)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    crate::fs_atomic::write_atomic_private(path, bytes)
}

#[cfg(windows)]
pub(super) struct StateLock(File);
#[cfg(windows)]
impl StateLock {
    pub(super) fn acquire(dir: &Path) -> io::Result<Self> {
        let path = dir.join("identity.lock");
        let file = super::windows_security::create_or_open_lock(&path)?;
        check_private_file(&file, "cluster lock", &path)?;
        file.lock()?;
        Ok(Self(file))
    }
}
#[cfg(windows)]
impl Drop for StateLock {
    fn drop(&mut self) {
        drop(self.0.unlock());
    }
}

#[cfg(not(windows))]
pub(super) fn verify_directory(dir: &Path) -> io::Result<()> {
    check_directory_type(dir)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
    }
    let file = options.open(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        let expected_uid = unsafe { libc::geteuid() };
        if meta.uid() != expected_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is owned by uid {}, expected {}; run chown {} {}",
                    dir.display(),
                    meta.uid(),
                    expected_uid,
                    expected_uid,
                    dir.display()
                ),
            ));
        }
        let mode = meta.mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} has loose permissions (mode {mode:04o}); run chmod 700 {}",
                    dir.display(),
                    dir.display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub(super) fn check_directory_type(dir: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(dir)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is a symlink; refusing (remove it to re-initialize)",
                dir.display()
            ),
        ));
    }
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", dir.display()),
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
pub(super) fn check_private_file(_file: &File, description: &str, path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = _file.metadata()?;
        let expected_uid = unsafe { libc::geteuid() };
        if meta.uid() != expected_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{description} {} is owned by uid {}, expected {}; run chown {} {}",
                    path.display(),
                    meta.uid(),
                    expected_uid,
                    expected_uid,
                    path.display()
                ),
            ));
        }
        let mode = meta.mode() & 0o777;
        if mode & 0o177 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{description} {} has loose permissions (mode {mode:04o}); run chmod 600 {}",
                    path.display(),
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    crate::fs_atomic::write_atomic(path, bytes, 0o600)
}

#[cfg(not(windows))]
pub(super) struct StateLock(File);
#[cfg(not(windows))]
impl StateLock {
    pub(super) fn acquire(dir: &Path) -> io::Result<Self> {
        let path = dir.join("identity.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path)?;
        check_private_file(&file, "cluster lock", &path)?;
        file.lock()?;
        Ok(Self(file))
    }
}
#[cfg(not(windows))]
impl Drop for StateLock {
    fn drop(&mut self) {
        drop(self.0.unlock());
    }
}
