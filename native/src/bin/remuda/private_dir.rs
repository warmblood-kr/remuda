//! A pinned private directory and the only way this launcher creates, reads or
//! removes the binding manifest.
//!
//! The manifest names a launch: nonce, pids, generation, endpoint, conversation
//! and auth secret. A path-based create/read/remove is racy — the parent can be
//! swapped between the permission check and the write, a truncated manifest
//! cannot be recognised by its own JSON, and a replacement file must never be
//! deleted. So the parent is opened once, checked with `fstat` on that opened
//! descriptor, and every leaf operation is relative to it (`openat`/`unlinkat`)
//! with `O_NOFOLLOW` and `O_NONBLOCK` so a symlink or FIFO cannot redirect or
//! block the open.
//!
//! Unix only, like the native capability itself.

#[cfg(unix)]
mod imp {
    use std::ffi::CString;
    use std::fs::{self, File};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    /// The `(device, inode)` pair that identifies one file, independent of its
    /// name. Cleanup compares it so a replacement at the same path survives.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct EntryId {
        pub dev: u64,
        pub ino: u64,
    }

    /// An opened, checked private directory. The descriptor is the authority;
    /// `path` is kept only for diagnostics.
    #[derive(Clone, Debug)]
    pub struct PrivateDir {
        fd: Arc<OwnedFd>,
        // Kept for diagnostics; the descriptor is the authority.
        #[allow(dead_code)]
        path: PathBuf,
    }

    /// A manifest this invocation created. `remove` is a no-op once the name
    /// no longer refers to this inode.
    #[derive(Clone, Debug)]
    pub struct CreatedFile {
        dir: PrivateDir,
        name: String,
        id: EntryId,
    }

    fn euid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    /// Root or this user, and not group/world writable unless sticky (which is
    /// what makes `/tmp` acceptable without making any private directory
    /// replaceable).
    fn ancestor_is_acceptable(uid: u32, mode: u32, euid: u32) -> bool {
        let owned = uid == 0 || uid == euid;
        let shared_write = mode & 0o022 != 0;
        let sticky = mode & 0o1000 != 0;
        owned && (!shared_write || sticky)
    }

    fn check_ancestors(dir: &Path) -> Result<(), String> {
        for ancestor in dir.ancestors() {
            let metadata =
                fs::metadata(ancestor).map_err(|error| format!("binding directory: {error}"))?;
            if !metadata.is_dir() {
                return Err("binding directory ancestor is not a directory".into());
            }
            if !ancestor_is_acceptable(metadata.uid(), metadata.mode(), euid()) {
                return Err("binding directory ancestor is not owned by this user or root".into());
            }
        }
        Ok(())
    }

    fn cstring(path: &Path) -> Result<CString, String> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| "binding path contains a NUL byte".to_string())
    }

    fn open_at(dir: RawFd, name: &str, flags: i32, mode: libc::mode_t) -> io::Result<File> {
        let name = CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: `dir` is an open directory descriptor, `name` is NUL
        // terminated, and a successful fd takes ownership below.
        let fd = unsafe { libc::openat(dir, name.as_ptr(), flags, mode as libc::c_uint) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just opened and is not owned anywhere else.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn entry_id(metadata: &fs::Metadata) -> EntryId {
        EntryId {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    fn valid_leaf(name: &str) -> bool {
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains('/')
            && !name.contains('\0')
    }

    impl PrivateDir {
        /// Open and pin `dir`. Its ancestors must be owned by root or this user
        /// and must not be group/world writable unless sticky, and `dir` itself
        /// must be owned by this user and not group/world accessible at all.
        pub fn open(dir: &Path) -> Result<Self, String> {
            let dir = dir
                .canonicalize()
                .map_err(|error| format!("binding directory: {error}"))?;
            check_ancestors(&dir)?;
            let path = cstring(&dir)?;
            // SAFETY: `path` is NUL terminated and the returned fd is owned below.
            let fd = unsafe {
                libc::open(
                    path.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(format!("binding directory: {}", io::Error::last_os_error()));
            }
            // SAFETY: `fd` was just opened and is not owned anywhere else.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let metadata = File::from(fd.try_clone().map_err(|error| error.to_string())?)
                .metadata()
                .map_err(|error| format!("binding directory: {error}"))?;
            if !metadata.is_dir() || metadata.uid() != euid() || metadata.mode() & 0o077 != 0 {
                return Err(
                    "binding directory must be a private directory owned by this user".into(),
                );
            }
            Ok(Self {
                fd: Arc::new(fd),
                path: dir,
            })
        }

        #[cfg(test)]
        #[allow(dead_code)]
        pub fn path(&self) -> &Path {
            &self.path
        }

        /// Create a new leaf with mode 0600. `O_EXCL` means a preexisting file
        /// is never truncated, adopted or removed.
        pub fn create_new(&self, name: &str) -> Result<(File, EntryId), String> {
            if !valid_leaf(name) {
                return Err("binding file name must be one path component".into());
            }
            let file = open_at(
                self.fd.as_raw_fd(),
                name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
            .map_err(|error| format!("cannot create binding manifest: {error}"))?;
            let metadata = file
                .metadata()
                .map_err(|error| format!("cannot inspect binding manifest: {error}"))?;
            if !metadata.is_file() || metadata.uid() != euid() || metadata.mode() & 0o077 != 0 {
                return Err("binding manifest is not a private regular file".into());
            }
            Ok((file, entry_id(&metadata)))
        }

        /// Open an existing leaf without following a symlink or blocking on a
        /// FIFO. The metadata checks run on the opened descriptor, so the leaf
        /// cannot be swapped after they pass.
        pub fn open_existing(&self, name: &str) -> Result<(File, EntryId), String> {
            if !valid_leaf(name) {
                return Err("binding file name must be one path component".into());
            }
            let file = open_at(
                self.fd.as_raw_fd(),
                name,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                0,
            )
            .map_err(|error| format!("cannot open binding manifest: {error}"))?;
            let metadata = file
                .metadata()
                .map_err(|error| format!("cannot inspect binding manifest: {error}"))?;
            if !metadata.is_file() || metadata.uid() != euid() || metadata.mode() & 0o077 != 0 {
                return Err("binding manifest is not a private regular file".into());
            }
            Ok((file, entry_id(&metadata)))
        }

        /// Read one private leaf with a hard bound on the opened descriptor.
        /// The metadata length is a fast rejection; the take bound and the
        /// post-read length decide, so a growing or swiped file cannot exceed it.
        pub fn read_limited(&self, name: &str, max: u64) -> Result<Vec<u8>, String> {
            let (mut file, _id) = self.open_existing(name)?;
            let metadata = file
                .metadata()
                .map_err(|error| format!("cannot inspect binding manifest: {error}"))?;
            if metadata.len() > max {
                return Err("binding manifest is too large".into());
            }
            let mut data = Vec::new();
            let mut bounded = io::Read::take(&mut file, max + 1);
            io::Read::read_to_end(&mut bounded, &mut data)
                .map_err(|error| format!("cannot read binding manifest: {error}"))?;
            if data.len() as u64 > max {
                return Err("binding manifest is too large".into());
            }
            Ok(data)
        }

        /// True only when `openat` on the opened parent reports ENOENT for
        /// this leaf. A missing/invalid parent is decided by `open` first; a
        /// dangling symlink or wrong-owner leaf is not "absent".
        pub fn leaf_absent(&self, name: &str) -> Result<bool, String> {
            if !valid_leaf(name) {
                return Err("binding file name must be one path component".into());
            }
            match open_at(
                self.fd.as_raw_fd(),
                name,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                0,
            ) {
                Ok(_) => Ok(false),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
                Err(error) => Err(format!("cannot open binding manifest: {error}")),
            }
        }

        /// Remove `name` only while it is still the inode this invocation
        /// created. A replacement (or a preexisting file) is left alone.
        pub fn remove_if_matches(&self, name: &str, id: EntryId) -> bool {
            if !valid_leaf(name) {
                return false;
            }
            let Ok(current) = open_at(
                self.fd.as_raw_fd(),
                name,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                0,
            ) else {
                return false;
            };
            let Ok(metadata) = current.metadata() else {
                return false;
            };
            if entry_id(&metadata) != id {
                return false;
            }
            let Ok(name) = CString::new(name) else {
                return false;
            };
            // SAFETY: `dir` is an open directory descriptor and `name` is NUL
            // terminated; unlinkat only removes an entry in that directory.
            unsafe { libc::unlinkat(self.fd.as_raw_fd(), name.as_ptr(), 0) == 0 }
        }
    }

    impl CreatedFile {
        pub fn new(dir: PrivateDir, name: String, id: EntryId) -> Self {
            Self { dir, name, id }
        }

        pub fn remove(&self) {
            let _ = self.dir.remove_if_matches(&self.name, self.id);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};

        fn scratch() -> PathBuf {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "remuda-private-dir-{}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir(&dir).expect("scratch dir");
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
            dir
        }

        #[test]
        fn owner_and_mode_policy_is_explicit() {
            assert!(ancestor_is_acceptable(0, 0o755, 501));
            assert!(ancestor_is_acceptable(501, 0o700, 501));
            assert!(!ancestor_is_acceptable(502, 0o700, 501));
            assert!(!ancestor_is_acceptable(501, 0o777, 501));
            assert!(ancestor_is_acceptable(0, 0o1777, 501));
            assert!(!ancestor_is_acceptable(0, 0o757, 501));
        }

        #[test]
        fn a_world_accessible_directory_is_refused() {
            let dir = scratch();
            let parent = dir.join("shared");
            fs::create_dir(&parent).expect("shared dir");
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).expect("chmod 0755");
            assert!(PrivateDir::open(&parent).is_err());
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
            assert!(PrivateDir::open(&parent).is_ok());
        }

        #[test]
        fn an_ancestor_symlink_is_resolved_and_pinned() {
            let dir = scratch();
            let real = dir.join("real");
            fs::create_dir(&real).expect("real dir");
            fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
            let link = dir.join("link");
            std::os::unix::fs::symlink(&real, &link).expect("symlink");
            let pinned = PrivateDir::open(&link).expect("resolved private ancestor");
            assert_eq!(pinned.path(), real.canonicalize().expect("canonical real"));

            let shared = dir.join("shared");
            fs::create_dir(&shared).expect("shared dir");
            fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).expect("chmod 0755");
            let bad = dir.join("bad-link");
            std::os::unix::fs::symlink(&shared, &bad).expect("symlink");
            assert!(PrivateDir::open(&bad).is_err());
        }

        #[test]
        fn create_never_adopts_a_preexisting_file() {
            let dir = scratch();
            let pinned = PrivateDir::open(&dir).expect("private dir");
            let existing = dir.join("manifest.json");
            fs::write(&existing, b"preexisting").expect("preexisting file");
            fs::set_permissions(&existing, fs::Permissions::from_mode(0o600)).expect("chmod 0600");

            assert!(pinned.create_new("manifest.json").is_err());
            assert_eq!(fs::read(&existing).unwrap(), b"preexisting");

            let (mut file, id) = pinned.create_new("fresh.json").expect("create fresh");
            file.write_all(b"{}").expect("write fresh");
            assert!(pinned.remove_if_matches("fresh.json", id));
            assert!(!dir.join("fresh.json").exists());
        }

        #[test]
        fn a_symlink_leaf_is_refused() {
            let dir = scratch();
            let pinned = PrivateDir::open(&dir).expect("private dir");
            let target = dir.join("target");
            fs::write(&target, b"{}").expect("target");
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
            std::os::unix::fs::symlink(&target, dir.join("leaf")).expect("symlink leaf");
            assert!(pinned.open_existing("leaf").is_err());
        }

        #[test]
        fn replacement_at_the_same_name_is_not_deleted() {
            let dir = scratch();
            let pinned = PrivateDir::open(&dir).expect("private dir");
            let (mut file, id) = pinned.create_new("manifest.json").expect("create");
            file.write_all(b"ours").expect("write");
            drop(file);
            // Keep the original inode allocated, or a filesystem may hand the
            // same inode number to the replacement and make this assertion
            // meaningless (Linux inode reuse).
            fs::hard_link(dir.join("manifest.json"), dir.join("keep")).expect("keep inode alive");
            fs::remove_file(dir.join("manifest.json")).expect("remove ours");
            fs::write(dir.join("manifest.json"), b"theirs").expect("replacement");
            fs::set_permissions(dir.join("manifest.json"), fs::Permissions::from_mode(0o600))
                .expect("chmod 0600");

            assert!(!pinned.remove_if_matches("manifest.json", id));
            assert_eq!(fs::read(dir.join("manifest.json")).unwrap(), b"theirs");
        }

        #[test]
        fn a_fifo_leaf_does_not_block_or_open() {
            let dir = scratch();
            let pinned = PrivateDir::open(&dir).expect("private dir");
            let name = CString::new("fifo").unwrap();
            // SAFETY: creating a FIFO in our own private directory.
            let made = unsafe { libc::mkfifo(cstring(&dir.join("fifo")).unwrap().as_ptr(), 0o600) };
            assert_eq!(made, 0);
            drop(name);
            assert!(pinned.open_existing("fifo").is_err());
            assert!(!pinned.remove_if_matches("fifo", EntryId { dev: 0, ino: 0 }));
        }
    }
}

#[cfg(unix)]
pub use imp::{CreatedFile, PrivateDir};

#[cfg(not(unix))]
mod unsupported {
    use std::fs::File;
    use std::path::{Path, PathBuf};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct EntryId {
        pub dev: u64,
        pub ino: u64,
    }

    #[derive(Clone, Debug)]
    pub struct PrivateDir {
        path: PathBuf,
    }

    #[derive(Clone, Debug)]
    pub struct CreatedFile;

    impl PrivateDir {
        pub fn open(_dir: &Path) -> Result<Self, String> {
            Err("OpenCode native binding requires macOS or Linux".into())
        }

        #[cfg(test)]
        pub fn path(&self) -> &Path {
            &self.path
        }

        pub fn create_new(&self, _name: &str) -> Result<(File, EntryId), String> {
            Err("OpenCode native binding requires macOS or Linux".into())
        }

        pub fn open_existing(&self, _name: &str) -> Result<(File, EntryId), String> {
            Err("OpenCode native binding requires macOS or Linux".into())
        }

        pub fn read_limited(&self, _name: &str, _max: u64) -> Result<Vec<u8>, String> {
            Err("OpenCode native binding requires macOS or Linux".into())
        }

        pub fn leaf_absent(&self, _name: &str) -> Result<bool, String> {
            Err("OpenCode native binding requires macOS or Linux".into())
        }

        pub fn remove_if_matches(&self, _name: &str, _id: EntryId) -> bool {
            false
        }
    }

    impl CreatedFile {
        pub fn new(_dir: PrivateDir, _name: String, _id: EntryId) -> Self {
            Self
        }

        pub fn remove(&self) {}
    }
}

#[cfg(not(unix))]
pub use unsupported::{CreatedFile, PrivateDir};
