//! Windows job objects keep assigned processes together and end them when the
//! session closes. The platform-independent setup decisions are tested on all
//! platforms; the Windows calls are below.

use std::io;

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
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
        JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
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

        pub(crate) fn raw_handle(&self) -> usize {
            self.0 as usize
        }
    }

    impl Drop for Opened {
        fn drop(&mut self) {
            // SAFETY: the handle came from OpenProcess and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// One session's job. Its one limit is kill-on-close: when this handle,
    /// the only one, closes, every process in the job ends. No breakaway is
    /// allowed (neither JOB_OBJECT_LIMIT_BREAKAWAY_OK nor the silent form).
    pub struct SessionJob {
        handle: HANDLE,
        ended: AtomicBool,
    }

    // SAFETY: a job handle names a kernel object; the calls made on it here
    // are safe from any thread, and the handle is closed once, in Drop.
    unsafe impl Send for SessionJob {}
    unsafe impl Sync for SessionJob {}

    impl SessionJob {
        pub fn new() -> io::Result<Self> {
            // SAFETY: null attributes and name ask for a private, unnamed job
            // whose handle no child inherits.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            // From here Drop closes the handle, on the error path too.
            let job = Self {
                handle,
                ended: AtomicBool::new(false),
            };
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `limits` is the structure this information class takes,
            // and the pointer and length are valid for the call.
            let set = unsafe {
                SetInformationJobObject(
                    job.handle,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if set == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        pub(crate) fn assign(&self, process: HANDLE) -> io::Result<()> {
            // SAFETY: both handles are live for the call.
            if unsafe { AssignProcessToJobObject(self.handle, process) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub(crate) fn holds(&self, process: HANDLE) -> io::Result<bool> {
            let mut inside = 0;
            // SAFETY: both handles are live and `inside` is a valid BOOL.
            if unsafe { IsProcessInJob(process, self.handle, &mut inside) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(inside != 0)
        }

        /// Whether `pid` is in this job, preserving query failures.
        pub fn contains(&self, pid: u32) -> io::Result<bool> {
            let process = Opened::query(pid)?;
            self.holds(process.0)
        }

        /// Ends every process in the job. Repeated calls succeed without
        /// sending another termination request.
        pub fn end(&self) -> io::Result<()> {
            if self.ended.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
            // SAFETY: this is the live job handle and zero requests termination.
            if unsafe { TerminateJobObject(self.handle, 0) } == 0 {
                self.ended.store(false, Ordering::Release);
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Drop for SessionJob {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateJobObjectW and is closed once.
            unsafe { CloseHandle(self.handle) };
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
