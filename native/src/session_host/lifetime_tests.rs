//! Isolated lifetime-lock evidence; no live panes, daemons or inventories.
use super::*;
use crate::session_host::api::Identity;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn root() -> PathBuf {
    let path = PathBuf::from(if cfg!(target_os = "macos") {
        "/private/tmp"
    } else {
        "/tmp"
    })
    .join(format!(
        "rhl-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
    }
    path
}

fn identity() -> Identity {
    Identity {
        host: [3; 16],
        session: "lifetime-session".into(),
        instance: "lifetime-instance".into(),
    }
}

fn eventually<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < until, "lifetime-lock deadline");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn acquire_records_the_reservation_and_the_locked_file() {
    let root = root();
    let lifetime = acquire(&root, &identity(), [5; 16]).unwrap();
    let binding = lifetime.binding().expect("lifetime lock");
    assert!(binding.confirms(&identity(), [5; 16]));
    assert!(!binding.confirms(&identity(), [6; 16]));
    assert!(lifetime.path_is_bound());
    let bytes = std::fs::read(root.join(FILE)).unwrap();
    let recorded: Binding = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(&recorded, binding);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_lock_file_is_private_and_close_on_exec() {
    let root = root();
    let lifetime = acquire(&root, &identity(), [5; 16]).unwrap();
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(root.join(FILE)).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o600);
        let file = lifetime.held.as_ref().unwrap();
        let flags = unsafe { libc::fcntl(file.file.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_directory_that_already_holds_a_lock_is_refused() {
    let root = root();
    std::fs::write(root.join(FILE), b"stale").unwrap();
    let error = match acquire(&root, &identity(), [5; 16]) {
        Ok(_) => panic!("a stale lock file must be refused"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn probe_reports_a_held_lock_and_released_after_the_holder_drops_it() {
    let root = root();
    let lifetime = acquire(&root, &identity(), [5; 16]).unwrap();
    assert!(matches!(probe(&root, &identity(), [5; 16]), Probe::Held));
    drop(lifetime);
    match probe(&root, &identity(), [5; 16]) {
        Probe::Released(held) => assert!(held.binding().confirms(&identity(), [5; 16])),
        _ => panic!("a dropped lifetime lock must be released"),
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_wrong_or_missing_binding_is_unknown_never_death() {
    let root = root();
    let lifetime = acquire(&root, &identity(), [5; 16]).unwrap();
    drop(lifetime);
    assert!(matches!(probe(&root, &identity(), [6; 16]), Probe::Unknown));
    let mut other = identity();
    other.instance = "another-instance".into();
    assert!(matches!(probe(&root, &other, [5; 16]), Probe::Unknown));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_missing_lock_file_is_unknown() {
    let root = root();
    assert!(matches!(probe(&root, &identity(), [5; 16]), Probe::Unknown));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_replaced_lock_file_is_unknown() {
    let root = root();
    let lifetime = acquire(&root, &identity(), [5; 16]).unwrap();
    let bytes = std::fs::read(root.join(FILE)).unwrap();
    std::fs::remove_file(root.join(FILE)).unwrap();
    std::fs::write(root.join(FILE), bytes).unwrap();
    assert!(!lifetime.path_is_bound());
    match probe(&root, &identity(), [5; 16]) {
        Probe::Unknown => {}
        Probe::Held | Probe::Released(_) => panic!("a replaced file is not evidence"),
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_malformed_or_truncated_record_is_unknown() {
    let root = root();
    let lifetime = acquire(&root, &identity(), [5; 16]).unwrap();
    drop(lifetime);
    std::fs::write(root.join(FILE), b"{\"schema\":1}").unwrap();
    assert!(matches!(probe(&root, &identity(), [5; 16]), Probe::Unknown));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "internal lifetime helper; invoked only by the parent fixture"]
fn holder_fixture() {
    let root = PathBuf::from(std::env::var_os("REMUDA_TEST_LIFETIME_ROOT").unwrap());
    let _held = acquire(&root, &identity(), [5; 16]).unwrap();
    std::fs::write(root.join("held"), b"1").unwrap();
    std::thread::sleep(Duration::from_secs(30));
}

fn holder(root: &Path) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "session_host::lifetime::tests::holder_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("REMUDA_TEST_LIFETIME_ROOT", root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn a_dead_process_releases_its_lock_and_a_live_one_never_does() {
    let root = root();
    let mut holder = holder(&root);
    eventually(|| root.join("held").exists().then_some(()));
    assert!(matches!(probe(&root, &identity(), [5; 16]), Probe::Held));
    holder.kill().unwrap();
    let _ = holder.wait();
    let released = eventually(|| match probe(&root, &identity(), [5; 16]) {
        Probe::Released(held) => Some(held),
        _ => None,
    });
    assert!(released.binding().confirms(&identity(), [5; 16]));
    std::fs::remove_dir_all(root).unwrap();
}
