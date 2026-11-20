//! Host lifetime lock: the kernel releasing it proves that the host process
//! exited. It proves nothing about a detached descendant, and a missing lock is
//! unknown, never death.
use super::api::Identity;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) const FILE: &str = "host.lock";

/// Recorded inside the locked file, so an acquired lock can be checked against
/// the reservation it belongs to before anything is retired.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    schema: u16,
    identity: Identity,
    nonce: [u8; 16],
    device: u64,
    inode: u64,
}

impl Binding {
    pub fn confirms(&self, identity: &Identity, nonce: [u8; 16]) -> bool {
        self.schema == 1 && &self.identity == identity && self.nonce == nonce
    }
}

/// A held lock. Dropping it releases the lock, which must happen only at the
/// end of the host process.
pub(crate) struct Held {
    file: File,
    binding: Binding,
}

impl Held {
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// The host's own lock, held from before publication until the process exits.
pub(crate) struct Lifetime {
    held: Option<Held>,
    path: PathBuf,
}

impl Lifetime {
    pub fn binding(&self) -> Option<&Binding> {
        self.held.as_ref().map(Held::binding)
    }

    /// True only while the path still names the file this process locked.
    #[cfg(unix)]
    pub fn path_is_bound(&self) -> bool {
        self.held.as_ref().is_some_and(|held| {
            std::fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
                use std::os::unix::fs::MetadataExt;
                metadata.dev() == held.binding.device && metadata.ino() == held.binding.inode
            })
        })
    }

    #[cfg(not(unix))]
    pub fn path_is_bound(&self) -> bool {
        false
    }
}

/// What a probe of another host's lock proves.
pub(crate) enum Probe {
    /// Someone holds the lock: the host may be alive.
    Held,
    /// The lock was acquired, so its holder is gone, and the recorded binding
    /// matches the expected reservation. The caller keeps it while it fences.
    Released(Held),
    /// Missing, unreadable, malformed, replaced or unsupported: never death.
    Unknown,
}

#[cfg(unix)]
mod imp {
    use super::{Binding, Held, Identity, Lifetime, Probe, FILE};
    use std::fs::{File, OpenOptions, TryLockError};
    use std::io::{self, Read, Write};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Path;

    fn invalid(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
    }

    struct FileIdentity {
        device: u64,
        inode: u64,
    }

    fn private(file: &File) -> io::Result<FileIdentity> {
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.mode() & 0o077 != 0 {
            return Err(invalid("host lifetime lock is not a private file"));
        }
        Ok(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn same_file(path: &Path, device: u64, inode: u64) -> bool {
        std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.dev() == device && metadata.ino() == inode)
    }

    /// Take the lifetime lock, then record the reservation it belongs to. A
    /// directory created for a new host must not already hold one.
    pub(super) fn acquire(
        directory: &Path,
        identity: &Identity,
        nonce: [u8; 16],
    ) -> io::Result<Lifetime> {
        if !directory.is_absolute() {
            return Err(invalid("absolute host directory required"));
        }
        let path = directory.join(FILE);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "host lifetime lock already held",
                ))
            }
            Err(TryLockError::Error(error)) => return Err(error),
        }
        let metadata = private(&file)?;
        let binding = Binding {
            schema: 1,
            identity: identity.clone(),
            nonce,
            device: metadata.device,
            inode: metadata.inode,
        };
        let bytes = serde_json::to_vec(&binding).map_err(io::Error::other)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        if !same_file(&path, binding.device, binding.inode) {
            return Err(invalid("host lifetime lock replaced while acquiring"));
        }
        Ok(Lifetime {
            held: Some(Held { file, binding }),
            path,
        })
    }

    /// Try to take another host's lock without ever removing it. Dropping the
    /// returned `Held` releases the acquired lock again.
    pub(super) fn probe(directory: &Path, identity: &Identity, nonce: [u8; 16]) -> Probe {
        if !directory.is_absolute() {
            return Probe::Unknown;
        }
        let path = directory.join(FILE);
        let mut file = match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => file,
            Err(_) => return Probe::Unknown,
        };
        let metadata = match private(&file) {
            Ok(metadata) => metadata,
            Err(_) => return Probe::Unknown,
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Probe::Held,
            Err(TryLockError::Error(_)) => return Probe::Unknown,
        }
        let held = read(&mut file)
            .filter(|binding| {
                binding.schema == 1
                    && binding.device == metadata.device
                    && binding.inode == metadata.inode
                    && binding.confirms(identity, nonce)
            })
            .map(|binding| Held { file, binding });
        match held {
            Some(held) => Probe::Released(held),
            None => Probe::Unknown,
        }
    }

    fn read(file: &mut File) -> Option<Binding> {
        let mut bytes = Vec::new();
        (&mut *file).take(4096).read_to_end(&mut bytes).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{Identity, Lifetime, Probe};
    use std::io;
    use std::path::{Path, PathBuf};

    // No platform primitive we can trust: record nothing and retire never.
    pub(super) fn acquire(_: &Path, _: &Identity, _: [u8; 16]) -> io::Result<Lifetime> {
        Ok(Lifetime {
            held: None,
            path: PathBuf::new(),
        })
    }

    pub(super) fn probe(_: &Path, _: &Identity, _: [u8; 16]) -> Probe {
        Probe::Unknown
    }
}

/// Take the lock and record `identity`/`nonce`. Called before the entry may be
/// published as `Registered`, so a registered entry implies this lock was held.
pub(crate) fn acquire(
    directory: &Path,
    identity: &Identity,
    nonce: [u8; 16],
) -> io::Result<Lifetime> {
    imp::acquire(directory, identity, nonce)
}

/// Probe a host's lifetime lock. Only `Released` with a matching binding may
/// ever be used as evidence that the host process exited.
pub(crate) fn probe(directory: &Path, identity: &Identity, nonce: [u8; 16]) -> Probe {
    imp::probe(directory, identity, nonce)
}

#[cfg(test)]
#[path = "lifetime_tests.rs"]
mod tests;
