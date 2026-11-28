//! macOS force close verifies the kernel's socket peer and process generation.
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

type AuditToken = [u32; 8];
type SignalWithToken = unsafe extern "C" fn(*const AuditToken, libc::c_int) -> libc::c_int;

#[repr(C)]
struct UniqueProcess {
    executable_uuid: [u8; 16],
    unique_id: u64,
    parent_unique_id: u64,
    pid_version: i32,
    original_parent_version: i32,
    reserved: [u64; 2],
}

const PROC_PIDUNIQIDENTIFIERINFO: libc::c_int = 17;
const PID: usize = 5;
const VERSION: usize = 7;

fn refused(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, reason)
}

fn peer_token(stream: &UnixStream) -> io::Result<AuditToken> {
    let mut token = AuditToken::default();
    let mut size = size_of::<AuditToken>() as libc::socklen_t;
    // SAFETY: the socket descriptor and both output buffers are valid.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERTOKEN,
            token.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if size as usize != size_of::<AuditToken>() || token[PID] <= 1 || token[VERSION] == 0 {
        return Err(refused(
            "could not verify the session host's process generation",
        ));
    }
    Ok(token)
}

fn process_identity(pid: libc::pid_t) -> io::Result<UniqueProcess> {
    let mut info = std::mem::MaybeUninit::<UniqueProcess>::uninit();
    // SAFETY: proc_pidinfo writes at most the supplied size.
    let size = unsafe {
        libc::proc_pidinfo(
            pid,
            PROC_PIDUNIQIDENTIFIERINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<UniqueProcess>() as libc::c_int,
        )
    };
    if size as usize != size_of::<UniqueProcess>() {
        return Err(refused("could not read the process generation"));
    }
    // SAFETY: a full result initialized the buffer.
    Ok(unsafe { info.assume_init() })
}

fn saved_executable(pid: libc::pid_t, endpoint: &Path) -> io::Result<std::ffi::OsString> {
    // The reserved host has three short arguments and a fixed environment.
    let mut args = vec![0_u8; 64 * 1024];
    let mut len = args.len();
    let query = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: both the MIB and the output buffer remain live through sysctl.
    let result = unsafe {
        libc::sysctl(
            query.as_ptr() as *mut libc::c_int,
            query.len() as libc::c_uint,
            args.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let executable = reserved_executable(&args[..len], endpoint)
        .ok_or_else(|| refused("saved host launch does not match the reserved endpoint"))?;
    Ok(std::ffi::OsStr::from_bytes(executable).to_os_string())
}

fn reserved_executable<'a>(args: &'a [u8], endpoint: &Path) -> Option<&'a [u8]> {
    let (count, args) = args.split_first_chunk::<4>()?;
    if i32::from_ne_bytes(*count) != 3 {
        return None;
    }
    let separator = args.iter().position(|byte| *byte == 0)?;
    let (executable, tail) = args.split_at(separator);
    let start = tail.iter().position(|byte| *byte != 0)?;
    let mut argv = tail[start..].split_inclusive(|byte| *byte == 0);
    let mut arg = || argv.next()?.strip_suffix(&[0]);
    if !executable.starts_with(b"/")
        || arg()? != executable
        || arg()? != b"run-reserved"
        || arg()? != endpoint.as_os_str().as_bytes()
    {
        return None;
    }
    Some(executable)
}

fn verify_host_executable(pid: libc::pid_t, endpoint: &Path) -> io::Result<()> {
    let mut path = [0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: path points to a writable buffer of the supplied size.
    let result = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    let path = if result > 0 {
        std::ffi::OsStr::from_bytes(path.split(|byte| *byte == 0).next().unwrap_or_default())
            .to_os_string()
    } else {
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) {
            return Err(error);
        }
        saved_executable(pid, endpoint)?
    };
    let path = Path::new(&path);
    let name = path.file_name().and_then(std::ffi::OsStr::to_str);
    let daemon = std::env::current_exe()?;
    let daemon = super::host_build::profile_daemon(&daemon).unwrap_or(daemon);
    if path.parent().map(Path::canonicalize).transpose()?
        != daemon.parent().map(Path::canonicalize).transpose()?
        || !name.is_some_and(|name| {
            name == "remuda-session-host" || name.starts_with("remuda-session-host-")
        })
    {
        return Err(refused(
            "the endpoint peer is not an installed Remuda session host",
        ));
    }
    Ok(())
}

fn direct_child(host: libc::pid_t, identity: &UniqueProcess) -> io::Result<Option<AuditToken>> {
    let mut children = [0_i32; 2];
    // SAFETY: children is a writable buffer of the supplied size.
    let count = unsafe {
        *libc::__error() = 0;
        libc::proc_listchildpids(
            host,
            children.as_mut_ptr().cast(),
            size_of_val(&children) as libc::c_int,
        )
    };
    if count == 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(0)
            && process_identity(host).is_ok_and(|current| current.unique_id == identity.unique_id)
        {
            Ok(None)
        } else {
            Err(refused("could not verify the session child"))
        };
    }
    if count != 1 || children[0] <= 1 {
        return Err(refused("could not identify a single direct session child"));
    }
    let child = process_identity(children[0])?;
    if child.parent_unique_id != identity.unique_id || child.pid_version == 0 {
        return Err(refused("the child no longer belongs to the verified host"));
    }
    let mut token = AuditToken::default();
    token[PID] = children[0] as u32;
    token[VERSION] = child.pid_version as u32;
    Ok(Some(token))
}

fn signal_function() -> io::Result<SignalWithToken> {
    // SAFETY: RTLD_DEFAULT and the symbol name are valid; the function is
    // optional on macOS versions that do not export this libproc entry point.
    let symbol =
        unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"proc_signal_with_audittoken".as_ptr()) };
    if symbol.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "macOS does not expose generation-checked signaling",
        ));
    }
    // SAFETY: Apple's libproc declares this symbol with this ABI.
    Ok(unsafe { std::mem::transmute::<*mut libc::c_void, SignalWithToken>(symbol) })
}

fn stop_process(token: AuditToken) -> io::Result<()> {
    let signal = signal_function()?;
    // SAFETY: the token contains the kernel-observed process ID and generation.
    let result = unsafe { signal(&token, libc::SIGKILL) };
    if result != 0 && result != libc::ESRCH {
        return Err(io::Error::from_raw_os_error(result));
    }
    let until = Instant::now() + Duration::from_secs(2);
    let pid = token[PID] as libc::pid_t;
    loop {
        if super::unix::owner_exited(pid)
            || process_identity(pid)
                .is_ok_and(|process| process.pid_version as u32 != token[VERSION])
        {
            return Ok(());
        }
        if Instant::now() >= until {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "process exit could not be confirmed",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn at_stage(stage: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{stage}: {error}"))
}

pub(super) fn end(endpoint: &Path) -> io::Result<()> {
    let stream = super::unix::connect_for_process_inspection(endpoint)
        .map_err(|error| at_stage("session host socket", error))?;
    let token = peer_token(&stream).map_err(|error| at_stage("session host socket peer", error))?;
    let pid = token[PID] as libc::pid_t;
    let identity = process_identity(pid)?;
    if identity.pid_version as u32 != token[VERSION] {
        return Err(refused("the endpoint peer was replaced"));
    }
    verify_host_executable(pid, endpoint)
        .map_err(|error| at_stage("session host executable lookup", error))?;
    if process_identity(pid)?.pid_version as u32 != token[VERSION] {
        return Err(refused(
            "the endpoint peer was replaced during executable lookup",
        ));
    }
    if let Some(child) = direct_child(pid, &identity)? {
        stop_process(child).map_err(|error| at_stage("terminate session child", error))?;
    }
    stop_process(token).map_err(|error| at_stage("terminate session host", error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_launch_requires_the_original_executable_and_exact_endpoint() {
        let mut args = 3_i32.to_ne_bytes().to_vec();
        let executable = b"/install/remuda-session-host-old";
        args.extend_from_slice(executable);
        args.extend_from_slice(b"\0\0");
        args.extend_from_slice(executable);
        args.extend_from_slice(b"\0run-reserved\0/tmp/session\0PATH=ignored\0");
        assert_eq!(
            reserved_executable(&args, Path::new("/tmp/session")),
            Some(executable.as_slice())
        );
        assert_eq!(reserved_executable(&args, Path::new("/tmp/other")), None);
        assert_eq!(
            reserved_executable(&args[..args.len() - 14], Path::new("/tmp/session")),
            None
        );
    }
}
