#![cfg(target_os = "macos")]
#[allow(dead_code)]
#[path = "../examples/real_local_view.rs"]
mod supervisor;
use std::{
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant},
};

#[test]
#[ignore = "fixture invoked by cancellation regression"]
fn fixture_supervisor() {
    assert!(supervisor::mac::run().is_err());
}
#[test]
#[ignore = "fixture invoked by spawn failure regression"]
fn fixture_spawn_failure() {
    assert!(supervisor::mac::run().is_err());
}
#[test]
fn spawn_failure_removes_scratch() {
    let base = PathBuf::from("/private/tmp").join(format!("rgs-fail-{}", std::process::id()));
    std::fs::create_dir(&base).unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "fixture_spawn_failure"])
        .env("REMUDA_GUI_ACCEPTANCE_REMUDA", base.join("missing"))
        .env("REMUDA_GUI_ACCEPTANCE_WORKER", base.join("unused"))
        .env("REMUDA_GUI_ACCEPTANCE_TMPDIR", &base)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
    std::fs::remove_dir(base).unwrap();
}
#[test]
#[ignore = "fixture invoked by cancellation regression"]
#[allow(clippy::zombie_processes)]
fn fixture_command() {
    let kind = std::env::var("REMUDA_GUI_FAKE_KIND").unwrap();
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    let _listener = if kind == "daemon" {
        std::fs::create_dir_all(runtime.join("remuda")).unwrap();
        Some(UnixListener::bind(runtime.join("remuda/default.sock")).unwrap())
    } else {
        None
    };
    let descendant = Command::new("/bin/sh")
        .args(["-c", "trap '' TERM; exec sleep 30"])
        .spawn()
        .unwrap();
    std::fs::write(
        runtime.join(format!("{kind}.pid")),
        format!("{} {}", std::process::id(), descendant.id()),
    )
    .unwrap();
    std::thread::sleep(Duration::from_secs(30));
}
struct Unrelated(Child);
impl Drop for Unrelated {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn wait_file(path: &Path, deadline: Instant) -> Vec<i32> {
    loop {
        if let Ok(value) = std::fs::read_to_string(path) {
            let parsed: Vec<_> = value
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if parsed.len() == 2 {
                return parsed;
            }
        }
        assert!(Instant::now() < deadline, "missing {}", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}
#[test]
fn sigterm_cleans_owned_sessions_during_creation_and_cargo() {
    for phase in ["creation", "cargo"] {
        let base =
            PathBuf::from("/private/tmp").join(format!("rgs-{}-{phase}", std::process::id()));
        std::fs::create_dir(&base).unwrap();
        let remuda = base.join("fake-remuda");
        let cargo = base.join("fake-cargo");
        std::fs::write(&remuda, "#!/bin/sh\ncase \"$1\" in daemon) kind=daemon ;; stop) kind=stop ;; -e) if [ \"$REMUDA_GUI_FAKE_PHASE\" = cargo ]; then exit 0; fi; kind=creation ;; *) exit 1 ;; esac\nREMUDA_GUI_FAKE_KIND=$kind exec \"$REMUDA_GUI_FAKE_BINARY\" --ignored --exact fixture_command\n").unwrap();
        std::fs::write(&cargo, "#!/bin/sh\nREMUDA_GUI_FAKE_KIND=cargo exec \"$REMUDA_GUI_FAKE_BINARY\" --ignored --exact fixture_command\n").unwrap();
        for path in [&remuda, &cargo] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let executable = std::env::current_exe().unwrap();
        let mut unrelated = Unrelated(Command::new("sleep").arg("30").spawn().unwrap());
        let mut supervisor = Command::new(&executable)
            .args(["--ignored", "--exact", "fixture_supervisor"])
            .env("REMUDA_GUI_ACCEPTANCE_REMUDA", &remuda)
            .env("REMUDA_GUI_ACCEPTANCE_WORKER", base.join("unused-gui"))
            .env("REMUDA_GUI_ACCEPTANCE_CARGO", &cargo)
            .env("REMUDA_GUI_ACCEPTANCE_TMPDIR", &base)
            .env("REMUDA_GUI_FAKE_BINARY", &executable)
            .env("REMUDA_GUI_FAKE_PHASE", phase)
            .spawn()
            .unwrap();
        let runtime = base.join(format!("rgtest-{}/runtime", supervisor.id()));
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut owned = wait_file(&runtime.join("daemon.pid"), deadline);
        owned.extend(wait_file(&runtime.join(format!("{phase}.pid")), deadline));
        assert_eq!(
            unsafe { libc::kill(supervisor.id() as i32, libc::SIGTERM) },
            0
        );
        owned.extend(wait_file(&runtime.join("stop.pid"), deadline));
        while supervisor.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "supervisor failed to exit");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            unrelated.0.try_wait().unwrap().is_none(),
            "unrelated process was signaled"
        );
        for pid in owned {
            while unsafe { libc::kill(pid, 0) } == 0 {
                assert!(Instant::now() < deadline, "owned process {pid} remains");
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        assert!(!runtime.exists());
        std::fs::remove_dir_all(base).unwrap();
    }
}
