//! `remuda.system.credential`: small secrets in the OS credential store.
//!
//! The service is always `"remuda"` and the account is the caller's name; on
//! Windows that is user name `remuda` and target name `remuda:` + name.
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
    use security_framework::base::Error;
    use security_framework::os::macos::keychain::SecKeychain;
    use security_framework::passwords;
    use std::sync::Mutex;
    use zeroize::Zeroizing;

    pub(super) const BACKEND: Option<&str> = Some("keychain");

    const SERVICE: &str = "remuda";
    const ITEM_NOT_FOUND: i32 = -25300;
    // errSecUserCanceled and errSecAuthFailed: the user refused the prompt.
    const DENIED: [i32; 2] = [-128, -25293];

    const NO_DEFAULT_KEYCHAIN: &str =
        "unavailable: no default keychain. Fix: if macOS asks, Cancel is safe (the password goes to the private file) and Reset to Defaults creates a new login keychain; or pass --dir";
    const KEYCHAIN_LOCKED: &str =
        "unavailable: keychain locked or no GUI session. Fix: unlock it (or log in on the console) or pass --dir";

    // SecKeychainSetUserInteractionAllowed is process-global. Serialize all
    // credential calls while it is disabled so another credential operation
    // cannot observe a state changed by its neighbor.
    static USER_INTERACTION_MUTEX: Mutex<()> = Mutex::new(());

    // errSecNotAvailable, errSecNoSuchKeychain and errSecNoDefaultKeychain:
    // there is no keychain to store into. errSecInteractionNotAllowed (a
    // locked keychain in an SSH session) is `unavailable` too, with its own
    // fix, like every failure that is not a miss or a refusal.
    fn reason(error: Error) -> String {
        match error.code() {
            ITEM_NOT_FOUND => "not_found".to_string(),
            -128 => {
                format!("denied: {error}. Cancel is safe: the password goes to the private file")
            }
            code if DENIED.contains(&code) => format!("denied: {error}"),
            -25291 | -25294 | -25307 => NO_DEFAULT_KEYCHAIN.to_string(),
            -25308 => KEYCHAIN_LOCKED.to_string(),
            _ => format!("unavailable: {error}"),
        }
    }

    fn with_user_interaction_disabled<T>(
        call: impl FnOnce() -> Result<T, Error>,
    ) -> Result<T, String> {
        let _serial = USER_INTERACTION_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // KeychainUserInteractionLock restores interaction in Drop, including
        // when the operation returns an error or unwinds with a panic.
        let _interaction = SecKeychain::disable_user_interaction().map_err(reason)?;
        call().map_err(reason)
    }

    pub(super) fn put(name: &str, secret: &[u8]) -> Result<(), String> {
        // Without a default keychain macOS opens its own "A keychain cannot be
        // found to store ..." dialog from inside the add; asking first fails
        // quietly instead. INFERRED, not verified on a Mac that lacks a default
        // keychain: the owner's Mac is the check.
        with_user_interaction_disabled(|| {
            SecKeychain::default()?;
            passwords::set_generic_password(SERVICE, name, secret)
        })
    }

    pub(super) fn get(name: &str) -> Result<Zeroizing<Vec<u8>>, String> {
        with_user_interaction_disabled(|| {
            passwords::get_generic_password(SERVICE, name).map(Zeroizing::new)
        })
    }

    pub(super) fn delete(name: &str) -> Result<(), String> {
        with_user_interaction_disabled(|| passwords::delete_generic_password(SERVICE, name))
    }

    #[cfg(test)]
    mod tests {
        use super::{reason, with_user_interaction_disabled, Error};

        // No usable default keychain: callers must still see `unavailable:`
        // (so they fall back to a file) plus words that say how to fix it.
        #[test]
        fn a_missing_default_keychain_is_unavailable_with_the_fix() {
            // errSecNoDefaultKeychain, errSecNoSuchKeychain, errSecNotAvailable.
            for code in [-25307, -25294, -25291] {
                let text = reason(Error::from_code(code));
                assert!(text.starts_with("unavailable: "), "{code}: {text}");
                assert!(text.contains("default keychain"), "{code}: {text}");
                assert!(text.contains("Fix:"), "{code}: {text}");
                assert!(text.contains("Cancel is safe"), "{code}: {text}");
                assert!(text.contains("Reset to Defaults"), "{code}: {text}");
                assert!(text.contains("new login keychain"), "{code}: {text}");
                assert!(text.len() < 200, "{code}: {} chars: {text}", text.len());
            }
        }

        #[test]
        fn a_locked_keychain_is_unavailable_with_its_own_fix() {
            let text = reason(Error::from_code(-25308));
            assert!(text.starts_with("unavailable: keychain locked"), "{text}");
            assert!(text.contains("Fix:"), "{text}");
        }

        #[test]
        fn a_noninteractive_store_call_maps_a_locked_status() {
            let result = with_user_interaction_disabled(|| -> Result<(), Error> {
                Err(Error::from_code(-25308))
            });
            assert_eq!(result.unwrap_err(), super::KEYCHAIN_LOCKED);
        }

        #[test]
        fn a_refused_prompt_stays_denied() {
            for code in [-128, -25293] {
                assert!(reason(Error::from_code(code)).starts_with("denied: "));
            }
            let cancelled = reason(Error::from_code(-128));
            assert!(cancelled.contains("Cancel is safe"), "{cancelled}");
        }
    }
}

#[cfg(windows)]
mod store {
    use std::io;
    use std::ptr;
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
        CRED_TYPE_GENERIC,
    };
    use zeroize::{Zeroize, Zeroizing};

    pub(super) const BACKEND: Option<&str> = Some("wincred");

    const ERROR_NOT_FOUND: i32 = 1168;
    // ERROR_ACCESS_DENIED, ERROR_PRIVILEGE_NOT_HELD and ERROR_LOGON_FAILURE.
    const DENIED: [i32; 3] = [5, 1314, 1326];

    // Only valid right after a Cred* call returned FALSE. A session with no
    // logon session, such as SSH or WinRM (ERROR_NO_SUCH_LOGON_SESSION), is
    // `unavailable`, like every failure that is not a miss or a refusal.
    fn reason() -> String {
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(ERROR_NOT_FOUND) => "not_found".to_string(),
            Some(code) if DENIED.contains(&code) => format!("denied: {error}"),
            _ => format!("unavailable: {error}"),
        }
    }

    // A generic credential's target name, NUL-terminated UTF-16.
    fn target(name: &str) -> Vec<u16> {
        wide(&format!("remuda:{name}"))
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    pub(super) fn put(name: &str, secret: &[u8]) -> Result<(), String> {
        let mut target = target(name);
        let mut user = wide("remuda");
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            CredentialBlobSize: u32::try_from(secret.len())
                .map_err(|_| "unavailable: the secret is too long".to_string())?,
            CredentialBlob: secret.as_ptr().cast_mut(),
            // This machine only: ENTERPRISE would roam the secret with the profile.
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: user.as_mut_ptr(),
            ..Default::default()
        };
        // SAFETY: `credential` and the NUL-terminated strings and the blob it
        // points to outlive the call, which only reads them.
        if unsafe { CredWriteW(&credential, 0) } == 0 {
            return Err(reason());
        }
        Ok(())
    }

    pub(super) fn get(name: &str) -> Result<Zeroizing<Vec<u8>>, String> {
        let target = target(name);
        let mut credential: *mut CREDENTIALW = ptr::null_mut();
        // SAFETY: `target` is NUL-terminated and `credential` is a valid out
        // pointer; both outlive the call.
        if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) } == 0 {
            return Err(reason());
        }
        // SAFETY: CredReadW succeeded, so `credential` points to one CREDENTIALW
        // whose blob, when not null, is CredentialBlobSize writable bytes that
        // nothing else borrows. Nothing between the read and CredFree can
        // return early, and `credential` is not used after CredFree.
        let secret = unsafe {
            let blob = (*credential).CredentialBlob;
            let secret = if blob.is_null() {
                Vec::new()
            } else {
                let bytes =
                    std::slice::from_raw_parts_mut(blob, (*credential).CredentialBlobSize as usize);
                let secret = bytes.to_vec();
                bytes.zeroize();
                secret
            };
            CredFree(credential.cast());
            secret
        };
        Ok(Zeroizing::new(secret))
    }

    pub(super) fn delete(name: &str) -> Result<(), String> {
        let target = target(name);
        // SAFETY: `target` is NUL-terminated and outlives the call.
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
            return Err(reason());
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
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
