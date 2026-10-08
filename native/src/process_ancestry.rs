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

/// Windows: the client, opened, if it is still the process that connected.
/// The pipe gives a PID, which can be reused once the client exits; the open
/// handle keeps it from being reused while the caller holds it.
#[cfg(windows)]
pub(crate) fn current_peer(
    stream: &crate::ipc::Stream,
    accepted: std::time::SystemTime,
) -> Option<(u32, crate::session_job::Opened)> {
    use crate::session_job::{file_time, peer_is_current, Opened};
    let pid = peer_pid(stream).ok().flatten().filter(|pid| *pid > 1)?;
    let process = Opened::query(pid).ok()?;
    let created = process.created().ok()?;
    let again = peer_pid(stream).ok().flatten()?;
    peer_is_current(created, file_time(accepted), pid, again).then_some((pid, process))
}

#[derive(Debug)]
pub(crate) enum Ancestry {
    Inside,
    Outside,
    Unreadable { pid: u32, error: io::Error },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CallerOrigin {
    Session(String),
    Outside,
    Unknown,
}

#[cfg(any(windows, test))]
pub(crate) fn resolve_job_first<J, A>(job: J, ancestry: A) -> CallerOrigin
where
    J: FnOnce() -> io::Result<Option<String>>,
    A: FnOnce() -> CallerOrigin,
{
    match job() {
        Ok(Some(name)) => CallerOrigin::Session(name),
        Ok(None) => ancestry(),
        Err(_) => CallerOrigin::Unknown,
    }
}

#[cfg(any(not(windows), test))]
pub(crate) fn resolve_caller(
    peer_pid: io::Result<Option<u32>>,
    sessions: &[(String, u32)],
) -> CallerOrigin {
    #[cfg(windows)]
    {
        return match peer_pid {
            Ok(Some(pid)) if pid > 1 => resolve_caller_windows(pid, sessions, None),
            Ok(Some(_)) | Ok(None) | Err(_) => CallerOrigin::Unknown,
        };
    }
    #[cfg(not(windows))]
    match peer_pid {
        Ok(Some(pid)) if pid > 1 => {
            resolve_caller_with(pid, sessions, std::process::id(), parent_pid)
        }
        Ok(Some(_)) | Ok(None) | Err(_) => CallerOrigin::Unknown,
    }
}

#[cfg(windows)]
pub(crate) fn resolve_caller_opened(
    pid: u32,
    peer: &crate::session_job::Opened,
    sessions: &[(String, u32)],
) -> CallerOrigin {
    resolve_caller_windows(pid, sessions, Some(peer))
}

#[cfg(windows)]
fn resolve_caller_windows(
    pid: u32,
    sessions: &[(String, u32)],
    peer: Option<&crate::session_job::Opened>,
) -> CallerOrigin {
    use crate::session_job::Opened;
    let parents = match process_parents() {
        Ok(parents) => parents,
        Err(_) => return CallerOrigin::Unknown,
    };
    chain_end(pid, sessions, |process_pid| {
        let Some(&parent_pid) = parents.get(&process_pid) else {
            return Ok(None);
        };
        let opened;
        let process = if process_pid == pid {
            match peer {
                Some(peer) => peer,
                None => {
                    opened = match Opened::query(process_pid) {
                        Ok(opened) => opened,
                        Err(error) if process_missing(&error) => {
                            return Ok(None);
                        }
                        Err(error) => return Err(error),
                    };
                    &opened
                }
            }
        } else {
            opened = match Opened::query(process_pid) {
                Ok(opened) => opened,
                Err(error) if process_missing(&error) => {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            &opened
        };
        process.created().map(|created| Some((parent_pid, created)))
    })
}

#[cfg(any(windows, test))]
fn process_missing(error: &io::Error) -> bool {
    const ERROR_INVALID_PARAMETER: i32 = 87;
    error.raw_os_error() == Some(ERROR_INVALID_PARAMETER)
}

#[cfg(any(windows, test))]
fn chain_end<F>(mut pid: u32, sessions: &[(String, u32)], mut process: F) -> CallerOrigin
where
    F: FnMut(u32) -> io::Result<Option<(u32, u64)>>,
{
    const MAX_HOPS: usize = 64;
    let mut visited = std::collections::HashSet::new();
    let mut child_created = None;
    let mut hops = 0;
    loop {
        let session = sessions.iter().find(|(_, session_pid)| *session_pid == pid);
        if child_created.is_none() {
            if let Some((name, _)) = session {
                return CallerOrigin::Session(name.clone());
            }
        }
        if pid <= 1 {
            return CallerOrigin::Outside;
        }
        if !visited.insert(pid) {
            return CallerOrigin::Unknown;
        }
        let (parent_pid, created) = match process(pid) {
            Ok(Some(info)) => info,
            Ok(None) if hops > 0 => return CallerOrigin::Outside,
            Ok(None) | Err(_) => return CallerOrigin::Unknown,
        };
        if child_created.is_some_and(|child| created > child) {
            return CallerOrigin::Outside;
        }
        if let Some((name, _)) = session {
            return CallerOrigin::Session(name.clone());
        }
        if parent_pid <= 1 {
            return CallerOrigin::Outside;
        }
        if hops == MAX_HOPS {
            return CallerOrigin::Unknown;
        }
        child_created = Some(created);
        pid = parent_pid;
        hops += 1;
    }
}

#[cfg(any(not(windows), test))]
fn resolve_caller_with<F>(
    mut pid: u32,
    sessions: &[(String, u32)],
    daemon_pid: u32,
    mut parent: F,
) -> CallerOrigin
where
    F: FnMut(u32) -> io::Result<Option<u32>>,
{
    let mut visited = std::collections::HashSet::new();
    loop {
        // The first matching session while walking upward is the innermost
        // one, even when registry iteration order is different.
        if let Some((name, _)) = sessions.iter().find(|(_, session_pid)| *session_pid == pid) {
            return CallerOrigin::Session(name.clone());
        }
        // A process reparented to launchd/init has left every session tree.
        // This says nothing about whether its owner is an operator.
        if pid == daemon_pid || pid <= 1 {
            return CallerOrigin::Outside;
        }
        if !visited.insert(pid) {
            return CallerOrigin::Unknown;
        }
        match parent(pid) {
            Ok(Some(next)) if next != pid => pid = next,
            Ok(Some(_)) | Ok(None) | Err(_) => return CallerOrigin::Unknown,
        }
    }
}

pub(crate) fn missing_peer_requires_refusal(self_reported_identity: bool) -> bool {
    self_reported_identity
}

pub(crate) fn is_self_or_descendant(pid: u32, ancestors: &[u32]) -> Ancestry {
    #[cfg(windows)]
    {
        return match process_parents() {
            Ok(parents) => walk_ancestry(pid, ancestors, std::process::id(), |pid| {
                parents.get(&pid).copied().map(Some).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("process {pid} is not in the process snapshot"),
                    )
                })
            }),
            Err(error) => Ancestry::Unreadable { pid, error },
        };
    }
    #[cfg(not(windows))]
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
    // The short form also answers for another user's process (the root-owned
    // /usr/bin/login above every terminal shell); PROC_PIDTBSDINFO gets EPERM.
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>();
    let result = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
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
    Ok(Some(info.pbsi_ppid))
}

#[cfg(windows)]
pub(crate) fn process_parents() -> io::Result<std::collections::HashMap<u32, u32>> {
    // Toolhelp parent PIDs are advisory: Windows may retain a stale PID after
    // its parent exits, and a later process can reuse that PID.
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE};
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
    let mut parents = std::collections::HashMap::new();
    loop {
        parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
        if unsafe { Process32NextW(handle, &mut entry) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
                return Err(error);
            }
            break;
        }
    }
    Ok(parents)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain_end<F>(pid: u32, sessions: &[(String, u32)], process: F) -> CallerOrigin
    where
        F: FnMut(u32) -> io::Result<Option<(u32, u64)>>,
    {
        super::chain_end(pid, sessions, process)
    }

    #[test]
    fn a_missing_parent_ends_a_caller_chain_outside() {
        let process = |pid| match pid {
            40 => Ok(Some((30, 100))),
            30 => Ok(None),
            _ => unreachable!(),
        };
        assert_eq!(chain_end(40, &[], process), CallerOrigin::Outside);
    }

    #[test]
    fn a_strictly_younger_parent_ends_a_caller_chain_outside() {
        let process = |pid| match pid {
            40 => Ok(Some((30, 100))),
            30 => Ok(Some((20, 200))),
            _ => unreachable!(),
        };
        assert_eq!(chain_end(40, &[], process), CallerOrigin::Outside);
    }

    #[test]
    fn an_equal_creation_time_parent_is_not_a_reused_pid() {
        let process = |pid| match pid {
            40 => Ok(Some((30, 100))),
            30 => Ok(Some((20, 100))),
            _ => unreachable!(),
        };
        assert_eq!(
            chain_end(40, &[("work".into(), 30)], process),
            CallerOrigin::Session("work".into())
        );
    }

    #[test]
    fn a_missing_peer_at_the_first_hop_is_unknown() {
        assert_eq!(chain_end(40, &[], |_| Ok(None)), CallerOrigin::Unknown);
    }

    #[test]
    fn a_caller_chain_can_reach_a_session_at_the_64_hop_boundary() {
        let process = |pid| Ok(Some((pid - 1, u64::from(pid))));
        assert_eq!(
            chain_end(100, &[("work".into(), 36)], process),
            CallerOrigin::Session("work".into())
        );
    }

    #[test]
    fn a_session_beyond_the_64_hop_boundary_is_unknown() {
        let process = |pid| Ok(Some((pid - 1, u64::from(pid))));
        assert_eq!(
            chain_end(100, &[("work".into(), 35)], process),
            CallerOrigin::Unknown
        );
    }

    #[test]
    fn root_pids_end_the_chain_outside() {
        for pid in [0, 1] {
            assert_eq!(
                chain_end(pid, &[], |_| unreachable!()),
                CallerOrigin::Outside
            );
        }
    }

    #[test]
    fn only_invalid_parameter_maps_to_a_missing_process() {
        assert!(process_missing(&io::Error::from_raw_os_error(87)));
        assert!(!process_missing(&io::Error::from_raw_os_error(5)));
    }

    #[test]
    fn a_membership_error_stays_unknown_even_if_the_parent_is_missing() {
        let held_peer = Some(40);
        let origin = resolve_job_first(
            || {
                assert_eq!(held_peer, Some(40));
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            },
            || {
                chain_end(40, &[], |pid| match pid {
                    40 => Ok(Some((30, 100))),
                    30 => Ok(None),
                    _ => unreachable!(),
                })
            },
        );
        assert_eq!(origin, CallerOrigin::Unknown);
    }

    #[test]
    fn unreadable_parent_information_keeps_the_caller_unknown() {
        let process = |pid| match pid {
            40 => Ok(Some((30, 100))),
            30 => Err(io::Error::new(io::ErrorKind::PermissionDenied, "hidden")),
            _ => unreachable!(),
        };
        assert_eq!(chain_end(40, &[], process), CallerOrigin::Unknown);

        let unreadable_creation = |_| {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "creation time unavailable",
            ))
        };
        assert_eq!(
            chain_end(40, &[], unreadable_creation),
            CallerOrigin::Unknown
        );
    }

    #[test]
    fn an_overlong_caller_chain_is_unknown() {
        let process = |pid| Ok(Some((pid - 1, u64::from(pid))));
        assert_eq!(chain_end(100, &[], process), CallerOrigin::Unknown);
    }

    #[test]
    fn a_cyclic_caller_chain_is_unknown() {
        let process = |pid| match pid {
            40 => Ok(Some((30, 100))),
            30 => Ok(Some((40, 90))),
            _ => unreachable!(),
        };
        assert_eq!(chain_end(40, &[], process), CallerOrigin::Unknown);
    }

    #[test]
    fn a_session_pid_ends_the_caller_chain_before_its_parent_is_read() {
        let process = |pid| match pid {
            40 => Ok(Some((30, 100))),
            30 => Ok(Some((20, 90))),
            _ => unreachable!(),
        };
        assert_eq!(
            chain_end(40, &[("work".into(), 30)], process),
            CallerOrigin::Session("work".into())
        );
    }

    #[test]
    fn a_reused_session_pid_is_outside_when_the_parent_is_younger() {
        let process = |pid| match pid {
            40 => Ok(Some((30, 100))),
            30 => Ok(Some((20, 200))),
            _ => unreachable!(),
        };
        assert_eq!(
            chain_end(40, &[("work".into(), 30)], process),
            CallerOrigin::Outside
        );
    }

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

    // Terminal apps start shells through the root-owned /usr/bin/login, so
    // the parent walk must read processes owned by another user (#632).
    #[cfg(target_os = "macos")]
    #[test]
    fn the_parent_of_a_root_owned_process_is_readable() {
        assert_eq!(parent_pid(1).expect("read launchd's parent"), Some(0));
    }

    #[test]
    fn missing_unreadable_and_exited_callers_are_unknown() {
        assert_eq!(resolve_caller(Ok(None), &[]), CallerOrigin::Unknown);
        assert_eq!(
            resolve_caller(Err(io::Error::other("peer credentials unavailable")), &[]),
            CallerOrigin::Unknown
        );
        let unreadable = |_: u32| Err(io::Error::new(io::ErrorKind::NotFound, "exited"));
        assert_eq!(
            resolve_caller_with(40, &[], 1, unreadable),
            CallerOrigin::Unknown
        );
    }

    #[test]
    fn a_peer_reparented_to_launchd_is_outside() {
        let parent = |pid| match pid {
            40 => Ok(Some(1)),
            _ => unreachable!(),
        };
        assert_eq!(
            resolve_caller_with(40, &[], 99, parent),
            CallerOrigin::Outside
        );
    }

    #[test]
    fn nested_session_ancestry_returns_the_innermost_session() {
        let parent = |pid| match pid {
            50 => Ok(Some(40)),
            40 => Ok(Some(30)),
            30 => Ok(Some(1)),
            _ => unreachable!(),
        };
        let sessions = vec![("outer".into(), 30), ("inner".into(), 40)];
        assert_eq!(
            resolve_caller_with(50, &sessions, 99, parent),
            CallerOrigin::Session("inner".into())
        );
    }
}
