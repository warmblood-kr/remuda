//! Experimental Windows control endpoint: one owner-only named pipe.
//!
//! This is the transport slice of #902, not a Windows host. It proves the
//! pipe's DACL, first-instance reservation, bounded framing and the capability
//! plus peer-SID handshake. Nothing here spawns a PTY, adopts a daemon or
//! reloads a host. Unix endpoints are a separate implementation.
//!
//! Every handshake/frame failure consumes the connection: the stream is
//! poisoned and later reads/writes refuse with [`io::ErrorKind::NotConnected`].
use super::wire::{self, MAX_FRAME};
use crate::windows_security::{create_private_root, verify_private_root, StorageDirectory};
use sha2::{Digest, Sha256};
use std::ffi::{c_void, OsStr};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_HANDLE_EOF,
    ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, WaitNamedPipeW,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcess, OpenProcessToken, ResetEvent,
    WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use zeroize::Zeroize;

/// The capability sits in the private directory, never in argv/env/logs.
pub(crate) const CAPABILITY: &str = "capability";
const CAPABILITY_LEN: usize = 32;
/// A fixed prelude, read before any framed message is allocated.
const AUTH_ACK: &[u8] = b"remuda-session-host-auth-v1";
/// Two instances: at most one connected client plus one reserved listener.
/// Keeping the listener reserved means the pipe name never disappears between
/// accepts, so another process cannot squat it and answer our clients.
const MAX_INSTANCES: u32 = 2;
const PIPE_BUFFER: u32 = 64 * 1024;

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "host endpoint capability or peer SID mismatch",
    )
}

fn closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "host pipe unusable after failure",
    )
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Compare in time independent of where the first difference is. The length
/// is a public protocol constant; the bytes are not.
pub(crate) fn constant_time_eq(first: &[u8], second: &[u8]) -> bool {
    if first.len() != second.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in first.iter().zip(second) {
        difference |= left ^ right;
    }
    difference == 0
}

/// The pipe DACL names exactly one principal: the current user, full access,
/// with no inherited ACEs. The owner defaults to the creating user.
pub(crate) fn owner_only_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;{user_sid})")
}

/// SHA-256 of the canonical private directory, rendered as lowercase hex.
/// Only hex digits reach the pipe name, so no caller path syntax can inject a
/// different endpoint or a suffix of its choosing.
pub(crate) fn pipe_name(private_dir: &Path) -> io::Result<String> {
    let canonical = std::fs::canonicalize(private_dir)?;
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    let stable: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(format!(r"\\.\pipe\remuda-session-host-{stable}"))
}

struct Handle(HANDLE);
// SAFETY: a kernel handle is owned exclusively by this wrapper; transferring
// that ownership to another thread does not share it, and CloseHandle is safe
// from any thread.
unsafe impl Send for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: this wrapper exclusively owns the handle it was built from.
        unsafe { CloseHandle(self.0) };
    }
}

struct Event(HANDLE);
// SAFETY: same ownership argument as Handle; waits and resets are serialized
// behind `&mut Stream`.
unsafe impl Send for Event {}
impl Event {
    fn new() -> io::Result<Self> {
        // SAFETY: a null attributes/name is the documented manual-reset default.
        let handle = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }
}
impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: this wrapper exclusively owns the event handle.
        unsafe { CloseHandle(self.0) };
    }
}

/// A LocalAlloc allocation from the security-descriptor API.
struct LocalMemory(*mut c_void);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: ConvertStringSecurityDescriptorToSecurityDescriptorW
            // documents LocalFree as the release for its output.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// A security descriptor that lives across the CreateNamedPipeW call.
struct Descriptor(LocalMemory);
impl Descriptor {
    fn raw(&self) -> *mut c_void {
        self.0 .0
    }

    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let encoded: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = ptr::null_mut();
        // SAFETY: encoded is NUL-terminated and descriptor is a valid out pointer.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                encoded.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if converted == 0 || descriptor.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(LocalMemory(descriptor.cast())))
    }
}

/// A token's user SID, kept in its own aligned buffer.
struct TokenSid {
    buffer: Vec<usize>,
}
impl TokenSid {
    fn from_token(token: HANDLE) -> io::Result<Self> {
        let mut needed = 0;
        // SAFETY: the sizing call intentionally passes no buffer.
        unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        let words = (needed as usize).div_ceil(std::mem::size_of::<usize>());
        let mut buffer = vec![0usize; words];
        // SAFETY: the buffer is aligned and at least `needed` bytes.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { buffer })
    }
    fn sid(&self) -> *mut c_void {
        // SAFETY: a successful TokenUser query initialized a TOKEN_USER in the
        // retained buffer, whose address does not move while self is alive.
        unsafe { (*self.buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

fn token_user(token: HANDLE) -> io::Result<TokenSid> {
    TokenSid::from_token(token)
}

/// The SID of the user this process runs as.
fn current_user() -> io::Result<TokenSid> {
    let mut token = ptr::null_mut();
    // SAFETY: GetCurrentProcess is a pseudo-handle; token is a valid out pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Handle(token);
    token_user(token.0)
}

fn current_user_text() -> io::Result<String> {
    let user = current_user()?;
    sid_text(user.sid())
}

fn sid_text(sid: *mut c_void) -> io::Result<String> {
    if sid.is_null() {
        return Err(invalid("missing user SID"));
    }
    let mut text = ptr::null_mut();
    // SAFETY: sid points into a live TOKEN_USER buffer.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 || text.is_null() {
        return Err(io::Error::last_os_error());
    }
    let memory = LocalMemory(text.cast());
    let mut len = 0;
    // SAFETY: the API returned a NUL-terminated UTF-16 allocation.
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
        let value = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        drop(memory);
        Ok(value)
    }
}

/// The server side of a named pipe, owned until it is connected.
struct Pipe(Handle);
impl Pipe {
    fn raw(&self) -> HANDLE {
        self.0 .0
    }
}

fn create_instance(name: &str, first: bool) -> io::Result<Pipe> {
    let user_sid = current_user_text()?;
    let descriptor = Descriptor::from_sddl(&owner_only_sddl(&user_sid))?;
    let attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.raw(),
        // No inheritance: only the current user, only this object.
        bInheritHandle: 0,
    };
    let encoded = wide(name);
    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        // A second server can never silently share this name.
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    // SAFETY: encoded and attributes remain live for the synchronous call.
    let handle = unsafe {
        CreateNamedPipeW(
            encoded.as_ptr(),
            open_mode,
            mode,
            MAX_INSTANCES,
            PIPE_BUFFER,
            PIPE_BUFFER,
            0,
            &attributes,
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(Pipe(Handle(handle)))
    }
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

/// Milliseconds until `until`, rounded up so a small remaining budget cannot become
/// an immediate zero-millisecond timeout.
fn millis_until(until: Instant) -> io::Result<u32> {
    let remaining = until
        .checked_duration_since(Instant::now())
        .ok_or_else(timeout_error)?;
    let millis = remaining
        .as_millis()
        .saturating_add(u128::from(
            !remaining.subsec_nanos().is_multiple_of(1_000_000),
        ))
        .min(u128::from(u32::MAX - 1));
    Ok(millis as u32)
}

fn timeout_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "host absolute I/O deadline")
}

fn wait_event(event: HANDLE, until: Instant) -> io::Result<()> {
    // SAFETY: event is owned by the caller for the duration of the wait.
    match unsafe { WaitForSingleObject(event, millis_until(until)?) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err(timeout_error()),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        other => Err(io::Error::other(format!(
            "unexpected host wait result {other}"
        ))),
    }
}

fn connect_server(pipe: &Pipe, until: Instant) -> io::Result<()> {
    let event = Event::new()?;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event.0;
    // SAFETY: the pipe handle is live and overlapped lives through the call.
    let connected = unsafe { ConnectNamedPipe(pipe.raw(), &mut overlapped) };
    if connected != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error().map(|code| code as u32) {
        // The client connected between CreateNamedPipeW and this call.
        Some(ERROR_PIPE_CONNECTED) => Ok(()),
        Some(ERROR_IO_PENDING) => finish_connect(pipe.raw(), &overlapped, until),
        _ => Err(error),
    }
}

fn finish_connect(pipe: HANDLE, overlapped: &OVERLAPPED, until: Instant) -> io::Result<()> {
    match wait_event(overlapped.hEvent, until) {
        Ok(()) => completed(pipe, overlapped, false).map(|_| ()),
        Err(error) => {
            // Every failed wait still has a pending operation, and the
            // OVERLAPPED cannot outlive this frame: cancel and reap first.
            // SAFETY: this exact operation is pending on this exact overlapped.
            unsafe { CancelIoEx(pipe, overlapped) };
            let _ = completed(pipe, overlapped, true);
            Err(error)
        }
    }
}

fn completed(pipe: HANDLE, overlapped: &OVERLAPPED, wait: bool) -> io::Result<u32> {
    let mut transferred = 0;
    // SAFETY: pipe and overlapped describe a completed or canceled operation;
    // `wait` only asks the API to reap a canceled one before we free it.
    if unsafe { GetOverlappedResult(pipe, overlapped, &mut transferred, i32::from(wait)) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(transferred)
    }
}

/// A connected pipe stream with one polling event and a per-frame deadline.
pub(crate) struct Stream {
    handle: Handle,
    event: Event,
    budget: Duration,
    /// Set while a whole frame or handshake runs; cleared afterwards.
    until: Option<Instant>,
    unusable: bool,
}

impl Stream {
    fn new(pipe: Pipe, budget: Duration) -> io::Result<Self> {
        Ok(Self {
            handle: pipe.0,
            event: Event::new()?,
            budget,
            until: None,
            unusable: false,
        })
    }

    fn deadline(&self) -> Instant {
        self.until.unwrap_or_else(|| Instant::now() + self.budget)
    }

    fn with_deadline<T>(&mut self, run: impl FnOnce(&mut Self) -> io::Result<T>) -> io::Result<T> {
        if self.unusable {
            return Err(closed());
        }
        // A handshake already fixed an absolute deadline; otherwise one frame
        // gets a fresh bounded budget.
        let previous = self.until;
        if previous.is_none() {
            self.until = Some(Instant::now() + self.budget);
        }
        let result = run(self);
        self.until = previous;
        if result.is_err() {
            self.unusable = true;
        }
        result
    }

    fn transfer(&mut self, writing: bool, bytes: *mut u8, len: usize) -> io::Result<usize> {
        if self.unusable {
            return Err(closed());
        }
        if len == 0 {
            return Ok(0);
        }
        let until = self.deadline();
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = self.event.0;
        // SAFETY: the event is owned by self and ResetEvent takes a live handle.
        if unsafe { ResetEvent(self.event.0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let length = u32::try_from(len).map_err(|_| invalid("host I/O length"))?;
        let mut transferred = 0;
        // SAFETY: the caller's buffer and overlapped live for the call; the
        // handle is live, and `bytes` is writable only for a read.
        let started = unsafe {
            if writing {
                WriteFile(
                    self.handle.0,
                    bytes.cast_const(),
                    length,
                    &mut transferred,
                    &mut overlapped,
                )
            } else {
                ReadFile(
                    self.handle.0,
                    bytes,
                    length,
                    &mut transferred,
                    &mut overlapped,
                )
            }
        };
        if started != 0 {
            return Ok(transferred as usize);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error().map(|code| code as u32) != Some(ERROR_IO_PENDING) {
            return self.after_error(writing, error);
        }
        match wait_event(self.event.0, until) {
            Ok(()) => match completed(self.handle.0, &overlapped, false) {
                Ok(count) => Ok(count as usize),
                Err(error) => self.after_error(writing, error),
            },
            Err(error) => {
                // Cancel and reap before the OVERLAPPED leaves this frame,
                // whether the deadline expired or the wait itself failed.
                // The stream is deliberately unusable afterwards: a late
                // completion must never be mistaken for a valid frame.
                // SAFETY: this exact operation is pending on this exact overlapped.
                unsafe { CancelIoEx(self.handle.0, &overlapped) };
                let _ = completed(self.handle.0, &overlapped, true);
                self.unusable = true;
                Err(error)
            }
        }
    }

    fn after_error(&mut self, writing: bool, error: io::Error) -> io::Result<usize> {
        match error.raw_os_error().map(|code| code as u32) {
            // A reader treats a closed peer as EOF; everything else, and every
            // write failure, consumes the connection.
            Some(ERROR_BROKEN_PIPE | ERROR_HANDLE_EOF | ERROR_NO_DATA) if !writing => Ok(0),
            _ => {
                self.unusable = true;
                Err(error)
            }
        }
    }
}

impl Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.transfer(false, bytes.as_mut_ptr(), bytes.len())
    }
}

impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.transfer(true, bytes.as_ptr().cast_mut(), bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// One bounded frame, with an absolute deadline for the whole frame.
pub(crate) fn read_frame(stream: &mut Stream) -> io::Result<Vec<u8>> {
    stream.with_deadline(|stream| wire::read_frame(stream, MAX_FRAME))
}

/// One bounded frame, refusing empty or oversized payloads before writing.
pub(crate) fn write_frame(stream: &mut Stream, bytes: &[u8]) -> io::Result<()> {
    stream.with_deadline(|stream| {
        wire::write_frame(stream, bytes)?;
        stream.flush()
    })
}

fn read_capability_file(root: &StorageDirectory) -> io::Result<[u8; CAPABILITY_LEN]> {
    let mut file = root.open_file(CAPABILITY, false)?;
    let mut capability = [0u8; CAPABILITY_LEN];
    file.read_exact(&mut capability)?;
    let mut trailing = [0u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Err(invalid("capability file has trailing bytes"));
    }
    Ok(capability)
}

/// Read the capability a bound endpoint wrote. The private root and the file
/// ACL are both re-verified, so a replaced or inherited file fails closed.
pub(crate) fn read_capability(private_dir: &Path) -> io::Result<[u8; CAPABILITY_LEN]> {
    let root = verify_private_root(private_dir)?;
    read_capability_file(&root)
}

fn verify_peer_sid(pipe: HANDLE) -> io::Result<()> {
    let mut pid = 0;
    // SAFETY: pipe is a connected server handle; pid is a valid out pointer.
    if unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: query-only access is enough to read the client's token.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() || process == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let process = Handle(process);
    let mut token = ptr::null_mut();
    // SAFETY: process is live and token is a valid out pointer.
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Handle(token);
    let client = token_user(token.0)?;
    let current = current_user()?;
    // SAFETY: both SIDs point into live, correctly aligned TOKEN_USER buffers.
    if unsafe { EqualSid(client.sid(), current.sid()) } == 0 {
        Err(denied())
    } else {
        Ok(())
    }
}

fn handshake_server(
    stream: &mut Stream,
    capability: &[u8; CAPABILITY_LEN],
    until: Instant,
) -> io::Result<()> {
    stream.until = Some(until);
    let result = (|| {
        let mut presented = [0u8; CAPABILITY_LEN];
        stream.read_exact(&mut presented)?;
        // Authentication precedes every allocation beyond the fixed prelude
        // and every process/token query.
        if !constant_time_eq(&presented, capability) {
            return Err(denied());
        }
        verify_peer_sid(stream.handle.0)?;
        write_frame(stream, AUTH_ACK)
    })();
    stream.until = None;
    if result.is_err() {
        stream.unusable = true;
    }
    result
}

fn handshake_client(
    stream: &mut Stream,
    capability: &[u8; CAPABILITY_LEN],
    until: Instant,
) -> io::Result<()> {
    stream.until = Some(until);
    let result = (|| {
        stream.write_all(capability)?;
        stream.flush()?;
        let ack = wire::read_frame(stream, AUTH_ACK.len())?;
        if ack != AUTH_ACK {
            return Err(denied());
        }
        Ok(())
    })();
    stream.until = None;
    if result.is_err() {
        stream.unusable = true;
    }
    result
}

fn open_client(name: &str, until: Instant) -> io::Result<Handle> {
    let encoded = wide(name);
    loop {
        // SAFETY: encoded is NUL-terminated; null security/template are allowed.
        let handle = unsafe {
            CreateFileW(
                encoded.as_ptr(),
                windows_sys::Win32::Foundation::GENERIC_READ
                    | windows_sys::Win32::Foundation::GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE && !handle.is_null() {
            return Ok(Handle(handle));
        }
        let error = io::Error::last_os_error();
        let raw = error.raw_os_error().map(|code| code as u32);
        if raw == Some(ERROR_FILE_NOT_FOUND) {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        if raw != Some(ERROR_PIPE_BUSY) {
            return Err(error);
        }
        // Every instance is connected; wait for the bound endpoint to publish
        // another one, but never past the caller's deadline.
        // SAFETY: encoded is NUL-terminated and stays alive for this call.
        let waited = unsafe { WaitNamedPipeW(encoded.as_ptr(), millis_until(until)?) };
        if waited == 0 {
            let error = io::Error::last_os_error();
            return Err(error);
        }
    }
}

/// Connect and authenticate to a bound endpoint in the private directory.
pub(crate) fn connect(
    private_dir: &Path,
    capability: &[u8; CAPABILITY_LEN],
    timeout: Duration,
) -> io::Result<Stream> {
    let until = Instant::now() + timeout;
    let name = pipe_name(private_dir)?;
    let handle = open_client(&name, until)?;
    let mut stream = Stream::new(Pipe(handle), timeout)?;
    handshake_client(&mut stream, capability, until)?;
    Ok(stream)
}

/// The bound endpoint: one reserved instance, one capability, one user.
pub(crate) struct Endpoint {
    name: String,
    capability: [u8; CAPABILITY_LEN],
    /// Holding the bound instance from `bind` reserves the name; `accept`
    /// consumes it and the next accept publishes a fresh instance.
    pipe: Mutex<Option<Pipe>>,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.capability.zeroize();
    }
}

impl Endpoint {
    /// Create the private directory and its capability, then reserve the pipe
    /// name. An existing directory is refused, exactly like the Unix endpoint.
    pub fn bind(private_dir: &Path) -> io::Result<Self> {
        if !private_dir.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "absolute private host directory required",
            ));
        }
        let root = create_private_root(private_dir)?;
        let name = pipe_name(private_dir)?;
        let mut capability = [0u8; CAPABILITY_LEN];
        getrandom::fill(&mut capability)
            .map_err(|error| io::Error::other(format!("host entropy unavailable: {error}")))?;
        root.write_atomic(CAPABILITY, &capability)?;
        let pipe = create_instance(&name, true)?;
        Ok(Self {
            name,
            capability,
            pipe: Mutex::new(Some(pipe)),
        })
    }

    /// Wait for and authenticate one client. Both the wait and the capability
    /// handshake share the caller's absolute deadline.
    pub fn accept(&self, timeout: Duration) -> io::Result<Stream> {
        let until = Instant::now() + timeout;
        let pipe = self.take_instance()?;
        connect_server(&pipe, until)?;
        // Now that this client owns the taken instance, publish a replacement
        // so the name stays reserved through the connection. Best effort:
        // the connected instance still reserves the name until it closes.
        self.reserve_replacement();
        let mut stream = Stream::new(pipe, timeout)?;
        handshake_server(&mut stream, &self.capability, until)?;
        Ok(stream)
    }

    fn take_instance(&self) -> io::Result<Pipe> {
        let mut slot = self.pipe.lock().unwrap_or_else(PoisonError::into_inner);
        match slot.take() {
            Some(pipe) => Ok(pipe),
            None => create_instance(&self.name, false),
        }
    }

    fn reserve_replacement(&self) {
        let replacement = create_instance(&self.name, false);
        let mut slot = self.pipe.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = replacement.ok();
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn private_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "remuda-host-pipe-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn bound(label: &str) -> Option<(Endpoint, std::path::PathBuf, Cleanup)> {
        let path = private_dir(label);
        match Endpoint::bind(&path) {
            Ok(endpoint) => Some((endpoint, path.clone(), Cleanup(path))),
            Err(error) => {
                eprintln!("skipping {label}: cannot create private endpoint: {error}");
                None
            }
        }
    }

    #[test]
    fn capability_and_frame_exchange_round_trip() {
        let Some((endpoint, dir, _cleanup)) = bound("exchange") else {
            return;
        };
        let capability = read_capability(&dir).unwrap();
        let task = std::thread::spawn(move || {
            let mut server = endpoint.accept(Duration::from_secs(2)).unwrap();
            assert_eq!(read_frame(&mut server).unwrap(), b"inspect");
            write_frame(&mut server, b"identity").unwrap();
        });
        let mut client = connect(&dir, &capability, Duration::from_secs(2)).unwrap();
        write_frame(&mut client, b"inspect").unwrap();
        assert_eq!(read_frame(&mut client).unwrap(), b"identity");
        task.join().unwrap();
    }

    #[test]
    fn wrong_capability_is_refused_by_both_sides() {
        let Some((endpoint, dir, _cleanup)) = bound("wrong-capability") else {
            return;
        };
        let mut wrong = read_capability(&dir).unwrap();
        wrong[0] ^= 0xff;
        let task = std::thread::spawn(move || {
            let error = endpoint
                .accept(Duration::from_secs(2))
                .err()
                .expect("refused");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        });
        assert!(connect(&dir, &wrong, Duration::from_secs(2)).is_err());
        task.join().unwrap();
    }

    #[test]
    fn oversize_frame_is_refused_before_payload_and_consumes_stream() {
        let Some((endpoint, dir, _cleanup)) = bound("oversize") else {
            return;
        };
        let capability = read_capability(&dir).unwrap();
        let (sender, receiver) = mpsc::channel();
        let task = std::thread::spawn(move || {
            let server = endpoint.accept(Duration::from_secs(2)).unwrap();
            sender.send(server).unwrap();
        });
        let mut client = connect(&dir, &capability, Duration::from_secs(2)).unwrap();
        let mut server = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        client
            .write_all(&(MAX_FRAME as u32 + 1).to_be_bytes())
            .unwrap();
        let error = read_frame(&mut server).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        // The header alone must decide: no payload was ever sent.
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            read_frame(&mut server).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        task.join().unwrap();
    }

    #[test]
    fn truncated_and_silent_handshakes_are_refused_and_bounded() {
        let Some((endpoint, dir, _cleanup)) = bound("truncated") else {
            return;
        };
        let task = std::thread::spawn(move || {
            assert!(endpoint.accept(Duration::from_millis(500)).is_err());
        });
        let until = Instant::now() + Duration::from_secs(1);
        let name = pipe_name(&dir).unwrap();
        let pipe = Pipe(open_client(&name, until).unwrap());
        let mut raw = Stream::new(pipe, Duration::from_millis(500)).unwrap();
        raw.write_all(b"short").unwrap();
        drop(raw);
        task.join().unwrap();

        let Some((endpoint, dir, _cleanup)) = bound("silent") else {
            return;
        };
        let task = std::thread::spawn(move || {
            let error = endpoint
                .accept(Duration::from_millis(150))
                .err()
                .expect("timeout");
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        });
        let until = Instant::now() + Duration::from_secs(1);
        let name = pipe_name(&dir).unwrap();
        let pipe = Pipe(open_client(&name, until).unwrap());
        let raw = Stream::new(pipe, Duration::from_secs(1)).unwrap();
        task.join().unwrap();
        drop(raw);
    }

    #[test]
    fn second_bind_and_second_first_instance_fail() {
        let Some((endpoint, dir, _cleanup)) = bound("first-instance") else {
            return;
        };
        assert!(Endpoint::bind(&dir).is_err());
        let name = pipe_name(&dir).unwrap();
        assert_eq!(name, endpoint.name());
        assert!(create_instance(&name, true).is_err());
    }

    #[test]
    fn pipe_name_is_stable_and_contains_only_hex() {
        let dir = private_dir("name");
        std::fs::create_dir(&dir).unwrap();
        let cleanup = Cleanup(dir.clone());
        let name = pipe_name(&dir).unwrap();
        let suffix = name
            .strip_prefix(r"\\.\pipe\remuda-session-host-")
            .expect("fixed prefix");
        assert_eq!(suffix.len(), 32);
        assert!(suffix.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(name, pipe_name(&dir).unwrap());
        drop(cleanup);
    }

    #[test]
    fn sddl_and_constant_time_policy_are_exact() {
        assert_eq!(owner_only_sddl("S-1-5-21-1"), "D:P(A;;GA;;;S-1-5-21-1)");
        assert!(constant_time_eq(b"abcdef", b"abcdef"));
        assert!(!constant_time_eq(b"abcdef", b"abcdeg"));
        assert!(!constant_time_eq(b"abcdef", b"abcde"));
        let user = current_user_text().unwrap();
        // Conversion and LocalFree both run; a malformed SID still fails.
        assert!(Descriptor::from_sddl(&owner_only_sddl(&user)).is_ok());
    }

    #[test]
    fn peer_sid_query_fails_closed_on_an_unconnected_pipe() {
        let path = private_dir("peer");
        std::fs::create_dir(&path).unwrap();
        let cleanup = Cleanup(path.clone());
        let name = pipe_name(&path).unwrap();
        let pipe = create_instance(&name, true).unwrap();
        assert!(verify_peer_sid(pipe.raw()).is_err());
        drop(cleanup);
    }
}
