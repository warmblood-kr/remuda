//! Shared hardened local state-file operations used by cluster units.

#[cfg(not(windows))]
use std::fs::{self, File, OpenOptions};
use std::io;
#[cfg(not(windows))]
use std::io::Write;
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
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = parent.join(format!(
        ".{name}-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
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
