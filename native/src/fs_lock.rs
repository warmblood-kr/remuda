//! An exclusive, non-blocking OS advisory lock on a file the caller names.
//!
//! The open file is the lock: the kernel drops it when the owner dies, so no
//! stale state survives a crash. The lock file itself is never written; the
//! owner's info line lives in a sidecar and is message text only.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// Every info line starts with this; a sidecar that does not is not ours.
pub const INFO_PREFIX: &str = "remuda-lock ";

/// What [`acquire`] found: the lock, or the holder's info line.
#[derive(Debug)]
pub enum Outcome {
    Acquired(File),
    Held(String),
}

/// The info line an owner records: `remuda-lock session=S pid=N since=T`.
pub fn info_line(session: &str) -> String {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let pid = std::process::id();
    let odd = |c: char| c.is_whitespace() || unsafe_to_show(c);
    let session: String = session
        .chars()
        .map(|c| if odd(c) { '?' } else { c })
        .collect();
    format!("{INFO_PREFIX}session={session} pid={pid} since={since}")
}

/// Where the info line for the lock at `path` is kept.
pub fn sidecar(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".info");
    PathBuf::from(name)
}

/// Take the lock at `path` or report who holds it. Never blocks. Only the
/// kernel lock decides; the sidecar text is written and read for messages.
pub fn acquire(path: &Path, info: &str) -> io::Result<Outcome> {
    let refuse = |why: &str| io::Error::new(io::ErrorKind::InvalidInput, why.to_owned());
    if !path.is_absolute() {
        return Err(refuse("the lock path must be absolute"));
    }
    let file = open_lock_file(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() != 0 {
        return Err(refuse("the lock path is not an empty lock file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o177 != 0 {
            return Err(refuse("the lock file is not private (run chmod 600 on it)"));
        }
    }
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(Outcome::Held(holder_info(path))),
        Err(std::fs::TryLockError::Error(error)) => return Err(error),
    }
    // Any error from here drops `file`, which gives the lock back.
    let found = read_sidecar(path)?.unwrap_or_default();
    if !found.is_empty() && !found.starts_with(INFO_PREFIX.as_bytes()) {
        return Err(refuse(
            "the lock's .info file is not a remuda lock info file",
        ));
    }
    write_sidecar(&sidecar(path), info.as_bytes())?;
    Ok(Outcome::Acquired(file))
}

/// The holder's info line for a message: ours by prefix, one line, at most
/// [`INFO_LIMIT`] bytes, with nothing that can drive a terminal.
fn holder_info(path: &Path) -> String {
    let bytes = read_sidecar(path).ok().flatten().unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let line = text.lines().next().unwrap_or("");
    if !line.starts_with(INFO_PREFIX) {
        return String::new();
    }
    let mut safe = String::new();
    for c in line
        .chars()
        .map(|c| if unsafe_to_show(c) { '?' } else { c })
    {
        if safe.len() + c.len_utf8() > INFO_LIMIT {
            break;
        }
        safe.push(c);
    }
    safe
}

const INFO_LIMIT: usize = 256;

/// Cc, U+2028/U+2029 and every Bidi_Control character.
fn unsafe_to_show(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}')
        || matches!(c, '\u{2028}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// The first bytes of the sidecar, or `None` when there is none.
fn read_sidecar(path: &Path) -> io::Result<Option<Vec<u8>>> {
    use std::io::Read;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = match options.open(sidecar(path)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(INFO_LIMIT as u64).read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

#[cfg(not(windows))]
fn open_lock_file(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

#[cfg(windows)]
fn open_lock_file(path: &Path) -> io::Result<File> {
    crate::cluster::windows_security::create_or_open_lock(path)
}

#[cfg(not(windows))]
fn write_sidecar(path: &Path, bytes: &[u8]) -> io::Result<()> {
    crate::fs_atomic::write_atomic_lua_private(path, bytes)
}

#[cfg(windows)]
fn write_sidecar(path: &Path, bytes: &[u8]) -> io::Result<()> {
    crate::fs_atomic::write_atomic_private(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::{acquire, info_line, sidecar, Outcome, INFO_PREFIX};
    use std::fs;
    use std::path::PathBuf;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("remuda-fs-lock-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn acquired(outcome: Outcome) -> fs::File {
        match outcome {
            Outcome::Acquired(file) => file,
            Outcome::Held(info) => panic!("held by {info:?}"),
        }
    }

    fn held(outcome: Outcome) -> String {
        match outcome {
            Outcome::Held(info) => info,
            Outcome::Acquired(_) => panic!("acquired a lock that is held"),
        }
    }

    #[test]
    fn a_second_acquire_sees_held_with_the_holders_info() {
        let scratch = Scratch::new("held");
        let path = scratch.0.join("lock");
        let first = info_line("first");
        let _owner = acquired(acquire(&path, &first).unwrap());
        assert_eq!(held(acquire(&path, &info_line("second")).unwrap()), first);
        // Its length, not its bytes: Windows refuses a read of a locked file.
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            0,
            "the lock file is never written"
        );
        assert_eq!(fs::read_to_string(sidecar(&path)).unwrap(), first);
    }

    #[test]
    fn dropping_the_file_releases_the_lock() {
        let scratch = Scratch::new("drop");
        let path = scratch.0.join("lock");
        drop(acquired(acquire(&path, &info_line("first")).unwrap()));
        let second = info_line("second");
        let _owner = acquired(acquire(&path, &second).unwrap());
        assert_eq!(fs::read_to_string(sidecar(&path)).unwrap(), second);
    }

    #[test]
    fn a_forged_info_text_neither_grants_nor_denies() {
        let scratch = Scratch::new("forged");
        let path = scratch.0.join("lock");
        // A leftover line naming a live-looking owner does not deny a free lock.
        fs::write(
            sidecar(&path),
            format!("{INFO_PREFIX}session=x pid=1 since=1"),
        )
        .unwrap();
        let _owner = acquired(acquire(&path, &info_line("real")).unwrap());
        // And rewriting the line under a held lock does not grant it.
        fs::write(sidecar(&path), format!("{INFO_PREFIX}released")).unwrap();
        assert_eq!(
            held(acquire(&path, &info_line("thief")).unwrap()),
            format!("{INFO_PREFIX}released")
        );
    }

    #[test]
    fn a_relative_path_is_refused() {
        assert!(acquire("relative-lock".as_ref(), &info_line("s")).is_err());
        assert!(!std::path::Path::new("relative-lock").exists());
    }

    #[test]
    fn a_non_empty_lock_file_is_refused_and_left_unchanged() {
        let scratch = Scratch::new("nonempty");
        let path = scratch.0.join("somebodys-file");
        fs::write(&path, b"precious").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(acquire(&path, &info_line("s")).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"precious");
        assert!(!sidecar(&path).exists());
    }

    #[test]
    fn a_foreign_sidecar_is_overwritten_once_the_lock_is_held() {
        let scratch = Scratch::new("sidecar");
        let path = scratch.0.join("lock");
        fs::write(sidecar(&path), b"somebody else's text").unwrap();
        let ours = info_line("s");
        let _owner = acquired(acquire(&path, &ours).unwrap());
        assert_eq!(fs::read_to_string(sidecar(&path)).unwrap(), ours);
    }

    #[test]
    fn a_directory_at_the_sidecar_fails_with_one_line_naming_it() {
        let scratch = Scratch::new("sidecar-dir");
        let path = scratch.0.join("lock");
        fs::create_dir(sidecar(&path)).unwrap();
        let error = acquire(&path, &info_line("s")).unwrap_err().to_string();
        let named = sidecar(&path).display().to_string();
        assert!(error.contains(&named) && !error.contains('\n'), "{error}");
        // The failure gave the lock back.
        fs::remove_dir(sidecar(&path)).unwrap();
        let _owner = acquired(acquire(&path, &info_line("s")).unwrap());
    }

    /// Run `work` on its own thread; a call that blocks fails the test.
    #[cfg(unix)]
    fn without_blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || done.send(work()));
        result
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the call blocked")
    }

    #[cfg(unix)]
    fn make_fifo(path: &std::path::Path) {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `name` is a valid NUL-terminated path for this call.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_sidecar_gives_an_empty_holder_info_without_blocking() {
        let scratch = Scratch::new("fifo-held");
        let path = scratch.0.join("lock");
        let _owner = acquired(acquire(&path, &info_line("s")).unwrap());
        fs::remove_file(sidecar(&path)).unwrap();
        make_fifo(&sidecar(&path));
        let asked = path.clone();
        let outcome = without_blocking(move || acquire(&asked, &info_line("t")));
        assert_eq!(held(outcome.unwrap()), "");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_sidecar_is_replaced_when_the_lock_is_taken() {
        let scratch = Scratch::new("fifo-free");
        let path = scratch.0.join("lock");
        make_fifo(&sidecar(&path));
        let (asked, ours) = (path.clone(), info_line("s"));
        let line = ours.clone();
        let outcome = without_blocking(move || acquire(&asked, &line));
        let _owner = acquired(outcome.unwrap());
        assert!(fs::symlink_metadata(sidecar(&path)).unwrap().is_file());
        assert_eq!(fs::read_to_string(sidecar(&path)).unwrap(), ours);
    }

    #[test]
    fn holder_info_is_capped_and_made_terminal_safe() {
        let scratch = Scratch::new("safe");
        let path = scratch.0.join("lock");
        let _owner = acquired(acquire(&path, &info_line("s")).unwrap());
        let forged = format!(
            "{INFO_PREFIX}a\u{1b}[31mb\u{202e}c{}\nsecond line",
            "x".repeat(400)
        );
        fs::write(sidecar(&path), forged).unwrap();
        let info = held(acquire(&path, &info_line("t")).unwrap());
        assert!(
            info.starts_with(&format!("{INFO_PREFIX}a?[31mb?c")),
            "{info:?}"
        );
        assert!(info.len() <= 256 && !info.contains('\n'), "{info:?}");
        // Text without our prefix is not shown at all.
        fs::write(sidecar(&path), "somebody's secret").unwrap();
        assert_eq!(held(acquire(&path, &info_line("t")).unwrap()), "");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_path_is_refused() {
        let scratch = Scratch::new("symlink");
        let target = scratch.0.join("target");
        let path = scratch.0.join("lock");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(acquire(&path, &info_line("s")).is_err());
        assert!(!target.exists(), "nothing was created through the symlink");
    }

    #[cfg(unix)]
    #[test]
    fn the_lock_file_and_sidecar_are_0600_and_a_loose_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new("mode");
        let path = scratch.0.join("lock");
        drop(acquired(acquire(&path, &info_line("s")).unwrap()));
        for file in [path.clone(), sidecar(&path)] {
            let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", file.display());
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(acquire(&path, &info_line("s")).is_err());
    }
}
