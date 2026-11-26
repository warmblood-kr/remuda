//! Open a file beneath a trusted session workdir without following links.
use std::fs::File;
use std::io;
use std::path::{Component, Path};

pub(crate) fn open(root: &Path, path: &Path) -> io::Result<File> {
    if !root.is_absolute() || root.parent().is_none() {
        return Err(denied(
            "expected an absolute session workdir below the filesystem root",
        ));
    }
    let relative = relative_path(root, path)?;
    let mut components = relative.components().peekable();
    if components.peek().is_none()
        || !components
            .clone()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(denied("expected a regular file below the session workdir"));
    }

    let mut parent = platform::open_root(root)?;
    while let Some(Component::Normal(name)) = components.next() {
        let directory = components.peek().is_some();
        let handle = platform::open_component(&parent, name, directory)?;
        if !directory {
            let metadata = handle.metadata()?;
            if !metadata.is_file() || platform::has_multiple_links(&handle, &metadata)? {
                return Err(denied("expected a regular file with a single link"));
            }
            return Ok(handle);
        }
        parent = handle;
    }
    Err(denied("expected a regular file below the session workdir"))
}

#[cfg(not(windows))]
fn relative_path<'a>(root: &Path, path: &'a Path) -> io::Result<&'a Path> {
    path.strip_prefix(root)
        .map_err(|_| denied("file is outside the session workdir"))
}

#[cfg(windows)]
fn relative_path<'a>(root: &Path, path: &'a Path) -> io::Result<&'a Path> {
    fn disk(part: Option<Component<'_>>) -> Option<u8> {
        use std::path::Prefix;

        let Component::Prefix(prefix) = part? else {
            return None;
        };
        match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => Some(letter),
            _ => None,
        }
    }

    // Canonicalize gives the trusted workdir a verbatim prefix. Ordinary file
    // links usually supply the same drive with a Win32 prefix instead.
    let mut root_parts = root.components();
    let mut path_parts = path.components();
    let root_disk = disk(root_parts.next());
    if root_disk.is_none()
        || root_disk != disk(path_parts.next())
        || !root_parts.all(|part| path_parts.next() == Some(part))
    {
        return Err(denied("file is outside the session workdir"));
    }
    Ok(path_parts.as_path())
}

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

#[cfg(unix)]
mod platform {
    use super::{denied, Component, File, Path};
    use std::ffi::{CString, OsStr};
    use std::fs::Metadata;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    pub(super) fn open_root(root: &Path) -> io::Result<File> {
        let mut parent = File::open("/")?;
        for part in root.components().skip(1) {
            let Component::Normal(name) = part else {
                return Err(denied("invalid session workdir component"));
            };
            parent = open_component(&parent, name, true)?;
        }
        Ok(parent)
    }

    pub(super) fn open_component(parent: &File, name: &OsStr, directory: bool) -> io::Result<File> {
        let name = CString::new(name.as_bytes()).map_err(|_| denied("invalid path component"))?;
        let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        let before = if directory {
            flags |= libc::O_DIRECTORY;
            None
        } else {
            flags |= libc::O_NONBLOCK | libc::O_NOCTTY;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: name is terminated, parent is open, and stat is writable.
            let result = unsafe {
                libc::fstatat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: A successful fstatat populated the structure.
            let stat = unsafe { stat.assume_init() };
            if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(denied("expected a regular file"));
            }
            Some(stat)
        };
        // SAFETY: name is terminated, parent is open, and the flags require no mode.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: A successful openat returned a new owned descriptor.
        let handle = unsafe { File::from_raw_fd(fd) };
        if let Some(stat) = before {
            let metadata = handle.metadata()?;
            #[allow(
                clippy::unnecessary_cast,
                reason = "libc stat fields vary by Unix target"
            )]
            let previous = (stat.st_dev as u64, stat.st_ino as u64);
            if (metadata.dev(), metadata.ino()) != previous {
                return Err(denied("file changed during open"));
            }
        }
        Ok(handle)
    }

    pub(super) fn has_multiple_links(_handle: &File, metadata: &Metadata) -> io::Result<bool> {
        Ok(metadata.nlink() != 1)
    }
}

#[cfg(windows)]
mod platform {
    use super::{denied, Component, File, Path};
    use std::ffi::OsStr;
    use std::fs::{Metadata, OpenOptions};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Prefix;
    use windows_sys::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    pub(super) fn open_root(root: &Path) -> io::Result<File> {
        let mut components = root.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            return Err(denied("expected a local disk path"));
        };
        let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind() else {
            return Err(denied("expected a local disk path"));
        };
        if components.next() != Some(Component::RootDir) {
            return Err(denied("expected a local disk path"));
        }
        let volume = format!("{}:\\", char::from(letter));
        let volume = Path::new(&volume);
        let drive_string: Vec<u16> = volume.as_os_str().encode_wide().chain(Some(0)).collect();
        // DRIVE_REMOVABLE, DRIVE_FIXED, DRIVE_CDROM, or DRIVE_RAMDISK.
        // SAFETY: The drive path is a terminated UTF-16 string.
        if !matches!(
            unsafe { GetDriveTypeW(drive_string.as_ptr()) },
            2 | 3 | 5 | 6
        ) {
            return Err(denied("expected a local disk path"));
        }
        let mut parent = OpenOptions::new()
            .access_mode(crate::windows_security::WORKDIR_DIRECTORY_ACCESS)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(volume)?;
        for part in components {
            let Component::Normal(name) = part else {
                return Err(denied("invalid session workdir component"));
            };
            parent = open_component(&parent, name, true)?;
        }
        Ok(parent)
    }

    pub(super) fn open_component(parent: &File, name: &OsStr, directory: bool) -> io::Result<File> {
        crate::windows_security::open_workdir_component(parent, name, directory)
    }

    pub(super) fn has_multiple_links(handle: &File, _metadata: &Metadata) -> io::Result<bool> {
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
        // SAFETY: handle is open and info is a writable buffer of the required size.
        if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: A successful information query populated the structure.
        Ok(unsafe { info.assume_init() }.nNumberOfLinks != 1)
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::{denied, File, Path};
    use std::ffi::OsStr;
    use std::fs::Metadata;
    use std::io;

    pub(super) fn open_root(_root: &Path) -> io::Result<File> {
        Err(denied(
            "confined file opening is unavailable on this platform",
        ))
    }

    pub(super) fn open_component(
        _parent: &File,
        _name: &OsStr,
        _directory: bool,
    ) -> io::Result<File> {
        Err(denied(
            "confined file opening is unavailable on this platform",
        ))
    }

    pub(super) fn has_multiple_links(_handle: &File, _metadata: &Metadata) -> io::Result<bool> {
        Err(denied(
            "confined file opening is unavailable on this platform",
        ))
    }
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::open;
    use std::fs;
    use std::io::Read;
    use std::path::PathBuf;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "remuda-workdir-read-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn opens_a_nested_file_and_denies_other_paths() {
        let temp = TempDir::new();
        let root = temp.0.join("work");
        let sibling = temp.0.join("work-other");
        let nested = root.join("nested 한글");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(&sibling).unwrap();
        fs::write(nested.join("report"), "report").unwrap();
        fs::write(sibling.join("report"), "outside").unwrap();

        let mut contents = String::new();
        open(&root, &nested.join("report"))
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert_eq!(contents, "report");
        assert!(open(&root, &root).is_err());
        assert!(open(&root, &nested).is_err());
        assert!(open(&root, &sibling.join("report")).is_err());
        assert!(open(&root, &root.join("..").join("work-other").join("report")).is_err());
        assert!(open(&root, std::path::Path::new("report")).is_err());

        fs::hard_link(sibling.join("report"), root.join("linked")).unwrap();
        assert!(open(&root, &root.join("linked")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn accepts_an_ordinary_drive_path_for_a_verbatim_workdir() {
        let temp = TempDir::new();
        let root = temp.0.join("workspace");
        fs::create_dir(&root).unwrap();
        let path = root.join("report");
        fs::write(&path, "report").unwrap();

        let ordinary = path.to_str().unwrap().strip_prefix(r"\\?\").unwrap();
        assert!(open(&root, std::path::Path::new(ordinary)).is_ok());
        assert!(open(&root, &root.join("report:hidden")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn denies_symbolic_links_in_root_ancestors_and_below_root() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let root = temp.0.join("work");
        let outside = temp.0.join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir_all(outside.join("nested")).unwrap();
        fs::write(outside.join("nested/report"), "outside").unwrap();
        symlink(outside.join("nested/report"), root.join("file-link")).unwrap();
        symlink(&outside, root.join("dir-link")).unwrap();

        assert!(open(&root, &root.join("file-link")).is_err());
        assert!(open(&root, &root.join("dir-link/nested/report")).is_err());
        assert!(open(
            &root.join("dir-link/nested"),
            &root.join("dir-link/nested/report")
        )
        .is_err());
    }
}
