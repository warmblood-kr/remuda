//! Which host binary a daemon starts (#713). Hosts outlive daemons, so an
//! install that lands while a daemon runs must not hand that daemon the new
//! build's host. The daemon starts only a host that reports its own build:
//! first the versioned copy the installer keeps (`remuda-session-host-<build>`),
//! then the plain name beside the daemon. Anything else is refused, and the
//! host checks [`super::EXPECTED_BUILD`] again when it starts.
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const HOST: &str = "remuda-session-host";
const PROBE_DEADLINE: Duration = Duration::from_secs(5);
/// A longer answer is not a build string.
const BUILD_LIMIT: usize = 200;

/// The host of `build` for the daemon at `daemon`, checked with `probe`.
pub(super) fn choose(
    daemon: &Path,
    build: &str,
    probe: impl Fn(&Path) -> Option<String>,
) -> Result<PathBuf, String> {
    let plain = daemon.with_file_name(format!("{HOST}{}", std::env::consts::EXE_SUFFIX));
    let mut seen = Vec::new();
    for candidate in versioned(daemon, build).into_iter().chain([plain.clone()]) {
        if !candidate.is_file() {
            continue;
        }
        match probe(&candidate) {
            Some(found) if found == build => return Ok(candidate),
            found => seen.push(format!(
                "{} is build {}",
                candidate.display(),
                found.as_deref().unwrap_or("unknown")
            )),
        }
    }
    if seen.is_empty() {
        return Err(format!(
            "session host binary not found: {}",
            plain.display()
        ));
    }
    Err(format!(
        "no session host of this daemon's build {build}: {}. The installed remuda \
         changed while this daemon runs; restart the daemon to use it",
        seen.join("; ")
    ))
}

/// The installer's name for the host of `build`, when the build string is a
/// safe file name. install.sh applies the same rule.
pub(super) fn versioned(daemon: &Path, build: &str) -> Option<PathBuf> {
    let safe = !build.is_empty()
        && build.len() <= BUILD_LIMIT
        && build
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b));
    safe.then(|| daemon.with_file_name(format!("{HOST}-{build}{}", std::env::consts::EXE_SUFFIX)))
}

/// Same file as last time: device, inode, size and change time all equal.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp(u64, u64, u64, i64, i64);

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp(
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.ctime(),
        meta.ctime_nsec(),
    ))
}

/// What each host file answered, and which file it was then.
type Probed = HashMap<PathBuf, (Stamp, Option<String>)>;

/// `probe_uncached`, remembered per file until the file changes.
pub(super) fn probe(path: &Path) -> Option<String> {
    static SEEN: Mutex<Option<Probed>> = Mutex::new(None);
    let before = stamp(path)?;
    if let Some((seen, build)) = SEEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .get(path)
    {
        if *seen == before {
            return build.clone();
        }
    }
    let build = probe_uncached(path);
    // Replaced while it ran: answer, but do not remember the answer.
    if stamp(path) == Some(before) {
        SEEN.lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(HashMap::new)
            .insert(path.to_owned(), (before, build.clone()));
    }
    build
}

/// Run `HOST version`. A host from before this check prints its usage to
/// stderr and nothing to stdout, which reads as an unknown build.
fn probe_uncached(path: &Path) -> Option<String> {
    let mut child = Command::new(path)
        .arg("version")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + PROBE_DEADLINE;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut out = Vec::new();
    child
        .stdout
        .take()?
        .take(BUILD_LIMIT as u64 + 2)
        .read_to_end(&mut out)
        .ok()?;
    let text = String::from_utf8(out).ok()?;
    let build = text.strip_suffix('\n').unwrap_or(&text);
    (status.success() && !build.is_empty() && !build.contains('\n')).then(|| build.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Dir(PathBuf);
    impl Dir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir() -> Dir {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "rm-host-build-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Dir(path)
    }

    fn script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn the_versioned_copy_of_the_daemons_build_wins_over_a_newer_plain_host() {
        let dir = tempdir();
        let daemon = dir.path().join("remuda");
        script(&dir.path().join(HOST), "echo 2+b");
        script(&dir.path().join(format!("{HOST}-1+a")), "echo 1+a");
        let chosen = choose(&daemon, "1+a", probe_uncached).unwrap();
        assert_eq!(chosen, dir.path().join(format!("{HOST}-1+a")));
    }

    #[test]
    fn the_plain_host_is_used_only_when_it_reports_the_daemons_build() {
        let dir = tempdir();
        let daemon = dir.path().join("remuda");
        script(&dir.path().join(HOST), "echo 1+a");
        assert_eq!(
            choose(&daemon, "1+a", probe_uncached).unwrap(),
            dir.path().join(HOST)
        );
        let refused = choose(&daemon, "0+old", probe_uncached).unwrap_err();
        assert!(refused.contains("is build 1+a"), "{refused}");
        assert!(refused.contains("restart the daemon"), "{refused}");
    }

    #[test]
    fn a_host_from_before_the_check_is_an_unknown_build_and_refused() {
        let dir = tempdir();
        let daemon = dir.path().join("remuda");
        // Today's hosts answer `version` with a usage error on stderr.
        script(&dir.path().join(HOST), "echo usage >&2; exit 1");
        let refused = choose(&daemon, "1+a", probe_uncached).unwrap_err();
        assert!(refused.contains("is build unknown"), "{refused}");
    }

    #[test]
    fn host_binary_is_looked_up_beside_the_daemon_with_the_platform_suffix() {
        let daemon = Path::new("/opt/bin").join(format!("remuda{}", std::env::consts::EXE_SUFFIX));
        let refused = choose(&daemon, "1+a", |_| None).unwrap_err();
        let want = format!(
            "/opt/bin/remuda-session-host{}",
            std::env::consts::EXE_SUFFIX
        );
        assert!(refused.ends_with(&want), "{refused}");
        let versioned = versioned(&daemon, "1+a").unwrap();
        let want = format!(
            "/opt/bin/remuda-session-host-1+a{}",
            std::env::consts::EXE_SUFFIX
        );
        assert_eq!(versioned, Path::new(&want));
    }

    #[test]
    fn a_missing_host_says_not_found() {
        let dir = tempdir();
        let refused = choose(&dir.path().join("remuda"), "1+a", probe_uncached).unwrap_err();
        assert!(
            refused.starts_with("session host binary not found"),
            "{refused}"
        );
    }

    #[test]
    fn unsafe_build_strings_have_no_versioned_name() {
        let daemon = Path::new("/x/remuda");
        assert!(versioned(daemon, "0.1.0-nightly.20261021112925.fc85004+gha.1.1.abc").is_some());
        for bad in ["", "../x", "a/b", "a b", &"9".repeat(BUILD_LIMIT + 1)] {
            assert!(versioned(daemon, bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_replaced_host_is_probed_again() {
        let dir = tempdir();
        let host = dir.path().join(HOST);
        script(&host, "echo 1+a");
        assert_eq!(probe(&host).as_deref(), Some("1+a"));
        // The installer lands a new host by rename: a new inode.
        let incoming = dir.path().join("incoming");
        script(&incoming, "echo 2+b");
        std::fs::rename(&incoming, &host).unwrap();
        assert_eq!(probe(&host).as_deref(), Some("2+b"));
    }

    #[test]
    fn a_hung_host_is_an_unknown_build() {
        let dir = tempdir();
        let host = dir.path().join(HOST);
        script(&host, "exec sleep 30");
        let started = Instant::now();
        assert_eq!(probe_uncached(&host), None);
        assert!(started.elapsed() < PROBE_DEADLINE + Duration::from_secs(2));
    }
}
