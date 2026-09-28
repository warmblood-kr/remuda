//! Process identities for the daemon's local shutdown guard.
//!
//! A shutdown request may come from a shell or tool several processes below a
//! session's PTY child. The kernel supplies the socket peer PID; this module
//! follows parent PIDs until it reaches one of the daemon's session children.
//! Failure to identify the peer or read a complete ancestry chain is treated
//! as unknown and callers must refuse shutdown unless they explicitly override.

#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
use interprocess::local_socket::traits::StreamCommon as _;
use std::io;

pub(crate) fn peer_pid(stream: &crate::ipc::Stream) -> io::Result<Option<u32>> {
    #[cfg(target_os = "macos")]
    {
        use interprocess::local_socket::Stream;
        use std::os::fd::AsRawFd;

        let Stream::UdSocket(socket) = stream;
        let mut pid: libc::pid_t = 0;
        let mut length = std::mem::size_of_val(&pid) as libc::socklen_t;
        // LOCAL_PEERPID is the Darwin socket credential equivalent of Linux's
        // SO_PEERCRED. interprocess's xucred wrapper exposes uid/gid, not PID.
        let result = unsafe {
            libc::getsockopt(
                socket.inner().as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut length,
            )
        };
        if result != 0 {
            return Err(io::Error::other(format!(
                "LOCAL_PEERPID failed: {}",
                io::Error::last_os_error()
            )));
        }
        u32::try_from(pid)
            .map(Some)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid peer PID"))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Ok(stream
            .peer_creds()?
            .pid()
            .and_then(|pid| u32::try_from(pid).ok()))
    }

    #[cfg(windows)]
    {
        Ok(stream.peer_creds()?.pid())
    }
}

pub(crate) fn is_self_or_descendant(mut pid: u32, ancestors: &[u32]) -> io::Result<bool> {
    let daemon_pid = std::process::id();
    let daemon_parent = own_parent_pid()?;
    let mut visited = std::collections::HashSet::new();
    loop {
        if ancestors.contains(&pid) {
            return Ok(true);
        }
        // A session process must be encountered before its ancestry reaches
        // the daemon. Reaching the daemon or its parent proves this caller is
        // outside every session owned by this daemon, and bounds the walk at
        // the process tree we can reliably inspect.
        if pid == daemon_pid || Some(pid) == daemon_parent {
            return Ok(false);
        }
        if pid <= 1 {
            return Ok(false);
        }
        if !visited.insert(pid) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cycle in process parent chain",
            ));
        }
        let Some(parent) = parent_pid(pid)? else {
            return Ok(false);
        };
        if parent == pid {
            return Ok(false);
        }
        pid = parent;
    }
}

fn own_parent_pid() -> io::Result<Option<u32>> {
    #[cfg(unix)]
    {
        let pid = unsafe { libc::getppid() };
        u32::try_from(pid)
            .map(Some)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid daemon parent PID"))
    }
    #[cfg(windows)]
    {
        parent_pid(std::process::id())
    }
}

#[cfg(target_os = "linux")]
fn parent_pid(pid: u32) -> io::Result<Option<u32>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let close = stat.rfind(')').ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "malformed /proc process stat")
    })?;
    let parent = stat[close + 1..]
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process parent PID"))?;
    parent
        .parse::<u32>()
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(target_os = "macos")]
fn parent_pid(pid: u32) -> io::Result<Option<u32>> {
    let pid = i32::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "process PID out of range"))?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let result = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size as i32,
        )
    };
    if result != size as i32 {
        return Err(io::Error::other(format!(
            "proc_pidinfo for {pid} failed: {}",
            io::Error::last_os_error()
        )));
    }
    let info = unsafe { info.assume_init() };
    Ok(Some(info.pbi_ppid))
}

#[cfg(windows)]
fn parent_pid(pid: u32) -> io::Result<Option<u32>> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    struct Snapshot(HANDLE);
    impl Drop for Snapshot {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if handle == -1isize as HANDLE {
        return Err(io::Error::last_os_error());
    }
    let _snapshot = Snapshot(handle);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(handle, &mut entry) } == 0 {
        return Err(io::Error::last_os_error());
    }
    loop {
        if entry.th32ProcessID == pid {
            return Ok(Some(entry.th32ParentProcessID));
        }
        if unsafe { Process32NextW(handle, &mut entry) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("process {pid} is not in the process snapshot"),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_its_own_ancestor() {
        assert!(is_self_or_descendant(std::process::id(), &[std::process::id()]).unwrap());
    }

    #[test]
    fn unrelated_root_pid_is_not_a_descendant() {
        assert!(!is_self_or_descendant(1, &[u32::MAX]).unwrap());
    }
}
