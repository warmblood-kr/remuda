//! Native synthetic ConPTY/Job tests. No daemon, real agent or live session.
#![cfg(windows)]
use portable_pty::{native_pty_system, CommandBuilder, PtyPair, PtySize};
use remuda_native::session_host::windows_process::Process;
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    GetHandleInformation, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, IsProcessInJob, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
const FIXTURE: &str = env!("CARGO_BIN_EXE_session_host_fixture");
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "rm-host-win-한글-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::write(self.0.join("release-grandchild"), b"done");
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn job() -> (String, OwnedHandle) {
    let name = format!(
        "Local\\rm-host-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let wide: Vec<u16> = std::ffi::OsStr::new(&name)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), wide.as_ptr()) };
    assert!(!handle.is_null(), "create private fixture Job");
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    assert_ne!(
        unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        },
        0
    );
    let mut flags = 0;
    assert_ne!(
        unsafe { GetHandleInformation(handle.as_raw_handle(), &mut flags) },
        0
    );
    assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
    (name, handle)
}
struct Terminal {
    pair: Option<PtyPair>,
    bytes: Arc<AtomicUsize>,
}
impl Terminal {
    fn new() -> Self {
        let pair = native_pty_system().openpty(PtySize::default()).unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let bytes = Arc::new(AtomicUsize::new(0));
        let count = bytes.clone();
        std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(size) = reader.read(&mut buffer) {
                if size == 0 {
                    break;
                }
                count.fetch_add(size, Ordering::Relaxed);
            }
        });
        Self {
            pair: Some(pair),
            bytes,
        }
    }
    fn spawn(&self, command: CommandBuilder, job: &OwnedHandle) -> Process {
        Process::spawn(
            self.pair.as_ref().unwrap().slave.as_ref(),
            command,
            job.as_handle(),
        )
        .unwrap()
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let pair = self.pair.take();
        let (done, finished) = mpsc::sync_channel(1);
        // Keep the drain thread independent while ClosePseudoConsole runs.
        std::thread::spawn(move || {
            drop(pair);
            let _ = done.send(());
        });
        assert!(
            finished.recv_timeout(Duration::from_secs(5)).is_ok(),
            "ConPTY close blocked despite independent drain"
        );
    }
}
fn command(mode: &str, name: &str, root: &Path) -> CommandBuilder {
    let mut command = CommandBuilder::new(FIXTURE);
    command.args([mode, name, root.to_str().unwrap()]);
    command.cwd(root);
    command.env_clear();
    for key in ["SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.env("HOST_PROBE", "한글 値");
    command
}
fn eventually<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(result) = check() {
            return result;
        }
        assert!(Instant::now() < until, "synthetic fixture timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn descendant(root: &Path, job: &OwnedHandle) -> OwnedHandle {
    let pid: u32 = eventually(|| {
        std::fs::read_to_string(root.join("grandchild.pid"))
            .ok()?
            .parse()
            .ok()
    });
    let handle = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    assert!(!handle.is_null());
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut member = 0;
    assert_ne!(
        unsafe { IsProcessInJob(handle.as_raw_handle(), job.as_raw_handle(), &mut member) },
        0
    );
    assert_ne!(member, 0, "fixture PID was not in its expected Job");
    handle
}
struct Outside(Child);
impl Drop for Outside {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn creation_contains_immediate_descendant_and_retains_unicode_exit259_and_drained_output() {
    let scratch = Scratch::new();
    let terminal = Terminal::new();
    let (name, job) = job();
    let mut outside = Outside(
        Command::new(FIXTURE)
            .arg("--outside-wait")
            .arg(scratch.0.join("release-outside"))
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut cmd = command("--job-probe", &name, &scratch.0);
    cmd.args(["259", "space arg", "", "quote\"slash\\", "한글"]);
    let process = terminal.spawn(cmd, &job);
    let grandchild = descendant(&scratch.0, &job);
    let marker: serde_json::Value = eventually(|| {
        serde_json::from_slice(&std::fs::read(scratch.0.join("direct.json")).ok()?).ok()
    });
    assert_eq!(
        marker["pid"].as_u64(),
        Some(u64::from(process.pid().unwrap()))
    );
    assert_eq!(
        marker["args"],
        serde_json::json!(["space arg", "", "quote\"slash\\", "한글"])
    );
    assert_eq!(marker["cwd"].as_str(), scratch.0.to_str());
    assert_eq!(marker["env"], "한글 値");
    assert_eq!(
        process.wait_timeout(Duration::from_secs(5)).unwrap(),
        Some(259)
    );
    eventually(|| (terminal.bytes.load(Ordering::Relaxed) >= 2 * 1024 * 1024).then_some(()));
    assert_eq!(
        unsafe { WaitForSingleObject(grandchild.as_raw_handle(), 0) },
        WAIT_TIMEOUT
    );
    drop(job);
    assert_eq!(
        unsafe { WaitForSingleObject(grandchild.as_raw_handle(), 5000) },
        WAIT_OBJECT_0
    );
    assert!(
        outside.0.try_wait().unwrap().is_none(),
        "Job close affected an outside process"
    );
}
#[test]
fn checked_direct_termination_observes_real_exit_and_invalid_job_never_runs_child() {
    let scratch = Scratch::new();
    let terminal = Terminal::new();
    let (name, job) = job();
    let process = terminal.spawn(command("--job-grandchild", &name, &scratch.0), &job);
    eventually(|| scratch.0.join("grandchild.pid").exists().then_some(()));
    assert_eq!(process.wait_timeout(Duration::ZERO).unwrap(), None);
    assert_eq!(
        process.terminate_direct(Duration::from_secs(5)).unwrap(),
        Some(1)
    );
    drop(job);
    std::fs::remove_file(scratch.0.join("grandchild.pid")).unwrap();
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
    assert!(!event.is_null());
    let event = unsafe { OwnedHandle::from_raw_handle(event) };
    let mut cmd = CommandBuilder::new(FIXTURE);
    cmd.args(["--uncontained-marker", scratch.0.to_str().unwrap()]);
    cmd.env_clear();
    // Test creation itself, not Process's postcondition cleanup: the probe
    // deliberately has no membership guard that could hide an accidental run.
    let refused = terminal
        .pair
        .as_ref()
        .unwrap()
        .slave
        .spawn_command_in_job(cmd, event.as_handle());
    assert!(
        refused.is_err(),
        "non-Job handle must fail creation, never fallback"
    );
    assert!(!scratch.0.join("unexpected-child").exists());
}
