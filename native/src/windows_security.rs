//! Pure Windows storage security policy, with OS calls added under `cfg(windows)`.

use std::ffi::OsString;
use std::path::PathBuf;

#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NtOpenPolicy {
    pub(crate) object_attributes: u32,
    pub(crate) create_options: u32,
}

#[cfg(any(windows, test))]
pub(crate) fn relative_component_utf16(component: &str) -> Option<Vec<u16>> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.len() > 255
        || !component.bytes().all(|byte| byte.is_ascii_graphic())
        || component
            .bytes()
            .any(|byte| matches!(byte, b'/' | b'\\' | b':' | b'\0'))
    {
        return None;
    }
    Some(component.encode_utf16().chain(Some(0)).collect())
}

#[cfg(any(windows, test))]
const OBJ_DONT_REPARSE: u32 = 0x0000_1000;
#[cfg(any(windows, test))]
const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(any(windows, test))]
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
#[cfg(any(windows, test))]
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
#[cfg(any(windows, test))]
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;

#[cfg(all(test, windows))]
thread_local! {
    static FAIL_NEXT_STORAGE_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(any(windows, test))]
pub(crate) fn nt_open_policy(directory: bool) -> NtOpenPolicy {
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
struct FileRenameInfoBuffer {
    words: Vec<usize>,
    byte_len: usize,
}

#[cfg(any(windows, test))]
impl FileRenameInfoBuffer {
    fn as_mut_bytes(&mut self) -> &mut [u8] {
        // SAFETY: `words` is aligned storage and every allocated byte is initialized.
        unsafe { std::slice::from_raw_parts_mut(self.words.as_mut_ptr().cast(), self.byte_len) }
    }
}

#[cfg(any(windows, test))]
impl std::ops::Deref for FileRenameInfoBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: `words` is aligned storage and every allocated byte is initialized.
        unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast(), self.byte_len) }
    }
}

#[cfg(any(windows, test))]
fn build_file_rename_info(
    root_directory: usize,
    name: &str,
    replace: bool,
) -> std::io::Result<FileRenameInfoBuffer> {
    if name.is_empty() || name.contains(['\0', '/', '\\']) {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    let encoded = name.encode_utf16().collect::<Vec<_>>();
    let name_bytes = encoded
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let name_length = u32::try_from(name_bytes)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let root_offset = std::mem::size_of::<u32>().next_multiple_of(std::mem::align_of::<usize>());
    let length_offset = root_offset + std::mem::size_of::<usize>();
    let name_offset = length_offset + std::mem::size_of::<u32>();
    let byte_len = name_offset + name_bytes + std::mem::size_of::<u16>();
    let word_count = byte_len.div_ceil(std::mem::size_of::<usize>());
    let mut info = FileRenameInfoBuffer {
        words: vec![0; word_count],
        byte_len,
    };
    let bytes = info.as_mut_bytes();
    let flags = 0x2 | u32::from(replace); // POSIX semantics plus optional replace.
    bytes[..4].copy_from_slice(&flags.to_ne_bytes());
    bytes[root_offset..length_offset].copy_from_slice(&root_directory.to_ne_bytes());
    bytes[length_offset..name_offset].copy_from_slice(&name_length.to_ne_bytes());
    for (index, unit) in encoded.into_iter().enumerate() {
        let offset = name_offset + index * std::mem::size_of::<u16>();
        bytes[offset..offset + 2].copy_from_slice(&unit.to_ne_bytes());
    }
    Ok(info)
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

#[cfg(any(windows, test))]
fn trusted_storage_owner(actual_sid: &str, current_user_sid: &str) -> bool {
    actual_sid == current_user_sid || matches!(actual_sid, "S-1-5-18" | "S-1-5-32-544")
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
    #[cfg(test)]
    use super::FAIL_NEXT_STORAGE_RENAME;
    use super::{
        ace_sid_fits, build_file_rename_info, nt_open_policy, protected_storage_sddl,
        relative_component_utf16, trusted_storage_owner,
    };
    use std::ffi::{c_void, OsStr};
    use std::fs;
    use std::io::{self, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::Path;
    use std::ptr;
    use std::sync::Mutex;
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        NtCreateFile, NtOpenFile, NtQueryDirectoryFile, FILE_NAMES_INFORMATION,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, HANDLE, INVALID_HANDLE_VALUE, UNICODE_STRING,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetSecurityInfo, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetAce, GetAclInformation, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        GetTokenInformation, TokenUser, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL_SIZE_INFORMATION,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, GetFileInformationByHandleEx, SetFileInformationByHandle,
        FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_ATTRIBUTE_TAG_INFO, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

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

    pub(crate) struct StorageDirectory {
        file: std::fs::File,
        names_lock: Mutex<()>,
    }

    impl StorageDirectory {
        #[cfg(test)]
        pub(crate) fn as_file_for_test(&self) -> &std::fs::File {
            &self.file
        }

        pub(crate) fn open_directory(&self, name: &str, create: bool) -> io::Result<Self> {
            if create
                && self
                    .names()?
                    .iter()
                    .any(|existing| existing != name && existing.eq_ignore_ascii_case(name))
            {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            let file = open_relative(&self.file, name, true, create, 0x0012_0087)?;
            verify_inherited_acl(&file, "storage child directory")?;
            Ok(Self {
                file,
                names_lock: Mutex::new(()),
            })
        }

        pub(crate) fn open_file(&self, name: &str, write: bool) -> io::Result<std::fs::File> {
            let file = open_relative(
                &self.file,
                name,
                false,
                write,
                if write { 0x0012_0082 } else { 0x0012_0081 },
            )?;
            verify_inherited_acl(&file, "storage child file")?;
            Ok(file)
        }

        pub(crate) fn write_atomic(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
            match self.open_file(name, false) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let mut temporary = None;
            for _ in 0..8 {
                let name = random_temporary_name()?;
                match create_relative_file(&self.file, &name) {
                    Ok(file) => {
                        temporary = Some(TempFileGuard {
                            file,
                            delete_on_drop: true,
                        });
                        break;
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            let mut temporary =
                temporary.ok_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists))?;
            verify_inherited_acl(&temporary.file, "storage temporary file")?;
            temporary.file.write_all(bytes)?;
            temporary.file.sync_all()?;

            #[cfg(test)]
            if FAIL_NEXT_STORAGE_RENAME.with(|fail| fail.replace(false)) {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }

            let mut rename_info =
                build_file_rename_info(self.file.as_raw_handle() as usize, name, true)?;
            let length = u32::try_from(rename_info.len())
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            // FileRenameInfoEx = 22; POSIX semantics and replace-if-exists are required.
            // SAFETY: the aligned buffer and owned file handle remain live through the call.
            if unsafe {
                SetFileInformationByHandle(
                    temporary.file.as_raw_handle(),
                    22,
                    rename_info.as_mut_bytes().as_mut_ptr().cast(),
                    length,
                )
            } == 0
            {
                return Err(last_error());
            }
            temporary.delete_on_drop = false;
            Ok(())
        }

        pub(crate) fn delete_file(&self, name: &str) -> io::Result<bool> {
            let file = match open_relative(&self.file, name, false, false, 0x0013_0080) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            verify_inherited_acl(&file, "storage child file")?;
            let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
            // SAFETY: file is a no-follow regular-file handle with DELETE access.
            if unsafe {
                SetFileInformationByHandle(
                    file.as_raw_handle(),
                    4,
                    (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
                    std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            } == 0
            {
                return Err(last_error());
            }
            Ok(true)
        }

        pub(crate) fn names(&self) -> io::Result<Vec<String>> {
            const MAX_NAMES: usize = 1024;
            let _guard = self
                .names_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut names = Vec::new();
            let mut restart = true;
            'query: loop {
                let mut buffer = vec![0u64; 8192];
                // SAFETY: This zeroed status block and aligned output buffer are valid for the call.
                let mut status_block: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
                // SAFETY: This is a synchronous query on an owned directory handle.
                let status = unsafe {
                    NtQueryDirectoryFile(
                        self.file.as_raw_handle(),
                        ptr::null_mut(),
                        None,
                        ptr::null(),
                        &mut status_block,
                        buffer.as_mut_ptr().cast(),
                        (buffer.len() * std::mem::size_of::<u64>()) as u32,
                        12,
                        false,
                        ptr::null(),
                        restart,
                    )
                };
                if status as u32 == 0x8000_0006 {
                    break;
                }
                if status < 0 {
                    return Err(nt_error(status));
                }
                restart = false;
                let used = status_block.Information;
                if used == 0 {
                    break;
                }
                if used > buffer.len() * std::mem::size_of::<u64>() {
                    return Err(io::Error::from(io::ErrorKind::InvalidData));
                }
                let header = std::mem::offset_of!(FILE_NAMES_INFORMATION, FileName);
                let mut offset = 0usize;
                loop {
                    if offset.checked_add(header).is_none_or(|end| end > used) {
                        return Err(io::Error::from(io::ErrorKind::InvalidData));
                    }
                    // SAFETY: The buffer is aligned and this record header is within `used`.
                    let record = unsafe {
                        &*buffer
                            .as_ptr()
                            .cast::<u8>()
                            .add(offset)
                            .cast::<FILE_NAMES_INFORMATION>()
                    };
                    let name_bytes = record.FileNameLength as usize;
                    if name_bytes % 2 != 0
                        || offset
                            .checked_add(header)
                            .and_then(|start| start.checked_add(name_bytes))
                            .is_none_or(|end| end > used)
                    {
                        return Err(io::Error::from(io::ErrorKind::InvalidData));
                    }
                    // SAFETY: The validated name range is fully inside the returned buffer.
                    let units = unsafe {
                        std::slice::from_raw_parts(record.FileName.as_ptr(), name_bytes / 2)
                    };
                    let name = String::from_utf16(units)
                        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
                    if name != "." && name != ".." {
                        names.push(name);
                        if names.len() == MAX_NAMES {
                            break 'query;
                        }
                    }
                    if record.NextEntryOffset == 0 {
                        break;
                    }
                    if (record.NextEntryOffset as usize) < header + name_bytes {
                        return Err(io::Error::from(io::ErrorKind::InvalidData));
                    }
                    offset = offset
                        .checked_add(record.NextEntryOffset as usize)
                        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
                }
            }
            Ok(names)
        }
    }

    struct TempFileGuard {
        file: std::fs::File,
        delete_on_drop: bool,
    }

    impl Drop for TempFileGuard {
        fn drop(&mut self) {
            if self.delete_on_drop {
                mark_file_for_delete(&self.file);
            }
        }
    }

    fn mark_file_for_delete(file: &std::fs::File) {
        let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: The owned temp handle has DELETE access and the disposition buffer is sized.
        unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                4,
                (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            );
        }
    }

    fn random_temporary_name() -> io::Result<String> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|_| io::Error::other("storage randomness unavailable"))?;
        let mut name = String::from(".remuda-atomic-");
        for byte in random {
            use std::fmt::Write as _;
            write!(&mut name, "{byte:02x}").expect("writing to a String cannot fail");
        }
        name.push_str(".tmp");
        Ok(name)
    }

    fn create_relative_file(parent: &std::fs::File, component: &str) -> io::Result<std::fs::File> {
        let name = relative_component_utf16(component)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let length = ((name.len() - 1) * std::mem::size_of::<u16>()) as u16;
        let mut unicode = UNICODE_STRING {
            Length: length,
            MaximumLength: (name.len() * std::mem::size_of::<u16>()) as u16,
            Buffer: name.as_ptr().cast_mut(),
        };
        let policy = nt_open_policy(false);
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle(),
            ObjectName: &mut unicode,
            Attributes: policy.object_attributes,
            SecurityDescriptor: ptr::null_mut(),
            SecurityQualityOfService: ptr::null_mut(),
        };
        // SAFETY: A zeroed IO_STATUS_BLOCK is the required initialized output structure.
        let mut status_block: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        let mut raw = ptr::null_mut();
        // FILE_CREATE makes collisions fail instead of opening an existing sibling.
        // SAFETY: all NT structures and the component buffer remain live through the call.
        let status = unsafe {
            NtCreateFile(
                &mut raw,
                0x0013_0082, // DELETE | READ_CONTROL | SYNCHRONIZE | READ_ATTRIBUTES | WRITE_DATA
                &attributes,
                &mut status_block,
                ptr::null(),
                0x80,
                7,
                2,
                policy.create_options,
                ptr::null(),
                0,
            )
        };
        if status < 0 {
            if !raw.is_null() {
                // SAFETY: A non-null failed output handle must be released.
                unsafe { CloseHandle(raw) };
            }
            return Err(nt_error(status));
        }
        if raw.is_null() {
            return Err(io::Error::other("storage temp create returned no handle"));
        }
        // SAFETY: A successful NtCreateFile call returned an owned file handle.
        let file = unsafe { std::fs::File::from_raw_handle(raw) };
        if let Err(error) = verify_handle(&file, false) {
            mark_file_for_delete(&file);
            return Err(error);
        }
        Ok(file)
    }

    fn nt_error(status: i32) -> io::Error {
        match status as u32 {
            0xC000_0034 | 0xC000_003A => io::Error::from(io::ErrorKind::NotFound),
            0xC000_0035 => io::Error::from(io::ErrorKind::AlreadyExists),
            0xC000_050B => io::Error::from(io::ErrorKind::PermissionDenied),
            0xC000_00BA | 0xC000_0103 => io::Error::from(io::ErrorKind::InvalidInput),
            _ => io::Error::other("storage I/O failed"),
        }
    }

    fn open_relative(
        parent: &std::fs::File,
        component: &str,
        directory: bool,
        create: bool,
        access: u32,
    ) -> io::Result<std::fs::File> {
        let name = relative_component_utf16(component)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let length = ((name.len() - 1) * std::mem::size_of::<u16>()) as u16;
        let mut unicode = UNICODE_STRING {
            Length: length,
            MaximumLength: (name.len() * std::mem::size_of::<u16>()) as u16,
            Buffer: name.as_ptr().cast_mut(),
        };
        let policy = nt_open_policy(directory);
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle(),
            ObjectName: &mut unicode,
            // NTFS lookup is case-insensitive by default; create-time folded checks keep
            // this API consistent while preserving the backing filesystem's lookup mode.
            Attributes: policy.object_attributes,
            SecurityDescriptor: ptr::null_mut(),
            SecurityQualityOfService: ptr::null_mut(),
        };
        // SAFETY: The initialized object/name/status structures outlive the synchronous NT call.
        let mut status_block: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        let mut raw = ptr::null_mut();
        // SAFETY: Parent is an owned directory handle and all pointers reference live buffers.
        let status = unsafe {
            if create {
                NtCreateFile(
                    &mut raw,
                    access,
                    &attributes,
                    &mut status_block,
                    ptr::null(),
                    if directory {
                        FILE_ATTRIBUTE_DIRECTORY
                    } else {
                        0x80
                    },
                    7,
                    3,
                    policy.create_options,
                    ptr::null(),
                    0,
                )
            } else {
                NtOpenFile(
                    &mut raw,
                    access,
                    &attributes,
                    &mut status_block,
                    7,
                    policy.create_options,
                )
            }
        };
        if status < 0 {
            if !raw.is_null() {
                // SAFETY: A non-null output handle from a failed open must be released.
                unsafe { CloseHandle(raw) };
            }
            return Err(nt_error(status));
        }
        if raw.is_null() {
            return Err(io::Error::other("storage open returned no handle"));
        }
        // SAFETY: A successful NT open returned an owned handle.
        let file = unsafe { std::fs::File::from_raw_handle(raw) };
        verify_handle(&file, directory)?;
        Ok(file)
    }

    fn verify_handle(file: &std::fs::File, directory: bool) -> io::Result<()> {
        let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
        // SAFETY: tag is a writable buffer of the exact size; file owns the queried handle.
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
        let is_directory = tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || is_directory != directory {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        Ok(())
    }

    fn verify_inherited_acl(file: &std::fs::File, subject: &str) -> io::Result<()> {
        let mut owner = ptr::null_mut();
        let mut dacl = ptr::null_mut();
        let mut raw_descriptor = ptr::null_mut();
        // SAFETY: file is a live handle opened with READ_CONTROL; output pointers are valid.
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
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        let descriptor = LocalMemory(raw_descriptor.cast());
        let user = UserSid::current()?;
        if owner.is_null() {
            return Err(policy_error(&format!("{subject} owner is missing")));
        }
        // Elevated setup can create children as Administrators or SYSTEM; trust those SIDs only.
        if !trusted_storage_owner(&sid_text(owner)?, &user.text) {
            return Err(policy_error(&format!("{subject} owner is untrusted")));
        }
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: descriptor owns a valid security descriptor and outputs are writable.
        if unsafe { GetSecurityDescriptorControl(descriptor.0.cast(), &mut control, &mut revision) }
            == 0
        {
            return Err(last_error());
        }
        if control & SE_DACL_PROTECTED != 0 {
            return Err(policy_error(&format!("{subject} DACL is protected")));
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
            return Err(policy_error(&format!("{subject} DACL is missing")));
        }
        let mut info = ACL_SIZE_INFORMATION::default();
        // SAFETY: actual_dacl is owned by descriptor; info is a sized output buffer.
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
            return Err(policy_error(&format!("{subject} ACE count mismatch")));
        }
        let mut principals = Vec::with_capacity(3);
        for index in 0..info.AceCount {
            let mut raw_ace = ptr::null_mut();
            // SAFETY: actual_dacl is valid and raw_ace is a writable output pointer.
            if unsafe { GetAce(actual_dacl, index, &mut raw_ace) } == 0 || raw_ace.is_null() {
                return Err(last_error());
            }
            // SAFETY: GetAce returned an ACE pointer owned by the live DACL.
            let header = unsafe { &*(raw_ace.cast::<ACE_HEADER>()) };
            if header.AceSize as usize > info.AclBytesInUse as usize
                || header.AceType != 0
                || header.AceFlags & 0x10 == 0
            {
                return Err(policy_error(&format!(
                    "{subject} ACE type or inheritance mismatch"
                )));
            }
            let fixed_size = std::mem::size_of::<ACE_HEADER>() + std::mem::size_of::<u32>();
            if (header.AceSize as usize) < fixed_size + 8 {
                return Err(policy_error(&format!("{subject} ACE header is truncated")));
            }
            // SAFETY: AceSize covers the ACCESS_ALLOWED_ACE prefix checked above.
            let ace = unsafe { &*(raw_ace.cast::<ACCESS_ALLOWED_ACE>()) };
            if ace.Mask != FILE_ALL_ACCESS {
                return Err(policy_error(&format!("{subject} ACE mask mismatch")));
            }
            let sid = (&ace.SidStart as *const u32).cast_mut().cast::<c_void>();
            let sid_bytes = sid.cast::<u8>();
            // SAFETY: the checked ACE prefix includes the initial eight-byte SID header.
            let (revision, sub_authority_count) = unsafe { (*sid_bytes, *sid_bytes.add(1)) };
            if revision != 1 || sub_authority_count > 15 {
                return Err(policy_error(&format!("{subject} ACE SID is invalid")));
            }
            let sid_size = 8 + usize::from(sub_authority_count) * std::mem::size_of::<u32>();
            if !ace_sid_fits(header.AceSize as usize, fixed_size, sid_size) {
                return Err(policy_error(&format!("{subject} ACE SID is truncated")));
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
            return Err(policy_error(&format!("{subject} principal set mismatch")));
        }
        Ok(())
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

    pub(crate) fn ensure_storage_root(path: &Path) -> io::Result<StorageDirectory> {
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

    pub(crate) fn verify_storage_root(path: &Path) -> io::Result<StorageDirectory> {
        let path_wide = wide(path.as_os_str());
        // SAFETY: path_wide is NUL-terminated; null optional pointers are permitted by CreateFileW.
        let handle = unsafe {
            CreateFileW(
                path_wide.as_ptr(),
                FILE_READ_ATTRIBUTES
                    | windows_sys::Win32::Storage::FileSystem::READ_CONTROL
                    | FILE_LIST_DIRECTORY
                    | SYNCHRONIZE,
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
        Ok(StorageDirectory {
            file,
            names_lock: Mutex::new(()),
        })
    }

    fn policy_error(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, message)
    }
}

#[cfg(windows)]
pub(crate) use platform::{ensure_storage_root, StorageDirectory};

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
    fn rename_info_builder_is_handle_relative_utf16_and_nul_terminated() {
        let root = 0x1234usize;
        let name = "résumé.tmp";
        let info = build_file_rename_info(root, name, true).unwrap();
        let root_offset = 4usize.next_multiple_of(std::mem::size_of::<usize>());
        let length_offset = root_offset + std::mem::size_of::<usize>();
        let name_offset = length_offset + std::mem::size_of::<u32>();
        let encoded = name.encode_utf16().collect::<Vec<_>>();

        assert_eq!(u32::from_le_bytes(info[0..4].try_into().unwrap()), 0x3);
        let no_replace = build_file_rename_info(root, name, false).unwrap();
        assert_eq!(
            u32::from_le_bytes(no_replace[0..4].try_into().unwrap()),
            0x2
        );
        assert_eq!(
            usize::from_le_bytes(info[root_offset..length_offset].try_into().unwrap()),
            root
        );
        assert_eq!(
            u32::from_le_bytes(info[length_offset..name_offset].try_into().unwrap()) as usize,
            encoded.len() * std::mem::size_of::<u16>()
        );
        let mut expected = encoded
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        expected.extend_from_slice(&[0, 0]);
        assert_eq!(&info[name_offset..], expected.as_slice());
    }

    #[test]
    fn rename_info_builder_rejects_empty_or_nul_containing_leaf_names() {
        assert!(build_file_rename_info(1, "", true).is_err());
        assert!(build_file_rename_info(1, "bad\0name", true).is_err());
        assert!(build_file_rename_info(1, "nested/name", true).is_err());
    }

    #[test]
    fn storage_sddl_grants_only_owner_system_and_administrators() {
        assert_eq!(
            protected_storage_sddl("S-1-5-21-42"),
            "O:S-1-5-21-42D:PAI(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
        );
    }

    #[test]
    fn storage_child_owner_accepts_only_current_user_system_or_administrators() {
        let current = "S-1-5-21-42";
        for trusted in [current, "S-1-5-18", "S-1-5-32-544"] {
            assert!(
                trusted_storage_owner(trusted, current),
                "trusted SID {trusted}"
            );
        }
        assert!(!trusted_storage_owner("S-1-5-21-99", current));
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

    #[test]
    fn failed_handle_relative_rename_leaves_no_target_or_temporary_sibling() {
        let root = test_root();
        let storage = ensure_storage_root(&root).unwrap();
        let namespace = storage.open_directory("atomic-failure", true).unwrap();
        FAIL_NEXT_STORAGE_RENAME.with(|fail| fail.set(true));
        let result = namespace.write_atomic("target", b"value");
        FAIL_NEXT_STORAGE_RENAME.with(|fail| fail.set(false));
        let error = result.unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(namespace.names().unwrap(), Vec::<String>::new());
        drop(namespace);
        drop(storage);
        fs::remove_dir_all(root).unwrap();
    }
}
