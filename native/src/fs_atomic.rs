//! Atomic replacement of one file for trusted Lua callers and local state.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Write bytes to a same-directory temporary file, then replace `path`.
/// Existing regular-file permissions are preserved; `mode` is applied to new
/// files at creation on Unix and is ignored on Windows.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    write_atomic_inner(path, bytes, mode, true)
}

/// Write an owner-only Lua state file on Unix. Windows uses its normal
/// inherited ACL for this public Lua option.
pub(crate) fn write_atomic_lua_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_inner(path, bytes, 0o600, false)
}

fn write_atomic_inner(
    path: &Path,
    bytes: &[u8],
    mode: u32,
    preserve_existing_mode: bool,
) -> io::Result<()> {
    use std::fmt::Write as _;
    #[cfg(not(unix))]
    let _ = (mode, preserve_existing_mode);

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write path must name a file",
        ));
    }

    #[cfg(unix)]
    let existing_mode = {
        use std::os::unix::fs::PermissionsExt;
        if !preserve_existing_mode {
            None
        } else {
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    Some(metadata.permissions().mode() & 0o777)
                }
                Ok(_) => None,
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            }
        }
    };

    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| {
        io::Error::other(format!("atomic write randomness unavailable: {error}"))
    })?;
    let mut random_hex = String::with_capacity(random.len() * 2);
    for byte in random {
        write!(&mut random_hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    let temporary = parent.join(format!(
        ".remuda-atomic-{}-{random_hex}.tmp",
        std::process::id()
    ));

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(existing_mode.unwrap_or(mode));
    }
    let mut file = options.open(&temporary)?;
    let written = file.write_all(bytes).and_then(|()| {
        #[cfg(unix)]
        if preserve_existing_mode {
            if let Some(existing_mode) = existing_mode {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(existing_mode))?;
            }
        }
        file.sync_all()
    });
    drop(file);
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Cluster state replacement using a Windows security descriptor at temp-file creation.
#[cfg(windows)]
pub(crate) fn write_atomic_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    crate::cluster::windows_security::write_atomic(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::write_atomic;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "remuda-fs-atomic-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("create atomic-write test directory");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn replaces_an_existing_file_with_the_new_bytes() {
        let scratch = Scratch::new();
        let target = scratch.0.join("state");
        fs::write(&target, b"old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        }

        write_atomic(&target, b"new", 0o600).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(target).unwrap().permissions().mode() & 0o777,
                0o644
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn preserves_an_existing_file_mode_when_replacing() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = Scratch::new();
        let target = scratch.0.join("secret");
        fs::write(&target, b"old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();

        write_atomic(&target, b"new", 0o644).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(
            fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_write_is_atomic_and_stays_owner_only_under_umask_022() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        const CHILD: &str = "REMUDA_PRIVATE_WRITE_UMASK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "fs_atomic::tests::private_write_is_atomic_and_stays_owner_only_under_umask_022",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .status()
                .expect("run isolated umask test child");
            assert!(status.success(), "isolated umask test child failed");
            return;
        }

        let scratch = Scratch::new();
        let target = scratch.0.join("private-state");
        unsafe { libc::umask(0o022) };
        super::write_atomic_lua_private(&target, b"first").unwrap();
        let first = fs::metadata(&target).unwrap();
        assert_eq!(first.permissions().mode() & 0o777, 0o600);

        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        super::write_atomic_lua_private(&target, b"replacement").unwrap();
        let replacement = fs::metadata(&target).unwrap();
        assert_eq!(replacement.permissions().mode() & 0o777, 0o600);
        assert_ne!(
            replacement.ino(),
            first.ino(),
            "private write replaces by rename"
        );
        assert_eq!(fs::read(&target).unwrap(), b"replacement");
        assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn replaces_a_symlink_itself_without_writing_through_it() {
        use std::os::unix::fs::symlink;

        let scratch = Scratch::new();
        let original = scratch.0.join("original");
        let target = scratch.0.join("state");
        fs::write(&original, b"keep").unwrap();
        symlink(&original, &target).unwrap();

        write_atomic(&target, b"new", 0o600).unwrap();

        assert_eq!(fs::read(&original).unwrap(), b"keep");
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert!(!fs::symlink_metadata(target)
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
