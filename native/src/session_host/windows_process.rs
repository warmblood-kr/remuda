//! Experimental creation-time containment and checked direct-child wait.
//! The caller owns the Job and ConPTY drain; this is not a Windows host yet.
use portable_pty::{Child, CommandBuilder, SlavePty};
use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::time::Duration;
use windows_sys::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::JobObjects::IsProcessInJob;
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessId, TerminateProcess, WaitForSingleObject,
};

/// Retains the real process HANDLE inside its sole upstream owner.
/// Never calls upstream kill, wait, try_wait or Future implementations.
pub struct Process {
    child: Box<dyn Child + Send + Sync>,
}
impl Process {
    /// The caller must retain its Job, and drop/terminate it on any error.
    /// Unsupported PTYs/outer Job configurations refuse before uncontained spawn.
    pub fn spawn(
        slave: &dyn SlavePty,
        command: CommandBuilder,
        job: BorrowedHandle<'_>,
    ) -> io::Result<Self> {
        let child = slave
            .spawn_command_in_job(command, job)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let process = Self { child };
        let mut member = 0;
        // SAFETY: both handles are live for the call; member is writable.
        let checked =
            unsafe { IsProcessInJob(process.handle()?, job.as_raw_handle(), &mut member) };
        if checked == 0 || member == 0 {
            let error = if checked == 0 {
                io::Error::last_os_error()
            } else {
                io::Error::other("created child is outside the requested Job")
            };
            let cleanup = process.terminate_direct(Duration::from_secs(2));
            return Err(io::Error::other(format!(
                "Job membership postcondition failed: {error}; direct-child cleanup: {cleanup:?}"
            )));
        }
        Ok(process)
    }
    fn handle(&self) -> io::Result<HANDLE> {
        self.child
            .as_raw_handle()
            .filter(|handle| !handle.is_null())
            .ok_or_else(|| io::Error::other("contained backend did not retain a process handle"))
    }
    pub fn pid(&self) -> io::Result<u32> {
        // SAFETY: the process handle remains owned by self.child.
        let pid = unsafe { GetProcessId(self.handle()?) };
        if pid == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(pid)
        }
    }
    /// None means the retained process handle has not signaled by this deadline.
    /// A signaled process's exit259 is final, not STILL_ACTIVE.
    pub fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<u32>> {
        observe(&Native, self.handle()?, timeout)
    }
    /// Confirm direct-child termination; descendants/Job and ConPTY are separate.
    /// A termination request alone never reports an observed exit.
    pub fn terminate_direct(&self, timeout: Duration) -> io::Result<Option<u32>> {
        terminate(&Native, self.handle()?, timeout)
    }
}

trait Api {
    fn wait(&self, process: HANDLE, timeout: Duration) -> io::Result<bool>;
    fn code(&self, process: HANDLE) -> io::Result<u32>;
    fn terminate(&self, process: HANDLE) -> io::Result<()>;
}
fn observe(api: &impl Api, process: HANDLE, timeout: Duration) -> io::Result<Option<u32>> {
    if api.wait(process, timeout)? {
        api.code(process).map(Some)
    } else {
        Ok(None)
    }
}
fn terminate(api: &impl Api, process: HANDLE, timeout: Duration) -> io::Result<Option<u32>> {
    if let Err(error) = api.terminate(process) {
        // Natural exit can race TerminateProcess. Only a checked signaled
        // wait plus exit query can turn this race into successful completion.
        return match observe(api, process, Duration::ZERO) {
            Ok(Some(code)) => Ok(Some(code)),
            _ => Err(error),
        };
    }
    observe(api, process, timeout)
}
struct Native;
impl Api for Native {
    fn wait(&self, process: HANDLE, timeout: Duration) -> io::Result<bool> {
        let millis = timeout
            .as_millis()
            .saturating_add(u128::from(
                !timeout.subsec_nanos().is_multiple_of(1_000_000),
            ))
            .min(u128::from(u32::MAX - 1)) as u32;
        // SAFETY: caller retains the real process handle during this call.
        match unsafe { WaitForSingleObject(process, millis) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            other => Err(io::Error::other(format!(
                "unexpected process wait result {other}"
            ))),
        }
    }
    fn code(&self, process: HANDLE) -> io::Result<u32> {
        let mut code = 0;
        // SAFETY: process is live and code is writable; called after signaled wait.
        if unsafe { GetExitCodeProcess(process, &mut code) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(code)
        }
    }
    fn terminate(&self, process: HANDLE) -> io::Result<()> {
        // SAFETY: caller retains the process handle; success initiates exit only.
        if unsafe { TerminateProcess(process, 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
#[path = "windows_process_tests.rs"]
mod tests;
