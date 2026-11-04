//! Preflight for `remuda vfs publish` (#891): turn a local directory into the
//! list of files a release prefix should hold, or into the reasons it cannot.
//!
//! The upload itself is one `put` request per file from the CLI, so this module
//! is pure: it walks, bounds, and states the plan. Nothing here touches the
//! cluster or the daemon.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Most files one release may hold. A browser artifact is tens of files; the
/// cap keeps a stray `publish ~` from filling the store's 10,000-file budget.
pub const MAX_FILES: usize = 500;
/// Largest file a release may hold, matching the bridge's own read cap.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Largest release overall. 500 × 32 MiB would be 15 GiB, which a viewer node
/// with a gigabyte free must never be asked to copy.
pub const MAX_RELEASE_BYTES: u64 = 1024 * 1024 * 1024;
/// Free space that must remain after a release is staged locally.
pub const FREE_RESERVE: u64 = 256 * 1024 * 1024;
/// Names a release reserves for itself.
pub const RESERVED: [&str; 2] = ["COMPLETE", "CURRENT"];

/// Free bytes on the filesystem holding `path`, when the platform can say.
pub fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) } != 0 {
            return None;
        }
        Some((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Read one planned file exactly once, without following a symlink and without
/// trusting the preflight: the descriptor must still hold the same size,
/// modification time and regular-file type after the read.
pub fn read_verified(file: &Planned) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A link swapped in after preflight must not be followed.
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut handle = options
        .open(&file.source)
        .map_err(|error| format!("cannot open {}: {error}", file.relative))?;
    let before = handle
        .metadata()
        .map_err(|error| format!("cannot read {}: {error}", file.relative))?;
    if !before.is_file() || before.len() != file.size || before.modified().ok() != file.modified {
        return Err(format!(
            "{} changed after preflight; publish again",
            file.relative
        ));
    }
    let mut bytes = Vec::with_capacity(file.size.min(1 << 20) as usize);
    (&mut handle)
        .take(file.size.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {}: {error}", file.relative))?;
    let after = handle
        .metadata()
        .map_err(|error| format!("cannot read {}: {error}", file.relative))?;
    if bytes.len() as u64 != file.size
        || after.len() != file.size
        || after.modified().ok() != file.modified
    {
        return Err(format!(
            "{} changed while it was read; publish again",
            file.relative
        ));
    }
    Ok(bytes)
}

/// Refuse a plan that cannot be staged on this machine.
pub fn check_budget(plan: &Plan, dir: &Path) -> Result<(), Refused> {
    if plan.bytes > MAX_RELEASE_BYTES {
        return Err(refused(format!(
            "{} is {}; a release may be at most {}",
            dir.display(),
            bytes_text(plan.bytes),
            bytes_text(MAX_RELEASE_BYTES)
        )));
    }
    if let Some(free) = free_bytes(dir) {
        if free < plan.bytes.saturating_add(FREE_RESERVE) {
            return Err(refused(format!(
                "{} is {} and only {} is free; publishing needs room for the source plus {}",
                dir.display(),
                bytes_text(plan.bytes),
                bytes_text(free),
                bytes_text(FREE_RESERVE)
            )));
        }
    }
    Ok(())
}

/// One file to upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub source: PathBuf,
    /// Relative to the published directory, `/`-separated.
    pub relative: String,
    pub size: u64,
    /// Taken at preflight so a source that moves under us is noticed.
    pub modified: Option<SystemTime>,
}

/// What `publish` should say before it uploads anything.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub files: Vec<Planned>,
    /// Symlinks and other non-regular entries, named so they are not silent.
    pub skipped: Vec<String>,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub detail: String,
}

fn refused(detail: impl Into<String>) -> Refused {
    Refused {
        detail: detail.into(),
    }
}

/// Walk `dir` and decide whether it can be published.
pub fn plan(dir: &Path, release: &str) -> Result<Plan, Refused> {
    if release.is_empty()
        || release.contains('/')
        || release == "."
        || release == ".."
        || release.chars().any(|ch| ch.is_control())
    {
        return Err(refused(
            "--release must be one path component: no '/', '.' or '..'",
        ));
    }
    let metadata = std::fs::symlink_metadata(dir)
        .map_err(|error| refused(format!("cannot read {}: {error}", dir.display())))?;
    if !metadata.is_dir() {
        return Err(refused(format!("{} is not a directory", dir.display())));
    }
    let mut plan = Plan::default();
    walk(dir, "", &mut plan)?;
    if let Some(reserved) = plan
        .files
        .iter()
        .find(|file| RESERVED.contains(&file.relative.as_str()))
    {
        return Err(refused(format!(
            "{} is a name a release uses for itself; rename it",
            reserved.relative
        )));
    }
    if plan.files.is_empty() {
        return Err(refused(format!("{} holds no regular files", dir.display())));
    }
    if plan.files.len() > MAX_FILES {
        return Err(refused(format!(
            "{} holds {} files; a release may hold {MAX_FILES}",
            dir.display(),
            plan.files.len()
        )));
    }
    Ok(plan)
}

fn walk(dir: &Path, prefix: &str, plan: &mut Plan) -> Result<(), Refused> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| refused(format!("cannot read {}: {error}", dir.display())))?;
    let mut names: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| refused(format!("cannot read {}: {error}", dir.display())))?;
        names.push(entry.path());
    }
    names.sort();
    for path in names {
        let name = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) => name,
            None => {
                plan.skipped.push(format!("{prefix}{{non-UTF-8 name}}"));
                continue;
            }
        };
        let relative = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => return Err(refused(format!("cannot read {}: {error}", path.display()))),
        };
        if metadata.is_dir() {
            walk(&path, &relative, plan)?;
        } else if metadata.is_file() {
            if metadata.len() > MAX_FILE_BYTES {
                return Err(refused(format!(
                    "{relative} is {} bytes; a release file may be at most {MAX_FILE_BYTES}",
                    metadata.len()
                )));
            }
            plan.bytes = plan.bytes.saturating_add(metadata.len());
            plan.files.push(Planned {
                source: path,
                relative,
                size: metadata.len(),
                modified: metadata.modified().ok(),
            });
        } else {
            // A symlink is not followed: publishing a link would copy whatever
            // it points at today, which is not what the caller staged.
            plan.skipped.push(relative);
        }
    }
    Ok(())
}

/// True when the file still looks like what preflight saw.
pub fn unchanged(file: &Planned) -> bool {
    match std::fs::metadata(&file.source) {
        Ok(metadata) => {
            metadata.is_file()
                && metadata.len() == file.size
                && metadata.modified().ok() == file.modified
        }
        Err(_) => false,
    }
}

pub fn bytes_text(bytes: u64) -> String {
    match bytes {
        bytes if bytes < 1024 => format!("{bytes} B"),
        bytes if bytes < 1024 * 1024 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        bytes => format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("vfs-publish-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_plain_tree_plans_every_file_in_order() {
        let root = scratch("plain");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("b.html"), b"<h1>b</h1>").unwrap();
        std::fs::write(root.join("sub/a.css"), b"a{}").unwrap();
        let plan = plan(&root, "v1").unwrap();
        let names: Vec<&str> = plan
            .files
            .iter()
            .map(|file| file.relative.as_str())
            .collect();
        assert_eq!(names, ["b.html", "sub/a.css"]);
        assert_eq!(plan.bytes, 13);
        assert!(plan.skipped.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_symlink_is_named_not_followed() {
        let root = scratch("link");
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("secret")).unwrap();
        let plan = plan(&root, "v1").unwrap();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.skipped, ["secret"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_release_name_is_one_component() {
        let root = scratch("name");
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        for bad in ["", "a/b", "..", "."] {
            assert!(plan(&root, bad).is_err(), "{bad}");
        }
        assert!(plan(&root, "2026-11-04").is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_reserved_name_is_refused() {
        let root = scratch("reserved");
        std::fs::write(root.join("COMPLETE"), b"x").unwrap();
        let error = plan(&root, "v1").unwrap_err();
        assert!(error.detail.contains("uses for itself"), "{}", error.detail);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_empty_directory_is_refused() {
        let root = scratch("empty");
        let error = plan(&root, "v1").unwrap_err();
        assert!(
            error.detail.contains("no regular files"),
            "{}",
            error.detail
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_file_that_changed_since_preflight_is_not_unchanged() {
        let root = scratch("changed");
        std::fs::write(root.join("a.txt"), b"aaa").unwrap();
        let plan = plan(&root, "v1").unwrap();
        assert!(unchanged(&plan.files[0]));
        std::fs::write(root.join("a.txt"), b"aaaa").unwrap();
        assert!(!unchanged(&plan.files[0]));
        std::fs::remove_dir_all(root).unwrap();
    }
}
