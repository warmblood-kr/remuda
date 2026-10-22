//! File descriptors over the reload socketpair (SCM_RIGHTS). A passed fd
//! shares the open file with the sender: the listener's backlog and the
//! socket lock's `flock` go with it, and either side may keep its copy.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

/// The most fds one message carries.
pub const MAX_FDS: usize = 4;

/// Send `bytes` with `fds` attached to their first byte.
pub fn send(stream: &UnixStream, bytes: &[u8], fds: &[RawFd]) -> io::Result<()> {
    if fds.is_empty() || fds.len() > MAX_FDS || bytes.is_empty() {
        return Err(io::Error::other("fd message needs 1..=4 fds and a body"));
    }
    let size = std::mem::size_of_val(fds);
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(size as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    // SAFETY: msghdr is plain data; every pointer set below outlives sendmsg.
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = space as _;
    // SAFETY: the control buffer holds exactly one cmsg of `size` bytes.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg).cast(), fds.len());
    }
    let mut sent = loop {
        // SAFETY: header points at live buffers.
        let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &header, NO_SIGPIPE) };
        if sent >= 0 {
            break sent as usize;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    // The fds went with the first chunk; the rest is plain bytes.
    while sent < bytes.len() {
        use std::io::Write;
        sent += (&*stream).write(&bytes[sent..])?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
const NO_SIGPIPE: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(not(target_os = "linux"))]
const NO_SIGPIPE: libc::c_int = 0;

/// Read exactly `buffer.len()` bytes, collecting the fds that came with
/// them. Each received fd is close-on-exec. Honors the read timeout.
pub fn recv(stream: &UnixStream, buffer: &mut [u8]) -> io::Result<Vec<OwnedFd>> {
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<[RawFd; MAX_FDS]>() as u32) };
    let mut control = vec![0u8; space as usize];
    let mut fds = Vec::new();
    let mut filled = 0;
    while filled < buffer.len() {
        let mut iov = libc::iovec {
            iov_base: buffer[filled..].as_mut_ptr().cast(),
            iov_len: buffer.len() - filled,
        };
        // SAFETY: as in `send`.
        let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = control.len() as _;
        // SAFETY: header points at live buffers.
        let read = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut header, RECV_CLOEXEC) };
        if read < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        // SAFETY: the kernel filled `control` up to msg_controllen.
        unsafe { take_fds(&header, &mut fds)? };
        if header.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::other("more fds than a reload message carries"));
        }
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        filled += read as usize;
    }
    Ok(fds)
}

#[cfg(target_os = "linux")]
const RECV_CLOEXEC: libc::c_int = libc::MSG_CMSG_CLOEXEC;
#[cfg(not(target_os = "linux"))]
const RECV_CLOEXEC: libc::c_int = 0;

/// # Safety
/// `header` must come from a completed `recvmsg`.
unsafe fn take_fds(header: &libc::msghdr, fds: &mut Vec<OwnedFd>) -> io::Result<()> {
    let mut cmsg = libc::CMSG_FIRSTHDR(header);
    while !cmsg.is_null() {
        if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
            let data = libc::CMSG_DATA(cmsg) as *const RawFd;
            let bytes = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
            for index in 0..bytes / std::mem::size_of::<RawFd>() {
                let fd = OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(index)));
                #[cfg(not(target_os = "linux"))]
                cloexec(&fd)?;
                fds.push(fd);
            }
        }
        cmsg = libc::CMSG_NXTHDR(header, cmsg);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn cloexec(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: fcntl on an fd this process owns.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rfds-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_passed_listener_keeps_its_backlog() {
        let dir = temp("backlog");
        let path = dir.join("s");
        let listener = UnixListener::bind(&path).unwrap();
        // Queued before the handover, never accepted by the sender.
        let mut early = std::os::unix::net::UnixStream::connect(&path).unwrap();
        early.write_all(b"hi").unwrap();

        let (old, new) = UnixStream::pair().unwrap();
        send(&old, b"take", &[listener.as_raw_fd()]).unwrap();
        drop(listener);
        let mut body = [0; 4];
        let fds = recv(&new, &mut body).unwrap();
        assert_eq!(&body, b"take");
        let handed = UnixListener::from(fds.into_iter().next().unwrap());
        let (mut accepted, _) = handed.accept().unwrap();
        let mut got = [0; 2];
        accepted.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hi");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_passed_lock_stays_held_until_every_copy_closes() {
        let dir = temp("lock");
        let path = dir.join("s.lock");
        let lock = std::fs::File::create(&path).unwrap();
        let try_lock = |file: &std::fs::File| unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0
        };
        assert!(try_lock(&lock));
        let (old, new) = UnixStream::pair().unwrap();
        send(&old, b"x", &[lock.as_raw_fd()]).unwrap();
        drop(lock);
        let mut body = [0; 1];
        let handed = recv(&new, &mut body).unwrap().pop().unwrap();
        // Another open of the file cannot lock while the handed copy lives.
        let other = std::fs::File::open(&path).unwrap();
        assert!(!try_lock(&other));
        // SAFETY: F_GETFD on an fd this test owns.
        let flags = unsafe { libc::fcntl(handed.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0, "received fds must not leak");
        drop(handed);
        assert!(try_lock(&other));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_body_arrives_whole_with_its_fds() {
        let (old, new) = UnixStream::pair().unwrap();
        let (a, b) = UnixStream::pair().unwrap();
        let body = vec![7u8; 60 * 1024];
        let sender = {
            let body = body.clone();
            std::thread::spawn(move || send(&old, &body, &[a.as_raw_fd(), b.as_raw_fd()]))
        };
        let mut got = vec![0; body.len()];
        let fds = recv(&new, &mut got).unwrap();
        sender.join().unwrap().unwrap();
        assert_eq!(got, body);
        assert_eq!(fds.len(), 2);
    }
}
