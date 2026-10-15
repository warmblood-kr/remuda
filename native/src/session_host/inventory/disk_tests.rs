use super::directory::Directory;
use super::disk::{self, Error, Store};
use super::model::{Error as DataError, Fence, Index, Lookup, Mutation, Revision};
use super::model_tests::{act, identity, key, reserve, target};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};
static NEXT: AtomicU64 = AtomicU64::new(0);
#[cfg(unix)]
#[test]
fn renamed_root_keeps_index_lock_and_replacement_anchored_to_original_directory() {
    let root = Scratch::new();
    let moved = Scratch::new();
    let (store, index, fence) = bootstrap(&root.0);
    std::fs::rename(&root.0, &moved.0).unwrap();
    let replacement = Store::create(&root.0).unwrap();
    let other = replacement.read().unwrap().cursor();
    let committed = store
        .commit(index.cursor(), Some(fence), reserve(1, 7), Duration::ZERO)
        .unwrap();
    assert_eq!(
        Store::open(&moved.0).unwrap().read().unwrap().cursor(),
        committed.next.cursor()
    );
    assert_eq!(replacement.read().unwrap().cursor(), other);
    let directory = Directory::open(&moved.0).unwrap();
    let held = directory.lock_file().unwrap();
    held.lock().unwrap();
    assert!(matches!(
        store.commit(
            committed.next.cursor(),
            Some(fence),
            reserve(2, 7),
            Duration::ZERO
        ),
        Err(Error::Busy)
    ));
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "rm-inventory-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn bootstrap(path: &Path) -> (Store, Index, Fence) {
    let store = Store::create(path).unwrap();
    let original = store.read().unwrap();
    let applied = store
        .commit(
            original.cursor(),
            None,
            Mutation::Claim { nonce: [2; 16] },
            Duration::ZERO,
        )
        .unwrap();
    let mut index = applied.next;
    let fence = applied.fence;
    let entry = super::model::Entry::reserved(identity(), [4; 16], "{}").unwrap();
    index = store
        .commit(
            index.cursor(),
            Some(fence),
            Mutation::ReserveLaunch {
                name: "synthetic".into(),
                entry,
            },
            Duration::ZERO,
        )
        .unwrap()
        .next;
    index = store
        .commit(
            index.cursor(),
            Some(fence),
            Mutation::Register(target()),
            Duration::ZERO,
        )
        .unwrap()
        .next;
    (store, index, fence)
}
#[derive(Serialize, Deserialize)]
struct Context {
    expected: Revision,
    fence: Fence,
    mode: String,
}
fn context(root: &Path, context: &Context) {
    crate::fs_atomic::write_atomic_lua_private(
        &root.join("fixture.json"),
        &serde_json::to_vec(context).unwrap(),
    )
    .unwrap();
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn child(root: &Path) -> Process {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "session_host::inventory::disk_tests::child_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env_clear()
        .env("REMUDA_TEST_INVENTORY_ROOT", root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    for key in ["PATH", "SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    Process(command.spawn().unwrap())
}
fn eventually(mut ready: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(8);
    while !ready() {
        assert!(
            Instant::now() < until,
            "private inventory fixture timed out"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
#[ignore = "internal synthetic subprocess helper; invoked only with a private fixture root"]
fn child_fixture() {
    let root = PathBuf::from(std::env::var_os("REMUDA_TEST_INVENTORY_ROOT").expect("private root"));
    let context: Context =
        serde_json::from_slice(&std::fs::read(root.join("fixture.json")).unwrap()).unwrap();
    let store = Store::open(&root).unwrap();
    match context.mode.as_str() {
        "write-then-wait" => {
            store
                .commit(
                    context.expected,
                    Some(context.fence),
                    reserve(1, 7),
                    Duration::from_secs(1),
                )
                .unwrap();
            std::fs::write(root.join("committed"), b"committed").unwrap();
            eventually(|| root.join("release").exists());
            std::fs::write(root.join("caller-ack"), b"ack").unwrap();
        }
        "read" => {
            let index = store.read().unwrap();
            let Lookup::Pending(pending) = index.lookup(&target(), &key(1, 7)).unwrap() else {
                panic!("original reservation absent")
            };
            assert_eq!(pending.act.digest().unwrap(), act().digest().unwrap());
            crate::fs_atomic::write_atomic_lua_private(
                &root.join("recovered.json"),
                &serde_json::to_vec(pending).unwrap(),
            )
            .unwrap();
        }
        "hold" => {
            let file = Directory::open(&root).unwrap().lock_file().unwrap();
            file.lock().unwrap();
            std::fs::write(root.join("locked"), b"held").unwrap();
            eventually(|| root.join("release").exists());
            drop(file);
        }
        _ => panic!("unknown private fixture mode"),
    }
}
#[test]
fn fresh_process_recovers_original_request_after_writer_dies_before_caller_ack() {
    let scratch = Scratch::new();
    let (store, index, fence) = bootstrap(&scratch.0);
    context(
        &scratch.0,
        &Context {
            expected: index.cursor(),
            fence,
            mode: "write-then-wait".into(),
        },
    );
    let mut writer = child(&scratch.0);
    eventually(|| scratch.0.join("committed").exists());
    writer.0.kill().unwrap();
    writer.0.wait().unwrap();
    assert!(!scratch.0.join("caller-ack").exists());
    drop(store);
    context(
        &scratch.0,
        &Context {
            expected: index.cursor(),
            fence,
            mode: "read".into(),
        },
    );
    let mut replacement = child(&scratch.0);
    eventually(|| replacement.0.try_wait().unwrap().is_some());
    assert!(replacement.0.wait().unwrap().success());
    let recovered: super::model::Pending =
        serde_json::from_slice(&std::fs::read(scratch.0.join("recovered.json")).unwrap()).unwrap();
    assert_eq!(recovered.key, key(1, 7));
    assert_eq!(recovered.intent, [7; 16]);
    assert!(
        matches!(recovered.act,super::super::wire::InputAct::InputText(text) if text=="original private payload")
    );
}
#[test]
fn held_os_lock_is_bounded_and_released_by_actual_process_death() {
    let scratch = Scratch::new();
    let (store, index, fence) = bootstrap(&scratch.0);
    context(
        &scratch.0,
        &Context {
            expected: index.cursor(),
            fence,
            mode: "hold".into(),
        },
    );
    let mut holder = child(&scratch.0);
    eventually(|| scratch.0.join("locked").exists());
    let since = Instant::now();
    assert!(matches!(
        store.commit(
            index.cursor(),
            Some(fence),
            reserve(1, 7),
            Duration::from_millis(30)
        ),
        Err(Error::Busy)
    ));
    assert!(since.elapsed() < Duration::from_secs(2));
    holder.0.kill().unwrap();
    holder.0.wait().unwrap();
    let applied = store
        .commit(
            index.cursor(),
            Some(fence),
            reserve(1, 7),
            Duration::from_secs(1),
        )
        .unwrap();
    assert!(matches!(
        applied.next.lookup(&target(), &key(1, 7)).unwrap(),
        Lookup::Pending(_)
    ));
}
#[test]
fn same_store_concurrent_transactions_have_one_exact_revision_winner() {
    let scratch = Scratch::new();
    let store = Arc::new(Store::create(&scratch.0).unwrap());
    let expected = store.read().unwrap().cursor();
    let start = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for nonce in [1, 2] {
        let store = store.clone();
        let start = start.clone();
        workers.push(std::thread::spawn(move || {
            start.wait();
            store.commit(
                expected,
                None,
                Mutation::Claim { nonce: [nonce; 16] },
                Duration::from_secs(2),
            )
        }));
    }
    start.wait();
    let mut won = 0;
    let mut stale = 0;
    for worker in workers {
        match worker.join().unwrap() {
            Ok(_) => won += 1,
            Err(Error::Data(DataError::StaleRevision)) => stale += 1,
            Err(error) => panic!("{error:?}"),
        }
    }
    assert_eq!((won, stale), (1, 1));
    assert_eq!(store.read().unwrap().cursor().number, 1);
}
#[test]
fn failed_commit_never_returns_admission_permission_and_reread_distinguishes_cuts() {
    let scratch = Scratch::new();
    let (store, index, fence) = bootstrap(&scratch.0);
    disk::fail_at(1);
    assert!(matches!(
        store.commit(index.cursor(), Some(fence), reserve(1, 7), Duration::ZERO),
        Err(Error::CommitUnknown(_))
    ));
    assert_eq!(store.read().unwrap().cursor(), index.cursor());
    disk::fail_at(2);
    assert!(matches!(
        store.commit(index.cursor(), Some(fence), reserve(1, 7), Duration::ZERO),
        Err(Error::CommitUnknown(_))
    ));
    let reopened = Store::open(&scratch.0).unwrap().read().unwrap();
    assert_eq!(reopened.cursor().number, index.cursor().number + 1);
    assert!(matches!(
        reopened.lookup(&target(), &key(1, 7)).unwrap(),
        Lookup::Pending(_)
    ));
}
#[test]
fn corrupt_missing_or_incompatible_existing_root_is_never_initialized_as_empty() {
    let scratch = Scratch::new();
    let (store, _, _) = bootstrap(&scratch.0);
    assert!(Store::create(&scratch.0).is_err());
    drop(store);
    Directory::open(&scratch.0).unwrap().replace(b"{").unwrap();
    let error = Store::open(&scratch.0).err();
    assert!(
        matches!(error, Some(Error::Data(DataError::Corrupt))),
        "unexpected malformed-index refusal: {error:?}"
    );
    std::fs::remove_file(scratch.0.join("inventory.json")).unwrap();
    assert!(Store::open(&scratch.0).is_err());
    assert!(Store::create(&scratch.0).is_err());
}
#[cfg(unix)]
#[test]
fn private_index_refuses_aliases_symlinks_fifos_and_loose_modes_without_repair() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let scratch = Scratch::new();
    let (store, _, _) = bootstrap(&scratch.0);
    drop(store);
    let file = scratch.0.join("inventory.json");
    let saved = scratch.0.join("saved.json");
    std::fs::hard_link(&file, &saved).unwrap();
    assert!(Store::open(&scratch.0).is_err());
    std::fs::remove_file(&saved).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(Store::open(&scratch.0).is_err());
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o644
    );
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::rename(&file, &saved).unwrap();
    symlink(&saved, &file).unwrap();
    assert!(Store::open(&scratch.0).is_err());
    std::fs::remove_file(&file).unwrap();
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(file.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let before = Instant::now();
    assert!(Store::open(&scratch.0).is_err());
    assert!(before.elapsed() < Duration::from_secs(1));
}
