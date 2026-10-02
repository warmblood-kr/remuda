//! Per-kind user directory resolution and Lua bindings.

use mlua::{Lua, Table};
use std::ffi::OsString;
use std::path::PathBuf;

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
    Ok(storage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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

    #[test]
    fn memory_handles_isolate_namespaces_and_support_blob_operations() {
        let lua = lua_with_storage();
        lua.load(
            r#"
            local storage = remuda.storage
            storage.set_default("memory")
            local first = storage.get("first"):config()
            local second = storage.get("second"):config()
            local state = storage.get("first"):state()
            local bytes = string.char(0, 255, 1)
            assert(first:write("mail/a.bin", bytes))
            assert(first:exists("mail/a.bin"))
            local value, err = first:read("mail/a.bin")
            assert(value == bytes and err == nil)
            assert(second:read("mail/a.bin") == nil)
            assert(state:read("mail/a.bin") == nil)
            local missing, reason = first:read("missing")
            assert(missing == nil and reason == "not_found")
            first:write("mail/b.bin", "b")
            local names = first:list("mail/")
            assert(#names == 2 and names[1] == "mail/a.bin" and names[2] == "mail/b.bin")
            assert(first:delete("mail/a.bin"))
            assert(not first:exists("mail/a.bin"))
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
            local storage = remuda.storage
            storage.set_default("memory")
            local secret = storage.default():secret()
            assert(secret:put("matrix_token", "bytes") == true)
            local value, err = secret:get("matrix_token")
            assert(value == "bytes" and err == nil)
            local other, reason = storage.get("other"):secret():get("matrix_token")
            assert(other == nil and reason == "not_found")
            assert(secret:delete("matrix_token") == true)
            assert(storage.backend() == "memory")
            assert(storage.path("secret", "matrix_token") == nil)
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
