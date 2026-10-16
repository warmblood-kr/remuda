//! Atomic bounded index replacement, separate never-renamed OS lockfile.
//! Lock deadlines bound acquisition, not kernel filesystem I/O. No callback,
//! host RPC, PTY/parser state, spawn or wait is executed beneath this lock.
use super::directory::Directory;
use super::model::{
    Applied, Error as DataError, Fence, Index, Lease, Mutation, Publication, Revision, MAX_BYTES,
};
use std::fs::{File, TryLockError};
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, Instant};
#[cfg(test)]
thread_local! { static FAULT: std::cell::Cell<u8> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
pub(crate) fn fail_at(stage: u8) {
    FAULT.with(|fault| fault.set(stage));
}
#[cfg(test)]
fn fault(stage: u8) -> Result<()> {
    if FAULT.with(|fault| {
        if fault.get() == stage {
            fault.set(0);
            true
        } else {
            false
        }
    }) {
        Err(Error::CommitUnknown(io::Error::other(
            "synthetic transaction fault",
        )))
    } else {
        Ok(())
    }
}
pub(crate) enum Error {
    Data(DataError),
    Io(io::Error),
    Busy,
    /// Rename/flush may have committed. Reread/recover; no send permission is returned.
    CommitUnknown(io::Error),
}
impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Data(error) => f.debug_tuple("Data").field(error).finish(),
            Self::Io(error) => f.debug_tuple("Io").field(error).finish(),
            Self::Busy => f.write_str("Busy"),
            Self::CommitUnknown(error) => f.debug_tuple("CommitUnknown").field(error).finish(),
        }
    }
}
pub(crate) type Result<T> = std::result::Result<T, Error>;
impl From<DataError> for Error {
    fn from(error: DataError) -> Self {
        Self::Data(error)
    }
}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
pub(crate) struct Store {
    directory: Directory,
}
impl Store {
    /// Only a new private root may initialize an empty index; never repair/reset an old root.
    pub fn create(path: &Path) -> Result<Self> {
        let directory = Directory::create(path)?;
        let store = Self { directory };
        let _lock = store.lock(Duration::ZERO)?;
        let mut incarnation = [0; 16];
        getrandom::fill(&mut incarnation).map_err(|_| Error::Data(DataError::Unavailable))?;
        let bytes = Index::new(incarnation).encode()?;
        store
            .directory
            .replace(&bytes)
            .map_err(Error::CommitUnknown)?;
        Ok(store)
    }
    pub fn open(path: &Path) -> Result<Self> {
        let store = Self {
            directory: Directory::open(path)?,
        };
        // Missing/corrupt/incompatible data is an error, never a fresh empty index.
        let _ = store.read()?;
        Ok(store)
    }
    /// One committed publication cut; host inspections need their own bounded revision protocol.
    pub fn read(&self) -> Result<Index> {
        let file = self.directory.read_index()?;
        if file.metadata()?.len() > MAX_BYTES as u64 {
            return Err(DataError::Capacity.into());
        }
        let mut bytes = Vec::new();
        file.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes)?;
        Ok(Index::decode(&bytes)?)
    }
    pub fn commit(
        &self,
        expected: Revision,
        fence: Option<Fence>,
        mutation: Mutation,
        timeout: Duration,
    ) -> Result<Applied> {
        let _lock = self.lock(timeout)?;
        let applied = self.read()?.apply(expected, fence, mutation)?;
        let bytes = applied.next.encode()?;
        #[cfg(test)]
        fault(1)?;
        self.directory
            .replace(&bytes)
            .map_err(Error::CommitUnknown)?;
        #[cfg(test)]
        fault(2)?;
        Ok(applied)
    }
    /// Scoped host-only publication; deliberately returns no global writer Fence.
    pub fn publish(
        &self,
        expected: Revision,
        lease: &Lease,
        publication: Publication,
        timeout: Duration,
    ) -> Result<Revision> {
        let _lock = self.lock(timeout)?;
        let index = self.read()?;
        let next = index.publish(expected, lease, publication)?;
        if next.cursor() == index.cursor() {
            return Ok(next.cursor());
        }
        let bytes = next.encode()?;
        #[cfg(test)]
        fault(1)?;
        self.directory
            .replace(&bytes)
            .map_err(Error::CommitUnknown)?;
        #[cfg(test)]
        fault(2)?;
        Ok(next.cursor())
    }
    fn lock(&self, timeout: Duration) -> Result<Lock> {
        let file = self.directory.lock_file()?;
        let until = Instant::now() + timeout.min(Duration::from_secs(2));
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Lock(file)),
                Err(TryLockError::Error(error)) => return Err(error.into()),
                Err(TryLockError::WouldBlock) if Instant::now() >= until => {
                    return Err(Error::Busy)
                }
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(2)),
            }
        }
    }
}
struct Lock(File);
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
