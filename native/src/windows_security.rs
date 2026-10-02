//! Pure Windows storage security policy, with OS calls added under `cfg(windows)`.

use std::ffi::OsString;
use std::path::PathBuf;

fn ace_sid_fits(ace_size: usize, fixed_size: usize, sid_size: usize) -> bool {
    ace_size >= fixed_size
}

#[allow(dead_code)]
pub(crate) fn protected_storage_sddl(owner_sid: &str) -> String {
    // Protect the root from parent ACLs; let its three trusted principals inherit to children.
    format!("O:{owner_sid}D:PAI(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")
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

#[cfg(windows)]
mod platform {
    use super::protected_storage_sddl;
    use std::ffi::{c_void, OsStr};
    use std::fs;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::Path;
    use std::ptr;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetSecurityInfo, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetAce, GetAclInformation, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        GetTokenInformation, TokenUser, ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, GetFileInformationByHandleEx, FILE_ALL_ACCESS,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    struct LocalMemory(*mut c_void);
    impl Drop for LocalMemory {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0) };
            }
        }
    }

    struct UserSid {
        _buffer: Vec<usize>,
        sid: *mut c_void,
        text: String,
    }

    impl UserSid {
        fn current() -> io::Result<Self> {
            let mut token = ptr::null_mut();
            if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
                return Err(last_error());
            }
            let token = Handle(token);
            let mut needed = 0;
            unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut needed) };
            if needed == 0 {
                return Err(last_error());
            }
            let words =
                (needed as usize + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
            let mut buffer = vec![0usize; words];
            if unsafe {
                GetTokenInformation(
                    token.0,
                    TokenUser,
                    buffer.as_mut_ptr().cast(),
                    needed,
                    &mut needed,
                )
            } == 0
            {
                return Err(last_error());
            }
            let user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
            let sid = user.User.Sid;
            let text = sid_text(sid)?;
            Ok(Self {
                _buffer: buffer,
                sid,
                text,
            })
        }
    }

    fn last_error() -> io::Error {
        io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    fn sid_text(sid: *mut c_void) -> io::Result<String> {
        if sid.is_null() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "missing SID"));
        }
        let mut text = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
            return Err(last_error());
        }
        if text.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SID conversion returned no string",
            ));
        }
        let memory = LocalMemory(text.cast());
        let mut len = 0;
        unsafe {
            while *text.add(len) != 0 {
                len += 1;
            }
        }
        let value = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        drop(memory);
        Ok(value)
    }

    fn descriptor(owner_sid: &str) -> io::Result<*mut c_void> {
        let sddl: Vec<u16> = protected_storage_sddl(owner_sid)
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(last_error());
        }
        if descriptor.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "security descriptor conversion returned no descriptor",
            ));
        }
        Ok(descriptor)
    }

    pub(crate) fn ensure_storage_root(path: &Path) -> io::Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let user = UserSid::current()?;
        let descriptor = LocalMemory(descriptor(&user.text)?);
        let attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>()
                as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let path_wide = wide(path.as_os_str());
        let created = unsafe { CreateDirectoryW(path_wide.as_ptr(), &attributes) };
        if created == 0 {
            let error = last_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        verify_storage_root(path)
    }

    pub(crate) fn verify_storage_root(path: &Path) -> io::Result<()> {
        let path_wide = wide(path.as_os_str());
        let handle = unsafe {
            CreateFileW(
                path_wide.as_ptr(),
                FILE_READ_ATTRIBUTES | windows_sys::Win32::Storage::FileSystem::READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(last_error());
        }
        let file = unsafe { std::fs::File::from_raw_handle(handle) };
        let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                windows_sys::Win32::Storage::FileSystem::FileAttributeTagInfo,
                (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        } == 0
        {
            return Err(last_error());
        }
        if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "storage root is not a regular directory",
            ));
        }

        let mut owner = ptr::null_mut();
        let mut dacl = ptr::null_mut();
        let mut raw_descriptor = ptr::null_mut();
        let result = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut raw_descriptor,
            )
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        if raw_descriptor.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "storage root security descriptor is missing",
            ));
        }
        let descriptor = LocalMemory(raw_descriptor.cast());
        let user = UserSid::current()?;
        if owner.is_null()
            || unsafe { windows_sys::Win32::Security::EqualSid(owner, user.sid) } == 0
        {
            return Err(policy_error("storage root owner mismatch"));
        }
        let mut control = 0;
        let mut revision = 0;
        if unsafe { GetSecurityDescriptorControl(descriptor.0.cast(), &mut control, &mut revision) }
            == 0
        {
            return Err(last_error());
        }
        if control & SE_DACL_PROTECTED == 0 {
            return Err(policy_error("storage root DACL is inheritable"));
        }
        let mut present = 0;
        let mut actual_dacl = ptr::null_mut();
        let mut defaulted = 0;
        if unsafe {
            GetSecurityDescriptorDacl(
                descriptor.0.cast(),
                &mut present,
                &mut actual_dacl,
                &mut defaulted,
            )
        } == 0
        {
            return Err(last_error());
        }
        if present == 0 || actual_dacl.is_null() {
            return Err(policy_error("storage root DACL missing"));
        }
        let mut info = ACL_SIZE_INFORMATION::default();
        if unsafe {
            GetAclInformation(
                actual_dacl,
                (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                windows_sys::Win32::Security::AclSizeInformation,
            )
        } == 0
        {
            return Err(last_error());
        }
        if info.AceCount != 3 {
            return Err(policy_error("storage root ACE set mismatch"));
        }
        let mut principals = Vec::with_capacity(3);
        for index in 0..info.AceCount {
            let mut raw_ace = ptr::null_mut();
            if unsafe { GetAce(actual_dacl, index, &mut raw_ace) } == 0 {
                return Err(last_error());
            }
            if raw_ace.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "storage root ACE is missing",
                ));
            }
            let ace =
                unsafe { &*(raw_ace.cast::<windows_sys::Win32::Security::ACCESS_ALLOWED_ACE>()) };
            if ace.Header.AceType != 0 || ace.Header.AceFlags != 0x03 || ace.Mask != FILE_ALL_ACCESS
            {
                return Err(policy_error("storage root ACE policy mismatch"));
            }
            let sid = (&ace.SidStart as *const u32).cast_mut().cast::<c_void>();
            principals.push(sid_text(sid)?);
        }
        principals.sort();
        let mut expected = vec![
            "S-1-3-4".to_owned(),
            "S-1-5-18".to_owned(),
            "S-1-5-32-544".to_owned(),
        ];
        expected.sort();
        if principals != expected {
            return Err(policy_error("storage root principals mismatch"));
        }
        Ok(())
    }

    fn policy_error(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, message)
    }
}

#[cfg(windows)]
pub(crate) use platform::ensure_storage_root;

#[cfg(all(windows, test))]
pub(crate) use platform::verify_storage_root;

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
    fn ace_size_must_contain_the_full_sid() {
        assert!(ace_sid_fits(28, 8, 20));
        assert!(!ace_sid_fits(27, 8, 20));
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "remuda-windows-security-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn creates_reopens_and_verifies_protected_storage_root() {
        let root = test_root();
        ensure_storage_root(&root).unwrap();
        verify_storage_root(&root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_preexisting_weak_dacl() {
        let root = test_root();
        fs::create_dir_all(&root).unwrap();
        assert!(ensure_storage_root(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_reparse_point_storage_root() {
        let parent = test_root();
        let target = parent.join("target");
        let link = parent.join("link");
        fs::create_dir_all(&target).unwrap();
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success(), "could not create junction for test");
        assert!(ensure_storage_root(&link).is_err());
        fs::remove_dir_all(parent).unwrap();
    }
}
