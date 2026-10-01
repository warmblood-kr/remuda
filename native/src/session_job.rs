//! A session's processes on Windows, held together by a job object. A parent
//! PID stops naming a session once the parent has exited; a job does not: a
//! process a member starts is in the job, stays in it when its parent exits,
//! and cannot leave it (the job sets no breakaway limit). The decisions are
//! plain functions, tested on every platform; the Windows calls are below them.

use std::io;

/// One row of the system's process list. Times are Windows file times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listed {
    pub pid: u32,
    pub parent: u32,
    pub created: u64,
}

/// The one line for a session that could not be put in its job. Such a
/// session is not started: without the job its orphans could not be told
/// from processes outside every session.
pub fn refusal(step: &str, error: &io::Error) -> String {
    format!(
        "{REFUSED} its job object could not be {step} ({error}). \
         Next: start remuda from an ordinary terminal, not from inside a restricted job."
    )
}

const REFUSED: &str = "the session was not started:";

/// What a failed spawn says to the caller. The job refusal is a whole line
/// of its own, so it is shown without the agent error's prefix.
pub fn spawn_error_line(error: remuda_core::AgentError) -> String {
    match error {
        remuda_core::AgentError::Io(line) if line.starts_with(REFUSED) => line,
        other => other.to_string(),
    }
}

/// Fail closed: the job, with the child assigned to it, or the child killed
/// and the refusal line.
pub fn set_up<J>(
    job: io::Result<J>,
    assign: impl FnOnce(&J) -> io::Result<()>,
    kill: impl FnOnce(),
) -> Result<J, String> {
    let attempt = job
        .map_err(|error| refusal("created", &error))
        .and_then(|job| match assign(&job) {
            Ok(()) => Ok(job),
            Err(error) => Err(refusal("assigned", &error)),
        });
    if attempt.is_err() {
        kill();
    }
    attempt
}

/// The processes `root` may have started before it was in the job: its
/// children that are not older than it, and theirs. A listed child that is
/// older than its parent has a reused parent PID and is someone else's.
pub fn early_descendants(list: &[Listed], root: u32, root_created: u64) -> Vec<u32> {
    let mut found = Vec::new();
    let mut parents = vec![(root, root_created)];
    while let Some((parent, parent_created)) = parents.pop() {
        for row in list {
            let is_child = row.parent == parent && row.pid != parent;
            if is_child && row.created >= parent_created && !found.contains(&row.pid) {
                found.push(row.pid);
                parents.push((row.pid, row.created));
            }
        }
    }
    found
}

/// Whether the process the daemon opened is the client that connected: it
/// existed before the connection was accepted (else its PID was reused since)
/// and the pipe still names the same PID.
pub fn peer_is_current(created: u64, accepted: u64, pid_before: u32, pid_after: u32) -> bool {
    created < accepted && pid_before == pid_after
}

/// `at` as a Windows file time: 100 ns ticks since 1601, the unit of a
/// process's creation time.
pub fn file_time(at: std::time::SystemTime) -> u64 {
    const UNIX_EPOCH_AS_FILE_TIME: u64 = 116_444_736_000_000_000;
    let since = at.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    UNIX_EPOCH_AS_FILE_TIME + (since.as_nanos() / 100) as u64
}

#[cfg(windows)]
pub(crate) use self::windows::Opened;
#[cfg(windows)]
pub use self::windows::SessionJob;

#[cfg(windows)]
mod windows {
    use super::{early_descendants, Listed};
    use std::io;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
        PROCESS_TERMINATE,
    };

    /// When the process behind `process` was created, as a file time.
    pub(crate) fn created(process: HANDLE) -> io::Result<u64> {
        let mut times = [FILETIME::default(); 4];
        let [created, exited, kernel, user] = &mut times;
        // SAFETY: the handle is live for the call and the four out-pointers
        // are distinct, valid FILETIMEs.
        if unsafe { GetProcessTimes(process, created, exited, kernel, user) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime))
    }

    /// A process opened by PID. While it is open its PID cannot be reused.
    pub(crate) struct Opened(HANDLE);

    impl Opened {
        pub(crate) fn open(pid: u32, access: u32) -> io::Result<Self> {
            // SAFETY: no pointer is passed; a null result is the error.
            let handle = unsafe { OpenProcess(access, 0, pid) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(handle))
        }

        pub(crate) fn query(pid: u32) -> io::Result<Self> {
            Self::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)
        }

        pub(crate) fn created(&self) -> io::Result<u64> {
            created(self.0)
        }
    }

    impl Drop for Opened {
        fn drop(&mut self) {
            // SAFETY: the handle came from OpenProcess and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// One session's job. It sets NO limit: no breakaway is allowed (neither
    /// JOB_OBJECT_LIMIT_BREAKAWAY_OK nor the silent form), and nothing is
    /// killed when the handle closes.
    pub struct SessionJob(HANDLE);

    // SAFETY: a job handle names a kernel object; the calls made on it here
    // are safe from any thread, and the handle is closed once, in Drop.
    unsafe impl Send for SessionJob {}
    unsafe impl Sync for SessionJob {}

    impl SessionJob {
        pub fn new() -> io::Result<Self> {
            // SAFETY: null attributes and name ask for a private, unnamed job.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(handle))
        }

        pub(crate) fn assign(&self, process: HANDLE) -> io::Result<()> {
            // SAFETY: both handles are live for the call.
            if unsafe { AssignProcessToJobObject(self.0, process) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        fn holds(&self, process: HANDLE) -> io::Result<bool> {
            let mut inside = 0;
            // SAFETY: both handles are live and `inside` is a valid BOOL.
            if unsafe { IsProcessInJob(process, self.0, &mut inside) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(inside != 0)
        }

        /// Whether `pid` is in this job. A process that cannot be opened or
        /// asked about is not counted as ours.
        pub fn contains(&self, pid: u32) -> bool {
            Opened::query(pid).is_ok_and(|process| self.holds(process.0).unwrap_or(false))
        }

        /// Puts what `child` started before it was assigned into the job too.
        /// One level is certain; a deeper process whose parent has already
        /// exited is not found (a suspended start would close that).
        pub(crate) fn sweep(&self, child_pid: u32, child: HANDLE) -> io::Result<()> {
            let root_created = created(child)?;
            let parents = crate::process_ancestry::process_parents()?;
            // A process that cannot be opened is another user's, not a child
            // this session started a moment ago.
            let list: Vec<Listed> = parents
                .iter()
                .filter_map(|(&pid, &parent)| {
                    let created = Opened::query(pid).ok()?.created().ok()?;
                    Some(Listed {
                        pid,
                        parent,
                        created,
                    })
                })
                .collect();
            let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_QUOTA | PROCESS_TERMINATE;
            for pid in early_descendants(&list, child_pid, root_created) {
                // Gone already, or its PID now names another process: skip.
                let Ok(process) = Opened::open(pid, access) else {
                    continue;
                };
                let listed = list
                    .iter()
                    .find(|row| row.pid == pid)
                    .map(|row| row.created);
                if process.created().ok() != listed || self.holds(process.0)? {
                    continue;
                }
                self.assign(process.0)?;
            }
            Ok(())
        }
    }

    impl Drop for SessionJob {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateJobObjectW and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn denied() -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, "access denied")
    }

    #[test]
    fn a_session_whose_job_cannot_be_assigned_is_killed_and_refused() {
        let killed = Cell::new(false);
        let refused = set_up(Ok(7), |_| Err(denied()), || killed.set(true)).unwrap_err();
        assert!(killed.get(), "the child must not run without its job");
        assert_eq!(
            refused,
            "the session was not started: its job object could not be assigned (access denied). \
             Next: start remuda from an ordinary terminal, not from inside a restricted job."
        );
        assert!(!refused.contains('\n'));
    }

    #[test]
    fn the_job_refusal_is_shown_without_the_agent_error_prefix() {
        use remuda_core::AgentError;
        let line = refusal("assigned", &denied());
        assert_eq!(spawn_error_line(AgentError::Io(line.clone())), line);
        assert!(line.starts_with("the session was not started: its job object"));
        // Every other spawn failure reads as before.
        assert_eq!(
            spawn_error_line(AgentError::Io("no such program".into())),
            "agent io error: no such program"
        );
        assert_eq!(
            spawn_error_line(AgentError::Exited),
            AgentError::Exited.to_string()
        );
    }

    #[test]
    fn a_job_that_cannot_be_created_refuses_before_any_assignment() {
        let killed = Cell::new(false);
        let assigned = Cell::new(false);
        let refused = set_up::<u8>(
            Err(denied()),
            |_| {
                assigned.set(true);
                Ok(())
            },
            || killed.set(true),
        )
        .unwrap_err();
        assert!(killed.get() && !assigned.get());
        assert!(refused.contains("could not be created"), "{refused}");
    }

    #[test]
    fn a_job_that_is_assigned_is_kept_and_nothing_is_killed() {
        let killed = Cell::new(false);
        assert_eq!(set_up(Ok(7), |_| Ok(()), || killed.set(true)), Ok(7));
        assert!(!killed.get());
    }

    #[test]
    fn early_descendants_are_the_younger_children_and_theirs() {
        let row = |pid, parent, created| Listed {
            pid,
            parent,
            created,
        };
        let list = [
            row(10, 1, 100),  // the session child itself
            row(11, 10, 105), // its child
            row(12, 11, 110), // its grandchild
            row(13, 10, 100), // a child from the same clock tick
            row(20, 10, 50),  // older than its "parent": a reused PID
            row(21, 20, 120), // so its children are not ours either
            row(30, 2, 130),  // unrelated
        ];
        let mut found = early_descendants(&list, 10, 100);
        found.sort_unstable();
        assert_eq!(found, [11, 12, 13]);
        assert!(early_descendants(&list, 99, 0).is_empty());
    }

    #[test]
    fn early_descendants_end_on_a_parent_cycle() {
        let list = [
            Listed {
                pid: 10,
                parent: 11,
                created: 100,
            },
            Listed {
                pid: 11,
                parent: 10,
                created: 100,
            },
        ];
        assert_eq!(early_descendants(&list, 10, 100), [11, 10]);
    }

    #[test]
    fn a_file_time_counts_100_ns_ticks_from_1601() {
        let epoch = std::time::UNIX_EPOCH;
        assert_eq!(file_time(epoch), 116_444_736_000_000_000);
        let later = epoch + std::time::Duration::from_secs(1);
        assert_eq!(file_time(later), 116_444_736_010_000_000);
    }

    #[test]
    fn a_peer_is_current_only_if_it_is_older_than_the_connection() {
        for (created, accepted, before, after, current) in [
            (100, 200, 40, 40, true),
            // Created after the accept: the PID was reused since.
            (300, 200, 40, 40, false),
            // The same tick is not "earlier".
            (200, 200, 40, 40, false),
            // The pipe names another PID now.
            (100, 200, 40, 41, false),
        ] {
            let got = peer_is_current(created, accepted, before, after);
            assert_eq!(got, current, "{created} {accepted} {before} {after}");
        }
    }
}
