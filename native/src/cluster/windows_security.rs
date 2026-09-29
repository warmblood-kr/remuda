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
    GetTokenInformation, TokenUser, ACCESS_ALLOWED_ACE, ACL_SIZE_INFORMATION,
    DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES,
    SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
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
    _buffer: Vec<usize>,
    sid: *mut c_void,
    text: Vec<u16>,
}

impl UserSid {
    fn current() -> io::Result<Self> {
        let mut token = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(last_error());
        }
        let token = Handle(token);
        let mut needed = 0;
        unsafe {
            GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut needed);
        }
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
        let token_user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
        let sid = token_user.User.Sid;
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
            _buffer: buffer,
            sid,
            text,
        })
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
    use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, OPEN_ALWAYS};
    let user = UserSid::current()?;
    let security = user.security()?;
    let path = wide(path);
    let mut handle = unsafe {
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
    if handle == INVALID_HANDLE_VALUE {
        let error = last_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
        handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_GENERIC_READ
                    | FILE_GENERIC_WRITE
                    | windows_sys::Win32::Storage::FileSystem::READ_CONTROL
                    | windows_sys::Win32::Storage::FileSystem::WRITE_DAC,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_ALWAYS,
                FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
    }
    file_from_handle(handle)
}

fn file_from_handle(handle: HANDLE) -> io::Result<File> {
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(last_error());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

pub(super) fn open_for_check(path: &Path, directory: bool, write_dac: bool) -> io::Result<File> {
    let path = wide(path);
    let access = FILE_READ_ATTRIBUTES
        | windows_sys::Win32::Storage::FileSystem::READ_CONTROL
        | if write_dac {
            windows_sys::Win32::Storage::FileSystem::WRITE_DAC
        } else {
            0
        }
        | if directory {
            FILE_GENERIC_READ
        } else {
            FILE_GENERIC_READ
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

/// Verify or migrate an opened object. Returns whether a legacy ACL was repaired.
pub(super) fn secure_or_upgrade(file: &File, path: &Path, directory: bool) -> io::Result<bool> {
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
    if owner.is_null() || unsafe { EqualSid(owner, user.sid) } == 0 {
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
    let result = unsafe {
        SetSecurityInfo(
            handle,
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
    if !dacl_conforms(handle, user.sid)? {
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
    use std::io::Read;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn loading_legacy_default_acl_file_tightens_acl_and_preserves_data() {
        let path = std::env::temp_dir().join(format!(
            "remuda-cluster-acl-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let expected = b"legacy state";
        std::fs::write(&path, expected).unwrap();

        let file = open_for_check(&path, false, true).unwrap();
        assert!(secure_or_upgrade(&file, &path, false).unwrap());
        // Windows temp directories inherit permissive ACLs, so migration must have happened.
        assert!(dacl_conforms(file.as_raw_handle(), UserSid::current().unwrap().sid).unwrap());
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
}
