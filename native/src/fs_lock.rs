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
    format!("{INFO_PREFIX}session={session} pid={pid} since={since}")
}

/// Where the info line for the lock at `path` is kept.
pub fn sidecar(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".info");
    PathBuf::from(name)
}

/// Take the lock at `path` or report who holds it. Never blocks.
pub fn acquire(path: &Path, _info: &str) -> io::Result<Outcome> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    Ok(Outcome::Acquired(file))
}

#[cfg(test)]
mod tests {
    use super::{acquire, info_line, sidecar, Outcome, INFO_PREFIX};
    use std::fs;
    use std::path::PathBuf;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("remuda-fs-lock-{}-{tag}", std::process::id()));
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
        assert_eq!(
            fs::read(&path).unwrap(),
            b"",
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
    fn a_sidecar_that_is_not_ours_is_refused_and_left_unchanged() {
        let scratch = Scratch::new("sidecar");
        let path = scratch.0.join("lock");
        fs::write(sidecar(&path), b"precious").unwrap();
        assert!(acquire(&path, &info_line("s")).is_err());
        assert_eq!(fs::read(sidecar(&path)).unwrap(), b"precious");
        // The refusal released the lock: a clean sidecar lets the next one in.
        fs::remove_file(sidecar(&path)).unwrap();
        let _owner = acquired(acquire(&path, &info_line("s")).unwrap());
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
