//! Private bootstrap/controller processes; no live panes or numeric PID signals.
use super::*;
use crate::session_host::api::{Identity, Info, Request, Response, Role};
use crate::session_host::entry::Client;
use crate::session_host::inventory::model::{Birth, Entry, Mutation, Phase, Target};
use std::os::unix::fs::DirBuilderExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::Instant;
static NEXT: AtomicU64 = AtomicU64::new(0);
fn eventually<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < until, "private birth fixture deadline");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn marker(root: &Path, name: &str) {
    crate::fs_atomic::write_atomic_lua_private(&root.join(name), b"1").unwrap();
}
pub(super) fn cut(stage: &str) {
    if std::env::var("REMUDA_TEST_BIRTH_CUT").ok().as_deref() != Some(stage) {
        return;
    }
    let root = PathBuf::from(std::env::var_os("REMUDA_TEST_BIRTH_ROOT").unwrap());
    let actor = std::env::var("REMUDA_TEST_BIRTH_ACTOR").unwrap();
    marker(&root, &format!("cut-{actor}"));
    let until = Instant::now() + Duration::from_secs(8);
    while !root.join(format!("release-{actor}")).exists() {
        if Instant::now() >= until {
            marker(&root, &format!("expired-{actor}"));
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
pub(super) fn ready_fault_once() {
    static USED: AtomicBool = AtomicBool::new(false);
    if std::env::var("REMUDA_TEST_BIRTH_FAULT").ok().as_deref() == Some("2")
        && !USED.swap(true, Ordering::SeqCst)
    {
        crate::session_host::inventory::disk::fail_at(2);
    }
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn process(root: &Path, mode: &str, cut: &str, actor: &str, fault: bool) -> Process {
    Process(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session_host::birth::tests::process_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("REMUDA_TEST_BIRTH_ROOT", root)
            .env("REMUDA_TEST_BIRTH_MODE", mode)
            .env("REMUDA_TEST_BIRTH_CUT", cut)
            .env("REMUDA_TEST_BIRTH_ACTOR", actor)
            .env("REMUDA_TEST_BIRTH_FAULT", if fault { "2" } else { "0" })
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}
struct Done(PathBuf);
impl Drop for Done {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"1");
    }
}
#[test]
#[ignore = "internal private host/controller helper; invoked only by parent fixtures"]
fn process_fixture() {
    let root = PathBuf::from(std::env::var_os("REMUDA_TEST_BIRTH_ROOT").expect("private root"));
    let actor = std::env::var("REMUDA_TEST_BIRTH_ACTOR").unwrap();
    let mode = std::env::var("REMUDA_TEST_BIRTH_MODE").unwrap();
    if mode == "host" {
        marker(&root, &format!("host-start-{actor}"));
        let _done = Done(root.join(format!("done-{actor}")));
        if run(&root.join("h")).is_err() {
            marker(&root, &format!("refused-{actor}"));
        }
        return;
    }
    let bytes = std::fs::read(root.join("bootstrap.json")).unwrap();
    let cut = std::env::var("REMUDA_TEST_BIRTH_CUT").unwrap();
    if cut == "before-spawn" {
        self::cut("before-spawn");
    }
    let mut children = Vec::new();
    for suffix in if mode == "race" {
        vec!["a", "b"]
    } else {
        vec!["a"]
    } {
        let actor = format!("{}-{suffix}", std::process::id());
        let mut host = process(
            &root,
            "host",
            &cut,
            &actor,
            std::env::var("REMUDA_TEST_BIRTH_FAULT").unwrap() == "2",
        );
        super::super::wire::write_frame(&mut host.0.stdin.take().unwrap(), &bytes).unwrap();
        children.push(host);
    }
    // These controlled hosts survive their actual launcher parent. Exact private
    // RPC cleanup stops them; no PID reopen/signal or live daemon is involved.
    for child in children {
        std::mem::forget(child);
    }
    let until = Instant::now() + Duration::from_secs(8);
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
}
struct Fixture {
    root: PathBuf,
    identity: Identity,
}
impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(if cfg!(target_os = "macos") {
            "/private/tmp"
        } else {
            "/tmp"
        })
        .join(format!(
            "rhb-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let identity = Identity {
            host: [9; 16],
            session: "reserved-session".into(),
            instance: "reserved-instance".into(),
        };
        let launch = Launch {name:"synthetic".into(),argv:vec!["/bin/sh".into(),"-c".into(),"printf '%s\\n' \"$$\" >> \"$STARTS\"; printf 'ready\\n'; while IFS= read -r line; do printf 'effect:%s\\n' \"$line\"; done".into()],cwd:root.to_string_lossy().into(),agent:"synthetic".into(),rows:24,cols:80,env:vec![("PATH".into(),"/usr/bin:/bin".into()),("STARTS".into(),root.join("starts").to_string_lossy().into())]};
        let raw = serde_json::to_string(&launch).unwrap();
        let store = Store::create(&root.join("inventory")).unwrap();
        let claim = store
            .commit(
                store.read().unwrap().cursor(),
                None,
                Mutation::Claim { nonce: [1; 16] },
                Duration::ZERO,
            )
            .unwrap();
        let mut entry = Entry::reserved(identity.clone(), [4; 16], &raw).unwrap();
        entry.birth = Some(Birth::new(root.join("h").to_string_lossy().into(), [7; 16]).unwrap());
        store
            .commit(
                claim.next.cursor(),
                Some(claim.fence),
                Mutation::ReserveLaunch {
                    name: "synthetic".into(),
                    entry,
                },
                Duration::ZERO,
            )
            .unwrap();
        let lease = Lease::new(
            store.read().unwrap().cursor().store,
            Target {
                name: "synthetic".into(),
                identity: identity.clone(),
            },
            [4; 16],
            [7; 16],
        );
        let bootstrap = Bootstrap {
            store: root.join("inventory"),
            lease,
            launch: RawValue::from_string(raw).unwrap(),
        };
        crate::fs_atomic::write_atomic_lua_private(
            &root.join("bootstrap.json"),
            &serde_json::to_vec(&bootstrap).unwrap(),
        )
        .unwrap();
        Self { root, identity }
    }
    fn store(&self) -> Store {
        Store::open(&self.root.join("inventory")).unwrap()
    }
    fn controller(&self, cut: &str, race: bool, fault: bool) -> Process {
        process(
            &self.root,
            if race { "race" } else { "controller" },
            cut,
            "controller",
            fault,
        )
    }
    fn starts(&self) -> Vec<u32> {
        std::fs::read_to_string(self.root.join("starts"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.parse().unwrap())
            .collect()
    }
    fn inspect(&self) -> Option<Info> {
        let mut peer =
            Client::open_expected(&self.root.join("h"), Role::View, self.identity.clone()).ok()?;
        match peer.request(&Request::Inspect).ok()? {
            Response::Info { info } => Some(info),
            _ => None,
        }
    }
    fn ready(&self) -> Info {
        eventually(|| {
            if self.store().read().ok()?.entries["synthetic"].phase != Phase::Registered {
                return None;
            }
            self.inspect()
                .filter(|info| info.publication == Some(PublicationStatus::Ready))
        })
    }
    fn actors(&self) -> Vec<String> {
        std::fs::read_dir(&self.root)
            .unwrap()
            .filter_map(|entry| {
                entry
                    .ok()?
                    .file_name()
                    .to_str()?
                    .strip_prefix("host-start-")
                    .map(str::to_owned)
            })
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let actors = self.actors();
            for actor in &actors {
                let _ = std::fs::write(self.root.join(format!("release-{actor}")), b"1");
            }
            let _ = std::fs::write(self.root.join("release-controller"), b"1");
            if actors
                .iter()
                .all(|actor| self.root.join(format!("done-{actor}")).exists())
            {
                break;
            }
            let _ = (|| -> Option<()> {
                let mut peer = Client::open_expected(
                    &self.root.join("h"),
                    Role::Control,
                    self.identity.clone(),
                )
                .ok()?;
                if !matches!(
                    peer.request(&Request::Acquire),
                    Ok(Response::Granted { .. })
                ) {
                    return None;
                }
                let _ = peer.request(&Request::Close);
                let _ = peer.request(&Request::Stop);
                Some(())
            })();
            if Instant::now() >= until {
                eprintln!("private birth cleanup deadline: {}", self.root.display());
                assert!(
                    std::thread::panicking(),
                    "private host did not complete authenticated cleanup"
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn kill(controller: &mut Process) {
    controller.0.kill().unwrap();
    eventually(|| controller.0.try_wait().unwrap());
}
#[test]
fn controller_death_during_each_bootstrap_cut_preserves_one_reserved_child_and_scoped_ready() {
    for cut in ["endpoint", "spawned", "manifest", "ready"] {
        let fixture = Fixture::new();
        let mut controller = fixture.controller(cut, false, false);
        let actor = format!("{}-a", controller.0.id());
        eventually(|| {
            fixture
                .root
                .join(format!("cut-{actor}"))
                .exists()
                .then_some(())
        });
        let before = if cut == "endpoint" {
            None
        } else {
            Some(eventually(|| fixture.starts().first().copied()))
        };
        kill(&mut controller);
        let store = fixture.store();
        store
            .commit(
                store.read().unwrap().cursor(),
                None,
                Mutation::Claim { nonce: [8; 16] },
                Duration::ZERO,
            )
            .unwrap();
        marker(&fixture.root, &format!("release-{actor}"));
        let info = fixture.ready();
        assert_eq!(info.identity, fixture.identity);
        eventually(|| (fixture.starts().len() == 1).then_some(()));
        assert_eq!(info.pid, fixture.starts().first().copied());
        if let Some(before) = before {
            assert_eq!(info.pid, Some(before));
        }
        assert!(!fixture.root.join(format!("expired-{actor}")).exists());
    }
}
#[test]
fn same_reservation_two_host_race_and_lost_ready_ack_never_create_second_child() {
    let fixture = Fixture::new();
    let mut controller = fixture.controller("", true, false);
    let before = fixture.ready();
    let parent = controller.0.id();
    eventually(|| {
        (fixture.root.join(format!("refused-{parent}-a")).exists()
            || fixture.root.join(format!("refused-{parent}-b")).exists())
        .then_some(())
    });
    kill(&mut controller);
    let mut duplicate = fixture.controller("", false, false);
    eventually(|| {
        fixture
            .root
            .join(format!("refused-{}-a", duplicate.0.id()))
            .exists()
            .then_some(())
    });
    kill(&mut duplicate);
    assert_eq!(fixture.starts().len(), 1);
    assert_eq!(fixture.inspect().unwrap().pid, before.pid);
}
#[test]
fn uncertain_ready_replace_preserves_child_and_reconciles_same_lease() {
    let fixture = Fixture::new();
    let mut controller = fixture.controller("", false, true);
    let info = fixture.ready();
    kill(&mut controller);
    assert_eq!(fixture.starts(), vec![info.pid.unwrap()]);
    assert!(fixture.store().read().unwrap().entries["synthetic"]
        .birth
        .is_some());
}
#[test]
fn death_before_host_spawn_leaves_same_reservation_recoverable_without_reallocation() {
    let fixture = Fixture::new();
    let original = fixture.store().read().unwrap().entries["synthetic"]
        .identity
        .clone();
    let mut first = fixture.controller("before-spawn", false, false);
    eventually(|| fixture.root.join("cut-controller").exists().then_some(()));
    kill(&mut first);
    assert!(!fixture.root.join("h").exists());
    assert!(fixture.starts().is_empty());
    let store = fixture.store();
    store
        .commit(
            store.read().unwrap().cursor(),
            None,
            Mutation::Claim { nonce: [8; 16] },
            Duration::ZERO,
        )
        .unwrap();
    let mut second = fixture.controller("", false, false);
    let info = fixture.ready();
    kill(&mut second);
    assert_eq!(info.identity, original);
    assert_eq!(fixture.starts(), vec![info.pid.unwrap()]);
}
#[test]
fn unpublished_host_drains_output_but_refuses_effects_until_scoped_ready() {
    let fixture = Fixture::new();
    let mut controller = fixture.controller("manifest", false, false);
    let actor = format!("{}-a", controller.0.id());
    eventually(|| {
        fixture
            .root
            .join(format!("cut-{actor}"))
            .exists()
            .then_some(())
    });
    let info = eventually(|| fixture.inspect().filter(|info| info.generation > 0));
    assert_eq!(info.publication, Some(PublicationStatus::Starting));
    assert_eq!(
        fixture.store().read().unwrap().entries["synthetic"].phase,
        Phase::Reserved
    );
    let mut peer = Client::open_expected(
        &fixture.root.join("h"),
        Role::Control,
        fixture.identity.clone(),
    )
    .unwrap();
    assert!(
        matches!(peer.request(&Request::Acquire).unwrap(), Response::Error {code} if code == "host_publication_pending")
    );
    assert!(
        matches!(peer.request(&Request::Input {client:[3;16],sequence:1,act:super::super::wire::InputAct::Bytes(b"must-not-run\n".to_vec())}).unwrap(), Response::Error {code} if code == "host_publication_pending")
    );
    marker(&fixture.root, &format!("release-{actor}"));
    fixture.ready();
    assert!(matches!(
        peer.request(&Request::Acquire).unwrap(),
        Response::Granted { .. }
    ));
    assert!(matches!(
        peer.request(&Request::Recover {
            client: [3; 16],
            sequence: 1,
            act: super::super::wire::InputAct::Bytes(b"must-not-run\n".to_vec())
        })
        .unwrap(),
        Response::Outcome {
            state: super::super::api::OutcomeState::New,
            completion: None
        }
    ));
    kill(&mut controller);
}
#[test]
fn wrong_payload_lease_store_or_required_schema_refuses_before_endpoint_and_child() {
    for case in ["payload", "lease", "store", "schema"] {
        let fixture = Fixture::new();
        let mut bootstrap: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture.root.join("bootstrap.json")).unwrap())
                .unwrap();
        match case {
            "payload" => bootstrap["launch"]["agent"] = "changed".into(),
            "lease" => bootstrap["lease"]["nonce"] = serde_json::to_value([8u8; 16]).unwrap(),
            "store" => {
                let path = fixture.root.join("wrong-store");
                drop(Store::create(&path).unwrap());
                bootstrap["store"] = path.to_string_lossy().into_owned().into();
            }
            _ => {
                let store = fixture.store();
                let mut index: serde_json::Value =
                    serde_json::from_slice(&store.read().unwrap().encode().unwrap()).unwrap();
                index["required"]
                    .as_array_mut()
                    .unwrap()
                    .push("future-owner-v2".into());
                crate::fs_atomic::write_atomic_lua_private(
                    &fixture.root.join("inventory/inventory.json"),
                    &serde_json::to_vec(&index).unwrap(),
                )
                .unwrap();
            }
        }
        crate::fs_atomic::write_atomic_lua_private(
            &fixture.root.join("bootstrap.json"),
            &serde_json::to_vec(&bootstrap).unwrap(),
        )
        .unwrap();
        let mut controller = fixture.controller("", false, false);
        let actor = format!("{}-a", controller.0.id());
        eventually(|| {
            fixture
                .root
                .join(format!("refused-{actor}"))
                .exists()
                .then_some(())
        });
        kill(&mut controller);
        assert!(!fixture.root.join("h").exists());
        assert!(fixture.starts().is_empty());
    }
}
#[test]
fn host_stop_retains_unconsumed_durable_outcome_until_exact_checkpoint_ack() {
    use crate::session_host::inventory::model::{
        ConsumerCheckpoint, InputMutation, MetadataUpdate, Namespace,
    };
    use crate::session_host::recovery::{Owner, Progress};
    let fixture = Fixture::new();
    let mut controller = fixture.controller("", false, false);
    fixture.ready();
    kill(&mut controller);
    let target = || Target {
        name: "synthetic".into(),
        identity: fixture.identity.clone(),
    };
    let owner = Owner::claim(fixture.store()).unwrap();
    let key = owner
        .reserve(
            target(),
            [3; 16],
            [4; 16],
            1,
            super::super::wire::InputAct::Bytes(b"once\n".to_vec()),
        )
        .unwrap();
    let completion = eventually(|| {
        match owner
            .resume(target(), key.clone(), &fixture.root.join("h"))
            .unwrap()
        {
            Progress::Complete(result) => Some(result),
            Progress::Pending | Progress::Busy | Progress::Unavailable => None,
            _ => panic!("unexpected original input recovery state"),
        }
    });
    let mut peer = acquire(&fixture);
    assert!(matches!(
        peer.request(&Request::Close).unwrap(),
        Response::Ok
    ));
    eventually(|| {
        fixture
            .inspect()
            .filter(|info| info.publication == Some(PublicationStatus::Complete))
    });
    assert!(
        matches!(peer.request(&Request::Stop).unwrap(), Response::Error {code} if code == "host_retention_pending")
    );
    assert!(
        fixture.inspect().is_some(),
        "host lost its retained result before consumer ACK"
    );
    let store = fixture.store();
    let claim = store
        .commit(
            store.read().unwrap().cursor(),
            None,
            Mutation::Claim { nonce: [8; 16] },
            Duration::ZERO,
        )
        .unwrap();
    store
        .commit(
            claim.next.cursor(),
            Some(claim.fence),
            Mutation::Input {
                target: target(),
                operation: InputMutation::Consume {
                    key,
                    checkpoint: ConsumerCheckpoint {
                        intent: [3; 16],
                        outcome: hash(&serde_json::to_vec(&completion).unwrap()),
                        metadata: MetadataUpdate {
                            namespace: "consumer".into(),
                            expected: None,
                            value: Namespace::new(1, "{}").unwrap(),
                        },
                    },
                },
            },
            Duration::ZERO,
        )
        .unwrap();
    assert!(matches!(
        peer.request(&Request::Stop).unwrap(),
        Response::Ok
    ));
    eventually(|| {
        fixture
            .actors()
            .iter()
            .all(|actor| fixture.root.join(format!("done-{actor}")).exists())
            .then_some(())
    });
}
fn acquire(fixture: &Fixture) -> Client {
    eventually(|| {
        let mut peer = Client::open_expected(
            &fixture.root.join("h"),
            Role::Control,
            fixture.identity.clone(),
        )
        .ok()?;
        matches!(
            peer.request(&Request::Acquire),
            Ok(Response::Granted { .. })
        )
        .then_some(peer)
    })
}
