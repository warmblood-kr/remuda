//! Synthetic Windows child: prove membership before markers or descendants.
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::JobObjects::{IsProcessInJob, OpenJobObjectW};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).expect("fixture mode");
    if mode == "--uncontained-marker" {
        std::fs::write(Path::new(&args[2]).join("unexpected-child"), b"ran").unwrap();
        return;
    }
    if mode == "--outside-wait" {
        wait(Path::new(&args[2]));
        return;
    }
    // The name is a test observation channel only. No inherited Job handle
    // is used; close this query handle before any marker or child creation.
    let name: Vec<u16> = std::ffi::OsStr::new(&args[2])
        .encode_wide()
        .chain(Some(0))
        .collect();
    let job = unsafe { OpenJobObjectW(0x0004, 0, name.as_ptr()) }; // JOB_OBJECT_QUERY
    assert!(!job.is_null(), "open fixture Job");
    let mut inside = 0;
    let checked = unsafe { IsProcessInJob(GetCurrentProcess(), job, &mut inside) };
    unsafe { CloseHandle(job) };
    assert!(
        checked != 0 && inside != 0,
        "not contained at fixture entry"
    );
    let root = Path::new(&args[3]);
    if mode == "--job-grandchild" {
        std::fs::write(root.join("grandchild.pid"), std::process::id().to_string()).unwrap();
        wait(&root.join("release-grandchild"));
        return;
    }
    assert_eq!(mode, "--job-probe");
    let _grandchild = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--job-grandchild", &args[2], &args[3]])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Exceeds pipe capacity, so an undrained ConPTY cannot reach the marker.
    let mut output = std::io::stdout().lock();
    for _ in 0..512 {
        output.write_all(&[b'x'; 4096]).unwrap();
    }
    output.flush().unwrap();
    drop(output);
    let marker = serde_json::json!({
        "pid": std::process::id(),
        "args": &args[5..],
        "cwd": std::env::current_dir().unwrap(),
        "env": std::env::var("HOST_PROBE").unwrap(),
    });
    std::fs::write(
        root.join("direct.json"),
        serde_json::to_vec(&marker).unwrap(),
    )
    .unwrap();
    std::process::exit(args[4].parse().unwrap());
}
fn wait(release: &Path) {
    let until = Instant::now() + Duration::from_secs(20);
    while !release.exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
}
