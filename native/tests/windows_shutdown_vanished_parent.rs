//! A vanished parent in a Windows caller's ancestry is a normal chain end.
#![cfg(windows)]

use remuda_native::daemon;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, Thread32First, Thread32Next,
    PROCESSENTRY32W, TH32CS_SNAPPROCESS, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenThread, ResumeThread, TerminateProcess, WaitForSingleObject, CREATE_SUSPENDED,
    PROCESS_TERMINATE, SYNCHRONIZATION_SYNCHRONIZE, THREAD_SUSPEND_RESUME,
};

#[path = "daemon_support/spawn.rs"]
mod spawn;

/// A test-owned runtime that is removed after the daemon has been reaped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("remuda-ws{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch runtime");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Holds the suspended CLI by process handle so a failed assertion cannot
/// leave a stopped test process behind or accidentally kill a reused PID.
struct ClientProcess(HANDLE);

impl ClientProcess {
    fn open(pid: u32) -> Self {
        let handle =
            unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZATION_SYNCHRONIZE, 0, pid) };
        assert!(!handle.is_null(), "open suspended stop client");
        Self(handle)
    }

    fn wait_for_exit(&self, timeout: Duration) -> bool {
        let millis = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
        unsafe { WaitForSingleObject(self.0, millis) == WAIT_OBJECT_0 }
    }
}

impl Drop for ClientProcess {
    fn drop(&mut self) {
        if unsafe { WaitForSingleObject(self.0, 0) } == WAIT_TIMEOUT {
            unsafe {
                TerminateProcess(self.0, 1);
                WaitForSingleObject(self.0, 1_000);
            }
        }
        unsafe { CloseHandle(self.0) };
    }
}

/// This test is also run in a short-lived child harness. That harness creates
/// remuda suspended, prints its PID, and exits; the main test resumes remuda
/// only after the parent process is gone.
#[test]
fn launch_stop_suspended_for_parent_exit() {
    if std::env::var_os("REMUDA_TEST_LAUNCH_STOP").is_none() {
        return;
    }
    let output = std::env::var_os("REMUDA_TEST_STOP_OUTPUT").expect("stop output path");
    let stdout = std::fs::File::create(&output).expect("create stop output");
    let stderr = stdout.try_clone().expect("clone stop output");
    let child = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .args(["-s", "s", "stop", "-f", "--yes"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .creation_flags(CREATE_SUSPENDED)
        .spawn()
        .expect("create suspended stop client");
    println!("STOP_CHILD {} {}", child.id(), std::process::id());
}

fn resume_primary_thread(process_id: u32) {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    assert_ne!(snapshot, -1isize as HANDLE, "snapshot threads");

    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut thread_id = None;
    unsafe {
        if Thread32First(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32OwnerProcessID == process_id {
                    thread_id = Some(entry.th32ThreadID);
                    break;
                }
                if Thread32Next(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    let thread_id = thread_id.expect("find suspended stop client's primary thread");
    let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
    assert!(!thread.is_null(), "open suspended stop client's thread");
    let previous = unsafe { ResumeThread(thread) };
    unsafe { CloseHandle(thread) };
    assert_ne!(previous, u32::MAX, "resume stop client");
}

fn process_parents() -> std::collections::HashMap<u32, u32> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    assert_ne!(snapshot, -1isize as HANDLE, "snapshot processes");

    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut parents = std::collections::HashMap::new();
    unsafe {
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    parents
}

#[test]
fn stop_cli_succeeds_after_its_parent_exits() {
    let scratch = Scratch::new();
    let mut daemon_process = spawn::Daemon::spawn(&scratch.0);
    let output = scratch.0.join("stop-output.txt");

    let helper = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "launch_stop_suspended_for_parent_exit",
            "--nocapture",
        ])
        .env("REMUDA_TEST_LAUNCH_STOP", "1")
        .env("REMUDA_TEST_STOP_OUTPUT", &output)
        .env("REMUDA_RUNTIME_DIR", &scratch.0)
        .env("HOME", scratch.0.join("home"))
        .env("LOCALAPPDATA", scratch.0.join("home"))
        .env("REMUDA_NO_UPDATE_CHECK", "1")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("REMUDA_DAEMON_ID")
        .env_remove("REMUDA_SESSION_ID")
        .env_remove("REMUDA_SESSION_NAME")
        .output()
        .expect("run the short-lived stop launcher");
    assert!(
        helper.status.success(),
        "launcher failed: {}{}",
        String::from_utf8_lossy(&helper.stdout),
        String::from_utf8_lossy(&helper.stderr)
    );
    let report = String::from_utf8_lossy(&helper.stdout);
    let launch_line = report
        .lines()
        .find_map(|line| line.strip_prefix("STOP_CHILD "))
        .unwrap_or_else(|| panic!("launcher omitted child PIDs: {report}"));
    let mut fields = launch_line.split_whitespace();
    let client_pid = fields
        .next()
        .expect("launcher reports client PID")
        .parse::<u32>()
        .expect("numeric client PID");
    let client_process = ClientProcess::open(client_pid);
    let exited_parent_pid = fields
        .next()
        .expect("launcher reports its PID")
        .parse::<u32>()
        .expect("numeric parent PID");
    assert_ne!(client_pid, exited_parent_pid);
    let parents = process_parents();
    assert_eq!(parents.get(&client_pid), Some(&exited_parent_pid));
    assert!(
        !parents.contains_key(&exited_parent_pid),
        "the stop client's recorded parent still exists"
    );

    // `output()` returned only after the launcher exited. The process was
    // created suspended, so its recorded parent has vanished before its first
    // instruction can connect to the daemon and ask it to shut down.
    resume_primary_thread(client_pid);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stop_output = String::new();
    while !stop_output.contains("stopped the daemon") && Instant::now() < deadline {
        stop_output = std::fs::read_to_string(&output).unwrap_or_default();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        daemon_process.left_on_its_own(),
        "stop was not allowed as an outside caller; output: {stop_output}"
    );
    assert!(
        stop_output.contains("stopped the daemon"),
        "unexpected stop output: {stop_output}"
    );
    assert!(
        client_process.wait_for_exit(Duration::from_secs(5)),
        "the stop CLI did not exit"
    );
}
