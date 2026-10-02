//! Per-kind user directory resolution and Lua bindings.

use mlua::{Lua, Table, UserData, UserDataMethods};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
#[cfg(unix)]
use std::ffi::{CStr, CString};
use std::fs;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub(crate) type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Kind {
    Config,
    Data,
    State,
    Cache,
}

impl Kind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "config" => Some(Self::Config),
            "data" => Some(Self::Data),
            "state" => Some(Self::State),
            "cache" => Some(Self::Cache),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Data => "data",
            Self::State => "state",
            Self::Cache => "cache",
        }
    }

    fn xdg_var(self) -> &'static str {
        match self {
            Self::Config => "XDG_CONFIG_HOME",
            Self::Data => "XDG_DATA_HOME",
            Self::State => "XDG_STATE_HOME",
            Self::Cache => "XDG_CACHE_HOME",
        }
    }

    fn unix_subdir(self) -> &'static str {
        match self {
            Self::Config => ".config",
            Self::Data => ".local/share",
            Self::State => ".local/state",
            Self::Cache => ".cache",
        }
    }
}

fn env_absolute(env: Env<'_>, name: &str, windows: bool) -> Option<PathBuf> {
    let path = env(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)?;
    is_absolute_for(&path, windows).then_some(path)
}

fn is_absolute_for(path: &std::path::Path, windows: bool) -> bool {
    let path = path.as_os_str().to_string_lossy();
    if !windows {
        // Judged by the rule of the platform being resolved, not the host's.
        return path.starts_with('/');
    }
    let bytes = path.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/'))
        || path.starts_with(r"\\")
        || path.starts_with("//")
}

/// Resolve the base directory for a kind. XDG values must be absolute; on
/// Windows the local app data folder is shared as the base for every kind.
pub(crate) fn base_dir_for(kind: Kind, windows: bool, env: Env<'_>) -> Result<PathBuf, String> {
    if let Some(path) = env_absolute(env, kind.xdg_var(), windows) {
        return Ok(path);
    }
    if windows {
        if let Some(path) = env_absolute(env, "LOCALAPPDATA", true) {
            return Ok(path);
        }
        if let Some(profile) = env_absolute(env, "USERPROFILE", true) {
            return Ok(profile.join("AppData").join("Local"));
        }
        return Err(
            "unavailable: LOCALAPPDATA and USERPROFILE are not set to absolute paths".into(),
        );
    }
    if let Some(home) = env_absolute(env, "HOME", false) {
        return Ok(home.join(kind.unix_subdir()));
    }
    match env("HOME") {
        None => Err("unavailable: HOME is not set".into()),
        Some(_) => Err("unavailable: HOME is not an absolute path".into()),
    }
}

/// Resolve the path exposed to Lua. Unix kinds use their XDG home plus
/// `remuda`; Windows data is `LocalAppData/remuda`, with other kinds below it.
pub(crate) fn resolve_dir_for(kind: &str, windows: bool, env: Env<'_>) -> Result<PathBuf, String> {
    let kind = Kind::parse(kind).ok_or_else(|| {
        "remuda.storage.dir kind must be 'config', 'data', 'state', or 'cache'".to_string()
    })?;
    let mut dir = base_dir_for(kind, windows, env)?.join("remuda");
    if windows && !matches!(kind, Kind::Data) {
        dir.push(kind.as_str());
    }
    if !is_absolute_for(&dir, windows) {
        return Err("unavailable: resolved storage path is not absolute".into());
    }
    Ok(dir)
}

/// Resolve a named Remuda user file below its per-kind storage directory.
pub(crate) fn user_file_path_for(
    kind: Kind,
    file: &str,
    windows: bool,
    env: Env<'_>,
) -> Option<PathBuf> {
    resolve_dir_for(kind.as_str(), windows, env)
        .ok()
        .map(|directory| directory.join(file))
}

pub(crate) fn bindings(lua: &Lua) -> mlua::Result<Table> {
    bindings_with_env(lua, &|name| std::env::var_os(name))
}

fn bindings_with_env(lua: &Lua, env: Env<'_>) -> mlua::Result<Table> {
    let environment = Arc::new(
        [
            "REMUDA_STORAGE_ROOT",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
            "HOME",
            "LOCALAPPDATA",
            "USERPROFILE",
        ]
        .into_iter()
        .filter_map(|name| env(name).map(|value| (name.to_string(), value)))
        .collect::<HashMap<_, _>>(),
    );
    let storage = lua.create_table()?;
    register_dir_binding(lua, &storage)?;
    let files = Arc::new(Mutex::new(BTreeMap::new()));
    let roots = Arc::new(Mutex::new(None::<Arc<FileRoots>>));
    let get_files = files.clone();
    let get_roots = roots.clone();
    storage.set(
        "get",
        lua.create_function(move |lua, namespace: String| {
            let namespace = checked_namespace(&namespace)?;
            lua.create_userdata(StorageView {
                namespace,
                files: get_files.clone(),
                roots: get_roots
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            })
        })?,
    )?;
    let default_files = files.clone();
    let default_roots = roots.clone();
    storage.set(
        "default",
        lua.create_function(move |lua, ()| {
            lua.create_userdata(StorageView {
                namespace: "default".into(),
                files: default_files.clone(),
                roots: default_roots
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            })
        })?,
    )?;
    let set_roots = roots.clone();
    let memory_roots = roots.clone();
    let backend_env = environment.clone();
    storage.set(
        "set_default",
        lua.create_function(move |_, backend: String| {
            let selected = match backend.as_str() {
                "memory" => None,
                "xdg" => Some(Arc::new(
                    file_roots(&|name| backend_env.get(name).cloned())
                        .map_err(mlua::Error::runtime)?,
                )),
                _ => return Err(mlua::Error::runtime("unknown storage backend")),
            };
            *set_roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = selected;
            Ok(true)
        })?,
    )?;
    storage.set(
        "backend",
        lua.create_function(move |_, ()| {
            Ok(
                if memory_roots
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_some()
                {
                    "xdg"
                } else {
                    "memory"
                },
            )
        })?,
    )?;
    register_path_binding(lua, &storage, roots)?;
    Ok(storage)
}

fn register_dir_binding(lua: &Lua, storage: &Table) -> mlua::Result<()> {
    storage.set(
        "dir",
        lua.create_function(|_, kind: String| {
            match resolve_dir_for(&kind, cfg!(windows), &|name| std::env::var_os(name)) {
                Ok(path) => match path.into_os_string().into_string() {
                    Ok(path) => Ok((Some(path), None::<String>)),
                    Err(_) => Ok((
                        None,
                        Some("unavailable: resolved path is not valid UTF-8".to_string()),
                    )),
                },
                Err(error) if error.starts_with("unavailable: ") => Ok((None, Some(error))),
                Err(error) => Err(mlua::Error::runtime(error)),
            }
        })?,
    )?;
    Ok(())
}

fn register_path_binding(
    lua: &Lua,
    storage: &Table,
    path_roots: Arc<Mutex<Option<Arc<FileRoots>>>>,
) -> mlua::Result<()> {
    storage.set(
        "path",
        lua.create_function(move |_, (kind, name): (String, String)| {
            if kind == "secret" {
                checked_name(&name)?;
                return Ok(None::<String>);
            }
            let Some(kind) = Kind::parse(&kind) else {
                return Err(mlua::Error::runtime("remuda.storage.path: unknown kind"));
            };
            checked_name(&name)?;
            Ok(path_roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .and_then(|roots| roots.get(kind.as_str()))
                .map(|root| root.join("storage").join("default").join(name))
                .and_then(|path| path.into_os_string().into_string().ok()))
        })?,
    )?;
    Ok(())
}

#[derive(Clone, Copy)]
enum HandleKind {
    Config,
    Data,
    State,
    Cache,
    Secret,
}

impl HandleKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Data => "data",
            Self::State => "state",
            Self::Cache => "cache",
            Self::Secret => "secret",
        }
    }
}

type MemoryFiles = Arc<Mutex<BTreeMap<(String, String, String), Vec<u8>>>>;
type FileRoots = BTreeMap<&'static str, PathBuf>;
const ATOMIC_TEMP_PREFIX: &str = ".remuda-atomic-";
const MAX_STORAGE_VALUE_BYTES: usize = 1024 * 1024;
const MAX_STORAGE_FILES: usize = 1024;
const MAX_STORAGE_ENTRIES: usize = 1024;
const MAX_STORAGE_PARTS: usize = 8;

struct FileLocation {
    #[cfg(unix)]
    directory: fs::File,
    #[cfg(unix)]
    name: CString,
    #[cfg(not(unix))]
    path: PathBuf,
}

fn io_denied() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "unsafe storage path")
}

#[cfg(not(unix))]
fn reject_path_case_clash(parent: &Path, wanted: &str) -> io::Result<()> {
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if entries.filter_map(Result::ok).any(|entry| {
        let name = entry.file_name();
        name.to_str()
            .is_some_and(|name| name != wanted && name.eq_ignore_ascii_case(wanted))
    }) {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "case clash"));
    }
    Ok(())
}

#[cfg(unix)]
fn c_name(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid path"))
}

#[cfg(unix)]
fn directory_names(directory: &fs::File) -> io::Result<Vec<String>> {
    let dot = c_name(".")?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            dot.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        unsafe { libc::close(fd) };
        return Err(io::Error::last_os_error());
    }
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            if let Ok(name) = name.to_str() {
                names.push(name.to_owned());
            }
        }
    }
    unsafe { libc::closedir(stream) };
    Ok(names)
}

#[cfg(unix)]
fn open_directory_at(parent: &fs::File, name: &str, create: bool) -> io::Result<fs::File> {
    reject_case_clash(parent, name)?;
    let name = c_name(name)?;
    if create {
        let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
    }
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let directory = unsafe { fs::File::from_raw_fd(fd) };
    if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(directory)
}

#[cfg(unix)]
fn open_storage_root(root: &Path, create: bool) -> io::Result<fs::File> {
    if create {
        fs::create_dir_all(root)?;
    }
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io_denied());
    }
    use std::os::unix::fs::OpenOptionsExt;
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(root)?;
    if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(directory)
}

#[cfg(unix)]
fn entry_stat(directory: &fs::File, name: &str) -> io::Result<Option<libc::stat>> {
    let name = c_name(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(Some(unsafe { stat.assume_init() }))
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

#[cfg(unix)]
fn collect_at(
    directory: &fs::File,
    prefix: &str,
    parts: usize,
    names: &mut Vec<String>,
    limit: usize,
) -> io::Result<()> {
    for name in directory_names(directory)? {
        if names.len() >= limit {
            break;
        }
        if name.starts_with(ATOMIC_TEMP_PREFIX) {
            continue;
        }
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if checked_name(&relative).is_err() {
            continue;
        }
        let Some(stat) = entry_stat(directory, &name)? else {
            continue;
        };
        let kind = stat.st_mode & libc::S_IFMT;
        if kind == libc::S_IFDIR && parts < MAX_STORAGE_PARTS {
            let child = open_directory_at(directory, &name, false)?;
            collect_at(&child, &relative, parts + 1, names, limit)?;
        } else if kind == libc::S_IFREG {
            names.push(relative);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_at(location: &FileLocation) -> io::Result<Vec<u8>> {
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let fd = unsafe {
        libc::openat(
            location.directory.as_raw_fd(),
            location.name.as_ptr(),
            flags,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { fs::File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io_denied());
    }
    read_limited(file, metadata.len())
}

fn read_limited(file: fs::File, size: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(size.min((MAX_STORAGE_VALUE_BYTES + 1) as u64) as usize);
    file.take((MAX_STORAGE_VALUE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_STORAGE_VALUE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "storage value too large",
        ));
    }
    Ok(bytes)
}

fn storage_io_error(error: io::Error) -> String {
    if matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied
            | io::ErrorKind::AlreadyExists
            | io::ErrorKind::InvalidInput
    ) {
        "denied: storage path is unsafe".into()
    } else {
        "unavailable: storage I/O failed".into()
    }
}

fn case_clashes(existing: &[String], name: &str) -> bool {
    let folded = name.to_ascii_lowercase();
    existing
        .iter()
        .any(|existing| existing != name && existing.to_ascii_lowercase() == folded)
}

#[cfg(unix)]
fn write_at(location: &FileLocation, bytes: &[u8]) -> io::Result<()> {
    if let Some(stat) = entry_stat(
        &location.directory,
        location.name.to_str().map_err(|_| io_denied())?,
    )? {
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(io_denied());
        }
    }
    let mut random = [0_u8; 12];
    getrandom::fill(&mut random).map_err(|_| io::Error::other("randomness unavailable"))?;
    let temp_name = format!(
        "{ATOMIC_TEMP_PREFIX}{}-{random:02x?}.tmp",
        std::process::id()
    );
    let temp = c_name(&temp_name)?;
    let fd = unsafe {
        libc::openat(
            location.directory.as_raw_fd(),
            temp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = unsafe { fs::File::from_raw_fd(fd) };
    let result = (|| {
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } < 0 {
            return Err(io::Error::last_os_error());
        }
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()?;
        if unsafe {
            libc::renameat(
                location.directory.as_raw_fd(),
                temp.as_ptr(),
                location.directory.as_raw_fd(),
                location.name.as_ptr(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        location.directory.sync_all()
    })();
    if result.is_err() {
        unsafe { libc::unlinkat(location.directory.as_raw_fd(), temp.as_ptr(), 0) };
    }
    result
}

#[cfg(unix)]
fn exists_at(location: &FileLocation) -> io::Result<bool> {
    Ok(entry_stat(
        &location.directory,
        location.name.to_str().map_err(|_| io_denied())?,
    )?
    .is_some_and(|stat| stat.st_mode & libc::S_IFMT == libc::S_IFREG))
}

#[cfg(unix)]
fn delete_at(location: &FileLocation) -> io::Result<bool> {
    let name = location.name.to_str().map_err(|_| io_denied())?;
    let Some(stat) = entry_stat(&location.directory, name)? else {
        return Ok(false);
    };
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(io_denied());
    }
    if unsafe { libc::unlinkat(location.directory.as_raw_fd(), location.name.as_ptr(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

fn read_location(location: &FileLocation) -> io::Result<Vec<u8>> {
    #[cfg(unix)]
    {
        read_at(location)
    }
    #[cfg(not(unix))]
    {
        read_regular_file(&location.path)
    }
}

fn write_location(location: &FileLocation, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        write_at(location, bytes)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(location.path.parent().unwrap())?;
        crate::fs_atomic::write_atomic_lua_private(&location.path, bytes)
    }
}

fn exists_location(location: &FileLocation) -> io::Result<bool> {
    #[cfg(unix)]
    {
        exists_at(location)
    }
    #[cfg(not(unix))]
    {
        match fs::symlink_metadata(&location.path) {
            Ok(metadata) => Ok(metadata.file_type().is_file()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn delete_location(location: &FileLocation) -> io::Result<bool> {
    #[cfg(unix)]
    {
        delete_at(location)
    }
    #[cfg(not(unix))]
    {
        match fs::symlink_metadata(&location.path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                fs::remove_file(&location.path)?;
                Ok(true)
            }
            Ok(_) => Err(io_denied()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn list_location(location: &FileLocation, limit: usize) -> io::Result<Vec<String>> {
    #[cfg(unix)]
    {
        let mut names = Vec::new();
        collect_at(&location.directory, "", 0, &mut names, limit)?;
        names.sort();
        Ok(names)
    }
    #[cfg(not(unix))]
    {
        let mut names = collect_files_if_present(&location.path)?;
        names.truncate(limit);
        names.sort();
        Ok(names)
    }
}

fn file_roots(env: Env<'_>) -> Result<FileRoots, String> {
    let override_root = env("REMUDA_STORAGE_ROOT")
        .filter(|root| !root.is_empty())
        .map(PathBuf::from);
    if override_root
        .as_ref()
        .is_some_and(|root| !root.is_absolute())
    {
        return Err("REMUDA_STORAGE_ROOT must be an absolute path".into());
    }
    [Kind::Config, Kind::Data, Kind::State, Kind::Cache]
        .into_iter()
        .map(|kind| {
            let root = match &override_root {
                Some(root) => root.join(kind.as_str()),
                None => resolve_dir_for(kind.as_str(), cfg!(windows), env)?,
            };
            Ok((kind.as_str(), root))
        })
        .collect()
}

#[cfg(any(not(unix), test))]
fn collect_files(
    base: &std::path::Path,
    dir: &std::path::Path,
    prefix: &str,
    names: &mut Vec<String>,
) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if dir == base && error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_files(base, &path, prefix, names)?;
        } else if kind.is_file()
            && !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(ATOMIC_TEMP_PREFIX))
        {
            if let Some(name) = path.strip_prefix(base).ok().and_then(lua_relative_name) {
                if name.starts_with(prefix)
                    && checked_name(&name).is_ok()
                    && names.len() < MAX_STORAGE_ENTRIES
                {
                    names.push(name);
                }
            }
        }
    }
    Ok(())
}

#[cfg(any(not(unix), test))]
fn collect_files_if_present(base: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    collect_files(base, base, "", &mut names)?;
    Ok(names)
}

#[cfg(any(not(unix), test))]
fn lua_relative_name(path: &Path) -> Option<String> {
    path.components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .map(|components| components.join("/"))
}

#[cfg(not(unix))]
fn read_regular_file(path: &Path) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe file",
        ));
    }
    let file = fs::File::open(path)?;
    read_limited(file, metadata.len())
}

struct StorageView {
    namespace: String,
    files: MemoryFiles,
    roots: Option<Arc<FileRoots>>,
}

impl StorageView {
    fn handle(&self, kind: HandleKind) -> KindHandle {
        KindHandle {
            namespace: self.namespace.clone(),
            kind,
            files: self.files.clone(),
            roots: self.roots.clone(),
        }
    }
}

impl UserData for StorageView {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        for (name, kind) in [
            ("config", HandleKind::Config),
            ("data", HandleKind::Data),
            ("state", HandleKind::State),
            ("cache", HandleKind::Cache),
        ] {
            methods.add_method(name, move |lua, this, ()| {
                lua.create_userdata(this.handle(kind))
            });
        }
        methods.add_method("secret", |lua, this, ()| {
            lua.create_userdata(SecretHandle(this.handle(HandleKind::Secret)))
        });
    }
}

struct KindHandle {
    namespace: String,
    kind: HandleKind,
    files: MemoryFiles,
    roots: Option<Arc<FileRoots>>,
}

impl KindHandle {
    fn file_path(&self, name: &str, create: bool) -> io::Result<FileLocation> {
        if matches!(self.kind, HandleKind::Secret) {
            return Err(io_denied());
        }
        let root = self
            .roots
            .as_ref()
            .and_then(|roots| roots.get(self.kind.as_str()))
            .ok_or_else(|| io::Error::other("storage root unavailable"))?;
        #[cfg(unix)]
        {
            let mut directory = open_storage_root(root, create)?;
            directory = open_directory_at(&directory, "storage", create)?;
            directory = open_directory_at(&directory, &self.namespace, create)?;
            let mut parts = if name.is_empty() {
                Vec::new()
            } else {
                name.split('/').collect::<Vec<_>>()
            };
            let leaf = parts.pop().unwrap_or(".");
            for part in parts {
                directory = open_directory_at(&directory, part, create)?;
            }
            Ok(FileLocation {
                directory,
                name: c_name(leaf)?,
            })
        }
        #[cfg(not(unix))]
        {
            // Windows keeps the plain path implementation; no-follow directory handles are Unix-only.
            let _ = create;
            let mut parent = root.clone();
            let name_parts = name.split('/').collect::<Vec<_>>();
            for component in ["storage", self.namespace.as_str()].into_iter().chain(
                name_parts
                    .iter()
                    .take(name_parts.len().saturating_sub(1))
                    .copied(),
            ) {
                reject_path_case_clash(&parent, component)?;
                parent.push(component);
            }
            Ok(FileLocation {
                path: root.join("storage").join(&self.namespace).join(name),
            })
        }
    }

    fn key(&self, name: String) -> mlua::Result<(String, String, String)> {
        Ok((
            self.namespace.clone(),
            self.kind.as_str().into(),
            checked_name(&name)?,
        ))
    }

    fn read(
        &self,
        lua: &Lua,
        name: String,
    ) -> mlua::Result<(Option<mlua::LuaString>, Option<String>)> {
        let key = self.key(name)?;
        if self.roots.is_some() {
            if matches!(self.kind, HandleKind::Secret) {
                return Ok((
                    None,
                    Some("unavailable: file secrets are disabled until PR C".into()),
                ));
            }
            let location = match self.file_path(&key.2, false) {
                Ok(location) => location,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok((None, Some("not_found".into())))
                }
                Err(error) => return Ok((None, Some(storage_io_error(error)))),
            };
            return match read_location(&location) {
                Ok(bytes) => Ok((Some(lua.create_string(bytes)?), None)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    Ok((None, Some("not_found".into())))
                }
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok((
                    None,
                    Some("denied: storage path is not a regular file".into()),
                )),
                Err(_) => Ok((None, Some("unavailable: storage read failed".into()))),
            };
        }
        let value = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned();
        match value {
            Some(bytes) => Ok((Some(lua.create_string(bytes)?), None)),
            None => Ok((None, Some("not_found".into()))),
        }
    }

    fn write(&self, name: String, bytes: &[u8]) -> mlua::Result<()> {
        if bytes.len() > 1024 * 1024 {
            return Err(mlua::Error::runtime("remuda.storage write exceeds 1 MiB"));
        }
        let key = self.key(name)?;
        if self.roots.is_some() {
            if matches!(self.kind, HandleKind::Secret) {
                return Err(mlua::Error::runtime(
                    "unavailable: file secrets are disabled until PR C",
                ));
            }
            let location = self
                .file_path(&key.2, true)
                .map_err(|error| mlua::Error::runtime(storage_io_error(error)))?;
            let namespace = self
                .file_path("", false)
                .map_err(|error| mlua::Error::runtime(storage_io_error(error)))?;
            let entries = list_location(&namespace, usize::MAX)
                .map_err(|error| mlua::Error::runtime(storage_io_error(error)))?;
            if case_clashes(&entries, &key.2) {
                return Err(mlua::Error::runtime("remuda.storage name collides by case"));
            }
            let exists = exists_location(&location)
                .map_err(|error| mlua::Error::runtime(storage_io_error(error)))?;
            if !exists && entries.len() >= MAX_STORAGE_FILES {
                return Err(mlua::Error::runtime("remuda.storage entry limit reached"));
            }
            write_location(&location, bytes)
                .map_err(|error| mlua::Error::runtime(storage_io_error(error)))?;
            return Ok(());
        }
        let mut files = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let folded = key.2.to_ascii_lowercase();
        if files.keys().any(|(namespace, kind, name)| {
            namespace == &key.0
                && kind == &key.1
                && name != &key.2
                && name.to_ascii_lowercase() == folded
        }) {
            return Err(mlua::Error::runtime("remuda.storage name collides by case"));
        }
        if !files.contains_key(&key)
            && files
                .keys()
                .filter(|(namespace, kind, _)| namespace == &key.0 && kind == &key.1)
                .count()
                >= 1024
        {
            return Err(mlua::Error::runtime("remuda.storage entry limit reached"));
        }
        files.insert(key, bytes.to_vec());
        Ok(())
    }

    fn exists(&self, name: String) -> mlua::Result<bool> {
        let key = self.key(name)?;
        if self.roots.is_some() {
            if matches!(self.kind, HandleKind::Secret) {
                return Ok(false);
            }
            let location = match self.file_path(&key.2, false) {
                Ok(location) => location,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(mlua::Error::runtime(storage_io_error(error))),
            };
            return exists_location(&location)
                .map_err(|error| mlua::Error::runtime(storage_io_error(error)));
        }
        Ok(self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&key))
    }

    fn delete(&self, name: String) -> mlua::Result<(Option<bool>, Option<String>)> {
        let key = self.key(name)?;
        if self.roots.is_some() {
            if matches!(self.kind, HandleKind::Secret) {
                return Ok((
                    None,
                    Some("unavailable: file secrets are disabled until PR C".into()),
                ));
            }
            let location = match self.file_path(&key.2, false) {
                Ok(location) => location,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok((None, Some("not_found".into())))
                }
                Err(error) => return Ok((None, Some(storage_io_error(error)))),
            };
            return match delete_location(&location) {
                Ok(true) => Ok((Some(true), None)),
                Ok(false) => Ok((None, Some("not_found".into()))),
                Err(error) => Ok((None, Some(storage_io_error(error)))),
            };
        }
        if self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key)
            .is_some()
        {
            Ok((Some(true), None))
        } else {
            Ok((None, Some("not_found".into())))
        }
    }

    fn list(&self, lua: &Lua, prefix: String) -> mlua::Result<Table> {
        let prefix = checked_prefix(prefix)?;
        if self.roots.is_some() {
            if matches!(self.kind, HandleKind::Secret) {
                return Err(mlua::Error::runtime(
                    "unavailable: file secrets are disabled until PR C",
                ));
            }
            let location = match self.file_path("", false) {
                Ok(location) => location,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return lua.create_sequence_from(Vec::<String>::new())
                }
                Err(error) => return Err(mlua::Error::runtime(storage_io_error(error))),
            };
            let mut names = list_location(&location, MAX_STORAGE_ENTRIES)
                .map_err(|error| mlua::Error::runtime(storage_io_error(error)))?;
            names.retain(|name| name.starts_with(&prefix));
            names.sort();
            return lua.create_sequence_from(names);
        }
        let names = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .filter(|(namespace, kind, name)| {
                namespace == &self.namespace
                    && kind == self.kind.as_str()
                    && name.starts_with(&prefix)
            })
            .map(|(_, _, name)| name.clone())
            .take(MAX_STORAGE_ENTRIES)
            .collect::<Vec<_>>();
        lua.create_sequence_from(names)
    }
}

impl UserData for KindHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("read", |lua, this, name: String| this.read(lua, name));
        methods.add_method(
            "write",
            |_, this, (name, value): (String, mlua::LuaString)| {
                this.write(name, &value.as_bytes())?;
                Ok((true, None::<String>))
            },
        );
        methods.add_method("exists", |_, this, name: String| this.exists(name));
        methods.add_method("delete", |_, this, name: String| this.delete(name));
        methods.add_method("list", |lua, this, prefix: Option<String>| {
            this.list(lua, prefix.unwrap_or_default())
        });
    }
}

struct SecretHandle(KindHandle);

impl UserData for SecretHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "put",
            |_, this, (name, secret): (String, mlua::LuaString)| {
                if !(1..=2048).contains(&secret.as_bytes().len()) {
                    return Err(mlua::Error::runtime(
                        "remuda.storage.secret.put secret must be 1..=2048 bytes",
                    ));
                }
                // File secrets stay unavailable until PR C adds the protected Windows DACL root.
                if this.0.roots.is_some() {
                    return Ok((
                        None,
                        Some("unavailable: file secrets are disabled until PR C".to_string()),
                    ));
                }
                this.0.write(name, &secret.as_bytes())?;
                Ok((Some(true), None::<String>))
            },
        );
        methods.add_method("get", |lua, this, name: String| this.0.read(lua, name));
        methods.add_method("delete", |_, this, name: String| this.0.delete(name));
        methods.add_method("exists", |_, this, name: String| this.0.exists(name));
    }
}

fn checked_name(name: &str) -> mlua::Result<String> {
    let invalid = name.is_empty()
        || name.split('/').count() > MAX_STORAGE_PARTS
        || name.starts_with('/')
        || name.contains('\\')
        || name.as_bytes().get(1) == Some(&b':') && name.as_bytes()[0].is_ascii_alphabetic()
        || name.split('/').any(|part| {
            let device = part
                .split('.')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            part.is_empty()
                || part == "."
                || part == ".."
                || part.len() > 255
                || part.starts_with(ATOMIC_TEMP_PREFIX)
                || !part.bytes().all(|byte| byte.is_ascii_graphic())
                || part
                    .bytes()
                    .any(|byte| matches!(byte, b':' | b'*' | b'?' | b'<' | b'>' | b'|' | b'"'))
                || part.ends_with('.')
                || matches!(
                    device.as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                )
        });
    if invalid {
        return Err(mlua::Error::runtime(
            "remuda.storage name must be relative, traversal-free printable ASCII, with components up to 255 bytes",
        ));
    }
    Ok(name.into())
}

fn checked_namespace(namespace: &str) -> mlua::Result<String> {
    if namespace.contains('/') {
        return Err(mlua::Error::runtime("remuda.storage invalid namespace"));
    }
    checked_name(namespace)
}

fn checked_prefix(prefix: String) -> mlua::Result<String> {
    if prefix.is_empty() {
        return Ok(prefix);
    }
    let checked = prefix.strip_suffix('/').unwrap_or(&prefix);
    checked_name(checked)?;
    Ok(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "remuda-storage-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn env(values: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let values: HashMap<_, _> = values
            .iter()
            .map(|(key, value)| (key.to_string(), OsString::from(value)))
            .collect();
        move |name| values.get(name).cloned()
    }

    fn xdg_var(kind: &str) -> &'static str {
        Kind::parse(kind).unwrap().xdg_var()
    }

    fn expected_xdg_base(windows: bool, kind: &str) -> PathBuf {
        let base = if windows {
            PathBuf::from(r"C:\xdg\base")
        } else {
            PathBuf::from("/xdg/base")
        };
        let mut expected = base.join("remuda");
        if windows && kind != "data" {
            expected.push(kind);
        }
        expected
    }

    fn expected_localappdata(kind: &str) -> PathBuf {
        let mut expected = PathBuf::from(r"C:\local").join("remuda");
        if kind != "data" {
            expected.push(kind);
        }
        expected
    }

    fn expected_fallback(windows: bool, kind: &str) -> PathBuf {
        if windows {
            let mut expected = PathBuf::from(r"C:\Users\user")
                .join("AppData")
                .join("Local")
                .join("remuda");
            if kind != "data" {
                expected.push(kind);
            }
            expected
        } else {
            let subdir = Kind::parse(kind).unwrap().unix_subdir();
            PathBuf::from("/home/test").join(subdir).join("remuda")
        }
    }

    fn lua_with_storage() -> Lua {
        let lua = Lua::new();
        let remuda = lua.create_table().unwrap();
        remuda.set("storage", bindings(&lua).unwrap()).unwrap();
        lua.globals().set("remuda", remuda).unwrap();
        lua
    }

    fn lua_with_xdg_root(root: &std::path::Path) -> Lua {
        let environment = |name: &str| match name {
            "REMUDA_STORAGE_ROOT" => Some(root.as_os_str().to_owned()),
            _ => None,
        };
        lua_with_env(&environment)
    }

    fn lua_with_env(environment: Env<'_>) -> Lua {
        let lua = Lua::new();
        let remuda = lua.create_table().unwrap();
        remuda
            .set("storage", bindings_with_env(&lua, environment).unwrap())
            .unwrap();
        lua.globals().set("remuda", remuda).unwrap();
        lua
    }

    #[test]
    fn collect_files_if_present_treats_a_missing_root_as_empty() {
        let root = TestRoot::new();
        let missing = root.0.join("not-created");
        assert!(collect_files_if_present(&missing).unwrap().is_empty());
    }

    #[test]
    fn case_clashes_fold_ascii_but_allow_the_same_name() {
        let existing = vec!["token".to_owned()];
        assert!(case_clashes(&existing, "Token"));
        assert!(!case_clashes(&existing, "token"));
        assert!(!case_clashes(&existing, "other"));
    }

    const HANDLE_CONFORMANCE: &str = r#"
        local s = remuda.storage
        local a, b = s.get("suite"):data(), s.get("other"):data()
        local value = string.char(0, 255, 1)
        assert(a:write("mail/a.bin", value) and a:exists("mail/a.bin"), "write and exists mail/a.bin")
        local read, err = a:read("mail/a.bin"); assert(read == value and err == nil, "read mail/a.bin")
        assert(b:read("mail/a.bin") == nil, "namespace isolation")
        assert(a:write("mail/b.bin", "b"), "write mail/b.bin")
        local names = a:list("mail/")
        assert(#names == 2 and names[1] == "mail/a.bin" and names[2] == "mail/b.bin", "list mail/")
        assert(a:delete("mail/a.bin") and not a:exists("mail/a.bin"), "delete mail/a.bin")
        local folded = s.get("folded"):data(); folded:write("token", "x")
        assert(not pcall(function() folded:write("Token", "x") end), "reject case-only name collision")
        local limited = s.get("limited"):data()
        for i = 1, 1024 do limited:write("entry" .. i, "x") end
        assert(not pcall(function() limited:write("overflow", "x") end), "reject 1025th entry")
    "#;

    fn select_backend(lua: &Lua, name: &str) {
        lua.load(format!("assert(remuda.storage.set_default('{name}'))"))
            .exec()
            .unwrap();
    }

    #[test]
    fn lua_relative_name_uses_forward_slashes() {
        let path = PathBuf::from("mail").join("nested").join("a.bin");
        assert_eq!(
            lua_relative_name(&path).as_deref(),
            Some("mail/nested/a.bin")
        );
    }

    #[test]
    fn xdg_namespaces_live_below_storage_subdirectory() {
        let root = TestRoot::new();
        let lua = lua_with_xdg_root(&root.0);
        select_backend(&lua, "xdg");
        lua.load("remuda.storage.get('cluster'):state():write('key', 'value')")
            .exec()
            .unwrap();
        assert!(root.0.join("state/storage/cluster/key").is_file());
        assert!(!root.0.join("state/cluster/key").exists());
        let path: String = lua
            .load("return remuda.storage.path('state', 'key')")
            .eval()
            .unwrap();
        assert_eq!(
            PathBuf::from(path),
            root.0.join("state/storage/default/key")
        );
    }

    #[test]
    fn xdg_list_skips_and_reserves_atomic_temporary_names() {
        let root = TestRoot::new();
        let namespace = root.0.join("data/storage/listed");
        fs::create_dir_all(&namespace).unwrap();
        fs::write(namespace.join("regular"), b"file").unwrap();
        fs::write(namespace.join(".remuda-atomic-123.tmp"), b"temporary").unwrap();
        let lua = lua_with_xdg_root(&root.0);
        select_backend(&lua, "xdg");
        lua.load(
            r#"
            local files = remuda.storage.get("listed"):data()
            local names = files:list()
            assert(#names == 1 and names[1] == "regular")
            assert(not pcall(function() files:write(".remuda-atomic-user.tmp", "x") end))
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn xdg_secret_operations_stay_unavailable_without_creating_files() {
        let root = TestRoot::new();
        let lua = lua_with_xdg_root(&root.0);
        select_backend(&lua, "xdg");
        lua.load(
            r#"
            local secret = remuda.storage.get("secrets"):secret()
            local value, reason = secret:put("token", "value")
            assert(value == nil and reason:match("^unavailable:"))
            value, reason = secret:get("token"); assert(value == nil and reason:match("^unavailable:"))
            value, reason = secret:delete("token"); assert(value == nil and reason:match("^unavailable:"))
            assert(secret:exists("token") == false)
            "#,
        )
        .exec()
        .unwrap();
        assert!(fs::read_dir(&root.0).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn xdg_read_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let root = TestRoot::new();
        let namespace = root.0.join("data/storage/reader");
        fs::create_dir_all(&namespace).unwrap();
        fs::write(namespace.join("target"), b"secret").unwrap();
        symlink(namespace.join("target"), namespace.join("linked")).unwrap();
        let lua = lua_with_xdg_root(&root.0);
        select_backend(&lua, "xdg");
        lua.load(
            r#"local value, reason = remuda.storage.get("reader"):data():read("linked"); assert(value == nil and reason)"#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn xdg_read_rejects_files_larger_than_one_mib() {
        let root = TestRoot::new();
        let namespace = root.0.join("data/storage/reader");
        fs::create_dir_all(&namespace).unwrap();
        fs::write(namespace.join("large"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
        let lua = lua_with_xdg_root(&root.0);
        select_backend(&lua, "xdg");
        lua.load(
            r#"local value, reason = remuda.storage.get("reader"):data():read("large"); assert(value == nil and reason)"#,
        )
        .exec()
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn xdg_read_does_not_block_on_fifo() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::time::Duration;

        let root = TestRoot::new();
        let namespace = root.0.join("data/storage/reader");
        fs::create_dir_all(&namespace).unwrap();
        let fifo = namespace.join("pipe");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let root_path = root.0.clone();
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let lua = lua_with_xdg_root(&root_path);
            select_backend(&lua, "xdg");
            let result = lua
                .load(r#"local value, reason = remuda.storage.get("reader"):data():read("pipe"); assert(value == nil and reason)"#)
                .exec();
            let _ = send.send(result.is_ok());
        });
        assert!(receive.recv_timeout(Duration::from_secs(2)).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn xdg_refuses_symlinked_namespace_directory() {
        use std::os::unix::fs::symlink;

        let root = TestRoot::new();
        let storage = root.0.join("data/storage");
        let outside = root.0.join("outside");
        fs::create_dir_all(&storage).unwrap();
        fs::create_dir(&outside).unwrap();
        symlink(&outside, storage.join("linked")).unwrap();
        let lua = lua_with_xdg_root(&root.0);
        select_backend(&lua, "xdg");
        lua.load(r#"assert(not pcall(function() remuda.storage.get("linked"):data():write("new", "x") end))"#)
            .exec()
            .unwrap();
        assert!(!outside.join("new").exists());
    }

    #[cfg(unix)]
    #[test]
    fn xdg_refuses_symlinked_files_for_all_handle_operations() {
        use std::os::unix::fs::symlink;

        let root = TestRoot::new();
        let namespace = root.0.join("data/storage/linked");
        fs::create_dir_all(&namespace).unwrap();
        fs::write(namespace.join("target"), b"keep").unwrap();
        symlink(namespace.join("target"), namespace.join("link")).unwrap();
        let lua = lua_with_xdg_root(&root.0);
        select_backend(&lua, "xdg");
        lua.load(
            r#"
            local f = remuda.storage.get("linked"):data()
            local ok, exists = pcall(function() return f:exists("link") end)
            assert(not ok or exists == false)
            local value, reason = f:read("link"); assert(value == nil and reason)
            assert(not pcall(function() f:write("link", "replace") end))
            local deleted, reason = f:delete("link"); assert(deleted == nil and reason)
            "#,
        )
        .exec()
        .unwrap();
        assert_eq!(fs::read(namespace.join("target")).unwrap(), b"keep");
        assert!(namespace.join("link").is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn xdg_refuses_directory_symlink_swapped_between_operations() {
        use std::os::unix::fs::symlink;

        let root = TestRoot::new();
        let namespace = root.0.join("data/storage/race");
        let outside = root.0.join("outside");
        fs::create_dir_all(namespace.join("part")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(namespace.join("part/item"), b"inside").unwrap();
        fs::write(outside.join("item"), b"outside").unwrap();
        fs::remove_file(namespace.join("part/item")).unwrap();
        fs::remove_dir(namespace.join("part")).unwrap();
        symlink(&outside, namespace.join("part")).unwrap();
        let lua = lua_with_xdg_root(&root.0);
        select_backend(&lua, "xdg");
        lua.load(
            r#"local value, reason = remuda.storage.get("race"):data():read("part/item"); assert(value == nil and reason)"#,
        )
        .exec()
        .unwrap();
        assert_eq!(fs::read(outside.join("item")).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[test]
    fn xdg_directories_and_existing_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestRoot::new();
        let lua = lua_with_xdg_root(&root.0);
        select_backend(&lua, "xdg");
        lua.load(r#"remuda.storage.get("mode"):data():write("item", "first")"#)
            .exec()
            .unwrap();
        let namespace = root.0.join("data/storage/mode");
        let file = namespace.join("item");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        lua.load(r#"remuda.storage.get("mode"):data():write("item", "second")"#)
            .exec()
            .unwrap();
        for dir in [root.0.join("data"), root.0.join("data/storage"), namespace] {
            assert_eq!(
                fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn xdg_rejects_case_clashes_in_directory_components() {
        let root = TestRoot::new();
        let lua = lua_with_xdg_root(&root.0);
        select_backend(&lua, "xdg");
        lua.load(
            r#"
            local files = remuda.storage.get("case"):data()
            files:write("Mail/a", "a")
            assert(not pcall(function() files:write("mail/b", "b") end))
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn xdg_rejects_more_than_eight_name_parts() {
        let root = TestRoot::new();
        let lua = lua_with_xdg_root(&root.0);
        select_backend(&lua, "xdg");
        lua.load(
            r#"assert(not pcall(function() remuda.storage.get("parts"):data():write("a/b/c/d/e/f/g/h/i", "x") end))"#,
        )
        .exec()
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn xdg_list_caps_entries_at_1024() {
        let root = TestRoot::new();
        let namespace = root.0.join("data/storage/listed");
        fs::create_dir_all(&namespace).unwrap();
        for index in 0..1025 {
            fs::write(namespace.join(format!("entry{index}")), b"x").unwrap();
        }
        let lua = lua_with_xdg_root(&root.0);
        select_backend(&lua, "xdg");
        let count: usize = lua
            .load(r#"return #remuda.storage.get("listed"):data():list()"#)
            .eval()
            .unwrap();
        assert!(count <= 1024);
    }

    #[cfg(unix)]
    #[test]
    fn xdg_list_never_returns_names_rejected_by_the_api() {
        let root = TestRoot::new();
        let namespace = root.0.join("data/storage/listed");
        fs::create_dir_all(&namespace).unwrap();
        fs::write(namespace.join("CON"), b"x").unwrap();
        let lua = lua_with_xdg_root(&root.0);
        select_backend(&lua, "xdg");
        let names: Vec<String> = lua
            .load(r#"return remuda.storage.get("listed"):data():list()"#)
            .eval()
            .unwrap();
        assert!(names.iter().all(|name| checked_name(name).is_ok()));
    }

    fn run_handle_conformance(lua: &Lua, backend: &str) {
        select_backend(lua, backend);
        lua.load(HANDLE_CONFORMANCE).exec().unwrap();
    }

    #[test]
    fn handle_conformance_runs_against_memory_and_xdg_file_backends() {
        let memory = lua_with_storage();
        run_handle_conformance(&memory, "memory");

        let root = TestRoot::new();
        let file = lua_with_xdg_root(&root.0);
        run_handle_conformance(&file, "xdg");
        assert_eq!(
            fs::read(root.0.join("data/storage/suite/mail/b.bin")).unwrap(),
            b"b"
        );
    }

    #[cfg(unix)]
    #[test]
    fn xdg_list_skips_directories_and_symlinks() {
        use std::os::unix::fs::symlink;

        let root = TestRoot::new();
        let directory = root.0.join("data/storage/listed");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("regular"), b"file").unwrap();
        fs::create_dir(directory.join("nested")).unwrap();
        symlink(directory.join("regular"), directory.join("linked")).unwrap();
        let lua = lua_with_xdg_root(&root.0);
        select_backend(&lua, "xdg");
        lua.load(
            r#"local names = remuda.storage.get("listed"):data():list(); assert(#names == 1 and names[1] == "regular")"#,
        )
        .exec()
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn xdg_without_root_uses_the_kind_resolver() {
        let root = TestRoot::new();
        let data_home = root.0.join("xdg-data");
        let environment = |name: &str| match name {
            "XDG_CONFIG_HOME" | "XDG_DATA_HOME" | "XDG_STATE_HOME" | "XDG_CACHE_HOME" => {
                Some(data_home.as_os_str().to_owned())
            }
            _ => None,
        };
        let lua = lua_with_env(&environment);
        select_backend(&lua, "xdg");
        lua.load("remuda.storage.get('resolved'):data():write('item', 'x')")
            .exec()
            .unwrap();
        assert_eq!(
            fs::read(data_home.join("remuda/storage/resolved/item")).unwrap(),
            b"x"
        );
    }

    #[test]
    fn memory_handles_isolate_namespaces_and_support_blob_operations() {
        let lua = lua_with_storage();
        lua.load(
            r#"
            local s = remuda.storage
            local a, b = s.get("first"):config(), s.get("second"):config(); local state = s.get("first"):state()
            local bytes = string.char(0, 255, 1); assert(a:write("mail/a.bin", bytes) and a:exists("mail/a.bin")); local value, err = a:read("mail/a.bin"); assert(value == bytes and err == nil)
            assert(b:read("mail/a.bin") == nil and state:read("mail/a.bin") == nil); local missing, reason = a:read("missing"); assert(missing == nil and reason == "not_found")
            a:write("mail/b.bin", "b"); local names = a:list("mail/"); assert(#names == 2 and names[1] == "mail/a.bin" and names[2] == "mail/b.bin")
            assert(a:delete("mail/a.bin") and not a:exists("mail/a.bin"))
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn memory_handles_reject_unsafe_names_and_namespace_aliases() {
        let lua = lua_with_storage();
        lua.load(
            r#"
            local s = remuda.storage
            for _, namespace in ipairs({"", ".", "..", "a/b", "a\\b"}) do
                assert(not pcall(function() s.get(namespace) end))
            end
            local files = s.get("safe"):data()
            for _, name in ipairs({"colon:name", "star*name", "question?name", "less<name", "greater>name", "pipe|name", 'quote"name', "trail.", "trail ", "CON", "prn", "AUX.txt", "nul.txt", "COM1", "com9.log", "LPT1", "lpt9.log"}) do
                assert(not pcall(function() files:write(name, "x") end), name)
            end
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn memory_writes_enforce_folded_names_and_size_limits_without_echoing_input() {
        let lua = lua_with_storage();
        lua.load(
            r#"
            local files = remuda.storage.get("safe"):data()
            files:write("token", "x")
            local ok, err = pcall(function() files:write("Token", "x") end)
            assert(not ok and not tostring(err):find("Token", 1, true))
            local payload = string.rep("s", 1024 * 1024 + 1)
            ok, err = pcall(function() files:write("large", payload) end)
            assert(not ok and not tostring(err):find(payload, 1, true) and not tostring(err):find("large", 1, true))
            local limited = remuda.storage.get("limit"):data()
            for i = 1, 1024 do limited:write("entry" .. i, "x") end
            ok, err = pcall(function() limited:write("overflow", "secret-bytes") end)
            assert(not ok and not tostring(err):find("overflow", 1, true) and not tostring(err):find("secret-bytes", 1, true))
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn memory_secret_handles_validate_names_and_have_no_path() {
        let lua = lua_with_storage();
        lua.load(
            r#"
            local storage = remuda.storage; storage.set_default("memory")
            local secret = storage.default():secret()
            assert(secret:put("matrix_token", "bytes") == true); local value, err = secret:get("matrix_token"); assert(value == "bytes" and err == nil)
            local other, reason = storage.get("other"):secret():get("matrix_token"); assert(other == nil and reason == "not_found")
            assert(secret:delete("matrix_token") == true); assert(storage.backend() == "memory" and storage.path("secret", "matrix_token") == nil)
            for _, name in ipairs({"../escape", "a/../b", "/absolute", "C:/drive", string.rep("x", 256)}) do
                assert(not pcall(function() secret:put(name, "x") end), name)
            end
            assert(not pcall(function() secret:put("too_large", string.rep("x", 2049)) end))
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn init_lua_path_uses_windows_user_profile_without_home() {
        let environment = env(&[("USERPROFILE", r"C:\Users\user")]);
        let expected = PathBuf::from(r"C:\Users\user")
            .join("AppData")
            .join("Local")
            .join("remuda")
            .join("config")
            .join("init.lua");
        assert_eq!(
            user_file_path_for(Kind::Config, "init.lua", true, &environment),
            Some(expected)
        );

        let xdg_environment = env(&[
            ("XDG_CONFIG_HOME", r"D:\config"),
            ("USERPROFILE", r"C:\Users\user"),
        ]);
        assert_eq!(
            user_file_path_for(Kind::Config, "init.lua", true, &xdg_environment),
            Some(
                PathBuf::from(r"D:\config")
                    .join("remuda")
                    .join("config")
                    .join("init.lua")
            )
        );
    }

    #[test]
    fn repl_history_path_uses_windows_user_profile_without_home() {
        let environment = env(&[("USERPROFILE", r"C:\Users\user")]);
        let expected = PathBuf::from(r"C:\Users\user")
            .join("AppData")
            .join("Local")
            .join("remuda")
            .join("state")
            .join("repl-history");
        assert_eq!(
            user_file_path_for(Kind::State, "repl-history", true, &environment),
            Some(expected)
        );

        let xdg_environment = env(&[
            ("XDG_STATE_HOME", r"D:\state"),
            ("USERPROFILE", r"C:\Users\user"),
        ]);
        assert_eq!(
            user_file_path_for(Kind::State, "repl-history", true, &xdg_environment),
            Some(
                PathBuf::from(r"D:\state")
                    .join("remuda")
                    .join("state")
                    .join("repl-history")
            )
        );
    }

    #[test]
    fn resolver_table_covers_platforms_kinds_and_environment_rules() {
        let kinds = ["config", "data", "state", "cache"];
        for (platform, windows) in [("linux", false), ("macos", false), ("windows", true)] {
            for kind in kinds {
                let xdg_name = xdg_var(kind);
                let xdg_value = if windows { r"C:\xdg\base" } else { "/xdg/base" };
                let cases = [
                    (
                        env(&[(xdg_name, xdg_value)]),
                        expected_xdg_base(windows, kind),
                    ),
                    (
                        env(&[
                            (xdg_name, ""),
                            ("HOME", "/home/test"),
                            ("LOCALAPPDATA", r"C:\local"),
                        ]),
                        if windows {
                            expected_localappdata(kind)
                        } else {
                            expected_fallback(windows, kind)
                        },
                    ),
                    (
                        env(&[
                            (xdg_name, "relative"),
                            ("HOME", "/home/test"),
                            ("LOCALAPPDATA", r"C:\local"),
                        ]),
                        if windows {
                            expected_localappdata(kind)
                        } else {
                            expected_fallback(windows, kind)
                        },
                    ),
                ];
                for (environment, expected) in cases {
                    assert_eq!(
                        resolve_dir_for(kind, windows, &environment),
                        Ok(expected),
                        "kind={kind}, platform={platform}"
                    );
                }
            }
        }
        for kind in kinds {
            assert_eq!(
                resolve_dir_for(kind, false, &env(&[])),
                Err("unavailable: HOME is not set".to_string()),
                "missing Unix HOME for {kind}"
            );
            assert_eq!(
                resolve_dir_for(kind, true, &env(&[("USERPROFILE", r"C:\Users\user")])),
                Ok(expected_fallback(true, kind)),
                "Windows profile fallback for {kind}"
            );
            assert!(resolve_dir_for(kind, true, &env(&[]))
                .unwrap_err()
                .starts_with("unavailable: "));
        }
    }

    #[test]
    fn lua_dir_returns_a_path_for_the_host() {
        let process_env = |name: &str| -> Option<OsString> { std::env::var_os(name) };
        let configured = env_absolute(&process_env, "XDG_DATA_HOME", cfg!(windows)).is_some()
            || if cfg!(windows) {
                env_absolute(&process_env, "LOCALAPPDATA", true).is_some()
                    || env_absolute(&process_env, "USERPROFILE", true).is_some()
            } else {
                env_absolute(&process_env, "HOME", false).is_some()
            };
        if !configured {
            return;
        }
        let lua = Lua::new();
        let remuda = lua.create_table().unwrap();
        remuda.set("storage", bindings(&lua).unwrap()).unwrap();
        lua.globals().set("remuda", remuda).unwrap();
        let path: String = lua
            .load("return remuda.storage.dir('data')")
            .eval()
            .unwrap();
        assert!(is_absolute_for(std::path::Path::new(&path), cfg!(windows)));
    }

    #[test]
    fn absoluteness_follows_the_target_platform_not_the_host() {
        let abs = |path: &str, windows| is_absolute_for(std::path::Path::new(path), windows);
        assert!(abs("/h/.local", false));
        assert!(!abs("rel/dir", false));
        assert!(!abs(r"C:\x", false));
        assert!(abs(r"C:\x", true));
        assert!(!abs("/h", true));
    }

    #[test]
    fn lua_dir_raises_for_an_unknown_kind() {
        let lua = Lua::new();
        let remuda = lua.create_table().unwrap();
        remuda.set("storage", bindings(&lua).unwrap()).unwrap();
        lua.globals().set("remuda", remuda).unwrap();
        let accepted: bool = lua
            .load("local ok = pcall(remuda.storage.dir, 'secret'); return ok")
            .eval()
            .unwrap();
        assert!(!accepted, "unknown kind must raise a usage error");
    }
}
