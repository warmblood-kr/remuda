//! Private storage only; fixed relative names and an independently held lockfile.
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::Path;

pub(super) struct Directory {
    #[cfg(unix)]
    handle: File,
    #[cfg(windows)]
    handle: crate::windows_security::StorageDirectory,
}
impl Directory {
    pub fn create(path: &Path) -> io::Result<Self> {
        validate_path(path)?;
        #[cfg(unix)]
        {
            std::fs::DirBuilder::new().mode(0o700).create(path)?;
            Self::open(path)
        }
        #[cfg(windows)]
        {
            Ok(Self {
                handle: crate::windows_security::create_private_root(path)?,
            })
        }
    }
    pub fn open(path: &Path) -> io::Result<Self> {
        validate_path(path)?;
        #[cfg(unix)]
        {
            Ok(Self {
                handle: super::super::unix::directory(path)?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                handle: crate::windows_security::verify_private_root(path)?,
            })
        }
    }
    pub fn read_index(&self) -> io::Result<File> {
        #[cfg(unix)]
        let file = self.open_relative(c"inventory.json", libc::O_RDONLY)?;
        #[cfg(windows)]
        let file = self.handle.open_file("inventory.json", false)?;
        private_file(&file)?;
        Ok(file)
    }
    pub fn lock_file(&self) -> io::Result<File> {
        #[cfg(unix)]
        let file = self.open_relative(c"inventory.lock", libc::O_RDWR | libc::O_CREAT)?;
        #[cfg(windows)]
        let file = self.handle.open_private_lock("inventory.lock")?;
        private_file(&file)?;
        if file.metadata()?.len() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "nonempty inventory lockfile",
            ));
        }
        Ok(file)
    }
    pub fn replace(&self, bytes: &[u8]) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.replace_relative(bytes)
        }
        #[cfg(windows)]
        {
            self.handle.write_atomic("inventory.json", bytes)
        }
    }
    #[cfg(unix)]
    fn open_relative(&self, name: &std::ffi::CStr, flags: i32) -> io::Result<File> {
        // SAFETY: held directory and NUL-terminated single-component name are live.
        let fd = unsafe {
            libc::openat(
                self.handle.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat transfers exactly one new descriptor here.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    #[cfg(unix)]
    fn replace_relative(&self, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|_| io::Error::other("inventory entropy"))?;
        let name =
            std::ffi::CString::new(format!(".inventory-{:032x}", u128::from_le_bytes(random)))
                .expect("fixed hex has no NUL");
        let mut temporary =
            self.open_relative(&name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        let result = (|| {
            private_file(&temporary)?;
            temporary.write_all(bytes)?;
            temporary.sync_all()?;
            // SAFETY: both names are fixed single components under one held directory.
            if unsafe {
                libc::renameat(
                    self.handle.as_raw_fd(),
                    name.as_ptr(),
                    self.handle.as_raw_fd(),
                    c"inventory.json".as_ptr(),
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            self.handle.sync_all()
        })();
        // SAFETY: best-effort cleanup of only our exclusive temporary name.
        unsafe {
            libc::unlinkat(self.handle.as_raw_fd(), name.as_ptr(), 0);
        }
        result
    }
}
fn validate_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "absolute private inventory directory required",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        if path.as_os_str().encode_wide().any(|c| c == 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "NUL inventory path",
            ));
        }
        if !matches!(path.components().next(),Some(std::path::Component::Prefix(prefix)) if matches!(prefix.kind(),std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)))
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "network/device inventory path unsupported",
            ));
        }
    }
    Ok(())
}
fn private_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "inventory object is not a regular private file",
        ));
    }
    #[cfg(unix)]
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "inventory file owner/mode/link mismatch",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: file remains live; info is a valid writable output structure.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.nNumberOfLinks != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "inventory file has aliases",
            ));
        }
    }
    Ok(())
}
