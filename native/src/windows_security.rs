//! Pure Windows storage security policy, with OS calls added under `cfg(windows)`.

use std::ffi::OsString;
use std::path::PathBuf;

#[allow(dead_code)]
pub(crate) fn protected_storage_sddl(owner_sid: &str) -> String {
    // RED baseline: this mirrors the current cluster owner-only policy.
    format!("O:{owner_sid}D:PAI(A;OICI;FA;;;OW)")
}

#[allow(dead_code)]
pub(crate) fn local_appdata_for(env: &dyn Fn(&str) -> Option<OsString>) -> Result<PathBuf, String> {
    let absolute_windows_path = |name| {
        env(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|path| {
                let text = path.as_os_str().to_string_lossy();
                let bytes = text.as_bytes();
                (bytes.len() >= 3
                    && bytes[0].is_ascii_alphabetic()
                    && bytes[1] == b':'
                    && matches!(bytes[2], b'\\' | b'/'))
                    || text.starts_with(r"\\")
                    || text.starts_with("//")
            })
    };
    absolute_windows_path("LOCALAPPDATA")
        .or_else(|| {
            absolute_windows_path("USERPROFILE").map(|profile| profile.join("AppData/Local"))
        })
        .ok_or_else(|| {
            "unavailable: LOCALAPPDATA and USERPROFILE are not set to absolute paths".into()
        })
}

#[allow(dead_code)]
pub(crate) fn storage_root_for(env: &dyn Fn(&str) -> Option<OsString>) -> Result<PathBuf, String> {
    Ok(local_appdata_for(env)?.join("remuda").join("storage"))
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
    fn storage_sddl_grants_only_owner_system_and_administrators() {
        assert_eq!(
            protected_storage_sddl("S-1-5-21-42"),
            "O:S-1-5-21-42D:PAI(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
        );
    }

    #[test]
    fn localappdata_mapping_prefers_absolute_localappdata() {
        assert_eq!(
            storage_root_for(&env(&[
                ("LOCALAPPDATA", r"C:\Users\user\AppData\Local"),
                ("USERPROFILE", r"D:\Users\other"),
            ]))
            .unwrap(),
            PathBuf::from(r"C:\Users\user\AppData\Local")
                .join("remuda")
                .join("storage")
        );
    }

    #[test]
    fn localappdata_mapping_falls_back_to_userprofile() {
        assert_eq!(
            storage_root_for(&env(&[("USERPROFILE", r"C:\Users\user")])).unwrap(),
            PathBuf::from(r"C:\Users\user")
                .join("AppData")
                .join("Local")
                .join("remuda")
                .join("storage")
        );
    }
}
