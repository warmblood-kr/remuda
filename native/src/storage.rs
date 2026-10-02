//! Per-kind user directory resolution and Lua bindings.

use mlua::{Lua, Table, UserData, UserDataMethods};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
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
    let storage = lua.create_table()?;
    storage.set(
        "dir",
        lua.create_function(|_, kind: String| {
            match resolve_dir_for(&kind, cfg!(windows), &|name| std::env::var_os(name)) {
                Ok(path) => match path.into_os_string().into_string() {
                    Ok(path) => Ok((Some(path), None::<String>)),
                    Err(_) => Ok((
                        None::<String>,
                        Some("unavailable: resolved path is not valid UTF-8".to_string()),
                    )),
                },
                Err(error) if error.starts_with("unavailable: ") => {
                    Ok((None::<String>, Some(error)))
                }
                Err(error) => Err(mlua::Error::runtime(error)),
            }
        })?,
    )?;
    let files = Arc::new(Mutex::new(BTreeMap::new()));
    let get_files = files.clone();
    storage.set(
        "get",
        lua.create_function(move |lua, namespace: String| {
            let namespace = checked_namespace(&namespace)?;
            lua.create_userdata(StorageView {
                namespace,
                files: get_files.clone(),
            })
        })?,
    )?;
    storage.set(
        "default",
        lua.create_function(move |lua, ()| {
            lua.create_userdata(StorageView {
                namespace: "default".into(),
                files: files.clone(),
            })
        })?,
    )?;
    storage.set(
        "set_default",
        lua.create_function(|_, backend: String| {
            (backend == "memory")
                .then_some(true)
                .ok_or_else(|| mlua::Error::runtime("only the memory backend is available"))
        })?,
    )?;
    storage.set("backend", lua.create_function(|_, ()| Ok("memory"))?)?;
    storage.set(
        "path",
        lua.create_function(|_, (kind, name): (String, String)| {
            if !matches!(
                kind.as_str(),
                "config" | "data" | "state" | "cache" | "secret"
            ) {
                return Err(mlua::Error::runtime("remuda.storage.path: unknown kind"));
            }
            checked_name(&name)?;
            Ok(None::<String>)
        })?,
    )?;
    Ok(storage)
}

#[cfg(test)]
fn bindings_with_env(lua: &Lua, _env: Env<'_>) -> mlua::Result<Table> {
    bindings(lua)
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

struct StorageView {
    namespace: String,
    files: MemoryFiles,
}

impl StorageView {
    fn handle(&self, kind: HandleKind) -> KindHandle {
        KindHandle {
            namespace: self.namespace.clone(),
            kind,
            files: self.files.clone(),
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
}

impl KindHandle {
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
        let value = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.key(name)?)
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
        Ok(self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&self.key(name)?))
    }

    fn delete(&self, name: String) -> mlua::Result<(Option<bool>, Option<String>)> {
        if self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key(name)?)
            .is_some()
        {
            Ok((Some(true), None))
        } else {
            Ok((None, Some("not_found".into())))
        }
    }

    fn list(&self, lua: &Lua, prefix: String) -> mlua::Result<Table> {
        let prefix = checked_prefix(prefix)?;
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
                this.0.write(name, &secret.as_bytes())?;
                Ok(true)
            },
        );
        methods.add_method("get", |lua, this, name: String| this.0.read(lua, name));
        methods.add_method("delete", |_, this, name: String| this.0.delete(name));
    }
}

fn checked_name(name: &str) -> mlua::Result<String> {
    let invalid = name.is_empty()
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

    const HANDLE_CONFORMANCE: &str = r#"
        local s = remuda.storage
        local a, b = s.get("suite"):data(), s.get("other"):data()
        local value = string.char(0, 255, 1)
        assert(a:write("mail/a.bin", value) and a:exists("mail/a.bin"))
        local read, err = a:read("mail/a.bin"); assert(read == value and err == nil)
        assert(b:read("mail/a.bin") == nil)
        assert(a:write("mail/b.bin", "b"))
        local names = a:list("mail/")
        assert(#names == 2 and names[1] == "mail/a.bin" and names[2] == "mail/b.bin")
        assert(a:delete("mail/a.bin") and not a:exists("mail/a.bin"))
    "#;

    fn select_backend(lua: &Lua, name: &str) {
        lua.load(format!("assert(remuda.storage.set_default('{name}'))"))
            .exec()
            .unwrap();
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
            fs::read(root.0.join("data/suite/mail/b.bin")).unwrap(),
            b"b"
        );
    }

    #[cfg(unix)]
    #[test]
    fn xdg_list_skips_directories_and_symlinks() {
        use std::os::unix::fs::symlink;

        let root = TestRoot::new();
        let directory = root.0.join("data/listed");
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
            "XDG_DATA_HOME" => Some(data_home.as_os_str().to_owned()),
            _ => None,
        };
        let lua = lua_with_env(&environment);
        select_backend(&lua, "xdg");
        lua.load("remuda.storage.get('resolved'):data():write('item', 'x')")
            .exec()
            .unwrap();
        assert_eq!(
            fs::read(data_home.join("remuda/resolved/item")).unwrap(),
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
