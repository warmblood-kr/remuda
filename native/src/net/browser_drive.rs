//! Permission-scoped VFS reads for the Browser transport.
use super::{Client, Operation, Reply};
use crate::cluster::{
    vfs::{self, VfsErrorKind, VfsIndex, VfsStore, MAX_BLOB_BYTES},
    vfs_sync,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use remuda_core::vfs::{validate_path, validate_sha256, VfsUri};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    fs::File,
    io::{self, Read, Seek},
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

const CHUNK_BYTES: usize = 32 * 1024;
// Reserve room for the reply tag, directory URI and continuation cursor.
const LIST_ENTRY_BYTES: usize = 48 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct DriveEntry {
    pub name: String,
    pub uri: String,
    pub is_directory: bool,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct DriveListing {
    pub directory: Option<String>,
    pub entries: Vec<DriveEntry>,
    pub complete: bool,
    pub next: Option<String>,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct DriveChunk {
    pub revision: String,
    pub offset: u64,
    pub size: u64,
    pub data_b64: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DriveReadError {
    Denied,
    Unavailable,
    Changed,
    Transport,
    InvalidResponse,
}

impl fmt::Display for DriveReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Denied => "Document access was denied",
            Self::Unavailable => "Document could not be read",
            Self::Changed => "Document changed while loading",
            Self::Transport => "Could not reach the document service",
            Self::InvalidResponse => "Document data failed verification",
        })
    }
}

impl std::error::Error for DriveReadError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceFile {
    uri: String,
    path: PathBuf,
    #[serde(default)]
    allow_cluster_read: bool,
}

struct SourceGrant {
    alias: String,
    path: PathBuf,
    allow_cluster_read: bool,
    file: Mutex<File>,
}

impl SourceGrant {
    fn open(source: SourceFile) -> io::Result<Self> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid Browser source file");
        let alias = local_path(&source.uri).ok_or_else(invalid)?;
        if !source.path.is_absolute() || !std::fs::symlink_metadata(&source.path)?.is_file() {
            return Err(invalid());
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let file = options.open(&source.path)?;
        if !file.metadata()?.is_file() {
            return Err(invalid());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
            if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(invalid());
            }
        }
        Ok(Self {
            alias,
            path: source.path,
            allow_cluster_read: source.allow_cluster_read,
            file: Mutex::new(file),
        })
    }

    fn read_chunk(
        &self,
        offset: u64,
        revision: Option<&str>,
    ) -> Result<DriveChunk, DriveReadError> {
        let mut file = self.file.lock().map_err(|_| DriveReadError::Unavailable)?;
        read_regular_chunk(&mut file, offset, revision)
    }
}

pub(super) fn read_regular_chunk(
    file: &mut File,
    offset: u64,
    revision: Option<&str>,
) -> Result<DriveChunk, DriveReadError> {
    const MAX_SOURCE_BYTES: usize = 1024 * 1024;
    if offset != 0 && revision.is_none() {
        return Err(DriveReadError::Unavailable);
    }
    file.rewind().map_err(|_| DriveReadError::Unavailable)?;
    let mut bytes = Vec::new();
    file.take((MAX_SOURCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| DriveReadError::Unavailable)?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(DriveReadError::Unavailable);
    }
    let current = vfs::sha256_hex(&bytes);
    if revision.is_some_and(|expected| expected != current) {
        return Err(DriveReadError::Changed);
    }
    let size = bytes.len() as u64;
    if offset != 0 && offset >= size {
        return Err(DriveReadError::Unavailable);
    }
    let end = (offset + CHUNK_BYTES as u64).min(size);
    Ok(DriveChunk {
        revision: current,
        offset,
        size,
        data_b64: STANDARD.encode(&bytes[offset as usize..end as usize]),
    })
}

pub(super) struct DriveGrant {
    document: Option<String>,
    directory: Option<String>,
    source: Option<SourceGrant>,
}

impl DriveGrant {
    #[cfg(test)]
    pub(super) fn new(document: Option<&str>, directory: Option<&str>) -> io::Result<Self> {
        Self::with_source(document, directory, None)
    }

    pub(super) fn with_source(
        document: Option<&str>,
        directory: Option<&str>,
        source: Option<SourceFile>,
    ) -> io::Result<Self> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid Browser Drive grant");
        let document = document
            .map(|value| local_path(value).ok_or_else(invalid))
            .transpose()?;
        let directory = directory
            .map(|value| local_path(value).ok_or_else(invalid))
            .transpose()?;
        if document.is_none() && directory.is_none() && source.is_none() {
            return Err(invalid());
        }
        Ok(Self {
            document,
            directory,
            source: source.map(SourceGrant::open).transpose()?,
        })
    }

    pub(super) fn cluster_source_enabled(&self) -> bool {
        self.source
            .as_ref()
            .is_some_and(|source| source.allow_cluster_read)
    }

    pub(super) fn read_source(
        &self,
        uri: &str,
        offset: u64,
        revision: Option<&str>,
        cluster: bool,
    ) -> Result<DriveChunk, DriveReadError> {
        use crate::terminal_link::Target;
        let Some(Target::File(url)) = Target::parse(uri) else {
            return Err(DriveReadError::Denied);
        };
        let path = url.to_file_path().map_err(|_| DriveReadError::Denied)?;
        self.source
            .as_ref()
            .filter(|source| (!cluster || source.allow_cluster_read) && source.path == path)
            .ok_or(DriveReadError::Denied)?
            .read_chunk(offset, revision)
    }

    pub(super) fn is_source(&self, uri: &str) -> bool {
        self.source
            .as_ref()
            .is_some_and(|source| local_path(uri).is_some_and(|path| path == source.alias))
    }

    pub(super) fn authorizes_document(&self, uri: &str) -> bool {
        local_path(uri).is_some_and(|path| {
            self.source
                .as_ref()
                .is_some_and(|source| source.alias == path)
                || self.document.as_deref() == Some(&path)
                || self
                    .directory
                    .as_deref()
                    .is_some_and(|dir| descendant(dir, &path))
        })
    }

    fn resolve_directory(&self, requested: Option<&str>) -> Result<Option<String>, DriveReadError> {
        let Some(uri) = requested else {
            return Ok(self.directory.clone());
        };
        let path = local_path(uri).ok_or(DriveReadError::Denied)?;
        match self.directory.as_deref() {
            Some(dir) if dir == path || descendant(dir, &path) => Ok(Some(path)),
            _ => Err(DriveReadError::Denied),
        }
    }

    pub(super) fn list(
        &self,
        directory: Option<&str>,
        after: Option<&str>,
    ) -> Result<DriveListing, DriveReadError> {
        let directory = self.resolve_directory(directory)?;
        let store = VfsStore::open_default().map_err(|_| DriveReadError::Unavailable)?;
        let index = store
            .load_index()
            .map_err(|_| DriveReadError::Unavailable)?;
        self.list_index(&index, directory.as_deref(), after)
    }

    fn list_index(
        &self,
        index: &VfsIndex,
        directory: Option<&str>,
        after: Option<&str>,
    ) -> Result<DriveListing, DriveReadError> {
        let cursor = after.map(parse_cursor).transpose()?;
        let entries: Box<dyn Iterator<Item = DriveEntry> + '_> = match directory {
            Some(dir) => {
                let prefix = if dir.is_empty() {
                    String::new()
                } else {
                    format!("{dir}/")
                };
                let children = index.children(&prefix);
                Box::new(
                    children
                        .dirs
                        .into_iter()
                        .map(move |name| DriveEntry {
                            uri: VfsUri::format("", &format!("{prefix}{name}")),
                            name,
                            is_directory: true,
                        })
                        .chain(
                            children
                                .files
                                .into_iter()
                                .map(|entry| file_row(&entry.path)),
                        ),
                )
            }
            None => Box::new(
                self.document
                    .as_deref()
                    .and_then(|path| index.live(path))
                    .map(|entry| file_row(&entry.path))
                    .into_iter(),
            ),
        };
        let mut page = DriveListing {
            directory: directory
                .filter(|path| !path.is_empty())
                .map(|path| VfsUri::format("", path)),
            entries: Vec::new(),
            complete: true,
            next: None,
        };
        let mut remaining = LIST_ENTRY_BYTES;
        for entry in entries {
            if let Some(cursor) = cursor {
                if order_key(&entry) <= cursor {
                    continue;
                }
            }
            let bytes = serde_json::to_vec(&entry)
                .map_err(|_| DriveReadError::Unavailable)?
                .len()
                + 1;
            if bytes > remaining {
                page.complete = false;
                page.next = page.entries.last().map(|last| {
                    format!(
                        "{}:{}",
                        if last.is_directory { "d" } else { "f" },
                        last.name
                    )
                });
                break;
            }
            remaining -= bytes;
            page.entries.push(entry);
        }
        Ok(page)
    }

    pub(super) fn read_chunk(
        &self,
        uri: &str,
        offset: u64,
        revision: Option<&str>,
    ) -> Result<DriveChunk, DriveReadError> {
        if !self.authorizes_document(uri) {
            return Err(DriveReadError::Denied);
        }
        if let Some(source) = &self.source {
            if local_path(uri).is_some_and(|path| path == source.alias) {
                return source.read_chunk(offset, revision);
            }
        }
        let store = VfsStore::open_default().map_err(|_| DriveReadError::Unavailable)?;
        read_store_chunk(&store, uri, offset, revision)
    }
}

fn local_path(uri: &str) -> Option<String> {
    let parsed = VfsUri::parse(uri).ok()?;
    parsed.cluster.is_none().then_some(parsed.path)
}

fn descendant(dir: &str, path: &str) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn file_row(path: &str) -> DriveEntry {
    DriveEntry {
        name: path.rsplit('/').next().unwrap_or(path).to_owned(),
        uri: VfsUri::format("", path),
        is_directory: false,
    }
}

fn order_key(entry: &DriveEntry) -> (bool, &str) {
    (!entry.is_directory, &entry.name)
}

fn parse_cursor(cursor: &str) -> Result<(bool, &str), DriveReadError> {
    match cursor.split_once(':') {
        Some((kind @ ("f" | "d"), name)) if !name.contains('/') && validate_path(name).is_ok() => {
            Ok((kind == "f", name))
        }
        _ => Err(DriveReadError::Unavailable),
    }
}

fn read_store_chunk(
    store: &VfsStore,
    uri: &str,
    offset: u64,
    revision: Option<&str>,
) -> Result<DriveChunk, DriveReadError> {
    if revision.is_none() && offset != 0 {
        return Err(DriveReadError::Unavailable);
    }
    let path = local_path(uri).ok_or(DriveReadError::Unavailable)?;
    let entry = store
        .live_entry(&path)
        .map_err(|_| DriveReadError::Unavailable)?
        .ok_or(if revision.is_some() {
            DriveReadError::Changed
        } else {
            DriveReadError::Unavailable
        })?;
    if revision.is_some_and(|expected| entry.sha256 != expected) {
        return Err(DriveReadError::Changed);
    }
    if entry.size > MAX_BLOB_BYTES || (offset >= entry.size && offset != 0) {
        return Err(DriveReadError::Unavailable);
    }

    // Only the first request verifies the whole on-disk object. Every request
    // resolves the authorized live URI again. The client checks the full hash.
    if offset == 0 {
        match store.verify_blob(&entry.sha256) {
            Ok(()) => {}
            Err(error) if matches!(error.kind, VfsErrorKind::NotLocal | VfsErrorKind::Corrupt) => {
                vfs_sync::fetch_blob(store, &entry).map_err(|_| DriveReadError::Unavailable)?;
            }
            Err(_) => return Err(DriveReadError::Unavailable),
        }
    }
    let len = (entry.size - offset).min(CHUNK_BYTES as u64) as usize;
    let bytes = store
        .read_blob_range(&entry.sha256, offset, len)
        .map_err(|_| DriveReadError::Unavailable)?;
    if bytes.len() != len {
        return Err(DriveReadError::Unavailable);
    }
    Ok(DriveChunk {
        revision: entry.sha256.clone(),
        offset,
        size: entry.size,
        data_b64: STANDARD.encode(bytes),
    })
}

/// Node-side browsing uses the same local-account access as the VFS CLI.
/// These helpers are in-process only and do not extend any listener grant.
pub fn list_local_drive(
    directory: Option<&str>,
    after: Option<&str>,
) -> Result<DriveListing, DriveReadError> {
    let directory = match directory {
        Some(uri) => local_path(uri).ok_or(DriveReadError::Denied)?,
        None => String::new(),
    };
    let store = VfsStore::open_default().map_err(|_| DriveReadError::Unavailable)?;
    let index = store
        .load_index()
        .map_err(|_| DriveReadError::Unavailable)?;
    DriveGrant {
        document: None,
        directory: None,
        source: None,
    }
    .list_index(&index, Some(&directory), after)
}

pub fn read_local_drive(uri: &str) -> Result<Vec<u8>, DriveReadError> {
    let path = local_path(uri).ok_or(DriveReadError::Denied)?;
    let store = VfsStore::open_default().map_err(|_| DriveReadError::Unavailable)?;
    let index = store
        .load_index()
        .map_err(|_| DriveReadError::Unavailable)?;
    let entry = index.live(&path).ok_or(DriveReadError::Unavailable)?;
    let bytes = match store.read_blob(&entry.sha256) {
        Ok(bytes) => bytes,
        Err(error) if matches!(error.kind, VfsErrorKind::NotLocal | VfsErrorKind::Corrupt) => {
            vfs_sync::fetch_blob(&store, entry).map_err(|_| DriveReadError::Unavailable)?
        }
        Err(_) => return Err(DriveReadError::Unavailable),
    };
    Ok(bytes)
}

impl Client {
    /// Load one authorized pinned document off the GUI thread.
    pub fn read_file(&self, uri: &str) -> Result<Vec<u8>, DriveReadError> {
        read_file_using(uri, |operation| self.send(operation))
    }
}

fn read_file_using(
    uri: &str,
    mut send: impl FnMut(&Operation) -> Result<Reply, super::ClientError>,
) -> Result<Vec<u8>, DriveReadError> {
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut state = Transfer::default();
    loop {
        if Instant::now() >= deadline {
            return Err(DriveReadError::Transport);
        }
        let chunk = match send(&Operation::ReadChunk {
            uri: uri.into(),
            offset: state.bytes.len() as u64,
            revision: state
                .descriptor
                .as_ref()
                .map(|(revision, _)| revision.clone()),
        }) {
            Ok(Reply::Chunk(chunk)) => chunk,
            // The old service returns Invalid for an unknown operation.
            Ok(Reply::Invalid) if state.descriptor.is_none() => {
                let reply = send(&Operation::Read { uri: uri.into() });
                return match reply {
                    Ok(Reply::Document(document)) => Ok(document.into_bytes()),
                    other => Err(read_error(other)),
                };
            }
            other => return Err(read_error(other)),
        };
        if state.accept(chunk)? {
            return Ok(state.bytes);
        }
    }
}

pub(super) fn transfer_cluster_source(
    mut fetch: impl FnMut(u64, Option<&str>) -> Result<Reply, super::ClientError>,
) -> Result<Vec<u8>, DriveReadError> {
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut state = Transfer::default();
    while Instant::now() < deadline {
        let response = fetch(
            state.bytes.len() as u64,
            state
                .descriptor
                .as_ref()
                .map(|(revision, _)| revision.as_str()),
        );
        let chunk = match response {
            Ok(Reply::Chunk(chunk)) if chunk.size <= 1024 * 1024 => chunk,
            other => return Err(read_error(other)),
        };
        if state.accept(chunk)? {
            return Ok(state.bytes);
        }
    }
    Err(DriveReadError::Transport)
}

fn read_error(reply: Result<Reply, super::ClientError>) -> DriveReadError {
    match reply {
        Ok(Reply::Denied) => DriveReadError::Denied,
        Ok(Reply::ReadFailed) => DriveReadError::Unavailable,
        Ok(Reply::DocumentChanged) => DriveReadError::Changed,
        Ok(_) => DriveReadError::InvalidResponse,
        Err(_) => DriveReadError::Transport,
    }
}

#[derive(Default)]
struct Transfer {
    descriptor: Option<(String, u64)>,
    bytes: Vec<u8>,
}

impl Transfer {
    fn accept(&mut self, chunk: DriveChunk) -> Result<bool, DriveReadError> {
        let invalid = DriveReadError::InvalidResponse;
        if chunk.offset != self.bytes.len() as u64
            || chunk.size > MAX_BLOB_BYTES
            || chunk.offset > chunk.size
            || chunk.data_b64.len() > CHUNK_BYTES.div_ceil(3) * 4
            || validate_sha256(&chunk.revision).is_err()
        {
            return Err(invalid);
        }
        let (revision, size) = self
            .descriptor
            .get_or_insert((chunk.revision.clone(), chunk.size));
        if *revision != chunk.revision || *size != chunk.size {
            return Err(DriveReadError::Changed);
        }
        let bytes = STANDARD.decode(chunk.data_b64).map_err(|_| invalid)?;
        if bytes.len() as u64 != (chunk.size - chunk.offset).min(CHUNK_BYTES as u64) {
            return Err(invalid);
        }
        self.bytes.extend(bytes);
        if self.bytes.len() as u64 != *size {
            return Ok(false);
        }
        if vfs::sha256_hex(&self.bytes) != *revision {
            return Err(invalid);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_core::vfs::VfsEntry;

    fn entry(path: String, deleted: bool) -> VfsEntry {
        VfsEntry {
            path,
            sha256: vfs::sha256_hex(b"x"),
            size: 1,
            replicas: 1,
            version: 1,
            writer: "fixture".into(),
            deleted,
            mtime: 0,
            vv: Default::default(),
            conflicts: Vec::new(),
        }
    }

    #[test]
    fn node_drive_root_lists_live_children_without_creating_a_browser_grant() {
        let local = DriveGrant {
            document: None,
            directory: None,
            source: None,
        };
        let mut index = VfsIndex::default();
        for (path, deleted) in [
            ("design/a.md", false),
            ("deleted/a.md", true),
            ("readme.md", false),
        ] {
            index
                .entries
                .insert(path.into(), entry(path.into(), deleted));
        }
        let root = local.list_index(&index, Some(""), None).unwrap();
        assert!(root.directory.is_none());
        assert_eq!(
            root.entries
                .iter()
                .map(|row| (row.name.as_str(), row.is_directory))
                .collect::<Vec<_>>(),
            [("design", true), ("readme.md", false)]
        );
        assert!(!local.authorizes_document("remuda:///readme.md"));
    }

    #[test]
    fn grants_keep_exact_document_and_enforce_directory_boundaries() {
        let exact = DriveGrant::new(Some("remuda:///design/note.md"), None).unwrap();
        assert!(exact.authorizes_document("remuda:///design/note.md"));
        assert!(!exact.authorizes_document("remuda:///design/other.md"));
        assert!(matches!(
            exact.resolve_directory(Some("remuda:///design")),
            Err(DriveReadError::Denied)
        ));

        let directory = DriveGrant::new(None, Some("remuda:///design")).unwrap();
        assert!(directory.authorizes_document("remuda:///design/sub/note.md"));
        for outside in [
            "remuda:///design",
            "remuda:///design-archive/note.md",
            "remuda:///design/../note.md",
            "remuda://foreign/design/note.md",
        ] {
            assert!(!directory.authorizes_document(outside));
        }
        assert!(DriveGrant::new(None, None).is_err());
        assert!(DriveGrant::new(None, Some("remuda:///design/")).is_err());

        let mut index = VfsIndex::default();
        for (path, deleted) in [
            ("design/note.md", false),
            ("design/sub/readme.md", false),
            ("design/deleted/readme.md", true),
            ("design-archive/secret.md", false),
        ] {
            index
                .entries
                .insert(path.into(), entry(path.into(), deleted));
        }
        let exact_page = exact.list_index(&index, None, None).unwrap();
        assert_eq!(exact_page.entries.len(), 1);
        assert_eq!(exact_page.entries[0].name, "note.md");
        let page = directory.list_index(&index, Some("design"), None).unwrap();
        assert_eq!(
            page.entries
                .iter()
                .map(|item| (&*item.name, item.is_directory))
                .collect::<Vec<_>>(),
            [("sub", true), ("note.md", false)]
        );
    }

    #[test]
    fn large_list_pages_fit_encrypted_frames_without_skipping_entries() {
        let dir = format!(
            "{}/{}/{}",
            "a".repeat(235),
            "a".repeat(235),
            "a".repeat(235)
        );
        let grant = DriveGrant::new(None, Some(&VfsUri::format("", &dir))).unwrap();
        let mut index = VfsIndex::default();
        for i in 0..200 {
            let path = format!("{dir}/{i:03}-{}.md", "\"".repeat(120));
            index.entries.insert(path.clone(), entry(path, false));
        }
        let mut cursor = None;
        let mut names = Vec::new();
        loop {
            let page = grant
                .list_index(&index, Some(&dir), cursor.as_deref())
                .unwrap();
            let response = serde_json::to_vec(&Reply::Directory(page.clone())).unwrap();
            assert!(response.len() <= crate::net::frame::MAX_RESPONSE_PAYLOAD);
            names.extend(page.entries.into_iter().map(|item| item.name));
            if page.complete {
                break;
            }
            assert_ne!(cursor, page.next);
            cursor = page.next;
        }
        assert_eq!(names.len(), 200);
        assert!(names.windows(2).all(|items| items[0] < items[1]));
    }

    #[test]
    fn legacy_read_is_used_only_for_an_unsupported_initial_operation() {
        let uri = "remuda:///design/note.md";
        let mut count = 0;
        let bytes = read_file_using(uri, |op| {
            count += 1;
            match op {
                Operation::ReadChunk { offset: 0, .. } => Ok(Reply::Invalid),
                Operation::Read { uri: path } if path == uri => Ok(Reply::Document("text".into())),
                _ => panic!("unexpected operation"),
            }
        })
        .unwrap();
        assert_eq!(count, 2);
        assert_eq!(bytes, b"text");
        assert_eq!(
            read_file_using(uri, |op| {
                assert!(matches!(op, Operation::ReadChunk { .. }));
                Ok(Reply::Denied)
            }),
            Err(DriveReadError::Denied)
        );
    }

    #[test]
    fn chunks_round_trip_binary_and_refuse_a_changed_live_document() {
        let root =
            std::env::temp_dir().join(format!("remuda-browser-drive-test-{}", std::process::id()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        std::fs::create_dir(&root).unwrap();
        let _cleanup = Cleanup(root.clone());
        let store = VfsStore::open_at(&root.join("vfs")).unwrap();
        let path = "design/image.bin";
        let uri = VfsUri::format("", path);
        let data = vec![0xff; CHUNK_BYTES * 2 + 17];
        store.put(path, &data, 1, "fixture").unwrap();
        let mut state = Transfer::default();
        loop {
            let revision = state.descriptor.as_ref().map(|(rev, _)| rev.as_str());
            let chunk = read_store_chunk(&store, &uri, state.bytes.len() as u64, revision).unwrap();
            let response = serde_json::to_vec(&Reply::Chunk(chunk.clone())).unwrap();
            assert!(response.len() <= crate::net::frame::MAX_RESPONSE_PAYLOAD);
            if state.accept(chunk).unwrap() {
                break;
            }
        }
        assert_eq!(state.bytes, data);
        let revision = state.descriptor.unwrap().0;
        store.put(path, b"new", 1, "fixture").unwrap();
        assert_eq!(
            read_store_chunk(&store, &uri, CHUNK_BYTES as u64, Some(&revision)),
            Err(DriveReadError::Changed)
        );

        let mut damaged = read_store_chunk(&store, &uri, 0, None).unwrap();
        damaged.data_b64 = STANDARD.encode(b"bad");
        assert_eq!(
            Transfer::default().accept(damaged),
            Err(DriveReadError::InvalidResponse)
        );
    }
}

#[cfg(test)]
#[test]
fn source_alias_reads_current_file_over_drive_chunks_without_a_store() {
    let path = std::env::temp_dir().join(format!("remuda-source-grant-{}", std::process::id()));
    let mut fixture = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = Cleanup(path.clone());
    let uri = "remuda:///source/guide.md";
    let body = vec![b'a'; CHUNK_BYTES + 1];
    use std::io::Write;
    fixture.write_all(&body).unwrap();
    let grant = DriveGrant::with_source(
        None,
        None,
        Some(SourceFile {
            allow_cluster_read: false,
            uri: uri.into(),
            path,
        }),
    )
    .unwrap();
    assert_eq!(
        grant.read_chunk("remuda:///source/other.md", 0, None),
        Err(DriveReadError::Denied)
    );
    let first = grant.read_chunk(uri, 0, None).unwrap();
    let second = grant
        .read_chunk(uri, CHUNK_BYTES as u64, Some(&first.revision))
        .unwrap();
    let mut arrived = STANDARD.decode(first.data_b64).unwrap();
    arrived.extend(STANDARD.decode(second.data_b64).unwrap());
    assert_eq!(arrived, body);
    fixture.rewind().unwrap();
    fixture.write_all(b"b").unwrap();
    assert_eq!(
        grant.read_chunk(uri, CHUNK_BYTES as u64, Some(&first.revision)),
        Err(DriveReadError::Changed)
    );
}
