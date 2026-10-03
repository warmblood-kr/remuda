//! Pure Windows storage security policy, with OS calls added under `cfg(windows)`.

use std::ffi::OsString;
use std::path::PathBuf;

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NtOpenPolicy {
    pub(crate) object_attributes: u32,
    pub(crate) create_options: u32,
}

#[cfg(test)]
pub(crate) fn relative_component_utf16(component: &str) -> Option<Vec<u16>> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || !component.bytes().all(|byte| byte.is_ascii_graphic())
        || component
            .bytes()
            .any(|byte| matches!(byte, b'/' | b'\\' | b':' | b'\0'))
    {
        return None;
    }
    Some(component.encode_utf16().chain(Some(0)).collect())
}

#[cfg(test)]
pub(crate) fn nt_open_policy(directory: bool) -> NtOpenPolicy {
    const OBJ_DONT_REPARSE: u32 = 0x0000_1000;
    const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
    const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
    const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
    NtOpenPolicy {
        object_attributes: OBJ_DONT_REPARSE,
        create_options: FILE_OPEN_REPARSE_POINT
            | FILE_SYNCHRONOUS_IO_NONALERT
            | if directory {
                FILE_DIRECTORY_FILE
            } else {
                FILE_NON_DIRECTORY_FILE
            },
    }
}

#[cfg(any(windows, test))]
fn ace_sid_fits(ace_size: usize, fixed_size: usize, sid_size: usize) -> bool {
    fixed_size
        .checked_add(sid_size)
        .is_some_and(|required| ace_size >= required)
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
    use super::{ace_sid_fits, protected_storage_sddl};
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
        GetTokenInformation, TokenUser, ACE_HEADER, ACL_SIZE_INFORMATION,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED, TOKEN_QUERY,
        TOKEN_USER,
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
            // SAFETY: This wrapper exclusively owns a valid handle from OpenProcessToken/CreateFileW.
            unsafe { CloseHandle(self.0) };
        }
    }

    struct LocalMemory(*mut c_void);
    impl Drop for LocalMemory {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: The pointer is an allocation returned by a Win32 LocalAlloc API.
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
            // SAFETY: GetCurrentProcess is a pseudo-handle and token is a valid output pointer.
            if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
                return Err(last_error());
            }
            let token = Handle(token);
            let mut needed = 0;
            // SAFETY: This sizing call intentionally supplies no buffer and a valid length pointer.
            unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut needed) };
            if needed == 0 {
                return Err(last_error());
            }
            let words =
                (needed as usize + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
            let mut buffer = vec![0usize; words];
            // SAFETY: buffer is aligned and at least `needed` bytes; token remains live.
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
            // SAFETY: Successful TokenUser query initialized a TOKEN_USER in the sized buffer.
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
        // SAFETY: GetLastError has no pointer or handle preconditions.
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
        // SAFETY: sid is non-null and points to a SID retained by its owning buffer/descriptor.
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
        // SAFETY: ConvertSidToStringSidW returned a NUL-terminated UTF-16 allocation.
        unsafe {
            while *text.add(len) != 0 {
                len += 1;
            }
        }
        // SAFETY: The preceding scan found the terminator within the returned allocation.
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
        // SAFETY: sddl is NUL-terminated and descriptor is a valid output pointer.
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
        // SAFETY: path_wide and attributes remain valid for this synchronous Win32 call.
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
        // SAFETY: path_wide is NUL-terminated; null optional pointers are permitted by CreateFileW.
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
        // SAFETY: This call takes ownership of the valid handle returned by CreateFileW.
        let file = unsafe { std::fs::File::from_raw_handle(handle) };
        let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
        // SAFETY: tag is a writable buffer of the exact requested size; file owns the handle.
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
        // SAFETY: file is a live handle and all output pointers are valid for this call.
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
        if owner.is_null() {
            return Err(policy_error("storage root owner mismatch"));
        }
        // SAFETY: Both SID pointers are valid for comparison and remain live for this call.
        if unsafe { windows_sys::Win32::Security::EqualSid(owner, user.sid) } == 0 {
            return Err(policy_error("storage root owner mismatch"));
        }
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: descriptor owns a valid security descriptor and outputs are writable.
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
        // SAFETY: descriptor is valid and the DACL fields are output pointers.
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
        // SAFETY: actual_dacl is the descriptor's live DACL; info is a writable sized buffer.
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
            // SAFETY: actual_dacl is valid and raw_ace is a writable output pointer.
            if unsafe { GetAce(actual_dacl, index, &mut raw_ace) } == 0 {
                return Err(last_error());
            }
            if raw_ace.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "storage root ACE is missing",
                ));
            }
            // SAFETY: GetAce returned an ACE pointer owned by the live DACL.
            let header = unsafe { &*(raw_ace.cast::<ACE_HEADER>()) };
            let fixed_size = std::mem::size_of::<ACE_HEADER>() + std::mem::size_of::<u32>();
            if (header.AceSize as usize) < fixed_size + 8 {
                return Err(policy_error("storage root ACE is truncated"));
            }
            if header.AceType != 0 || header.AceFlags != 0x03 {
                return Err(policy_error("storage root ACE policy mismatch"));
            }
            // SAFETY: AceSize covers the fixed ACCESS_ALLOWED_ACE prefix checked above.
            let ace =
                unsafe { &*(raw_ace.cast::<windows_sys::Win32::Security::ACCESS_ALLOWED_ACE>()) };
            if ace.Mask != FILE_ALL_ACCESS {
                return Err(policy_error("storage root ACE policy mismatch"));
            }
            let sid = (&ace.SidStart as *const u32).cast_mut().cast::<c_void>();
            // SAFETY: The ACE's fixed prefix and minimum eight-byte SID header are in bounds.
            let sid_bytes = sid.cast::<u8>();
            let (revision, sub_authority_count) = unsafe { (*sid_bytes, *sid_bytes.add(1)) };
            if revision != 1 || sub_authority_count > 15 {
                return Err(policy_error("storage root ACE SID is invalid"));
            }
            let sub_authority_count = sub_authority_count as usize;
            let sid_size = 8usize + sub_authority_count * std::mem::size_of::<u32>();
            if !ace_sid_fits(header.AceSize as usize, fixed_size, sid_size) {
                return Err(policy_error("storage root ACE SID is truncated"));
            }
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
    fn windows_component_validation_rejects_path_syntax() {
        for component in ["", ".", "..", "a/b", r"a\b", "nul\0byte"] {
            assert!(
                relative_component_utf16(component).is_none(),
                "{component:?}"
            );
        }
    }

    #[test]
    fn windows_relative_name_builder_encodes_one_nul_terminated_component() {
        assert_eq!(
            relative_component_utf16("mail_1.bin"),
            Some("mail_1.bin\0".encode_utf16().collect())
        );
    }

    #[test]
    fn windows_open_flag_calculation_requires_no_reparse_and_expected_type() {
        assert_eq!(
            nt_open_policy(true),
            NtOpenPolicy {
                object_attributes: 0x0000_1000, // OBJ_DONT_REPARSE
                create_options: 0x0020_0021,    // FILE_OPEN_REPARSE_POINT | sync | directory
            }
        );
        assert_eq!(
            nt_open_policy(false),
            NtOpenPolicy {
                object_attributes: 0x0000_1000,
                create_options: 0x0020_0060, // FILE_OPEN_REPARSE_POINT | sync | non-directory
            }
        );
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
