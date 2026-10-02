//! `remuda.system.credential`: small secrets in the OS credential store.
//!
//! The service is always `"remuda"` and the account is the caller's name.
//! Bad arguments raise a Lua error; a store that cannot answer returns
//! `nil, reason`, where reason starts with `not_found`, `unavailable: ` or
//! `denied: `. No error or reason ever carries the secret.

use mlua::{Lua, Table, Value};

const MAX_NAME_BYTES: usize = 255;
// Inside the Windows credential blob limit of 2560 bytes.
const MAX_SECRET_BYTES: usize = 2048;

fn checked_name(word: &str, value: &Value) -> mlua::Result<String> {
    let name = match value {
        Value::String(name) => name.as_bytes().to_vec(),
        _ => Vec::new(),
    };
    if !(1..=MAX_NAME_BYTES).contains(&name.len()) || !name.iter().all(u8::is_ascii_graphic) {
        return Err(mlua::Error::runtime(format!(
            "remuda.system.credential.{word} name must be a string of 1..=255 printable ASCII characters, no spaces"
        )));
    }
    // ASCII graphic bytes are always valid UTF-8.
    Ok(String::from_utf8_lossy(&name).into_owned())
}

fn checked_secret(value: &Value) -> mlua::Result<mlua::BorrowedBytes> {
    match value {
        Value::String(secret) if (1..=MAX_SECRET_BYTES).contains(&secret.as_bytes().len()) => {
            Ok(secret.as_bytes())
        }
        _ => Err(mlua::Error::runtime(
            "remuda.system.credential.put secret must be a string of 1..=2048 bytes",
        )),
    }
}

#[cfg(target_os = "macos")]
mod store {
    use zeroize::Zeroizing;

    pub(super) const BACKEND: Option<&str> = Some("keychain");

    // errSecUserCanceled and errSecAuthFailed: the user refused the prompt.
    const DENIED: [i32; 2] = [-128, -25293];

    fn reason(error: keyring::Error) -> String {
        match error {
            keyring::Error::NoEntry => "not_found".to_string(),
            keyring::Error::PlatformFailure(error)
                if error
                    .downcast_ref::<security_framework::base::Error>()
                    .is_some_and(|error| DENIED.contains(&error.code())) =>
            {
                format!("denied: {error}")
            }
            // A locked keychain in an SSH session lands here
            // (errSecInteractionNotAllowed), and so does every other failure.
            keyring::Error::PlatformFailure(error) | keyring::Error::NoStorageAccess(error) => {
                format!("unavailable: {error}")
            }
            // Display only: these variants' Debug output could hold stored bytes.
            other => format!("unavailable: {other}"),
        }
    }

    fn entry(name: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new("remuda", name).map_err(reason)
    }

    pub(super) fn put(name: &str, secret: &[u8]) -> Result<(), String> {
        entry(name)?.set_secret(secret).map_err(reason)
    }

    pub(super) fn get(name: &str) -> Result<Zeroizing<Vec<u8>>, String> {
        entry(name)?
            .get_secret()
            .map(Zeroizing::new)
            .map_err(reason)
    }

    pub(super) fn delete(name: &str) -> Result<(), String> {
        entry(name)?.delete_credential().map_err(reason)
    }
}

#[cfg(not(target_os = "macos"))]
mod store {
    pub(super) const BACKEND: Option<&str> = None;

    fn unavailable<T>() -> Result<T, String> {
        Err("unavailable: no credential store on this OS".to_string())
    }

    pub(super) fn put(_name: &str, _secret: &[u8]) -> Result<(), String> {
        unavailable()
    }

    pub(super) fn get(_name: &str) -> Result<Vec<u8>, String> {
        unavailable()
    }

    pub(super) fn delete(_name: &str) -> Result<(), String> {
        unavailable()
    }
}

/// The `remuda.system` table: `credential.{put,get,delete,backend}`.
pub(crate) fn bindings(lua: &Lua) -> mlua::Result<Table> {
    let credential = lua.create_table()?;
    credential.set(
        "put",
        lua.create_function(|_, (name, secret): (Value, Value)| {
            let name = checked_name("put", &name)?;
            let secret = checked_secret(&secret)?;
            Ok(match store::put(&name, &secret) {
                Ok(()) => (Some(true), None),
                Err(reason) => (None, Some(reason)),
            })
        })?,
    )?;
    credential.set(
        "get",
        lua.create_function(|lua, name: Value| {
            let name = checked_name("get", &name)?;
            Ok(match store::get(&name) {
                Ok(secret) => (Some(lua.create_string(secret.as_slice())?), None),
                Err(reason) => (None, Some(reason)),
            })
        })?,
    )?;
    credential.set(
        "delete",
        lua.create_function(|_, name: Value| {
            let name = checked_name("delete", &name)?;
            Ok(match store::delete(&name) {
                Ok(()) => (Some(true), None),
                Err(reason) => (None, Some(reason)),
            })
        })?,
    )?;
    credential.set("backend", lua.create_function(|_, ()| Ok(store::BACKEND))?)?;

    let system = lua.create_table()?;
    system.set("credential", credential)?;
    Ok(system)
}

#[cfg(test)]
mod tests {
    use super::{checked_name, checked_secret};
    use mlua::{Lua, Value};

    // The accepting side of the limits; the rejecting side is in
    // tests/script.rs, where a valid call would reach the real store.
    #[test]
    fn the_longest_name_and_secret_pass_validation() {
        let lua = Lua::new();
        let string = |text: String| Value::String(lua.create_string(text).unwrap());
        for name in [
            "!".to_string(),
            "~".repeat(255),
            "butler/matrix/@bot:server/password".into(),
        ] {
            assert_eq!(checked_name("put", &string(name.clone())).unwrap(), name);
        }
        for length in [1, 2048] {
            let secret = string("\0".repeat(length));
            assert_eq!(checked_secret(&secret).unwrap().len(), length);
        }
    }
}
