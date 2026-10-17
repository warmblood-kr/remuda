//! Capability probe: may staged publication run under this directory?
//! Every refusal is a typed [`Unsupported`] so the caller can log the reason
//! and fall back to the legacy bind. Nothing here mutates the filesystem.

use super::{
    fchmodat_nofollow, preflight_socket_path, rename_noreplace, startup::open_dir_nofollow,
};
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Unsupported {
    /// No audited implementation for this OS (Android and the BSDs included).
    Platform,
    /// The parent cannot be opened or inspected at all.
    Parent,
    /// Exclusive rename is missing in the kernel or the filesystem.
    RenameFlags,
    /// No-follow chmod (fchmodat2 on Linux) is missing or filtered.
    NoFollowChmod,
    /// The staged path would not fit `sun_path`.
    SunPathBudget,
    /// The parent carries a (default) ACL that the mode bits do not describe.
    Acl,
}

/// An absent name makes the probe calls fail with ENOENT when the operation
/// itself works, and mutate nothing either way.
const ABSENT: &str = ".r660-probe-absent";

/// Longest staging name `startup::open` can generate, for the budget check.
fn widest_staged_path(parent: &Path) -> std::path::PathBuf {
    let name = format!(".r660-{:08x}-{:016x}", u32::MAX, u64::MAX);
    parent.join(name).join("s")
}

fn is_missing(result: io::Result<()>) -> Result<(), io::Error> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

pub(crate) fn qualify(public: &Path) -> Result<(), Unsupported> {
    // Android seccomp can answer an unlisted syscall with SIGSYS, so it is
    // allowlisted by platform and never probed by calling.
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return Err(Unsupported::Platform);
    }
    let parent_path = public.parent().ok_or(Unsupported::Parent)?;
    preflight_socket_path(public)
        .and_then(|()| preflight_socket_path(&widest_staged_path(parent_path)))
        .map_err(|_| Unsupported::SunPathBudget)?;
    let parent = open_dir_nofollow(parent_path).map_err(|_| Unsupported::Parent)?;
    let absent = OsStr::new(ABSENT);
    is_missing(rename_noreplace(
        &parent,
        absent,
        OsStr::new(".r660-probe-absent2"),
    ))
    .map_err(|_| Unsupported::RenameFlags)?;
    is_missing(fchmodat_nofollow(&parent, absent, 0o600))
        .map_err(|_| Unsupported::NoFollowChmod)?;
    if has_acl(&parent).map_err(|_| Unsupported::Parent)? {
        return Err(Unsupported::Acl);
    }
    Ok(())
}

/// Default ACL on the directory (Linux).
#[cfg(target_os = "linux")]
fn has_acl(dir: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    let n = unsafe {
        libc::fgetxattr(
            dir.as_raw_fd(),
            c"system.posix_acl_default".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if n >= 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    // No ACL, or a filesystem without xattrs: nothing extends the mode bits.
    match error.raw_os_error() {
        Some(libc::ENODATA | libc::ENOTSUP) => Ok(false),
        _ => Err(error),
    }
}

/// Extended ACL on the directory (macOS).
#[cfg(target_os = "macos")]
fn has_acl(dir: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
    unsafe extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> *mut libc::c_void;
        fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
    }
    let acl = unsafe { acl_get_fd_np(dir.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTSUP) => Ok(false),
            _ => Err(error),
        };
    }
    if unsafe { acl_free(acl) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

/// Unreachable in practice: `qualify` refuses these platforms first.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn has_acl(_dir: &File) -> io::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualifies_a_plain_private_directory() {
        let dir = std::env::temp_dir().join(format!("r660-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = qualify(&dir.join("s"));
        std::fs::remove_dir_all(&dir).unwrap();
        // tmpfs/overlay without exclusive rename is the only acceptable refusal.
        assert!(
            matches!(result, Ok(()) | Err(Unsupported::RenameFlags)),
            "{result:?}"
        );
    }

    #[test]
    fn over_budget_path_is_a_typed_refusal() {
        let long = Path::new("/tmp").join("x".repeat(SOCKET_LEN)).join("s");
        assert_eq!(qualify(&long), Err(Unsupported::SunPathBudget));
    }

    #[test]
    fn staged_name_is_part_of_the_budget() {
        // Public fits, the longer staged path does not: still refused.
        let room = super::super::SOCKET_PATH_LIMIT - "/tmp/".len() - "/s".len();
        let long = Path::new("/tmp").join("y".repeat(room)).join("s");
        assert!(preflight_socket_path(&long).is_ok());
        assert_eq!(qualify(&long), Err(Unsupported::SunPathBudget));
    }

    #[test]
    fn missing_parent_is_a_typed_refusal() {
        let missing = Path::new("/tmp/r660-no-such-dir/s");
        assert_eq!(qualify(missing), Err(Unsupported::Parent));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extended_acl_on_the_parent_is_refused() {
        let dir = std::env::temp_dir().join(format!("r660-acl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let added = std::process::Command::new("chmod")
            .args(["+a", "everyone allow list"])
            .arg(&dir)
            .status()
            .unwrap();
        let result = qualify(&dir.join("s"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(added.success());
        assert_eq!(result, Err(Unsupported::Acl));
    }

    const SOCKET_LEN: usize = 200;
}
