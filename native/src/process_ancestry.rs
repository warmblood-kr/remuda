//! Process identities for the daemon's local shutdown guard.
//!
//! A shutdown request may come from a shell or tool several processes below a
//! session's PTY child. The kernel supplies the socket peer PID; this module
//! follows parent PIDs until it reaches one of the daemon's session children.
//! A caller is hosted only when its ancestry reaches one of the daemon's
//! session children before the chain leaves processes we can inspect.

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

#[derive(Debug)]
pub(crate) enum Ancestry {
    Inside,
    Outside,
    Unreadable { pid: u32, error: io::Error },
}

pub(crate) fn missing_peer_requires_refusal(self_reported_identity: bool) -> bool {
    self_reported_identity
}

pub(crate) fn is_self_or_descendant(pid: u32, ancestors: &[u32]) -> Ancestry {
    walk_ancestry(pid, ancestors, std::process::id(), parent_pid)
}

fn walk_ancestry<F>(mut pid: u32, ancestors: &[u32], daemon_pid: u32, mut parent: F) -> Ancestry
where
    F: FnMut(u32) -> io::Result<Option<u32>>,
{
    let mut visited = std::collections::HashSet::new();
    loop {
        if ancestors.contains(&pid) {
            return Ancestry::Inside;
        }
        // Session children belong to this daemon. Once the walk reaches the
        // daemon, init, or an unreadable process, it has left that tree.
        if pid == daemon_pid || pid <= 1 {
            return Ancestry::Outside;
        }
        if !visited.insert(pid) {
            return Ancestry::Unreadable {
                pid,
                error: io::Error::new(io::ErrorKind::InvalidData, "cycle in process parent chain"),
            };
        }
        let next = match parent(pid) {
            Ok(next) => next,
            Err(error) => return Ancestry::Unreadable { pid, error },
        };
        let Some(next) = next else {
            return Ancestry::Outside;
        };
        if next == pid {
            return Ancestry::Outside;
        }
        pid = next;
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
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
        assert!(matches!(
            is_self_or_descendant(std::process::id(), &[std::process::id()]),
            Ancestry::Inside
        ));
    }

    #[test]
    fn unrelated_root_pid_is_not_a_descendant() {
        assert!(matches!(
            is_self_or_descendant(1, &[u32::MAX]),
            Ancestry::Outside
        ));
    }

    #[test]
    fn unreadable_ancestor_is_outside_but_session_hit_before_it_is_inside() {
        let parent = |pid| match pid {
            40 => Ok(Some(30)),
            30 => Err(io::Error::new(io::ErrorKind::PermissionDenied, "hidden")),
            _ => unreachable!(),
        };
        assert!(matches!(
            walk_ancestry(40, &[], 1, parent),
            Ancestry::Unreadable { pid: 30, .. }
        ));

        let parent = |pid| match pid {
            40 => Ok(Some(25)),
            25 => Err(io::Error::new(io::ErrorKind::PermissionDenied, "hidden")),
            _ => unreachable!(),
        };
        assert!(matches!(
            walk_ancestry(40, &[25], 1, parent),
            Ancestry::Inside
        ));
    }

    #[test]
    fn missing_peer_pid_refuses_only_with_self_reported_identity() {
        assert!(missing_peer_requires_refusal(true));
        assert!(!missing_peer_requires_refusal(false));
    }
}
