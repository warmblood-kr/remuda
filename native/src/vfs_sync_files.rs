//! `remuda drive sync SRC DST`: one-way directory copy between a local
//! directory and this cluster's store. It is a practical subset of
//! `rsync`/`aws s3 sync`: no delete, no watch, no filters yet.
//!
//! The CLI process walks the local side and asks the daemon for the store
//! side over `Request::Vfs`, so the store, its URIs and the protocol do not
//! change. Every local write goes through a temporary file and a rename; every
//! store write is an ordinary `put`.

use crate::cluster::vfs::{sha256_hex, VfsStore, MAX_BLOB_BYTES};
use remuda_core::vfs::VfsUri;
use serde_json::Value;
use std::collections::BTreeMap;
#[cfg(unix)]
use std::ffi::CString;
use std::fs;
#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

/// Bound one run before it starts, so a mistyped root cannot run for hours.
const MAX_FILES: usize = 10_000;
#[cfg(unix)]
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Which side is local. Exactly one side is always the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    LocalToVfs,
    VfsToLocal,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub dry_run: bool,
    pub verbose: bool,
}

/// A setup problem: bad arguments exit 2, anything else exits 1.
#[derive(Debug, PartialEq, Eq)]
pub enum SyncError {
    Usage(String),
    Failed(String),
}

impl SyncError {
    pub fn detail(&self) -> &str {
        match self {
            Self::Usage(detail) | Self::Failed(detail) => detail,
        }
    }

    pub fn is_usage(&self) -> bool {
        matches!(self, Self::Usage(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub path: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub source: String,
    pub dest: String,
    pub direction: Direction,
    pub dry_run: bool,
    pub copied: usize,
    pub unchanged: usize,
    pub skipped: Vec<String>,
    pub failed: Vec<Failure>,
    pub bytes: u64,
    /// Files stored here but not yet on every wanted node (a `put` exit 75).
    pub temporary: usize,
    /// One line per copied file, only when `--verbose` asked for them.
    pub verbose: Vec<String>,
    verbose_enabled: bool,
}

impl Report {
    fn new(source: &str, dest: &str, direction: Direction, options: Options) -> Self {
        Self {
            source: source.to_owned(),
            dest: dest.to_owned(),
            direction,
            dry_run: options.dry_run,
            copied: 0,
            unchanged: 0,
            skipped: Vec::new(),
            failed: Vec::new(),
            bytes: 0,
            temporary: 0,
            verbose: Vec::new(),
            verbose_enabled: options.verbose,
        }
    }

    /// 0 done, 75 copied but not everywhere or partly done, 1 a hard failure.
    pub fn exit_code(&self) -> u8 {
        if !self.failed.is_empty() {
            1
        } else if self.temporary > 0 {
            75
        } else {
            0
        }
    }

    /// The headline, then optional detail. Failures go to stderr separately.
    pub fn lines(&self) -> Vec<String> {
        let action = if self.dry_run { "Would copy" } else { "Copied" };
        let files = if self.copied == 1 { "file" } else { "files" };
        let mut lines = vec![format!(
            "{action} {} {files} ({}), {} unchanged, {} skipped, {} failed.",
            self.copied,
            crate::vfs_publish::bytes_text(self.bytes),
            self.unchanged,
            self.skipped.len(),
            self.failed.len()
        )];
        lines.extend(self.verbose.iter().cloned());
        for skipped in self.skipped.iter().take(20) {
            lines.push(format!("  skipped: {skipped}"));
        }
        if self.skipped.len() > 20 {
            lines.push(format!("  … and {} more skipped", self.skipped.len() - 20));
        }
        if self.temporary > 0 {
            lines.push(format!(
                "  {} stored here but not on every wanted node yet; `remuda drive status --copies` shows the copies.",
                self.temporary
            ));
        }
        lines
    }
}

/// Run one sync. `source` and `dest` are exactly as the user typed them.
pub fn run(socket: &Path, source: &str, dest: &str, options: Options) -> Result<Report, SyncError> {
    match direction(source, dest)? {
        Direction::LocalToVfs => local_to_vfs(socket, source, dest, options),
        Direction::VfsToLocal => vfs_to_local(socket, source, dest, options),
    }
}

/// Exactly one side must be a `remuda://` URI.
pub fn direction(source: &str, dest: &str) -> Result<Direction, SyncError> {
    match (is_vfs(source), is_vfs(dest)) {
        (false, true) => Ok(Direction::LocalToVfs),
        (true, false) => Ok(Direction::VfsToLocal),
        (true, true) => Err(SyncError::Usage(
            "give one remuda:/// prefix and one local directory, not two remuda:// URIs".to_owned(),
        )),
        (false, false) => Err(SyncError::Usage(
            "give one remuda:/// prefix and one local directory, not two local paths".to_owned(),
        )),
    }
}

fn is_vfs(text: &str) -> bool {
    text.starts_with("remuda://")
}

/// The store path under a `remuda:///` URI. Named clusters and the whole store
/// are refused: a typo should not copy everything.
fn vfs_prefix(text: &str) -> Result<String, SyncError> {
    let uri = VfsUri::parse(text)
        .map_err(|error| SyncError::Usage(format!("{text:?} is not a remuda:// URI: {error}")))?;
    if uri.cluster.is_some() {
        return Err(SyncError::Usage(
            "sync uses this cluster's remuda:/// URIs; named clusters are not supported yet"
                .to_owned(),
        ));
    }
    let path = uri.path.trim_matches('/');
    if path.is_empty() {
        return Err(SyncError::Usage(
            "name a directory prefix such as remuda:///dir-a, not the whole store".to_owned(),
        ));
    }
    Ok(path.to_owned())
}

fn local_source_dir(text: &str) -> Result<PathBuf, SyncError> {
    let path = PathBuf::from(text);
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        SyncError::Failed(format!("cannot read source {}: {error}", path.display()))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(SyncError::Usage(format!(
            "{} is a symlink; name the directory it points at",
            path.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(SyncError::Usage(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    Ok(path)
}

/// A missing destination is accepted and created by the caller after the
/// store-overlap check. A symlink is refused so writes cannot land outside
/// the tree.
#[cfg(unix)]
fn local_dest_dir(text: &str) -> Result<PathBuf, SyncError> {
    let path = PathBuf::from(text);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(SyncError::Usage(format!(
            "{} is a symlink; name the directory it points at",
            path.display()
        ))),
        Ok(metadata) if metadata.is_dir() => Ok(path),
        Ok(_) => Err(SyncError::Usage(format!(
            "{} is not a directory",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(error) => Err(SyncError::Failed(format!(
            "cannot read destination {}: {error}",
            path.display()
        ))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalFile {
    relative: String,
    path: PathBuf,
}

struct Walk {
    files: Vec<LocalFile>,
    skipped: Vec<String>,
    failed: Vec<Failure>,
}

fn walk_local(root: &Path) -> Result<Walk, SyncError> {
    let mut walk = Walk {
        files: Vec::new(),
        skipped: Vec::new(),
        failed: Vec::new(),
    };
    walk_dir(root, "", &mut walk)?;
    walk.files
        .sort_by(|left, right| left.relative.cmp(&right.relative));
    if walk.files.len() > MAX_FILES {
        return Err(SyncError::Failed(format!(
            "{} holds {} files; one sync may copy {MAX_FILES}",
            root.display(),
            walk.files.len()
        )));
    }
    Ok(walk)
}

fn walk_dir(dir: &Path, prefix: &str, walk: &mut Walk) -> Result<(), SyncError> {
    let mut names: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|error| {
            SyncError::Failed(format!("cannot read directory {}: {error}", dir.display()))
        })?
        .map(|entry| {
            entry.map(|entry| entry.path()).map_err(|error| {
                SyncError::Failed(format!("cannot read {}: {error}", dir.display()))
            })
        })
        .collect::<Result<_, _>>()?;
    names.sort();
    for path in names {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            walk.skipped.push(format!("{prefix}{{non-UTF-8 name}}"));
            continue;
        };
        if name.chars().any(char::is_control) {
            walk.skipped
                .push(format!("{prefix}{{name with a control character}}"));
            continue;
        }
        let relative = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            SyncError::Failed(format!("cannot read {}: {error}", path.display()))
        })?;
        if metadata.file_type().is_symlink() {
            walk.skipped.push(relative);
        } else if metadata.is_dir() {
            walk_dir(&path, &relative, walk)?;
        } else if metadata.is_file() {
            if metadata.len() > MAX_BLOB_BYTES {
                walk.failed.push(Failure {
                    path: relative.clone(),
                    detail: format!(
                        "{} bytes is over the {MAX_BLOB_BYTES}-byte limit; skipped",
                        metadata.len()
                    ),
                });
            } else {
                walk.files.push(LocalFile { relative, path });
            }
        } else {
            walk.skipped.push(relative);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VfsRow {
    relative: String,
    sha256: String,
    size: u64,
}

fn list_vfs(socket: &Path, prefix: &str) -> Result<Vec<VfsRow>, SyncError> {
    let uri = format!("remuda:///{prefix}/");
    let (code, stdout, stderr) = crate::vfs_cli::daemon_call(
        socket,
        vec!["ls".into(), uri.clone(), "-R".into(), "--json".into()],
        None,
    )
    .map_err(|error| SyncError::Failed(format!("cannot list {uri}: {error}")))?;
    if code != 0 {
        return Err(SyncError::Failed(if stderr.is_empty() {
            format!("cannot list {uri}: exit {code}")
        } else {
            stderr
        }));
    }
    let rows: Vec<Value> = serde_json::from_slice(&stdout)
        .map_err(|error| SyncError::Failed(format!("cannot read the listing of {uri}: {error}")))?;
    let prefix_slash = format!("{prefix}/");
    let mut files = Vec::new();
    for row in rows {
        if row.get("kind").and_then(Value::as_str) != Some("file") {
            continue;
        }
        let path = row
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| SyncError::Failed(format!("the listing of {uri} has no path")))?;
        let relative = path.strip_prefix(&prefix_slash).ok_or_else(|| {
            SyncError::Failed(format!(
                "the listing of {uri} named {path}, outside the prefix"
            ))
        })?;
        validate_relative(relative)?;
        let sha256 = row
            .get("sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| SyncError::Failed(format!("the listing of {uri} has no sha256")))?
            .to_owned();
        let size = row
            .get("size")
            .and_then(Value::as_u64)
            .ok_or_else(|| SyncError::Failed(format!("the listing of {uri} has no size")))?;
        files.push(VfsRow {
            relative: relative.to_owned(),
            sha256,
            size,
        });
    }
    files.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(files)
}

/// Refuse a store prefix that is already a file, so a directory copy cannot
/// create children under it.
fn require_directory_prefix(socket: &Path, prefix: &str) -> Result<(), SyncError> {
    let uri = format!("remuda:///{prefix}");
    match crate::vfs_cli::daemon_call(socket, vec!["stat".into(), uri.clone()], None) {
        Ok((0, _, _)) => Err(SyncError::Usage(format!(
            "{uri} is a file; name a directory prefix"
        ))),
        Ok(_) | Err(_) => Ok(()),
    }
}

fn local_to_vfs(
    socket: &Path,
    source: &str,
    dest: &str,
    options: Options,
) -> Result<Report, SyncError> {
    let root = local_source_dir(source)?;
    refuse_store_overlap(&root)?;
    let prefix = vfs_prefix(dest)?;
    require_directory_prefix(socket, &prefix)?;
    let rows = list_vfs(socket, &prefix)?;
    let known: BTreeMap<&str, &VfsRow> = rows
        .iter()
        .map(|row| (row.relative.as_str(), row))
        .collect();
    let walk = walk_local(&root)?;
    let mut report = Report::new(source, dest, Direction::LocalToVfs, options);
    report.skipped = walk.skipped;
    report.failed = walk.failed;
    for file in walk.files {
        copy_one_to_vfs(socket, &prefix, &known, &file, &mut report);
    }
    Ok(report)
}

fn copy_one_to_vfs(
    socket: &Path,
    prefix: &str,
    known: &BTreeMap<&str, &VfsRow>,
    file: &LocalFile,
    report: &mut Report,
) {
    let bytes = match fs::read(&file.path) {
        Ok(bytes) => bytes,
        Err(error) => {
            report.failed.push(Failure {
                path: file.relative.clone(),
                detail: format!("cannot read {}: {error}", file.path.display()),
            });
            return;
        }
    };
    if bytes.len() as u64 > MAX_BLOB_BYTES {
        report.failed.push(Failure {
            path: file.relative.clone(),
            detail: format!(
                "{} bytes is over the {MAX_BLOB_BYTES}-byte limit",
                bytes.len()
            ),
        });
        return;
    }
    if let Some(detail) = vfs_path_conflict(&file.relative, known) {
        report.failed.push(Failure {
            path: file.relative.clone(),
            detail,
        });
        return;
    }
    let hash = sha256_hex(&bytes);
    if let Some(row) = known.get(file.relative.as_str()) {
        if row.sha256 == hash && row.size == bytes.len() as u64 {
            report.unchanged += 1;
            return;
        }
    }
    if report.dry_run {
        report.copied += 1;
        report.bytes = report.bytes.saturating_add(bytes.len() as u64);
        if report.verbose_enabled {
            report
                .verbose
                .push(format!("  would copy {}", file.relative));
        }
        return;
    }
    let uri = format!("remuda:///{prefix}/{}", file.relative);
    let size = bytes.len() as u64;
    match crate::vfs_cli::daemon_call(socket, vec!["put".into(), "-".into(), uri], Some(bytes)) {
        Ok((0, _, _)) => {
            report.copied += 1;
            report.bytes = report.bytes.saturating_add(size);
            if report.verbose_enabled {
                report.verbose.push(format!("  copied {}", file.relative));
            }
        }
        Ok((75, _, _)) => {
            report.copied += 1;
            report.bytes = report.bytes.saturating_add(size);
            report.temporary += 1;
            if report.verbose_enabled {
                report
                    .verbose
                    .push(format!("  copied {} (not everywhere yet)", file.relative));
            }
        }
        Ok((code, _, stderr)) => report.failed.push(Failure {
            path: file.relative.clone(),
            detail: if stderr.is_empty() {
                format!("put exited {code}")
            } else {
                stderr
            },
        }),
        Err(error) => report.failed.push(Failure {
            path: file.relative.clone(),
            detail: error,
        }),
    }
}

/// A store file and a directory cannot share a path. Report the first side
/// that would make one.
fn vfs_path_conflict(relative: &str, known: &BTreeMap<&str, &VfsRow>) -> Option<String> {
    let mut ancestor = String::new();
    for part in relative.split('/') {
        if !ancestor.is_empty() && known.contains_key(ancestor.as_str()) {
            return Some(format!(
                "remuda:///{ancestor} is a file; cannot store {relative} under it"
            ));
        }
        if !ancestor.is_empty() {
            ancestor.push('/');
        }
        ancestor.push_str(part);
    }
    let prefix = format!("{relative}/");
    if let Some((path, _)) = known.range(prefix.as_str()..).next() {
        if path.starts_with(&prefix) {
            return Some(format!(
                "{relative} would be a file, but remuda:///{path} is stored under it"
            ));
        }
    }
    None
}

#[cfg(unix)]
fn vfs_to_local(
    socket: &Path,
    source: &str,
    dest: &str,
    options: Options,
) -> Result<Report, SyncError> {
    let prefix = vfs_prefix(source)?;
    require_directory_prefix(socket, &prefix)?;
    let rows = list_vfs(socket, &prefix)?;
    if rows.is_empty() {
        return Err(SyncError::Failed(format!(
            "no files under remuda:///{prefix}/"
        )));
    }
    let root = local_dest_dir(dest)?;
    refuse_store_overlap(&root)?;
    if !options.dry_run {
        fs::create_dir_all(&root).map_err(|error| {
            SyncError::Failed(format!(
                "cannot create destination {}: {error}",
                root.display()
            ))
        })?;
    }
    let mut report = Report::new(source, dest, Direction::VfsToLocal, options);
    for row in rows {
        copy_one_to_local(socket, &prefix, &root, &row, &mut report);
    }
    Ok(report)
}

#[cfg(not(unix))]
fn vfs_to_local(
    _socket: &Path,
    _source: &str,
    _dest: &str,
    _options: Options,
) -> Result<Report, SyncError> {
    Err(SyncError::Failed(
        "sync from the store to a local directory needs no-follow destination writes, which this platform does not have yet; use a Unix node"
            .to_owned(),
    ))
}

#[cfg(unix)]
fn copy_one_to_local(socket: &Path, prefix: &str, root: &Path, row: &VfsRow, report: &mut Report) {
    match read_existing(root, &row.relative) {
        Ok(Some(bytes)) if bytes.len() as u64 == row.size && sha256_hex(&bytes) == row.sha256 => {
            report.unchanged += 1;
            return;
        }
        Ok(_) => {}
        Err(detail) => {
            report.failed.push(Failure {
                path: row.relative.clone(),
                detail,
            });
            return;
        }
    }
    if report.dry_run {
        report.copied += 1;
        report.bytes = report.bytes.saturating_add(row.size);
        if report.verbose_enabled {
            report
                .verbose
                .push(format!("  would copy {}", row.relative));
        }
        return;
    }
    let uri = format!("remuda:///{prefix}/{}", row.relative);
    let bytes = match fetch_unchanged(socket, &uri, row) {
        Ok(bytes) => bytes,
        Err(detail) => {
            report.failed.push(Failure {
                path: row.relative.clone(),
                detail,
            });
            return;
        }
    };
    match write_atomic(root, &row.relative, &bytes) {
        Ok(()) => {
            report.copied += 1;
            report.bytes = report.bytes.saturating_add(bytes.len() as u64);
            if report.verbose_enabled {
                report.verbose.push(format!("  copied {}", row.relative));
            }
        }
        Err(detail) => report.failed.push(Failure {
            path: row.relative.clone(),
            detail,
        }),
    }
}

/// Fetch one file and insist it still matches the listing. One retry covers a
/// write that landed between `ls` and `get`; a second mismatch is reported.
#[cfg(unix)]
fn fetch_unchanged(socket: &Path, uri: &str, row: &VfsRow) -> Result<Vec<u8>, String> {
    for attempt in 0..2 {
        let (code, bytes, stderr) =
            crate::vfs_cli::daemon_call(socket, vec!["get".into(), uri.to_owned()], None)
                .map_err(|error| format!("cannot get {uri}: {error}"))?;
        if code != 0 {
            return Err(if stderr.is_empty() {
                format!("get {uri} exited {code}")
            } else {
                stderr
            });
        }
        if bytes.len() as u64 == row.size && sha256_hex(&bytes) == row.sha256 {
            return Ok(bytes);
        }
        if attempt == 1 {
            return Err(format!(
                "{uri} changed during the sync; run the command again"
            ));
        }
    }
    unreachable!("the loop returns on both attempts")
}

/// Refuse a local path that is, contains, or sits inside this node's store.
/// Writing a store path back into `blobs/` would corrupt copies.
fn refuse_store_overlap(path: &Path) -> Result<(), SyncError> {
    let Ok(store) = VfsStore::open_default() else {
        return Ok(());
    };
    let store_root = canonicalish(store.root());
    let local = canonicalish(path);
    if local == store_root || local.starts_with(&store_root) || store_root.starts_with(&local) {
        return Err(SyncError::Usage(format!(
            "{} overlaps the remuda VFS store at {}; choose a directory outside it",
            path.display(),
            store.root().display()
        )));
    }
    Ok(())
}

/// Canonicalize the longest existing prefix, then append the rest. A missing
/// destination still compares correctly before it is created.
fn canonicalish(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canonical) = fs::canonicalize(existing) {
            let mut joined = canonical;
            for part in tail.iter().rev() {
                if part.as_os_str() == std::ffi::OsStr::new("..") {
                    joined.pop();
                } else if part.as_os_str() != std::ffi::OsStr::new(".") {
                    joined.push(part);
                }
            }
            return joined;
        }
        let Some(name) = existing.file_name() else {
            return path.to_path_buf();
        };
        tail.push(name.to_owned());
        let Some(parent) = existing.parent() else {
            return path.to_path_buf();
        };
        existing = parent;
    }
}

fn validate_relative(relative: &str) -> Result<(), SyncError> {
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.contains('\\')
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || relative.chars().any(char::is_control)
    {
        return Err(SyncError::Failed(format!(
            "refusing unsafe store path {relative:?}"
        )));
    }
    Ok(())
}

/// Read a destination file without following a symlink in any component.
/// `None` means the file (or a parent) is absent.
#[cfg(unix)]
fn read_existing(root: &Path, relative: &str) -> Result<Option<Vec<u8>>, String> {
    let Some((parent, name)) = open_parent(root, relative, false)? else {
        return Ok(None);
    };
    let display = root.join(relative);
    let fd = match open_at(
        &parent,
        &name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC,
        0,
    ) {
        Ok(fd) => fd,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err(format!(
                "{} is a symlink; not following it",
                display.display()
            ))
        }
        Err(error) => return Err(format!("cannot open {}: {error}", display.display())),
    };
    let mut file = std::fs::File::from(fd);
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect {}: {error}", display.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", display.display()));
    }
    if metadata.len() > MAX_BLOB_BYTES {
        return Err(format!(
            "{} is {} bytes; over the {MAX_BLOB_BYTES}-byte limit",
            display.display(),
            metadata.len()
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {}: {error}", display.display()))?;
    Ok(Some(bytes))
}

/// Write a file beside its target, then rename. The parent directory is held
/// open and each component is opened with `O_NOFOLLOW`, so replacing a path
/// with a symlink after the check cannot redirect the write outside `root`.
#[cfg(unix)]
fn write_atomic(root: &Path, relative: &str, bytes: &[u8]) -> Result<(), String> {
    let Some((parent, name)) = open_parent(root, relative, true)? else {
        return Err(format!("destination {} is missing", root.display()));
    };
    let display = root.join(relative);
    check_final(&parent, &name, &display)?;
    let temporary = temporary_name();
    let fd = open_at(
        &parent,
        &temporary,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0o600,
    )
    .map_err(|error| {
        format!(
            "cannot create a temporary file beside {}: {error}",
            display.display()
        )
    })?;
    let mut file = std::fs::File::from(fd);
    let written = file.write_all(bytes).and_then(|_| file.sync_all());
    drop(file);
    if let Err(error) = written {
        unlink_at(&parent, &temporary);
        return Err(format!("cannot write {}: {error}", display.display()));
    }
    if let Err(error) = rename_at(&parent, &temporary, &name) {
        unlink_at(&parent, &temporary);
        return Err(format!("cannot replace {}: {error}", display.display()));
    }
    Ok(())
}

#[cfg(unix)]
fn open_root(root: &Path) -> Result<Option<OwnedFd>, String> {
    let name = CString::new(root.as_os_str().as_bytes())
        .map_err(|_| format!("{} contains NUL", root.display()))?;
    let fd = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(format!("cannot open {}: {error}", root.display()));
    }
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
}

#[cfg(unix)]
fn open_parent(
    root: &Path,
    relative: &str,
    create: bool,
) -> Result<Option<(OwnedFd, String)>, String> {
    validate_relative(relative).map_err(|error| error.detail().to_owned())?;
    let Some(mut parent) = open_root(root)? else {
        return Ok(None);
    };
    let display = root.join(relative);
    let mut parts: Vec<&str> = relative.split('/').collect();
    let name = parts
        .pop()
        .ok_or_else(|| format!("{} has no file name", display.display()))?
        .to_owned();
    for part in parts {
        match open_at(
            &parent,
            part,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        ) {
            Ok(next) => parent = next,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return Ok(None);
                }
                match mkdir_at(&parent, part) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(format!(
                            "cannot create a directory for {}: {error}",
                            display.display()
                        ))
                    }
                }
                parent = open_at(
                    &parent,
                    part,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0,
                )
                .map_err(|error| {
                    format!("cannot open a directory for {}: {error}", display.display())
                })?;
            }
            Err(error)
                if error.raw_os_error() == Some(libc::ELOOP)
                    || error.raw_os_error() == Some(libc::ENOTDIR) =>
            {
                return Err(format!(
                    "{} has a symlink or non-directory component; refusing to follow it",
                    display.display()
                ))
            }
            Err(error) => {
                return Err(format!(
                    "cannot open a directory for {}: {error}",
                    display.display()
                ))
            }
        }
    }
    Ok(Some((parent, name)))
}

#[cfg(unix)]
fn open_at(
    parent: &OwnedFd,
    name: &str,
    flags: i32,
    mode: libc::mode_t,
) -> std::io::Result<OwnedFd> {
    let name = CString::new(name)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "name contains NUL"))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
fn mkdir_at(parent: &OwnedFd, name: &str) -> std::io::Result<()> {
    let name = CString::new(name)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "name contains NUL"))?;
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn unlink_at(parent: &OwnedFd, name: &str) {
    if let Ok(name) = CString::new(name) {
        let _ = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    }
}

#[cfg(unix)]
fn rename_at(parent: &OwnedFd, from: &str, to: &str) -> std::io::Result<()> {
    let from = CString::new(from)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "name contains NUL"))?;
    let to = CString::new(to)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "name contains NUL"))?;
    let result = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Refuse a final symlink or directory. A rename replaces a symlink rather
/// than following it, so a race after this check still cannot escape `root`.
#[cfg(unix)]
fn check_final(parent: &OwnedFd, name: &str, display: &Path) -> Result<(), String> {
    let name = CString::new(name).map_err(|_| format!("{} contains NUL", display.display()))?;
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(());
        }
        return Err(format!("cannot inspect {}: {error}", display.display()));
    }
    let kind = stat.st_mode & libc::S_IFMT;
    if kind == libc::S_IFLNK {
        return Err(format!(
            "{} is a symlink; not writing through it",
            display.display()
        ));
    }
    if kind == libc::S_IFDIR {
        return Err(format!("{} is a directory", display.display()));
    }
    if kind != libc::S_IFREG {
        return Err(format!("{} is not a regular file", display.display()));
    }
    Ok(())
}

#[cfg(unix)]
fn temporary_name() -> String {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(".remuda-sync-{}-{sequence}.tmp", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("vfs-sync-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn one_side_must_be_the_store() {
        assert_eq!(direction("dir", "remuda:///dir"), Ok(Direction::LocalToVfs));
        assert_eq!(direction("remuda:///dir", "dir"), Ok(Direction::VfsToLocal));
        assert!(direction("a", "b").unwrap_err().is_usage());
        assert!(direction("remuda:///a", "remuda:///b")
            .unwrap_err()
            .is_usage());
    }

    #[test]
    fn the_whole_store_and_named_clusters_are_refused() {
        assert!(vfs_prefix("remuda:///").unwrap_err().is_usage());
        assert!(vfs_prefix("remuda://other/dir").unwrap_err().is_usage());
        assert_eq!(vfs_prefix("remuda:///dir-a").unwrap(), "dir-a");
    }

    #[test]
    fn a_plain_tree_is_sorted_and_a_link_is_skipped() {
        let root = scratch("walk");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("b.txt"), b"b").unwrap();
        fs::write(root.join("sub/a.txt"), b"a").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("b.txt"), root.join("link")).unwrap();
        let walk = walk_local(&root).unwrap();
        let names: Vec<&str> = walk
            .files
            .iter()
            .map(|file| file.relative.as_str())
            .collect();
        assert_eq!(names, ["b.txt", "sub/a.txt"]);
        #[cfg(unix)]
        assert_eq!(walk.skipped, ["link"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_store_file_and_a_directory_cannot_share_a_path() {
        let file = VfsRow {
            relative: "a".into(),
            sha256: "0".repeat(64),
            size: 0,
        };
        let child = VfsRow {
            relative: "a/b".into(),
            sha256: "1".repeat(64),
            size: 0,
        };
        let mut under_file = BTreeMap::new();
        under_file.insert("a", &file);
        assert!(vfs_path_conflict("a/b", &under_file).is_some());
        assert!(vfs_path_conflict("a", &under_file).is_none());
        let mut over_child = BTreeMap::new();
        over_child.insert("a/b", &child);
        assert!(vfs_path_conflict("a", &over_child).is_some());
    }

    #[test]
    fn a_missing_tail_is_normalized_before_the_overlap_check() {
        let root = scratch("canonical");
        fs::create_dir_all(root.join("a")).unwrap();
        let path = root.join("a/../b");
        assert_eq!(
            canonicalish(&path),
            fs::canonicalize(&root).unwrap().join("b")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_unsafe_store_path_is_refused() {
        assert!(validate_relative("../a").is_err());
        assert!(validate_relative("a//b").is_err());
        assert!(validate_relative("/a").is_err());
        assert!(validate_relative("a\\b").is_err());
        assert!(validate_relative("a/b").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_local_write_replaces_the_old_file_without_a_temporary() {
        let root = scratch("atomic");
        let target = root.join("dir/a.txt");
        fs::create_dir_all(root.join("dir")).unwrap();
        fs::write(&target, b"old").unwrap();
        write_atomic(&root, "dir/a.txt", b"new").unwrap();
        write_atomic(&root, "new/sub/b.txt", b"b").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::read(root.join("new/sub/b.txt")).unwrap(), b"b");
        let leftovers: Vec<_> = fs::read_dir(root.join("dir"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with(".remuda-sync-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_intermediate_symlink_cannot_redirect_a_write() {
        let root = scratch("link-intermediate");
        let outside = scratch("link-intermediate-outside");
        fs::write(outside.join("secret.txt"), b"old").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let error = write_atomic(&root, "link/secret.txt", b"new").unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"old");
        assert!(!outside.join("new.txt").exists());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_final_symlink_is_not_followed() {
        let root = scratch("link-final");
        let outside = scratch("link-final-outside");
        fs::write(outside.join("secret.txt"), b"old").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("secret.txt")).unwrap();
        let error = write_atomic(&root, "secret.txt", b"new").unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"old");
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_replaced_parent_cannot_redirect_an_open_handle() {
        use std::io::Write as _;
        let root = scratch("link-race");
        let outside = scratch("link-race-outside");
        fs::create_dir(root.join("real")).unwrap();
        let (parent, _name) = open_parent(&root, "real/file.txt", true).unwrap().unwrap();
        fs::rename(root.join("real"), root.join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("real")).unwrap();
        let fd = open_at(
            &parent,
            "probe",
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
        .unwrap();
        let mut file = std::fs::File::from(fd);
        file.write_all(b"x").unwrap();
        file.sync_all().unwrap();
        drop(file);
        rename_at(&parent, "probe", "file.txt").unwrap();
        assert!(root.join("moved/file.txt").exists());
        assert!(!outside.join("file.txt").exists());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_destination_reads_as_absent() {
        let root = scratch("missing");
        assert_eq!(read_existing(&root.join("gone"), "a/b").unwrap(), None);
        assert_eq!(read_existing(&root, "a/b").unwrap(), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_oversize_file_is_a_per_file_failure() {
        let root = scratch("oversize");
        fs::write(root.join("ok.txt"), b"ok").unwrap();
        let big = fs::File::create(root.join("big.bin")).unwrap();
        big.set_len(MAX_BLOB_BYTES + 1).unwrap();
        let walk = walk_local(&root).unwrap();
        assert_eq!(walk.files.len(), 1);
        assert_eq!(walk.failed.len(), 1);
        assert!(walk.failed[0].detail.contains("limit"), "{:?}", walk.failed);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_oversize_destination_is_not_read() {
        let root = scratch("oversize-dest");
        fs::create_dir(root.join("dest")).unwrap();
        let big = fs::File::create(root.join("dest/big.bin")).unwrap();
        big.set_len(MAX_BLOB_BYTES + 1).unwrap();
        let error = read_existing(&root, "dest/big.bin").unwrap_err();
        assert!(error.contains("limit"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_exit_code_separates_a_failure_from_a_temporary_copy() {
        let mut report = Report::new("a", "b", Direction::LocalToVfs, Options::default());
        assert_eq!(report.exit_code(), 0);
        report.temporary = 1;
        assert_eq!(report.exit_code(), 75);
        report.failed.push(Failure {
            path: "a".into(),
            detail: "no".into(),
        });
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn the_summary_names_a_dry_run_and_counts() {
        let mut report = Report::new(
            "a",
            "b",
            Direction::LocalToVfs,
            Options {
                dry_run: true,
                verbose: false,
            },
        );
        report.copied = 2;
        report.unchanged = 3;
        report.bytes = 1024;
        let lines = report.lines();
        assert!(
            lines[0].starts_with("Would copy 2 files (1.0 KiB), 3 unchanged"),
            "{lines:?}"
        );
    }
}
