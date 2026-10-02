//! Per-kind user directory resolution and Lua bindings.

use mlua::{Lua, Table};
use std::ffi::OsString;
use std::path::PathBuf;

type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

fn resolve_dir_for(_kind: &str, _windows: bool, _env: Env<'_>) -> Result<PathBuf, String> {
    Err("unavailable: storage resolver stub".to_string())
}

pub(crate) fn bindings(lua: &Lua) -> mlua::Result<Table> {
    let storage = lua.create_table()?;
    storage.set(
        "dir",
        lua.create_function(|_, kind: String| {
            match resolve_dir_for(&kind, cfg!(windows), &|name| std::env::var_os(name)) {
                Ok(path) => Ok((Some(path.to_string_lossy().into_owned()), None::<String>)),
                Err(error) => Ok((None::<String>, Some(error))),
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

    #[test]
    fn resolver_table_covers_platforms_kinds_and_environment_rules() {
        let kinds = ["config", "data", "state", "cache"];
        for windows in [false, true] {
            for kind in kinds {
                let xdg_name = match kind {
                    "config" => "XDG_CONFIG_HOME",
                    "data" => "XDG_DATA_HOME",
                    "state" => "XDG_STATE_HOME",
                    "cache" => "XDG_CACHE_HOME",
                    _ => unreachable!(),
                };
                let unix_sub = match kind {
                    "config" => ".config",
                    "data" => ".local/share",
                    "state" => ".local/state",
                    "cache" => ".cache",
                    _ => unreachable!(),
                };
                let kind_suffix = if windows && kind != "data" {
                    format!("/{kind}")
                } else {
                    String::new()
                };
                let cases = [
                    (
                        env(&[(xdg_name, "/xdg/base")]),
                        format!("/xdg/base/remuda{kind_suffix}"),
                    ),
                    (
                        env(&[
                            (xdg_name, ""),
                            ("HOME", "/home/test"),
                            ("LOCALAPPDATA", "/local"),
                        ]),
                        if windows {
                            format!("/local/remuda{kind_suffix}")
                        } else {
                            format!("/home/test/{unix_sub}/remuda")
                        },
                    ),
                    (
                        env(&[
                            (xdg_name, "relative"),
                            ("HOME", "/home/test"),
                            ("LOCALAPPDATA", "/local"),
                        ]),
                        if windows {
                            format!("/local/remuda{kind_suffix}")
                        } else {
                            format!("/home/test/{unix_sub}/remuda")
                        },
                    ),
                ];
                for (environment, expected) in cases {
                    assert_eq!(
                        resolve_dir_for(kind, windows, &environment)
                            .map(|path| path.to_string_lossy().into_owned()),
                        Ok(expected),
                        "kind={kind}, windows={windows}"
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
                Ok(PathBuf::from(format!(
                    r"C:\Users\user\AppData\Local\remuda{}",
                    if kind == "data" {
                        String::new()
                    } else {
                        format!(r"\{kind}")
                    }
                ))),
                "Windows profile fallback for {kind}"
            );
        }
    }

    #[test]
    fn lua_dir_returns_a_path_for_the_host() {
        let lua = Lua::new();
        let remuda = lua.create_table().unwrap();
        remuda.set("storage", bindings(&lua).unwrap()).unwrap();
        lua.globals().set("remuda", remuda).unwrap();
        let path: String = lua
            .load("return remuda.storage.dir('data')")
            .eval()
            .unwrap();
        assert!(PathBuf::from(path).is_absolute());
    }
}
