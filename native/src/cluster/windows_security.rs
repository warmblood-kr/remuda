//! Windows ACL helpers for the cluster's private state directory.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::{fs::File, ptr};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SetSecurityInfo, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
    GetTokenInformation, TokenOwner, TokenUser, ACCESS_ALLOWED_ACE, ACL_SIZE_INFORMATION,
    DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES,
    SE_DACL_PROTECTED, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandleEx, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct LocalMemory(*mut c_void);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

/// Current process user's SID, kept in an aligned buffer for the borrowed SID pointer.
struct UserSid {
    _user_buffer: Vec<usize>,
    _owner_buffer: Vec<usize>,
    sid: *mut c_void,
    token_owner_sid: *mut c_void,
    text: Vec<u16>,
}

impl UserSid {
    fn current() -> io::Result<Self> {
        let mut token = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(last_error());
        }
        let token = Handle(token);
        let user_buffer = token_information(token.0, TokenUser)?;
        let owner_buffer = token_information(token.0, TokenOwner)?;
        let token_user = unsafe { &*(user_buffer.as_ptr().cast::<TOKEN_USER>()) };
        let token_owner = unsafe { &*(owner_buffer.as_ptr().cast::<TOKEN_OWNER>()) };
        let sid = token_user.User.Sid;
        let token_owner_sid = token_owner.Owner;
        let mut sid_text = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut sid_text) } == 0 {
            return Err(last_error());
        }
        let sid_text_mem = LocalMemory(sid_text.cast());
        let mut len = 0;
        unsafe {
            while *sid_text.add(len) != 0 {
                len += 1;
            }
        }
        let text = unsafe { std::slice::from_raw_parts(sid_text, len + 1) }.to_vec();
        drop(sid_text_mem);
        Ok(Self {
            _user_buffer: user_buffer,
            _owner_buffer: owner_buffer,
            sid,
            token_owner_sid,
            text,
        })
    }

    #[cfg(test)]
    fn accepts_owner(&self, owner: *mut c_void) -> bool {
        !owner.is_null()
            && (unsafe { EqualSid(owner, self.sid) } != 0
                || unsafe { EqualSid(owner, self.token_owner_sid) } != 0)
    }

    fn security(&self) -> io::Result<OwnerOnlySecurity> {
        let sid = String::from_utf16_lossy(&self.text[..self.text.len() - 1]);
        let sddl: Vec<u16> = format!("O:{sid}D:P(A;;FA;;;{sid})")
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
        Ok(OwnerOnlySecurity { descriptor })
    }
}

fn token_information(token: HANDLE, class: i32) -> io::Result<Vec<usize>> {
    let mut needed = 0;
    unsafe { GetTokenInformation(token, class, ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(last_error());
    }
    let words = (needed as usize + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
    let mut buffer = vec![0usize; words];
    if unsafe {
        GetTokenInformation(
            token,
            class,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(last_error());
    }
    Ok(buffer)
}

pub(super) struct OwnerOnlySecurity {
    descriptor: *mut c_void,
}
impl OwnerOnlySecurity {
    pub(super) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        }
    }
}
impl Drop for OwnerOnlySecurity {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            unsafe {
                LocalFree(self.descriptor);
            }
        }
    }
}

pub(super) fn create_directory(path: &Path) -> io::Result<()> {
    let user = UserSid::current()?;
    let security = user.security()?;
    let path = wide(path);
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::CreateDirectoryW(
            path.as_ptr(),
            &security.attributes(),
        )
    } != 0
    {
        Ok(())
    } else {
        Err(last_error())
    }
}

/// Create a new file with the protected owner-only descriptor.
pub(crate) fn create_new_file(path: &Path) -> io::Result<File> {
    let user = UserSid::current()?;
    let security = user.security()?;
    let path = wide(path);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &security.attributes(),
            windows_sys::Win32::Storage::FileSystem::CREATE_NEW,
            FILE_FLAG_WRITE_THROUGH | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    file_from_handle(handle)
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::fmt::Write as _;
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write path must name a file",
        ));
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|e| io::Error::other(format!("atomic write randomness unavailable: {e}")))?;
    let mut hex = String::with_capacity(32);
    for byte in random {
        write!(&mut hex, "{byte:02x}").expect("String write");
    }
    let temporary = parent.join(format!(".remuda-atomic-{}-{hex}.tmp", std::process::id()));
    let result = (|| {
        let mut file = create_new_file(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn create_or_open_lock(path: &Path) -> io::Result<File> {
    use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, OPEN_EXISTING};
    let user = UserSid::current()?;
    let security = user.security()?;
    let path = wide(path);
    for _ in 0..5 {
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_GENERIC_READ
                    | FILE_GENERIC_WRITE
                    | windows_sys::Win32::Storage::FileSystem::READ_CONTROL
                    | windows_sys::Win32::Storage::FileSystem::WRITE_DAC,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                &security.attributes(),
                CREATE_NEW,
                FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return file_from_handle(handle);
        }
        let error = last_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
        let existing = unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_GENERIC_READ
                    | FILE_GENERIC_WRITE
                    | windows_sys::Win32::Storage::FileSystem::READ_CONTROL
                    | windows_sys::Win32::Storage::FileSystem::WRITE_DAC
                    | windows_sys::Win32::Storage::FileSystem::WRITE_OWNER,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if existing != INVALID_HANDLE_VALUE {
            return file_from_handle(existing);
        }
        let error = last_error();
        if error.kind() == io::ErrorKind::NotFound {
            continue;
        }
        return Err(error);
    }
    Err(io::Error::new(
        io::ErrorKind::Other,
        "cluster lock kept disappearing while opening",
    ))
}

fn file_from_handle(handle: HANDLE) -> io::Result<File> {
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(last_error());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

pub(super) fn open_for_check(path: &Path, directory: bool, write_dac: bool) -> io::Result<File> {
    let path = wide(path);
    let access = windows_sys::Win32::Storage::FileSystem::READ_CONTROL
        | if write_dac {
            windows_sys::Win32::Storage::FileSystem::WRITE_DAC
        } else {
            0
        };
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | if directory {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            0
        };
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            flags,
            ptr::null_mut(),
        )
    };
    file_from_handle(handle)
}

fn open_for_owner_update(path: &Path, directory: bool) -> io::Result<File> {
    let path = wide(path);
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | if directory {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            0
        };
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_READ_ATTRIBUTES
                | windows_sys::Win32::Storage::FileSystem::READ_CONTROL
                | windows_sys::Win32::Storage::FileSystem::WRITE_DAC
                | windows_sys::Win32::Storage::FileSystem::WRITE_OWNER,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            flags,
            ptr::null_mut(),
        )
    };
    file_from_handle(handle)
}

/// Open state for reading only after any legacy ACL has been repaired through a metadata handle.
pub(super) fn open_for_read(path: &Path) -> io::Result<File> {
    let checked = open_for_check(path, false, true)?;
    secure_or_upgrade(&checked, path, false)?;
    drop(checked);

    let path_wide = wide(path);
    let access = FILE_GENERIC_READ
        | windows_sys::Win32::Storage::FileSystem::READ_CONTROL
        | windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    let file = file_from_handle(handle)?;
    secure_or_upgrade(&file, path, false)?;
    Ok(file)
}

/// Verify or migrate an opened object. Returns whether a legacy ACL was repaired.
pub(super) fn secure_or_upgrade(file: &File, path: &Path, directory: bool) -> io::Result<bool> {
    validate_open_object(file, path, directory)?;
    let user = UserSid::current()?;
    let handle = file.as_raw_handle();
    let mut tag = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    if unsafe {
        GetFileInformationByHandleEx(
            handle,
            windows_sys::Win32::Storage::FileSystem::FileAttributeTagInfo,
            (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } == 0
    {
        return Err(last_error());
    }
    if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a reparse point; refusing", path.display()),
        ));
    }
    if (tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} has the wrong file type", path.display()),
        ));
    }
    if dacl_conforms(handle, user.sid)? {
        return Ok(false);
    }

    // Ownership mismatch is the only ACL migration refusal. Reparse points were rejected above.
    let (owner, _sd) = security_descriptor(handle)?;
    let owner_matches_user = !owner.is_null() && unsafe { EqualSid(owner, user.sid) } != 0;
    let owner_matches_token_owner =
        !owner.is_null() && unsafe { EqualSid(owner, user.token_owner_sid) } != 0;
    if !owner_matches_user && !owner_matches_token_owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is owned by another SID; refusing", path.display()),
        ));
    }
    let security = user.security()?;
    let mut dacl = ptr::null_mut();
    let mut present = 0;
    let mut defaulted = 0;
    if unsafe {
        GetSecurityDescriptorDacl(security.descriptor, &mut present, &mut dacl, &mut defaulted)
    } == 0
    {
        return Err(last_error());
    }
    let owner_update = if owner_matches_user {
        None
    } else {
        let update = open_for_owner_update(path, directory)?;
        validate_open_object(&update, path, directory)?;
        Some(update)
    };
    let update_handle = owner_update
        .as_ref()
        .map_or(handle, |update| update.as_raw_handle());
    let mut security_info = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
    if !owner_matches_user {
        security_info |= windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
    }
    let result = unsafe {
        SetSecurityInfo(
            update_handle,
            SE_FILE_OBJECT,
            security_info,
            if owner_matches_user {
                ptr::null_mut()
            } else {
                user.sid
            },
            ptr::null_mut(),
            dacl,
            ptr::null_mut(),
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    if !dacl_conforms(update_handle, user.sid)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "failed to secure cluster state ACL",
        ));
    }
    eprintln!(
        "remuda: secured legacy cluster state ACL at {}",
        path.display()
    );
    Ok(true)
}

pub(super) fn validate_open_object(file: &File, path: &Path, directory: bool) -> io::Result<()> {
    let handle = file.as_raw_handle();
    let mut tag = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    if unsafe {
        GetFileInformationByHandleEx(
            handle,
            windows_sys::Win32::Storage::FileSystem::FileAttributeTagInfo,
            (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } == 0
    {
        return Err(last_error());
    }
    if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a reparse point; refusing", path.display()),
        ));
    }
    if (tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} has the wrong file type", path.display()),
        ));
    }
    Ok(())
}

fn security_descriptor(handle: HANDLE) -> io::Result<(*mut c_void, LocalMemory)> {
    let mut owner = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    let result = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    Ok((owner, LocalMemory(descriptor.cast())))
}

pub(crate) fn dacl_conforms(handle: HANDLE, user_sid: *mut c_void) -> io::Result<bool> {
    let (owner, descriptor) = security_descriptor(handle)?;
    if owner.is_null() || unsafe { EqualSid(owner, user_sid) } == 0 {
        return Ok(false);
    }
    let mut control = 0;
    let mut revision = 0;
    if unsafe { GetSecurityDescriptorControl(descriptor.0.cast(), &mut control, &mut revision) }
        == 0
    {
        return Err(last_error());
    }
    if control & SE_DACL_PROTECTED == 0 {
        return Ok(false);
    }
    let mut present = 0;
    let mut dacl = ptr::null_mut();
    let mut defaulted = 0;
    if unsafe {
        GetSecurityDescriptorDacl(descriptor.0.cast(), &mut present, &mut dacl, &mut defaulted)
    } == 0
    {
        return Err(last_error());
    }
    if present == 0 || dacl.is_null() {
        return Ok(false);
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            windows_sys::Win32::Security::AclSizeInformation,
        )
    } == 0
    {
        return Err(last_error());
    }
    if info.AceCount != 1 {
        return Ok(false);
    }
    let mut ace = ptr::null_mut();
    if unsafe { GetAce(dacl, 0, &mut ace) } == 0 {
        return Err(last_error());
    }
    let ace = unsafe { &*(ace.cast::<ACCESS_ALLOWED_ACE>()) };
    if ace.Header.AceType != 0 || ace.Header.AceFlags != 0 || ace.Mask != FILE_ALL_ACCESS {
        return Ok(false);
    }
    let sid = (&ace.SidStart as *const u32).cast_mut().cast::<c_void>();
    Ok(unsafe { EqualSid(sid, user_sid) } != 0)
}

#[cfg(test)]
pub(crate) fn is_owner_acl_conforming(file: &File) -> io::Result<bool> {
    let user = UserSid::current()?;
    dacl_conforms(file.as_raw_handle(), user.sid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn enable_restore_privilege() -> io::Result<Handle> {
        use windows_sys::Win32::Foundation::{SetLastError, ERROR_NOT_ALL_ASSIGNED};
        use windows_sys::Win32::Security::{
            AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES,
            SE_PRIVILEGE_ENABLED, SE_RESTORE_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
        };

        let mut token = ptr::null_mut();
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
                &mut token,
            )
        } == 0
        {
            return Err(last_error());
        }
        let token = Handle(token);
        let mut luid = windows_sys::Win32::Foundation::LUID::default();
        if unsafe { LookupPrivilegeValueW(ptr::null(), SE_RESTORE_NAME, &mut luid) } == 0 {
            return Err(last_error());
        }
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        unsafe { SetLastError(0) };
        if unsafe {
            AdjustTokenPrivileges(token.0, 0, &privileges, 0, ptr::null_mut(), ptr::null_mut())
        } == 0
        {
            return Err(last_error());
        }
        if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SeRestorePrivilege is not present in the process token",
            ));
        }
        Ok(token)
    }

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn owner_sid_from_sddl(sddl: &str) -> io::Result<(*mut c_void, LocalMemory)> {
        let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(last_error());
        }
        let descriptor = LocalMemory(descriptor.cast());
        let mut owner = ptr::null_mut();
        let mut defaulted = 0;
        if unsafe {
            windows_sys::Win32::Security::GetSecurityDescriptorOwner(
                descriptor.0.cast(),
                &mut owner,
                &mut defaulted,
            )
        } == 0
        {
            return Err(last_error());
        }
        Ok((owner, descriptor))
    }

    fn set_owner_from_sddl(file: &File, sddl: &str) -> io::Result<()> {
        let (owner, _descriptor) = owner_sid_from_sddl(sddl)?;
        let result = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION,
                owner,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        Ok(())
    }

    fn set_extra_world_allow(file: &File, user: &UserSid) -> io::Result<()> {
        let user_text = String::from_utf16_lossy(&user.text[..user.text.len() - 1]);
        let sddl: Vec<u16> = format!("O:{user_text}D:P(A;;FA;;;{user_text})(A;;FA;;;WD)")
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
        let descriptor = LocalMemory(descriptor.cast());
        let mut present = 0;
        let mut dacl = ptr::null_mut();
        let mut defaulted = 0;
        if unsafe {
            GetSecurityDescriptorDacl(descriptor.0.cast(), &mut present, &mut dacl, &mut defaulted)
        } == 0
        {
            return Err(last_error());
        }
        let result = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl,
                ptr::null_mut(),
            )
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        Ok(())
    }

    #[test]
    fn loading_legacy_default_acl_file_tightens_acl_and_preserves_data() {
        let path = std::env::temp_dir().join(format!(
            "remuda-cluster-acl-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let expected = b"legacy state";
        std::fs::write(&path, expected).unwrap();

        let user = UserSid::current().unwrap();
        let legacy = open_for_check(&path, false, true).unwrap();
        assert!(
            !dacl_conforms(legacy.as_raw_handle(), user.sid).unwrap(),
            "temp directory should give the legacy file a non-private inherited ACL"
        );
        drop(legacy);

        let file = open_for_read(&path).unwrap();
        assert!(dacl_conforms(file.as_raw_handle(), user.sid).unwrap());
        let mut actual = Vec::new();
        (&file).read_to_end(&mut actual).unwrap();
        assert_eq!(actual, expected);
        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn new_cluster_directory_and_atomic_file_get_owner_only_acl() {
        let dir = std::env::temp_dir().join(format!(
            "remuda-cluster-created-acl-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        super::super::storage::create_private_directory(&dir).unwrap();
        let directory = open_for_check(&dir, true, true).unwrap();
        assert!(is_owner_acl_conforming(&directory).unwrap());

        let path = dir.join("settings.json");
        super::super::storage::atomic_write(&path, b"{}\n").unwrap();
        let file = open_for_check(&path, false, true).unwrap();
        assert!(is_owner_acl_conforming(&file).unwrap());
        drop(file);
        drop(directory);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_an_object_owned_by_an_unrelated_sid() {
        let path = std::env::temp_dir().join(format!(
            "remuda-cluster-other-owner-{}-{}.key",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = create_new_file(&path).unwrap();
        file.write_all(b"test").unwrap();
        drop(file);
        let file = open_for_owner_update(&path, false).unwrap();
        let user = UserSid::current().unwrap();
        let _restore_privilege = match enable_restore_privilege() {
            Ok(token) => token,
            Err(error) if std::env::var_os("CI").is_some() => {
                panic!("CI runner must enable SeRestorePrivilege for owner test: {error}");
            }
            Err(error) => {
                eprintln!(
                    "skipping unrelated-owner ACL test: cannot enable SeRestorePrivilege: {error}"
                );
                drop(file);
                std::fs::remove_file(path).unwrap();
                return;
            }
        };
        let mut assigned = false;
        let mut last_error = None;
        for candidate in ["O:SY", "O:BG", "O:BA"] {
            let (sid, _descriptor) = owner_sid_from_sddl(candidate).unwrap();
            if user.accepts_owner(sid) {
                continue;
            }
            match set_owner_from_sddl(&file, candidate) {
                Ok(()) => {
                    assigned = true;
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        if !assigned {
            let reason = last_error.map_or_else(
                || "all candidate SIDs are token owners".to_owned(),
                |error| error.to_string(),
            );
            drop(file);
            std::fs::remove_file(path).unwrap();
            if std::env::var_os("CI").is_some() {
                panic!("CI runner could not assign a non-token owner despite SeRestorePrivilege: {reason}");
            }
            eprintln!(
                "skipping unrelated-owner ACL test: cannot assign a non-token owner: {reason}"
            );
            return;
        }
        let error = secure_or_upgrade(&file, &path, false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn repairs_a_protected_dacl_with_an_extra_world_allow_ace() {
        let path = std::env::temp_dir().join(format!(
            "remuda-cluster-extra-ace-{}-{}.key",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        drop(create_new_file(&path).unwrap());
        let file = open_for_check(&path, false, true).unwrap();
        let user = UserSid::current().unwrap();
        set_extra_world_allow(&file, &user).unwrap();
        assert!(secure_or_upgrade(&file, &path, false).unwrap());
        assert!(is_owner_acl_conforming(&file).unwrap());
        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    fn scratch(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "remuda-cluster-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        root
    }

    /// Owner and DACL of a directory as SDDL text, to compare before and after.
    fn directory_sddl(path: &Path) -> String {
        use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;

        let directory = open_for_check(path, true, false).unwrap();
        let (_owner, descriptor) = security_descriptor(directory.as_raw_handle()).unwrap();
        let mut text = ptr::null_mut();
        let mut len = 0;
        let converted = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.0.cast(),
                1,
                windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION,
                &mut text,
                &mut len,
            )
        };
        assert_ne!(converted, 0, "SDDL of {}: {}", path.display(), last_error());
        let memory = LocalMemory(text.cast());
        let units = unsafe { std::slice::from_raw_parts(text, len as usize) };
        let sddl = String::from_utf16_lossy(units)
            .trim_end_matches('\0')
            .to_owned();
        drop(memory);
        sddl
    }

    fn is_owner_only(path: &Path, directory: bool) -> bool {
        is_owner_acl_conforming(&open_for_check(path, directory, false).unwrap()).unwrap()
    }

    /// A same-user round trip with plain `std::fs`: list, create, append, read.
    fn assert_plainly_usable(dir: &Path) {
        let fail =
            |what: &str, error: io::Error| -> ! { panic!("{what} in {}: {error}", dir.display()) };
        if let Err(error) = std::fs::read_dir(dir).map(Iterator::count) {
            fail("list", error);
        }
        let probe = dir.join("plain-probe.txt");
        if let Err(error) = std::fs::write(&probe, b"one") {
            fail("create a file", error);
        }
        let appended = std::fs::OpenOptions::new()
            .append(true)
            .open(&probe)
            .and_then(|mut file| file.write_all(b"two"));
        if let Err(error) = appended {
            fail("write a file", error);
        }
        match std::fs::read(&probe) {
            Ok(bytes) => assert_eq!(bytes, b"onetwo"),
            Err(error) => fail("read a file", error),
        }
        if let Err(error) = std::fs::remove_file(&probe) {
            fail("remove a file", error);
        }
    }

    /// Give FILE an owner the current token does not accept. `None` means the
    /// runner cannot do that; in CI that is a failure, not a skip.
    fn assign_unrelated_owner(file: &File) -> Option<Handle> {
        let in_ci = std::env::var_os("CI").is_some();
        let privilege = match enable_restore_privilege() {
            Ok(token) => token,
            Err(error) if in_ci => panic!("CI runner must enable SeRestorePrivilege: {error}"),
            Err(error) => {
                eprintln!("skipping: cannot enable SeRestorePrivilege: {error}");
                return None;
            }
        };
        let user = UserSid::current().unwrap();
        for candidate in ["O:SY", "O:BG", "O:BA"] {
            let (sid, _descriptor) = owner_sid_from_sddl(candidate).unwrap();
            if !user.accepts_owner(sid) && set_owner_from_sddl(file, candidate).is_ok() {
                return Some(privilege);
            }
        }
        assert!(!in_ci, "CI runner could not assign a non-token owner");
        eprintln!("skipping: cannot assign a non-token owner");
        None
    }

    // The base `remuda` dir also holds mods, the channel file and the update
    // cache: only `cluster` inside it is private state.
    #[test]
    fn creating_the_cluster_dir_leaves_a_plain_base_and_its_siblings_alone() {
        use super::super::storage;

        let root = scratch("narrow");
        let base = root.join("remuda");
        let mods = base.join("mods");
        let channel = base.join("channel");
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(&channel, b"nightly").unwrap();
        let before = directory_sddl(&base);

        storage::check_base_directory(&base).unwrap();
        assert_eq!(
            directory_sddl(&base),
            before,
            "checking the base dir changed its ACL"
        );
        let cluster = base.join("cluster");
        storage::create_private_directory(&cluster).unwrap();
        storage::verify_directory(&cluster).unwrap();
        let key = cluster.join("identity.key");
        storage::atomic_write(&key, b"key").unwrap();

        assert_eq!(
            directory_sddl(&base),
            before,
            "creating the cluster dir changed the base dir's ACL"
        );
        assert!(!is_owner_only(&base, true));
        assert_eq!(std::fs::read(&channel).unwrap(), b"nightly");
        std::fs::write(&channel, b"stable").unwrap();
        assert_plainly_usable(&mods);
        assert_plainly_usable(&base);
        assert!(is_owner_only(&cluster, true));
        assert!(is_owner_only(&key, false));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn base_directory_check_refuses_a_reparse_point_and_a_file() {
        use super::super::storage::check_base_directory;
        use std::os::windows::fs::symlink_dir;

        let root = scratch("base-type");
        let holder = root.join("file");
        std::fs::create_dir(&holder).unwrap();
        let file_base = holder.join("remuda");
        std::fs::write(&file_base, b"not a directory").unwrap();
        let error = check_base_directory(&file_base).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let target = root.join("target");
        let link = root.join("remuda");
        std::fs::create_dir(&target).unwrap();
        match symlink_dir(&target, &link) {
            Ok(()) => {
                let error = check_base_directory(&link).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            }
            Err(error) => eprintln!(
                "skipping base reparse check: cannot create symlink on this runner: {error}"
            ),
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    // An older version made the base dir owner-only. It is never loosened, and
    // what plain `std::fs` creates in it afterwards must still be usable.
    #[test]
    fn an_already_protected_base_is_left_protected_and_new_children_work() {
        use super::super::storage;

        let root = scratch("protected-base");
        let base = root.join("remuda");
        create_directory(&base).unwrap();
        let before = directory_sddl(&base);

        storage::check_base_directory(&base).unwrap();
        storage::create_private_directory(&base.join("cluster")).unwrap();

        assert_eq!(directory_sddl(&base), before);
        assert!(is_owner_only(&base, true));
        let mods = base.join("mods");
        std::fs::create_dir(&mods).unwrap();
        assert_plainly_usable(&mods);
        assert_plainly_usable(&base);
        std::fs::remove_dir_all(root).unwrap();
    }

    // A base dir made by an elevated installer is owned by another SID.
    #[test]
    fn a_base_owned_by_another_sid_no_longer_blocks_the_cluster_dir() {
        use super::super::storage;

        let root = scratch("foreign-base");
        let base = root.join("remuda");
        std::fs::create_dir(&base).unwrap();
        let handle = open_for_owner_update(&base, true).unwrap();
        let Some(_privilege) = assign_unrelated_owner(&handle) else {
            drop(handle);
            std::fs::remove_dir_all(root).unwrap();
            return;
        };
        drop(handle);
        let before = directory_sddl(&base);

        storage::check_base_directory(&base).unwrap();
        let cluster = base.join("cluster");
        storage::create_private_directory(&cluster).unwrap();

        assert_eq!(directory_sddl(&base), before);
        assert!(is_owner_only(&cluster, true));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_file_and_cluster_directory_reparse_points() {
        use std::os::windows::fs::{symlink_dir, symlink_file};

        let base = std::env::temp_dir().join(format!(
            "remuda-cluster-reparse-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&base).unwrap();
        let file_target = base.join("target.key");
        let file_link = base.join("identity.key");
        std::fs::write(&file_target, b"target").unwrap();
        match symlink_file(&file_target, &file_link) {
            Ok(()) => {
                let error = open_for_read(&file_link).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            }
            Err(error) => eprintln!(
                "skipping file reparse check: cannot create symlink on this runner: {error}"
            ),
        }

        let dir_target = base.join("target-dir");
        let dir_link = base.join("cluster");
        std::fs::create_dir(&dir_target).unwrap();
        match symlink_dir(&dir_target, &dir_link) {
            Ok(()) => {
                let error = super::super::storage::verify_directory(&dir_link).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            }
            Err(error) => eprintln!(
                "skipping directory reparse check: cannot create symlink on this runner: {error}"
            ),
        }
        std::fs::remove_dir_all(base).unwrap();
    }
}
